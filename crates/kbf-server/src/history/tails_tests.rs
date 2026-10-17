//! The tail workers: what a failed run is offered with, and that nothing they wait for can
//! delay or fail anything else. Each test names the defect it catches. Time is the paused
//! clock's, so no test measures a real wait.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_history::records::{
    FLOG_STREAM_BYTES, FlogOmitted, FlogSummary, RunOutcome, RunRecord, TAIL_BYTES, TailOmitted,
    WireDigest,
};
use kbf_history::{
    Event, EventQueue, FlogStore, HistoryRoot, InvRef, QueueLimits, RunId, StreamId, WriterId,
    container,
};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::reapi::{Command, command::EnvironmentVariable};
use kbf_types::Digest;
use prost::Message as _;

use super::{
    BlobReadError, BlobReads, COMMAND_READ_BYTES, FlogSink, Job, Parts, RESULT_COPY_BYTES,
    TAIL_READ_BYTES, TailOptions, Tails, Workers, pool,
};
use crate::history::reads::Shared;
use crate::history::stats::Stats;
use crate::history::tap::OpenSet;
use crate::http_blobs::ReadBudget;

/// The blobs the workers read: bytes by digest, with counts, an optional stall and the most
/// bytes ever being read at once.
#[derive(Default)]
struct Blobs {
    held: Mutex<BTreeMap<Digest, Bytes>>,
    /// Digests whose read never returns.
    stall: Mutex<BTreeSet<Digest>>,
    reads: AtomicU64,
    /// Bytes being read now, and the most there have been at once.
    now: AtomicUsize,
    most: AtomicUsize,
    /// Each read waits this long (on the paused clock) before it answers.
    delay: Mutex<Duration>,
}

impl Blobs {
    fn put(&self, bytes: &[u8]) -> WireDigest {
        let d = kbf_segments::sha256(bytes);
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(d, Bytes::copy_from_slice(bytes));
        WireDigest::new(d.hash, d.size_bytes)
    }

    fn stall(&self, d: &WireDigest) {
        let d = Digest::new(
            kbf_types::DigestFunction::Sha256,
            <[u8; 32]>::try_from(d.hash.as_slice()).expect("a hash"),
            d.size,
        );
        self.stall
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(d);
    }
}

impl BlobReads for Blobs {
    fn read<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, BlobReadError>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let size = usize::try_from(digest.size_bytes).unwrap_or(usize::MAX);
            let now = self.now.fetch_add(size, Ordering::SeqCst) + size;
            self.most.fetch_max(now, Ordering::SeqCst);
            let stalled = self
                .stall
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(digest);
            if stalled {
                std::future::pending::<()>().await;
            }
            let delay = *self.delay.lock().unwrap_or_else(PoisonError::into_inner);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            self.now.fetch_sub(size, Ordering::SeqCst);
            let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            held.get(digest)
                .cloned()
                .ok_or_else(|| BlobReadError("not held".to_owned()))
        })
    }
}

/// An object store that counts writes and can be made to fail them.
struct Objects {
    memory: MemoryStore,
    puts: AtomicU64,
    fail: std::sync::atomic::AtomicBool,
}

