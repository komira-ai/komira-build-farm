//! Fakes and a fixture the history tests share: an object store that counts calls and
//! fails on demand, an index wrapper that fails on demand, and a writer and reader over
//! them.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::SystemTime;

use bytes::Bytes;
use kbf_history::records::{
    CommandRendering, FailedRun, HitHow, HitRecord, InvocationHeader, RunOutcome, RunRecord, Tail,
    Timestamps, WireDigest,
};
use kbf_history::{
    ChunkCursor, Event, EventQueue, HistoryIndex, HistoryRoot, IndexRoot, Inline, InvKey, InvRef,
    InvocationView, Limits, MAX_PAGE_CHUNKS, Minter, Offload, Offloaded, QueueLimits, ReadError,
    Reader, Writer, WriterId,
};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Op, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};

/// 2026-10-16T00:00:00Z.
pub const T: u64 = 1_792_108_800_000;

/// The object store the tests write through: a conditional-put memory store that counts
/// calls and fails on demand.
#[derive(Clone)]
pub struct Objects(Arc<ObjectsInner>);

pub struct ObjectsInner {
    store: MemoryStore,
    pub puts: AtomicU64,
    pub gets: AtomicU64,
    pub lists: AtomicU64,
    pub deletes: AtomicU64,
    pub may_holds: AtomicU64,
    /// Fail this many next writes without storing.
    pub fail_puts: AtomicU32,
    /// Store this many next writes, then answer with an error (a lost acknowledgement).
    pub lose_acks: AtomicU32,
    /// Fail this many next reads.
    pub fail_gets: AtomicU32,
}

fn take_one(n: &AtomicU32) -> bool {
    n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
        .is_ok()
}

fn broken() -> ObjectStoreError {
    ObjectStoreError::Protocol("planted failure".into())
}

impl Objects {
    pub fn new() -> Objects {
        Objects(Arc::new(ObjectsInner {
            store: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            puts: AtomicU64::new(0),
            gets: AtomicU64::new(0),
            lists: AtomicU64::new(0),
            deletes: AtomicU64::new(0),
            may_holds: AtomicU64::new(0),
            fail_puts: AtomicU32::new(0),
            lose_acks: AtomicU32::new(0),
            fail_gets: AtomicU32::new(0),
        }))
    }

    pub fn counts(&self) -> &ObjectsInner {
        &self.0
    }

    /// Every key held, in order (read past the counters).
    pub fn keys(&self) -> Vec<String> {
        let page = futures::executor::block_on(self.0.store.list(
            &KeyPrefix::default(),
            None,
            PageSize::new(1000).unwrap(),
        ))
        .unwrap();
        page.objects
            .into_iter()
            .map(|o| o.key.as_str().to_owned())
            .collect()
    }

    /// The whole object (read past the counters).
    pub fn get(&self, key: &str) -> Bytes {
        let key = ObjectKey::new(key).unwrap();
        let all = ByteRange::new(0, u64::MAX / 2).unwrap();
        futures::executor::block_on(self.0.store.get_range(&key, all)).unwrap()
    }

    /// Replaces (or plants) an object's bytes, as a corrupting store would.
    pub fn replace(&self, key: &str, body: Vec<u8>) {
        let key = ObjectKey::new(key).unwrap();
        futures::executor::block_on(async {
            self.0.store.delete(&key).await.unwrap();
            self.0
                .store
                .put_new(&key, Bytes::from(body), None)
                .await
                .unwrap();
        });
    }

    /// Removes an object, as an expiry would.
    pub fn remove(&self, key: &str) {
        let key = ObjectKey::new(key).unwrap();
        futures::executor::block_on(self.0.store.delete(&key)).unwrap();
    }
}

impl ObjectStore for Objects {
    fn capabilities(&self) -> Capabilities {
        self.0.store.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.puts.fetch_add(1, Ordering::SeqCst);
        if take_one(&self.0.fail_puts) {
            return Err(broken());
        }
        let stored = self.0.store.put_new(key, body, retain_until).await;
        if stored.is_ok() && take_one(&self.0.lose_acks) {
            return Err(broken());
        }
        stored
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.gets.fetch_add(1, Ordering::SeqCst);
        if take_one(&self.0.fail_gets) {
            return Err(broken());
        }
        self.0.store.get_range(key, range).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        self.0.may_holds.fetch_add(1, Ordering::SeqCst);
        self.0.store.may_hold(key).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.deletes.fetch_add(1, Ordering::SeqCst);
        self.0.store.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.lists.fetch_add(1, Ordering::SeqCst);
        self.0.store.list(prefix, after, max_keys).await
    }
}

/// An index that fails on demand, over any store.
pub struct Index {
    pub inner: Arc<dyn Store>,
    /// Fail this many next transactions with a store error.
    pub fail_txns: AtomicU32,
    /// Apply this many next transactions, then answer with a store error.
    pub lose_acks: AtomicU32,
    /// Fail every transaction that increments a counter (the `ui/streams` refill).
    pub block_counters: AtomicBool,
    /// Fail every `get`.
    pub fail_gets: AtomicBool,
    /// Answer a counter increment with no counter value.
    pub empty_counters: AtomicBool,
    pub txns: AtomicU64,
    pub gets: AtomicU64,
    pub scans: AtomicU64,
    /// A call on this thread panics: the thread that polls the writer and reader.
    pub forbid: Mutex<Option<ThreadId>>,
}

