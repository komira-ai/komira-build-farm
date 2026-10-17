//! What a failed run's tails cost the farm (`docs/design/ui-data.md` 5.3 and Q21, phase U3):
//! nothing a `Result`, the recorder or another run waits for. Over the in-process farm with a
//! fake daemon, and a blob source whose reads can be made never to answer. Each test names
//! the defect it catches.

mod support;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::REQUEST_METADATA_HEADER;
use kbf_history::records::TailOmitted;
use kbf_history::{HistoryIndex, HistoryRoot, IndexRoot, Inline, Limits, Reader, RunId, RunView};
use kbf_invocation::{InvocationId, RecordKey};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{ActionResult, ExecuteRequest, RequestMetadata, ToolDetails};
use kbf_server::history::{BlobReadError, BlobReads, Options, TailOptions, over};
use kbf_server::http_blobs::ReadBudget;
use kbf_server::{EpochPolicy, StateStore};
use kbf_state::ManualClock;
use kbf_state::Store;
use kbf_state::fake::{Fake, Flaw};
use kbf_types::Digest;
use prost::Message as _;
use support::{Blob, Cell, FakeDaemon, Job, done, ran};
use tonic::Streaming;
use tonic::metadata::MetadataValue;

const BUILD: &str = "0b8c2f0e-6d1a-4c55-9a7e-2f43d1c0a9b4";

fn root() -> HistoryRoot {
    HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root")
}

/// An object store in memory the writer and the test's reader share.
#[derive(Clone)]
struct Objects(Arc<MemoryStore>);

impl ObjectStore for Objects {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.list(prefix, after, max_keys).await
    }
}

/// Blobs the tail workers read in place of the cache: those of 100 bytes never answer; the
/// rest are not held.
struct StallsOnHundred;

impl BlobReads for StallsOnHundred {
    fn read<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, BlobReadError>> {
        Box::pin(async move {
            if digest.size_bytes == 100 {
                std::future::pending::<()>().await;
            }
            Err(BlobReadError("not held".to_owned()))
        })
    }
}

/// Blobs the tail workers read slowly: every read answers, after a pause, with the same log.
struct Slow(Duration);

impl BlobReads for Slow {
    fn read<'a>(&'a self, _: &'a Digest) -> BoxFuture<'a, Result<Bytes, BlobReadError>> {
        Box::pin(async move {
            tokio::time::sleep(self.0).await;
            Ok(Bytes::from_static(b"a slow log"))
        })
    }
}

fn tagged<T>(id: &str, message: T) -> tonic::Request<T> {
    let metadata = RequestMetadata {
        tool_details: Some(ToolDetails {
            tool_name: "buck2".to_owned(),
            tool_version: "0.1.0".to_owned(),
        }),
        tool_invocation_id: id.to_owned(),
        ..Default::default()
    };
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert_bin(
        REQUEST_METADATA_HEADER,
        MetadataValue::from_bytes(&metadata.encode_to_vec()),
    );
    request
}

async fn execute(cell: &Cell, id: &str, job: &Job) -> Streaming<Operation> {
    let request = ExecuteRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(job.action.proto.clone()),
        ..Default::default()
    };
    ExecutionClient::new(cell.channel())
        .execute(tagged(id, request))
        .await
        .expect("Execute")
        .into_inner()
}

/// Runs a new job `text` as invocation `id`, answered by `daemon` with a result that exits
/// `exit_code` and names `stdout` for its log; the job and its run id.
async fn run(
    cell: &Cell,
    daemon: &mut FakeDaemon,
    text: &str,
    stdout: &Blob,
    exit_code: i32,
) -> (Job, [u8; 12]) {
    let job = Job::new(text, &[]);
    cell.upload(&job.blobs()).await;
    cell.upload(&[stdout]).await;
    let mut ops = execute(cell, BUILD, &job).await;
    let start = daemon.start().await;
    let result = ActionResult {
        exit_code,
        stdout_digest: Some(stdout.proto.clone()),
        ..Default::default()
    };
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;
    // A recorder stuck behind the tap would never catch up.
    tokio::time::timeout(support::PROMPT, cell.recorder.caught_up())
        .await
        .expect("the recorder keeps up");
    let record = cell
        .recorder
        .get(&RecordKey::owner(InvocationId::new(BUILD)))
        .expect("a record");
    let row = record
        .rows()
        .iter()
        .find(|r| r.action == job.action.digest)
        .expect("a row");
    let id = row.attempts[0]
        .run_id
        .expect("an id minted for the attempt");
    (job, id)
}