impl Objects {
    fn new() -> Arc<Objects> {
        Arc::new(Objects {
            memory: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            puts: AtomicU64::new(0),
            fail: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

impl ObjectStore for Objects {
    fn capabilities(&self) -> Capabilities {
        self.memory.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Protocol("planted failure".into()));
        }
        self.memory.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.memory.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.memory.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.memory.list(prefix, after, max_keys).await
    }
}

fn root() -> HistoryRoot {
    HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root")
}

struct Rig {
    tails: Tails,
    queue: Arc<EventQueue>,
    stats: Arc<Stats>,
    blobs: Arc<Blobs>,
    objects: Arc<Objects>,
    open: Arc<OpenSet>,
    budget: ReadBudget,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for h in &self.handles {
            h.abort();
        }
    }
}

fn rig(options: TailOptions, budget: usize) -> Rig {
    rig_with(options, budget, QueueLimits::default())
}

fn rig_with(options: TailOptions, budget: usize, limits: QueueLimits) -> Rig {
    let queue = Arc::new(EventQueue::new(limits));
    let stats = Arc::new(Stats::new(Arc::clone(&queue)));
    let blobs = Arc::new(Blobs::default());
    let objects = Objects::new();
    let open = Arc::new(OpenSet::default());
    let budget = ReadBudget::new(budget);
    let flog = Arc::new(FlogStore::new(
        Shared(Arc::clone(&objects)),
        root(),
        WriterId(1),
    ));
    let parts = Parts {
        queue: Arc::clone(&queue),
        blobs: blobs.clone(),
        budget: budget.clone(),
        flog,
        stats: Arc::clone(&stats),
        open: Arc::clone(&open),
    };
    let (tails, workers): (Tails, Workers) = pool(parts, options);
    let handles = workers.spawn();
    Rig {
        tails,
        queue,
        stats,
        blobs,
        objects,
        open,
        budget,
        handles,
    }
}

/// Queues `job` as the recorder's tap does: its invocation is open.
fn submit(rig: &Rig, job: Job) -> Result<(), Job> {
    if let Some(name) = &job.inv.record_id {
        rig.open.add(name);
    }
    rig.tails.submit(job)
}

fn options() -> TailOptions {
    TailOptions::default()
}

fn id(n: u32) -> RunId {
    RunId::new(1_792_108_800_000 + 5, StreamId(0x80_0001), n)
}

/// A failed run of invocation `name` that left `stdout`, `stderr`, `result` and `command`.
fn job(name: &str, n: u32, logs: [Option<&WireDigest>; 4]) -> Job {
    let [stdout, stderr, result, command] = logs;
    let mut record = RunRecord {
        instance: "main".to_owned(),
        outcome: RunOutcome::Failed.into(),
        exit_code: 1,
        stdout: stdout.cloned(),
        stderr: stderr.cloned(),
        result: result.cloned(),
        command: command.cloned(),
        ..RunRecord::default()
    };
    record.set_run_id(id(n));
    Job {
        inv: InvRef::named(name),
        record: Box::new(record),
        stream_opened_ms: 7,
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// The events the queue holds, oldest first.
fn events(rig: &Rig) -> Vec<Event> {
    rig.queue.drain()
}

/// The run event among `events`, with its failed section.
fn run_of(events: &[Event]) -> (&RunRecord, &kbf_history::records::FailedRun) {
    events
        .iter()
        .find_map(|e| match e {
            Event::Run {
                record,
                failed: Some(f),
                ..
            } => Some((&**record, f)),
            _ => None,
        })
        .expect("a run with a failed section")
}

/// The summary of the failure-log object `event` names.
fn summary_of(rig: &Rig, event: &Event) -> FlogSummary {
    let Event::Flog(f) = event else {
        panic!("a flog event, got {event:?}");
    };
    let object = futures::executor::block_on(
        rig.objects
            .get_range(&f.key, ByteRange::new(0, f.bytes).expect("a range")),
    )
    .expect("the object is there");
    let (_, footer) = container::footer_of(&object).expect("a footer");
    FlogSummary::decode(footer.summary.as_slice()).expect("a summary")
}

fn command_blob(blobs: &Blobs) -> WireDigest {
    let command = Command {
        arguments: vec!["cc".into(), "-c".into(), "x.c".into()],
        environment_variables: vec![
            EnvironmentVariable {
                name: "PATH".into(),
                value: "/secret/bin".into(),
            },
            EnvironmentVariable {
                name: "TOKEN".into(),
                value: "hunter2".into(),
            },
        ],
        working_directory: "work".into(),
        output_paths: vec!["a.o".into()],
        ..Command::default()
    };
    blobs.put(&command.encode_to_vec())
}

/// Catches: a failed run offered without its tails, its command rendering or its copy: the
/// last 16 KiB of each log (not the first), each log's whole length, the argv and the
/// environment variable NAMES with no value (a secret in the environment would be stored),
/// a copy that is not of the whole log, a run offered before its copy's object was written
/// (its row would name no object), and a flog event for a run with the object's own key.
#[tokio::test]
async fn a_failed_run_is_offered_with_tails_a_rendering_and_a_copy_of_its_logs() {
    let rig = rig(options(), 1 << 30);
    let out = pattern(TAIL_BYTES + 5000);
    let (stdout, stderr) = (rig.blobs.put(&out), rig.blobs.put(b"error: nope\n"));
    let result = rig.blobs.put(b"an ActionResult");
    let command = command_blob(&rig.blobs);
    let j = job(
        "b1",
        3,
        [Some(&stdout), Some(&stderr), Some(&result), Some(&command)],
    );
    submit(&rig, j).expect("queued");
    rig.tails.quiesce().await;
    let events = events(&rig);
    assert_eq!(events.len(), 2, "{events:?}");
    let (record, failed) = run_of(&events);
    assert_eq!(record.run_id(), id(3));
    let tail = failed.stdout.as_ref().expect("a stdout tail");
    assert_eq!(tail.bytes, out[out.len() - TAIL_BYTES..]);
    assert_eq!(tail.total_bytes, out.len() as u64);
    assert_eq!(tail.omitted(), TailOmitted::No);
    let tail = failed.stderr.as_ref().expect("a stderr tail");
    assert_eq!(
        (tail.bytes.as_slice(), tail.total_bytes),
        (&b"error: nope\n"[..], 12)
    );
    let c = failed.command.as_ref().expect("a rendering");
    assert_eq!(c.argv, ["cc", "-c", "x.c"]);
    assert_eq!(c.env_names, ["PATH", "TOKEN"]);
    assert_eq!((c.working_directory.as_str(), c.truncated), ("work", false));
    let rendered = format!("{c:?}");
    assert!(
        !rendered.contains("hunter2") && !rendered.contains("secret"),
        "{rendered}"
    );
    assert_eq!(failed.flog_omitted(), FlogOmitted::No);
    assert_eq!((failed.seq, failed.instance.as_str()), (record.seq, "main"));
    // The object first and the run after it.
    let Some(Event::Flog(f)) = events.first() else {
        panic!("the copy's event comes first: {events:?}");
    };
    assert_eq!(f.key, root().flog(id(3)));
    assert!(matches!(events.last(), Some(Event::Run { .. })));
    let s = summary_of(&rig, &events[0]);
    assert_eq!(
        (
            s.stdout_len,
            s.stderr_len,
            s.result_len,
            s.instance.as_str()
        ),
        (Some(out.len() as u64), Some(12), Some(15), "main")
    );
    assert_eq!(rig.objects.puts.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.budget.available_permits(),
        1 << 30,
        "every permit is back"
    );
}

/// Catches: a stream over the copy bound copied anyway (the object would hold 16 MiB it
/// was never to), one that loses its tail with its copy, the omission not said on the run,
/// and the other stream's copy lost with it.
#[tokio::test]
async fn a_stream_over_16_mib_keeps_a_tail_and_says_too_large() {
    let rig = rig(options(), 1 << 30);
    let big = pattern(usize::try_from(FLOG_STREAM_BYTES).expect("fits") + 1);
    let (stdout, stderr) = (rig.blobs.put(&big), rig.blobs.put(b"small"));
    submit(
        &rig,
        job("b1", 1, [Some(&stdout), Some(&stderr), None, None]),
    )
    .expect("queued");
    rig.tails.quiesce().await;
    let events = events(&rig);
    let (_, failed) = run_of(&events);
    let tail = failed.stdout.as_ref().expect("a tail");
    assert_eq!(
        tail.bytes,
        big[big.len() - TAIL_BYTES..],
        "the tail was read"
    );
    assert_eq!(failed.flog_omitted(), FlogOmitted::TooLarge);
    let s = summary_of(&rig, &events[0]);
    assert_eq!((s.stdout_len, s.stderr_len), (None, Some(5)), "{s:?}");
    // The exact bound is copied.
    let exact = pattern(usize::try_from(FLOG_STREAM_BYTES).expect("fits"));
    let stdout = rig.blobs.put(&exact);
    submit(&rig, job("b1", 2, [Some(&stdout), None, None, None])).expect("queued");
    rig.tails.quiesce().await;
    let events = self::events(&rig);
    assert_eq!(run_of(&events).1.flog_omitted(), FlogOmitted::No);
    assert_eq!(
        summary_of(&rig, &events[0]).stdout_len,
        Some(FLOG_STREAM_BYTES)
    );
}

/// Catches: a log over 64 MiB read for its tail (a 1 GiB log held in memory by a worker),
/// no reason given for the missing tail, its length lost, and the copy not marked omitted.
#[tokio::test]
async fn a_log_over_64_mib_is_tail_omitted_too_large() {
    let rig = rig(options(), 1 << 30);
    let huge = WireDigest::new([9; 32], TAIL_READ_BYTES + 1);
    submit(&rig, job("b1", 1, [Some(&huge), None, None, None])).expect("queued");
    rig.tails.quiesce().await;
    let events = events(&rig);
    assert_eq!(events.len(), 1, "no copy of what was not read");
    let (_, failed) = run_of(&events);
    let tail = failed.stdout.as_ref().expect("a tail record");
    assert_eq!(tail.omitted(), TailOmitted::TooLarge);
    assert_eq!(
        (tail.bytes.len(), tail.total_bytes),
        (0, TAIL_READ_BYTES + 1)
    );
    assert_eq!(failed.flog_omitted(), FlogOmitted::TooLarge);
    assert_eq!(
        rig.blobs.reads.load(Ordering::SeqCst),
        0,
        "nothing was read"
    );
    assert_eq!(rig.objects.puts.load(Ordering::SeqCst), 0);
    // A stderr over the bound, with a stdout that is small, is too large all the same.
    let small = rig.blobs.put(b"small");
    let huge = WireDigest::new([8; 32], TAIL_READ_BYTES + 1);
    submit(&rig, job("b1", 2, [Some(&small), Some(&huge), None, None])).expect("queued");
    rig.tails.quiesce().await;
    let events = self::events(&rig);
    let (_, failed) = run_of(&events);
    assert_eq!(failed.flog_omitted(), FlogOmitted::TooLarge);
    assert_eq!(
        failed.stderr.as_ref().expect("a tail record").omitted(),
        TailOmitted::TooLarge
    );
    assert_eq!(failed.stdout.as_ref().expect("a tail").bytes, b"small");
}

/// Catches: a read that waits for the budget for ever (a download holding the memory would
/// stop the writer), one that does not wait the wait (a tail `busy` the moment the budget
/// is short), the missing tail given no reason, and the run held back by it: the run still
/// arrives, with `busy` tails and no copy.
#[tokio::test(start_paused = true)]
async fn a_read_the_budget_cannot_serve_in_time_is_busy_and_the_run_still_arrives() {
    // A budget of 100 bytes, all held by a read of 100 that never ends.
    let rig = rig(options(), 100);
    let held = rig.blobs.put(&[1; 100]);
    rig.blobs.stall(&held);
    let small = rig.blobs.put(&[2; 10]);
    submit(&rig, job("a", 1, [Some(&held), None, None, None])).expect("queued");
    submit(&rig, job("b", 2, [Some(&small), Some(&small), None, None])).expect("queued");
    // Past the budget's wait, on the paused clock: the second run arrives with `busy`
    // tails while the first is still reading.
    tokio::time::sleep(options().wait * 4).await;
    let events = events(&rig);
    assert_eq!(events.len(), 1, "{events:?}");
    let (record, failed) = run_of(&events);
    assert_eq!(record.run_id(), id(2));
    for tail in [&failed.stdout, &failed.stderr] {
        let tail = tail.as_ref().expect("a tail record");
        assert_eq!(tail.omitted(), TailOmitted::Busy);
        assert_eq!((tail.bytes.len(), tail.total_bytes), (0, 10));
    }
    assert_eq!(
        failed.flog_omitted(),
        FlogOmitted::No,
        "busy is not too large"
    );
    assert_eq!(rig.objects.puts.load(Ordering::SeqCst), 0);
    assert_eq!(
        rig.budget.available_permits(),
        0,
        "the stalled read holds it all"
    );
}

/// Catches: a read that bypasses the budget (two workers each reading 60 bytes under a
/// budget of 100 would hold 120), and permits not given back after a run (the budget would
/// shrink with every failed run).
#[tokio::test(start_paused = true)]
async fn tails_never_hold_the_read_budget_past_the_cap() {
    let rig = rig(options(), 100);
    *rig.blobs.delay.lock().unwrap() = Duration::from_millis(100);
    let (a, b) = (rig.blobs.put(&[1; 60]), rig.blobs.put(&[2; 60]));
    submit(&rig, job("a", 1, [Some(&a), None, None, None])).expect("queued");
    submit(&rig, job("b", 2, [Some(&b), None, None, None])).expect("queued");
    tokio::time::sleep(Duration::from_secs(5)).await;
    rig.tails.quiesce().await;
    let events = events(&rig);
    let tails: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Run {
                failed: Some(f), ..
            } => f.stdout.as_ref().map(|t| t.omitted()),
            _ => None,
        })
        .collect();
    // One of them read; the other waited past its wait and was busy, or read after.
    assert_eq!(tails.len(), 2);
    assert!(
        rig.blobs.most.load(Ordering::SeqCst) <= 100,
        "{}",
        rig.blobs.most.load(Ordering::SeqCst)
    );
    assert_eq!(rig.budget.available_permits(), 100, "every permit is back");
}

/// Catches: a queue that is not bounded (a CAS that stops answering would take the
/// server's memory), a full pool that waits (the tap would hold the recorder), one that
/// drops the run with its tails, and a drop not counted as `stage="tail"`.
#[tokio::test]
async fn a_full_tail_queue_counts_stage_tail() {
    let rig = rig(
        TailOptions {
            workers: 1,
            queue: 1,
            ..options()
        },
        1 << 30,
    );
    let held = rig.blobs.put(&[1; 10]);
    rig.blobs.stall(&held);
    let first = job("a", 1, [Some(&held), None, None, None]);
    submit(&rig, first).expect("the worker takes it");
    // Wait for the worker to take it and stall on the read.
    for _ in 0..1000 {
        if rig.blobs.reads.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(rig.blobs.reads.load(Ordering::SeqCst), 1);
    submit(&rig, job("a", 2, [None; 4])).expect("the queue holds one");
    assert_eq!(rig.stats.dropped("tail"), 0);
    let back = submit(&rig, job("a", 3, [None; 4])).expect_err("a full queue gives the job back");
    assert_eq!(
        back.record.run_id(),
        id(3),
        "the run is the caller's to offer"
    );
    assert_eq!(rig.stats.dropped("tail"), 1);
    let mut text = String::new();
    rig.stats.render(&mut text);
    assert!(
        text.contains("kbf_history_dropped_total{stage=\"tail\"} 1\n"),
        "{text}"
    );
    assert!(
        events(&rig).is_empty(),
        "nothing was offered by the pool yet"
    );
}

/// Catches: a worker stuck for ever on a blob read that never ends (two such reads would
/// leave no worker, and every later failed run without tails), and a timeout that loses
/// the run: it arrives with `unreadable` tails, and the next run is served.
#[tokio::test(start_paused = true)]
async fn a_read_that_never_ends_times_out_and_the_pool_goes_on() {
    let rig = rig(
        TailOptions {
            workers: 1,
            ..options()
        },
        1 << 30,
    );
    let stuck = rig.blobs.put(b"stuck");
    rig.blobs.stall(&stuck);
    let fine = rig.blobs.put(b"fine");
    submit(&rig, job("a", 1, [Some(&stuck), None, None, None])).expect("queued");
    submit(&rig, job("a", 2, [Some(&fine), None, None, None])).expect("queued");
    let begun = tokio::time::Instant::now();
    tokio::time::sleep(options().read_timeout + Duration::from_secs(1)).await;
    rig.tails.quiesce().await;
    // On the paused clock the pool was not held longer than its read timeout.
    assert!(
        begun.elapsed() < 2 * options().read_timeout,
        "{:?}",
        begun.elapsed()
    );
    let events = events(&rig);
    let runs: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Run {
                record,
                failed: Some(f),
                ..
            } => Some((record.run_id(), f.stdout.as_ref().map(|t| t.omitted()))),
            _ => None,
        })
        .collect();
    assert_eq!(
        runs,
        [
            (id(1), Some(TailOmitted::Unreadable)),
            (id(2), Some(TailOmitted::No))
        ]
    );
    assert_eq!(rig.budget.available_permits(), 1 << 30);
}