impl Index {
    pub fn over(inner: Arc<dyn Store>) -> Arc<Index> {
        Arc::new(Index {
            inner,
            fail_txns: AtomicU32::new(0),
            lose_acks: AtomicU32::new(0),
            block_counters: AtomicBool::new(false),
            fail_gets: AtomicBool::new(false),
            empty_counters: AtomicBool::new(false),
            txns: AtomicU64::new(0),
            gets: AtomicU64::new(0),
            scans: AtomicU64::new(0),
            forbid: Mutex::new(None),
        })
    }

    pub fn fake() -> Arc<Index> {
        Index::over(Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        )))
    }

    /// Every row under `prefix`, keys only.
    pub fn keys(&self, prefix: &str) -> Vec<String> {
        self.inner
            .scan_prefix(prefix)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    pub fn put(&self, key: &str, value: Vec<u8>) {
        self.inner
            .txn(Txn::new().put(key, value, kbf_state::Cond::Any))
            .unwrap()
            .unwrap();
    }
}

impl Index {
    fn check_thread(&self) {
        let here = std::thread::current().id();
        assert_ne!(
            *self.forbid.lock().unwrap(),
            Some(here),
            "a blocking index call ran on the polling thread"
        );
    }
}

fn failed(what: &str) -> StoreError {
    StoreError::Failed(format!("planted {what} failure"))
}

impl Store for Index {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.check_thread();
        self.txns.fetch_add(batch.len() as u64, Ordering::SeqCst);
        let counts = batch
            .iter()
            .any(|t| t.ops().iter().any(|op| matches!(op, Op::Next { .. })));
        if counts && self.block_counters.load(Ordering::SeqCst) {
            return Err(failed("counter"));
        }
        if take_one(&self.fail_txns) {
            return Err(failed("transaction"));
        }
        let mut out = self.inner.apply_batch(batch)?;
        if take_one(&self.lose_acks) {
            return Err(failed("acknowledgement"));
        }
        if counts && self.empty_counters.load(Ordering::SeqCst) {
            for c in out.iter_mut().flatten() {
                c.counters.clear();
            }
        }
        Ok(out)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.check_thread();
        self.gets.fetch_add(1, Ordering::SeqCst);
        if self.fail_gets.load(Ordering::SeqCst) {
            return Err(failed("get"));
        }
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.check_thread();
        self.scans.fetch_add(1, Ordering::SeqCst);
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.check_thread();
        self.scans.fetch_add(1, Ordering::SeqCst);
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// Runs each job on a thread of its own, as a server's blocking pool would, and counts
/// them.
#[derive(Default)]
pub struct Threaded {
    pub jobs: AtomicU64,
}

impl Offload for Threaded {
    fn offload(&self, job: Box<dyn FnOnce() + Send>) -> Offloaded {
        self.jobs.fetch_add(1, Ordering::SeqCst);
        let (done, ran) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            job();
            let _ = done.send(());
        });
        Box::pin(async move {
            let _ = ran.await;
        })
    }
}

/// Never runs a job, as a blocking task cancelled before it started.
pub struct Never;

impl Offload for Never {
    fn offload(&self, job: Box<dyn FnOnce() + Send>) -> Offloaded {
        drop(job);
        Box::pin(std::future::ready(()))
    }
}

/// A frontend's history: one writer life over a shared object store and index.
pub struct Life {
    pub objects: Objects,
    pub index: Arc<Index>,
    pub index_root: IndexRoot,
    pub offload: Arc<dyn Offload>,
    pub minter: Arc<Minter>,
    pub queue: Arc<EventQueue>,
    pub writer: Writer<Objects>,
    pub writer_id: WriterId,
}

pub fn root() -> HistoryRoot {
    HistoryRoot::under(&KeyPrefix::new("kbf/").unwrap()).unwrap()
}

impl Life {
    pub fn start(objects: &Objects, index: &Arc<Index>, writer: u64, limits: Limits) -> Life {
        Life::start_with(
            objects,
            index,
            writer,
            limits,
            IndexRoot::default(),
            Arc::new(Inline),
        )
    }

    pub fn start_with(
        objects: &Objects,
        index: &Arc<Index>,
        writer: u64,
        limits: Limits,
        index_root: IndexRoot,
        offload: Arc<dyn Offload>,
    ) -> Life {
        let minter = Arc::new(Minter::new(0));
        let queue = Arc::new(EventQueue::new(QueueLimits::default()));
        let as_store: Arc<dyn Store> = index.clone();
        let history = HistoryIndex::new(as_store, index_root.clone(), offload.clone());
        Life {
            objects: objects.clone(),
            index: index.clone(),
            index_root,
            offload,
            writer: Writer::new(
                objects.clone(),
                root(),
                history,
                WriterId(writer),
                minter.clone(),
                queue.clone(),
                limits,
            ),
            minter,
            queue,
            writer_id: WriterId(writer),
        }
    }

