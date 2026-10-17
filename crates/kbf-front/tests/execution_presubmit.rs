//! What an Execute does before its first stream item is measured and, when it is slow,
//! logged: the time from the handler's start to the first `QUEUED` operation, split into
//! the input-tree walk and the scheduler's submit, with the number of directories read
//! (issue komira-ai/komira-build-farm#666). On the A8 cold build about 500 Executes
//! stayed in this window for minutes while 40 of 88 slots were idle, and nothing in
//! kbf's metrics or logs said so.
//!
//! The service is called in process on a paused tokio clock, so a GET's 100 ms is
//! virtual time and the timings below are exact. This binary owns the global tracing
//! subscriber, which the log assertions read.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::{
    Cache, Dispatch, ExecutionService, MemoryMetaLog, Stage, Submission, Ticket, VerifiedBlob,
};
use kbf_meta::Retention;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::reapi::execution_server::Execution;
use kbf_proto::reapi::{Action, Command, Directory, DirectoryNode, ExecuteRequest, FileNode};
use kbf_types::Digest;
use prost::Message;
use tokio::sync::watch;
use tonic::{Code, Request, Status};

/// How long each store GET takes.
const GET: Duration = Duration::from_millis(100);

/// A memory bucket whose range GETs take `delay`.
struct Store {
    inner: MemoryStore,
    delay: Duration,
    gets: AtomicUsize,
}

impl ObjectStore for Store {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.inner.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

/// A dispatch that queues every submission and never runs it.
#[derive(Default)]
struct Queue {
    submitted: Mutex<Vec<watch::Sender<Stage>>>,
}

impl Dispatch for Queue {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let (tx, stage) = watch::channel(Stage::Queued);
        let mut held = self
            .submitted
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let name = format!("operations/{}", held.len());
        held.push(tx);
        Ok(Ticket {
            name,
            instance: submission.request.key.instance,
            action: submission.request.key.action,
            stage,
        })
    }

    fn wait(&self, _name: &str) -> Option<Ticket> {
        None
    }
}

type Service = ExecutionService<Queue>;

struct Rig {
    cache: Arc<Cache<MemoryMetaLog, Store>>,
    service: Arc<Service>,
}

impl Rig {
    async fn new(delay: Duration) -> Self {
        let store = Store {
            inner: MemoryStore::new(Capabilities::default()),
            delay,
            gets: AtomicUsize::new(0),
        };
        let log = MemoryMetaLog::new(Retention::default());
        let cache = Arc::new(
            Cache::open(log, store, KeyPrefix::default())
                .await
                .expect("open"),
        );
        let (_, closing) = kbf_front::closing();
        let service = ExecutionService::with_authorizers(
            Arc::clone(&cache),
            Arc::new(Queue::default()),
            closing,
            Arc::new(Authorizers::allow_all()),
        );
        Self {
            cache,
            service: Arc::new(service),
        }
    }

    async fn store(&self, blobs: &[&Blob]) {
        let blobs = blobs.iter().map(|b| b.verified.clone()).collect();
        self.cache.store_blobs(blobs).await.expect("store");
    }
}

struct Blob {
    verified: VerifiedBlob,
    digest: Digest,
}

fn blob(data: Vec<u8>) -> Blob {
    let verified = VerifiedBlob::hashed(Bytes::from(data));
    let digest = verified.digest();
    Blob { verified, digest }
}

fn proto(blob: &Blob) -> kbf_proto::reapi::Digest {
    kbf_proto::reapi::Digest {
        hash: blob.digest.hash_hex(),
        size_bytes: i64::try_from(blob.digest.size_bytes).expect("size"),
    }
}

/// An action over a tree of a root and one subdirectory, each holding a file; `absent`
/// leaves the subdirectory's file out of the CAS.
struct Job {
    action: Blob,
    blobs: Vec<Blob>,
}