/// Catches: a blob that is gone treated as an empty log (an empty tail shown for a log
/// that was lost), a hash of the wrong length read as a digest, an empty log read from the
/// CAS (one more call for nothing), and an empty log not copied as an empty one.
#[tokio::test]
async fn a_missing_a_malformed_and_an_empty_log_are_each_told_apart() {
    let rig = rig(options(), 1 << 30);
    let missing = WireDigest::new([5; 32], 10);
    let malformed = WireDigest {
        hash: vec![1, 2, 3],
        size: 10,
    };
    let empty = WireDigest::new(kbf_segments::sha256(b"").hash, 0);
    submit(
        &rig,
        job("a", 1, [Some(&missing), Some(&malformed), None, None]),
    )
    .expect("queued");
    rig.tails.quiesce().await;
    let events = events(&rig);
    let (_, failed) = run_of(&events);
    for tail in [&failed.stdout, &failed.stderr] {
        let tail = tail.as_ref().expect("a tail record");
        assert_eq!(tail.omitted(), TailOmitted::Unreadable);
        assert_eq!((tail.bytes.len(), tail.total_bytes), (0, 10));
    }
    assert_eq!(
        rig.blobs.reads.load(Ordering::SeqCst),
        1,
        "the missing one was asked for, the malformed one was not"
    );
    // An empty stdout: an empty tail, no read, and an empty copy.
    let rig2 = self::rig(options(), 1 << 30);
    submit(&rig2, job("a", 2, [Some(&empty), None, None, None])).expect("queued");
    rig2.tails.quiesce().await;
    let events = self::events(&rig2);
    let (_, failed) = run_of(&events);
    let tail = failed.stdout.as_ref().expect("a tail");
    assert_eq!(
        (tail.bytes.len(), tail.total_bytes, tail.omitted()),
        (0, 0, TailOmitted::No)
    );
    assert_eq!(rig2.blobs.reads.load(Ordering::SeqCst), 0);
    assert_eq!(summary_of(&rig2, &events[0]).stdout_len, Some(0));
}