    pub fn fresh(limits: Limits) -> Life {
        Life::start(&Objects::new(), &Index::fake(), 1, limits)
    }

    pub fn history_index(&self) -> HistoryIndex {
        let as_store: Arc<dyn Store> = self.index.clone();
        HistoryIndex::new(as_store, self.index_root.clone(), self.offload.clone())
    }

    pub fn reader(&self) -> Reader<Objects> {
        Reader::new(self.objects.clone(), root(), self.history_index())
    }

    pub fn step(&mut self, now_ms: u64) {
        futures::executor::block_on(self.writer.step(now_ms));
    }

    /// Steps until nothing is pending, kept or unwritten (at most `n` steps).
    pub fn drain(&mut self, now_ms: u64, n: usize) -> usize {
        for i in 0..n {
            self.step(now_ms);
            let w = &self.writer;
            if w.pending_rows() == 0 && w.kept_objects() == 0 && w.unwritten_chunks() == 0 {
                return i + 1;
            }
        }
        panic!("not drained after {n} steps");
    }

    /// Mints a run of `inv` committed at `t` and queues it.
    pub fn run(&self, inv: &InvRef, t: u64, instance: &str, outcome: RunOutcome) -> RunRecord {
        let minted = self.minter.mint(&inv.key, t);
        let mut r = run_record(instance, outcome, t);
        r.set_run_id(minted.id);
        let failed = (outcome != RunOutcome::Ok).then(|| failed_section(instance));
        assert!(self.queue.offer(Event::Run {
            inv: inv.clone(),
            record: Box::new(r.clone()),
            failed,
            stream_opened_ms: minted.stream_opened_ms,
        }));
        r
    }

    pub fn hit(&self, inv: &InvRef, t: u64, instance: &str) {
        assert!(self.queue.offer(Event::Hit {
            inv: inv.clone(),
            record: HitRecord {
                time_ms: t,
                instance: instance.into(),
                action: Some(digest(9)),
                how: HitHow::CacheHit.into(),
                row: 0,
            },
        }));
    }

    pub fn close(&self, inv: &InvRef) {
        assert!(self.queue.offer(Event::Close { inv: inv.clone() }));
    }
}

pub fn digest(n: u8) -> WireDigest {
    WireDigest::new([n; 32], u64::from(n))
}

pub fn run_record(instance: &str, outcome: RunOutcome, t: u64) -> RunRecord {
    RunRecord {
        instance: instance.into(),
        action: Some(digest(1)),
        node: "node-1".into(),
        outcome: outcome.into(),
        exit_code: i32::from(outcome != RunOutcome::Ok),
        timestamps: Some(Timestamps {
            queued_us: t * 1000,
            execution_start_us: t * 1000 + 10,
            execution_completed_us: t * 1000 + 500,
            worker_completed_us: t * 1000 + 900,
            ..Timestamps::default()
        }),
        ..RunRecord::default()
    }
}

pub fn failed_section(instance: &str) -> FailedRun {
    FailedRun {
        seq: 0,
        instance: instance.into(),
        stdout: Some(Tail::of(b"compiling\n")),
        stderr: Some(Tail::of(format!("error in {instance}\n").as_bytes())),
        command: Some(CommandRendering {
            argv: vec!["cc".into(), "-c".into(), "x.c".into()],
            env_names: vec!["PATH".into()],
            ..CommandRendering::default()
        }),
        flog_omitted: 0,
    }
}

pub fn all(_: &str) -> bool {
    true
}

/// An invocation read whole: its summary and every record, page after page.
#[derive(Debug, PartialEq)]
pub struct Whole {
    pub header: InvocationHeader,
    pub complete: bool,
    pub gap_from_ms: Option<u64>,
    pub runs: Vec<RunRecord>,
    pub failed: Vec<FailedRun>,
    pub hits: Vec<HitRecord>,
}

/// Reads `inv` whole through [`Reader::invocation`] and every page of
/// [`Reader::records`].
pub fn whole(
    reader: &Reader<Objects>,
    inv: &InvKey,
    allow: kbf_history::Allow<'_>,
) -> Result<Option<Whole>, ReadError> {
    let Some(InvocationView {
        header,
        complete,
        gap_from_ms,
        ..
    }) = futures::executor::block_on(reader.invocation(inv, allow))?
    else {
        return Ok(None);
    };
    let mut got = Whole {
        header,
        complete,
        gap_from_ms,
        runs: Vec::new(),
        failed: Vec::new(),
        hits: Vec::new(),
    };
    let mut after: Option<ChunkCursor> = None;
    loop {
        let page = futures::executor::block_on(reader.records(
            inv,
            after.as_ref(),
            MAX_PAGE_CHUNKS,
            allow,
        ))?
        .expect("the header was there a moment ago");
        got.runs.extend(page.runs);
        got.failed.extend(page.failed);
        got.hits.extend(page.hits);
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(Some(got)),
        }
    }
}