fn job(name: &str, uploaded_file: bool) -> Job {
    let file = blob(format!("file of {name}").into_bytes());
    let top = blob(format!("top of {name}").into_bytes());
    let sub = blob(
        Directory {
            files: vec![FileNode {
                name: "f".to_owned(),
                digest: Some(proto(&file)),
                ..Default::default()
            }],
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let root = blob(
        Directory {
            files: vec![FileNode {
                name: "t".to_owned(),
                digest: Some(proto(&top)),
                ..Default::default()
            }],
            directories: vec![DirectoryNode {
                name: "sub".to_owned(),
                digest: Some(proto(&sub)),
            }],
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let command = blob(
        Command {
            arguments: vec![name.to_owned()],
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let action = blob(
        Action {
            command_digest: Some(proto(&command)),
            input_root_digest: Some(proto(&root)),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let mut blobs = vec![
        action.verified.clone(),
        command.verified,
        root.verified,
        sub.verified,
    ]
    .into_iter()
    .map(|verified| {
        let digest = verified.digest();
        Blob { verified, digest }
    })
    .collect::<Vec<_>>();
    blobs.push(top);
    if uploaded_file {
        blobs.push(file);
    }
    Job { action, blobs }
}

fn execute(rig: &Rig, job: &Job) -> tokio::task::JoinHandle<Result<(), Status>> {
    let service = Arc::clone(&rig.service);
    let request = ExecuteRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(proto(&job.action)),
        ..Default::default()
    };
    tokio::spawn(async move { service.execute(Request::new(request)).await.map(|_| ()) })
}

/// Where the global subscriber writes.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The lines logged so far, the global subscriber installed on first use.
fn log_lines() -> Vec<String> {
    static LOG: OnceLock<Log> = OnceLock::new();
    let log = LOG.get_or_init(|| {
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");
        log
    });
    let bytes = log.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
    String::from_utf8(bytes)
        .expect("utf-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Catches: Execute time before its first operation that no metric shows (the A8 stall
/// was invisible); a presubmit timer that stops before the walk or the submit, or starts
/// after the action read (the action, the command and the two directories are four
/// 100 ms GETs, so the sum must be 400 ms and the walk's 200 ms); a walk timer around
/// the wrong span; a directory count that counts files or the root twice (two
/// directories); a gauge that is not up while the call is inside the window or stays up
/// after it; a submit that is not timed.
#[tokio::test(start_paused = true)]
async fn the_time_before_the_first_operation_is_split_into_walk_and_submit() {
    let _ = log_lines();
    let rig = Rig::new(GET).await;
    let job = job("timed", true);
    rig.store(&job.blobs.iter().collect::<Vec<_>>()).await;
    let m = rig.cache.metrics();

    let call = execute(&rig, &job);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(m.execute_presubmit_in_flight.get(), 1, "inside the window");
    assert_eq!(
        m.execute_presubmit.count(),
        0,
        "not observed before it ends"
    );
    call.await.expect("joined").expect("Execute");

    assert_eq!(m.execute_presubmit_in_flight.get(), 0);
    assert_eq!(m.execute_presubmit.count(), 1);
    assert_eq!(m.execute_presubmit.sum(), 400_000_000, "four GETs");
    assert_eq!(m.execute_walk.count(), 1);
    assert_eq!(m.execute_walk.sum(), 200_000_000, "the two directories");
    assert_eq!(m.execute_dir_reads.get(), 2);
    assert_eq!(m.execute_submit.count(), 1);
    assert_eq!(
        m.execute_submit.sum(),
        0,
        "the scripted dispatch takes no time"
    );
}

/// Catches: an Execute that fails in the window (inputs missing: the likeliest to be
/// slow, a client retrying it) left out of the histogram and the gauge, so the
/// stalled calls are exactly the ones not shown; and a walk observed when it did not
/// finish.
#[tokio::test(start_paused = true)]
async fn an_execute_that_fails_before_its_first_operation_is_still_measured() {
    let rig = Rig::new(GET).await;
    let job = job("missing", false);
    rig.store(&job.blobs.iter().collect::<Vec<_>>()).await;
    let m = rig.cache.metrics();

    let status = execute(&rig, &job)
        .await
        .expect("joined")
        .expect_err("missing");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(m.execute_presubmit.count(), 1);
    assert_eq!(m.execute_presubmit.sum(), 400_000_000);
    assert_eq!(m.execute_presubmit_in_flight.get(), 0);
    assert_eq!(m.execute_submit.count(), 0, "never reached the scheduler");
}

/// Catches: no warning for an Execute that spent 10 s or more before its first
/// operation (the operator's only signal short of a scrape), one that omits the action,
/// the walk or the directory count the operator reads the cause from, and one for a
/// fast Execute. At 6 s a GET, the four GETs take 24 s.
#[tokio::test(start_paused = true)]
async fn an_execute_slow_before_its_first_operation_warns_with_the_phases() {
    // Install the subscriber before the Execute logs; without this the warning can be
    // emitted before any test of this binary has read the log (it passed only when a
    // sibling test ran first).
    let _ = log_lines();
    let slow = Rig::new(Duration::from_secs(6)).await;
    let slow_job = job("slow one", true);
    slow.store(&slow_job.blobs.iter().collect::<Vec<_>>()).await;
    let fast = Rig::new(Duration::from_millis(1)).await;
    let fast_job = job("fast one", true);
    fast.store(&fast_job.blobs.iter().collect::<Vec<_>>()).await;

    execute(&fast, &fast_job)
        .await
        .expect("joined")
        .expect("Execute");
    execute(&slow, &slow_job)
        .await
        .expect("joined")
        .expect("Execute");

    let lines = log_lines();
    let action = slow_job.action.digest;
    let [line] = lines
        .iter()
        .filter(|l| l.contains(&format!("action={action}")))
        .collect::<Vec<_>>()[..]
    else {
        panic!("one warning for the slow action expected in {lines:?}");
    };
    assert!(line.contains("WARN"), "{line}");
    for field in [
        "presubmit_ms=24000",
        "walk_ms=12000",
        "submit_ms=0",
        "dir_reads=2",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
    let fast_action = fast_job.action.digest;
    assert!(
        !lines.iter().any(|l| l.contains(&format!("{fast_action}"))),
        "a fast Execute warned: {lines:?}"
    );
}