/// Catches: a command that cannot be rendered failing the run (a Command that is not a
/// Command, one too large to read, one that is gone), a rendering that is not bounded to
/// 4 KiB (a build with a huge argv would store it for every failure), and an ActionResult
/// over its bound copied.
#[tokio::test]
async fn a_command_that_cannot_be_rendered_leaves_the_run_without_one() {
    let rig = rig(options(), 1 << 30);
    let junk = rig.blobs.put(&[0xff, 0xff, 0xff]);
    let too_big = WireDigest::new([6; 32], COMMAND_READ_BYTES + 1);
    let gone = WireDigest::new([7; 32], 10);
    for (n, command) in [(1, &junk), (2, &too_big), (3, &gone)] {
        submit(&rig, job("a", n, [None, None, None, Some(command)])).expect("queued");
    }
    // A huge argv is bounded.
    let long = Command {
        arguments: (0..100)
            .map(|i| format!("{i}{}", "x".repeat(300)))
            .collect(),
        ..Command::default()
    };
    let long = rig.blobs.put(&long.encode_to_vec());
    submit(&rig, job("a", 4, [None, None, None, Some(&long)])).expect("queued");
    let big_result = WireDigest::new([8; 32], RESULT_COPY_BYTES + 1);
    submit(&rig, job("a", 5, [None, None, Some(&big_result), None])).expect("queued");
    rig.tails.quiesce().await;
    let events = events(&rig);
    let by_run: BTreeMap<u32, _> = events
        .iter()
        .filter_map(|e| match e {
            Event::Run {
                record,
                failed: Some(f),
                ..
            } => Some((record.seq, f)),
            _ => None,
        })
        .collect();
    assert_eq!(by_run.len(), 5);
    for n in [1, 2, 3, 5] {
        assert!(by_run[&n].command.is_none(), "run {n}");
    }
    let bounded = by_run[&4].command.as_ref().expect("a rendering");
    let size: usize = bounded.argv.iter().map(String::len).sum();
    assert!(bounded.truncated && size <= 4096, "{size} bytes");
    assert_eq!(by_run[&5].flog_omitted(), FlogOmitted::TooLarge);
    assert_eq!(
        rig.blobs.reads.load(Ordering::SeqCst),
        3,
        "junk, gone and long"
    );
}