/// The run `id` as history holds it, once the writer has put its chunk there.
async fn resolved(reader: &Reader<Objects>, id: [u8; 12]) -> RunView {
    let id = RunId::from_bytes(id);
    for _ in 0..500 {
        if let Ok(Some(view)) = reader.run(id, &|_| true).await {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("run {id} never reached history");
}

/// Catches (mutant "read the tails inline in the tap"): a log read that never answers
/// stopping the recorder (its task runs the tap: no later run of any build would be
/// recorded), a Result held for it, a run held back for it, and a read the budget cannot
/// serve in time waiting for ever or failing the run. Of two failed runs, the first reads a
/// log that never answers and holds the whole read budget; the second finds the budget busy.
/// Both Results are acknowledged, the recorder goes on (a third run, which passes, is
/// recorded and arrives), and the second run arrives in history with `busy` tails. The first
/// does not arrive: its read is the pool's, not the farm's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_blob_read_never_delays_a_result_and_the_run_arrives_with_busy() {
    let objects = Objects(Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    })));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let options = Options {
        step_every: Duration::from_millis(20),
        limits: Limits {
            chunk_age_ms: 0,
            ..Limits::default()
        },
        tails: TailOptions {
            wait: Duration::from_millis(50),
            ..TailOptions::default()
        },
        ..Options::default()
    };
    // The source given here stays: the server gives its own cache's only to a history that
    // was given none.
    let history = over(
        objects.clone(),
        root(),
        Arc::clone(&store),
        IndexRoot::default(),
        options,
    )
    .with_blobs(Arc::new(StallsOnHundred), ReadBudget::new(100));
    let index = HistoryIndex::new(Arc::clone(&store), IndexRoot::default(), Arc::new(Inline));
    let reader = Reader::new(objects, root(), index);
    let cell = Cell::start_with_state(StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: Some(history),
    })
    .await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    // The first run's log is 100 bytes: its read never ends, and holds all 100 of the budget.
    let (_, one) = run(&cell, &mut daemon, "stalls", &Blob::new(vec![b'x'; 100]), 2).await;
    // The second's is 10 bytes, and the budget is not free.
    let (_, two) = run(&cell, &mut daemon, "waits", &Blob::new(vec![b'y'; 10]), 2).await;
    let view = resolved(&reader, two).await;
    let failed = view.failed.expect("a failed-run section");
    let tail = failed.stdout.expect("a stdout tail record");
    assert_eq!(tail.omitted(), TailOmitted::Busy);
    assert_eq!((tail.bytes.len(), tail.total_bytes), (0, 10));
    // The recorder is not stuck: a run after both, that passes, is recorded and arrives.
    let (_, three) = run(&cell, &mut daemon, "after", &Blob::new(vec![b'z'; 5]), 0).await;
    assert!(resolved(&reader, three).await.failed.is_none());
    let first = reader
        .run(RunId::from_bytes(one), &|_| true)
        .await
        .expect("a read");
    assert!(
        first.is_none(),
        "the run whose read never ends is in the pool"
    );
}

/// Catches (mutant "a stop does not wait for the pool"): a failed run whose tails were being
/// read when the server stopped lost with them (the writer's last step would run before the
/// run reached its queue), and a stop that waits for ever for a pool that is slow. The log
/// takes 300 ms to read, and the server stops at once: the run is in the chunk the last step
/// flushed, with its tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_waits_for_the_failed_runs_in_the_pool() {
    let objects = Objects(Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    })));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let history = over(
        objects.clone(),
        root(),
        Arc::clone(&store),
        IndexRoot::default(),
        Options {
            // A chunk is cut at its age (two minutes) or at a close: only the stop's last
            // step, with every open invocation closed, flushes this one.
            step_every: Duration::from_millis(20),
            // The last step reserves a block of run ids as well as writing the rows.
            limits: Limits {
                txns_per_step: 4,
                ..Limits::default()
            },
            ..Options::default()
        },
    )
    .with_blobs(
        Arc::new(Slow(Duration::from_millis(300))),
        ReadBudget::default(),
    );
    let index = HistoryIndex::new(Arc::clone(&store), IndexRoot::default(), Arc::new(Inline));
    let reader = Reader::new(objects, root(), index);
    let cell = Cell::start_with_state(StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: Some(history),
    })
    .await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let (_, id) = run(
        &cell,
        &mut daemon,
        "slow",
        &Blob::new(b"a slow log".to_vec()),
        2,
    )
    .await;
    let _restarted = cell.cold_restart().await;
    let view = resolved(&reader, id).await;
    let tail = view
        .failed
        .expect("a failed-run section")
        .stdout
        .expect("a tail");
    assert_eq!(tail.bytes, b"a slow log");
}