/// Catches: a failure-log object that could not be written failing the run (it must
/// arrive, with its tails), a copy lost without being counted (`stage="put"`), and a row
/// offered for an object that was not written.
#[tokio::test]
async fn a_copy_that_cannot_be_written_is_counted_and_the_run_still_arrives() {
    let rig = rig(options(), 1 << 30);
    rig.objects.fail.store(true, Ordering::SeqCst);
    let out = rig.blobs.put(b"out");
    submit(&rig, job("a", 1, [Some(&out), None, None, None])).expect("queued");
    rig.tails.quiesce().await;
    let events = events(&rig);
    assert_eq!(events.len(), 1, "no flog event: {events:?}");
    let (_, failed) = run_of(&events);
    assert_eq!(failed.stdout.as_ref().expect("a tail").bytes, b"out");
    assert_eq!(rig.stats.dropped("put"), 1);
    assert_eq!(rig.objects.puts.load(Ordering::SeqCst), 1);
}

/// Catches: a copy write that never answers wedging the worker for good (the object store
/// is the writer's, and the writer's queue drops when it is stuck: so must the pool), and
/// the timeout not counted.
#[tokio::test(start_paused = true)]
async fn a_copy_write_that_never_ends_times_out_and_is_counted() {
    /// An object store whose writes never return.
    struct Stuck;
    impl ObjectStore for Stuck {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn put_new(
            &self,
            _: &ObjectKey,
            _: Bytes,
            _: Option<SystemTime>,
        ) -> Result<(), ObjectStoreError> {
            std::future::pending().await
        }
        async fn get_range(&self, _: &ObjectKey, _: ByteRange) -> Result<Bytes, ObjectStoreError> {
            Err(ObjectStoreError::Protocol("unused".into()))
        }
        async fn delete(&self, _: &ObjectKey) -> Result<(), ObjectStoreError> {
            Ok(())
        }
        async fn list(
            &self,
            _: &KeyPrefix,
            _: Option<&ListToken>,
            _: PageSize,
        ) -> Result<ListPage, ObjectStoreError> {
            Err(ObjectStoreError::Protocol("unused".into()))
        }
    }
    let queue = Arc::new(EventQueue::new(QueueLimits::default()));
    let stats = Arc::new(Stats::new(Arc::clone(&queue)));
    let blobs = Arc::new(Blobs::default());
    let out = blobs.put(b"out");
    let open = Arc::new(OpenSet::default());
    let parts = Parts {
        queue: Arc::clone(&queue),
        blobs: blobs.clone(),
        budget: ReadBudget::default(),
        flog: Arc::new(FlogStore::new(Shared(Arc::new(Stuck)), root(), WriterId(1))),
        stats: Arc::clone(&stats),
        open: Arc::clone(&open),
    };
    let (tails, workers) = pool(parts, options());
    let handles = workers.spawn();
    open.add("a");
    tails
        .submit(job("a", 1, [Some(&out), None, None, None]))
        .expect("queued");
    let begun = tokio::time::Instant::now();
    tokio::time::sleep(options().read_timeout + Duration::from_secs(1)).await;
    tails.quiesce().await;
    assert!(
        begun.elapsed() < 2 * options().read_timeout,
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(stats.dropped("put"), 1);
    let events = queue.drain();
    assert_eq!(events.len(), 1, "the run, with no copy: {events:?}");
    handles.iter().for_each(tokio::task::JoinHandle::abort);
}

/// Catches: a run that reaches the writer after its invocation was closed leaving the
/// invocation open (it would read incomplete until the age cut), a second close sent while
/// the invocation is still open (it would flush a chunk of one run), and one for a run
/// whose event the queue dropped.
#[tokio::test]
async fn an_invocation_closed_while_its_logs_were_read_is_closed_again() {
    let rig = rig(options(), 1 << 30);
    // Still open: no close.
    rig.open.add("held");
    rig.tails.submit(job("held", 1, [None; 4])).expect("queued");
    rig.tails.quiesce().await;
    let events = events(&rig);
    assert_eq!(events.len(), 1, "{events:?}");
    // Closed meanwhile: the run, then a close.
    rig.tails
        .submit(job("closed", 2, [None; 4]))
        .expect("queued");
    rig.tails.quiesce().await;
    let events = self::events(&rig);
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(matches!(&events[1], Event::Close { inv } if *inv == InvRef::named("closed")));
    // A full event queue drops the run, and no close is tried after it: the one drop is
    // the run's.
    let full = self::rig_with(
        options(),
        1 << 30,
        QueueLimits {
            events: 0,
            bytes: 0,
        },
    );
    full.tails
        .submit(job("closed", 3, [None; 4]))
        .expect("queued");
    full.tails.quiesce().await;
    assert_eq!(full.queue.dropped(), 1);
}

/// Catches: a stop that does not wait for the runs in the pool (the last step would flush
/// without them), and one that never returns for an idle pool.
#[tokio::test(start_paused = true)]
async fn quiesce_returns_at_once_when_idle_and_waits_for_a_run_in_the_pool() {
    let rig = rig(options(), 1 << 30);
    rig.tails.quiesce().await;
    *rig.blobs.delay.lock().unwrap() = Duration::from_secs(3);
    let out = rig.blobs.put(b"out");
    submit(&rig, job("a", 1, [Some(&out), None, None, None])).expect("queued");
    let begun = tokio::time::Instant::now();
    rig.tails.quiesce().await;
    assert!(
        begun.elapsed() >= Duration::from_secs(3),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(events(&rig).len(), 2);
}

/// Catches: a pool of no workers or no queue (it would never run a job, or panic making
/// its channel), and a pool shown as a bare struct.
#[tokio::test]
async fn a_pool_of_none_is_a_pool_of_one() {
    let rig = rig(
        TailOptions {
            workers: 0,
            queue: 0,
            ..options()
        },
        1 << 30,
    );
    assert_eq!(rig.handles.len(), 1);
    assert_eq!(format!("{:?}", rig.tails), "Tails { .. }");
    submit(&rig, job("a", 1, [None; 4])).expect("queued");
    rig.tails.quiesce().await;
    assert_eq!(events(&rig).len(), 1);
}

/// Catches: the CAS source reading another blob than the digest names, a missing blob read
/// as an empty one, and the source not being the cache the server holds.
#[tokio::test]
async fn the_cache_is_a_blob_source() {
    use kbf_front::{Backend, Cache, VerifiedBlob};
    let cache = Arc::new(Cache::memory());
    let blob = VerifiedBlob::hashed(Bytes::from_static(b"stdout of a failed run"));
    let digest = kbf_segments::sha256(b"stdout of a failed run");
    cache.store_blobs(vec![blob]).await.expect("stored");
    let backend = Backend::from(cache);
    let read = BlobReads::read(&backend, &digest).await.expect("the blob");
    assert_eq!(read, Bytes::from_static(b"stdout of a failed run"));
    let missing = Digest::new(kbf_types::DigestFunction::Sha256, [3; 32], 4);
    let err = BlobReads::read(&backend, &missing)
        .await
        .expect_err("absent");
    assert!(err.to_string().contains("not in the cache"), "{err}");
}

#[path = "tails_edges_tests.rs"]
mod edges;
