//! The history writer core: events into per-invocation chunks, chunks into immutable
//! objects, and pointer rows into the `ui/` index, object first and row second.
//!
//! The server owns the loop: it offers events to the [`EventQueue`] from the request
//! path and calls [`Writer::step`] on a cadence of its choosing (twice a second keeps
//! index writes at the background lane's two transactions a second). Index calls go
//! through the [`HistoryIndex`]'s offload, so `step` may be awaited on a runtime worker.
//!
//! - **Chunks.** An invocation's records are gathered per writer and cut into a chunk at
//!   [`Limits::chunk_bytes`] of raw records, when the invocation closes, when its oldest
//!   unflushed record has waited [`Limits::chunk_age_ms`], or when its stream or UTC day
//!   changes (a chunk holds one stream, so a run id's sequence locates its block).
//! - **Object first, pointer second.** A cut chunk is written with `put_new` (a key is
//!   never reused, so `AlreadyExists` on a retry is success); only then do its `ichunk`,
//!   `stream` and `invid` rows enter [`Pending`]. A crash between the two leaves an
//!   orphan object, never a row naming an object that was not written. A failed write is
//!   counted (`stage="put"`) and retried with the same key and bytes next step.
//! - **Bounded memory.** Chunks not yet written count against [`Limits::buffer_bytes`];
//!   past it the oldest is dropped (`stage="buffer"`) and its invocation's header counts
//!   the hole. Index rows past [`Pending`]'s bounds, and rows whose transaction failed,
//!   are dropped (`stage="index"`) and their object kept for re-insertion once the
//!   backlog drains: the rows come back from the object's footer with no LIST. The
//!   invocations held are bounded by [`Limits::invocations`] (idle ones forgotten
//!   first, then new ones refused, `stage="invocations"`).
//! - **Headers.** A newly seen invocation's stored header (a previous life's, or this
//!   writer's from before it forgot the invocation) is read after the drain, at most
//!   [`Limits::header_reads_per_step`] a step, and merged with what the writer added
//!   meanwhile. Until the read succeeds no header row is written, so a failed read
//!   never replaces the stored header with a fresh count. A header row that did not
//!   commit is written again from the writer's state, and its invocation held until
//!   one does: a closed invocation gets no later chunk to carry it.
//! - **Catalog and status rows.** A run of a shape the writer has not seen puts the
//!   shape's `ui/shape` row once; a run of a test shape adds to its `ui/status` row by
//!   read-modify-write with the by-status entry in the same transaction (see
//!   [`DerivedLimits`] and the module `derived`). A header row carries the `ui/inv` list
//!   entry of each instance it is first to name.
//!
//! TODO: mark the index transactions background once the `kbf-state` trait has the
//! mark (the C7 background lane); until then they are ordinary transactions, sent at
//! most [`Limits::txns_per_step`] a step.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_state::{Cond, Txn, Versioned};
use prost::Message;

use crate::container::{self, Encoded, Header};
use crate::derived::{Derived, DerivedLimits, TestId};
use crate::index::HistoryIndex;
use crate::keys::{Day, Family, HistoryRoot, InvKey, WriterId};
use crate::pending::{Also, Kept, KeptObject, Pending, PendingLimits, Row};
use crate::queue::{Event, EventQueue, InvRef};
use crate::records::{
    ChunkBody, ChunkRecord, ChunkRow, ChunkSummary, DigestRow, FailedRun, HitRecord, InvState,
    InvocationHeader, MAX_INSTANCES, RunRecord, Slow, StreamRow,
};
use crate::rows::{ShapeKind, ShapeRow};
use crate::run_id::{Minter, StreamId};
use crate::scoped::Scoped;
use crate::status::{self, Observed};

const MS_PER_DAY: u64 = 86_400_000;

/// How many of the slowest runs an invocation's summary keeps, by each measure.
pub const SLOWEST: usize = 20;

/// The writer's bounds and cadence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Raw record bytes at which a chunk is cut.
    pub chunk_bytes: usize,
    /// How long the oldest unflushed record of an invocation waits before its chunk is
    /// cut, milliseconds.
    pub chunk_age_ms: u64,
    /// Raw bytes of chunks not yet written, at most (inside `--invocation-memory`).
    pub buffer_bytes: usize,
    /// The pending index bounds.
    pub pending: PendingLimits,
    /// Objects kept for re-insertion at most.
    pub kept: usize,
    /// Rows per index transaction at most.
    pub txn_rows: usize,
    /// Bytes per index transaction at most.
    pub txn_bytes: usize,
    /// Index transactions per step at most.
    pub txns_per_step: usize,
    /// Kept objects re-read per step at most.
    pub reinserts_per_step: usize,
    /// Invocations the writer holds state for at most. Each holds its header, its two
    /// lists of the [`SLOWEST`] runs (about 2 KiB together), its instance names and one
    /// entry per stream; past the bound an idle one is forgotten to make room, and with
    /// none idle a new invocation's event is dropped (`stage="invocations"`).
    pub invocations: usize,
    /// Stored headers read per step at most, for invocations the writer has just
    /// opened (one index read each).
    pub header_reads_per_step: usize,
    /// The shape and status rows' bounds.
    pub derived: DerivedLimits,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            chunk_bytes: 256 * 1024,
            chunk_age_ms: 120_000,
            buffer_bytes: 256 * 1024 * 1024,
            pending: PendingLimits::default(),
            kept: 65_536,
            txn_rows: 256,
            txn_bytes: 256 * 1024,
            txns_per_step: 1,
            reinserts_per_step: 64,
            invocations: 4096,
            header_reads_per_step: 256,
            derived: DerivedLimits::default(),
        }
    }
}

/// Records the writer has dropped, by stage. The queue's own count is
/// [`EventQueue::dropped`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drops {
    /// Records dropped with a chunk buffer past [`Limits::buffer_bytes`].
    pub buffer: u64,
    /// Chunk writes that failed (each retried).
    pub put: u64,
    /// Index rows dropped: a pointer row is restored from its object later, a header
    /// row is written again from the writer's state.
    pub index: u64,
    /// Events dropped because the writer held [`Limits::invocations`] invocations and
    /// none was idle enough to forget.
    pub invocations: u64,
}

/// Records gathered for one chunk.
#[derive(Debug, Default)]
struct Building {
    day: Option<Day>,
    stream: Option<u32>,
    runs: Vec<RunRecord>,
    failed: Vec<FailedRun>,
    hits: Vec<HitRecord>,
    raw: usize,
    since_ms: u64,
}

impl Building {
    fn records(&self) -> usize {
        self.runs.len() + self.failed.len() + self.hits.len()
    }

    /// Whether a record of `day` and `stream` must start a new chunk.
    fn breaks_on(&self, day: Day, stream: Option<u32>) -> bool {
        let other_day = self.day.is_some_and(|d| d != day);
        let other_stream = matches!((self.stream, stream), (Some(a), Some(b)) if a != b);
        other_day || other_stream
    }
}

/// A chunk encoded and waiting for its write.
#[derive(Debug)]
struct Cut {
    inv: InvKey,
    key: ObjectKey,
    encoded: Encoded,
    seq: u32,
    row: ChunkRow,
    stream_first_ms: u64,
    records: usize,
}

/// What the writer holds for one invocation.
#[derive(Debug, Default)]
struct Inv {
    /// The header; while `unread`, only what this writer added since it opened the
    /// invocation.
    header: InvocationHeader,
    /// The stored header has not been read yet. No header row is written until it is
    /// and the two are merged, so a failed read never replaces the stored header.
    unread: bool,
    /// A header row write was held back while `unread`.
    held: bool,
    /// A header row was queued and no transaction carrying it has committed: it was
    /// dropped (a failed transaction, a full backlog) or replaced by a restored row. It
    /// is queued again from this state once the index drains, and the invocation is not
    /// forgotten meanwhile, since no later chunk may come to rewrite it.
    unsaved: bool,
    building: Building,
    slowest_execution: Vec<Slow>,
    slowest_wall: Vec<Slow>,
    /// The open time of each stream this writer has seen, its row key's time.
    stream_first: BTreeMap<u32, u64>,
    /// The (day, stream) pairs whose `ui/stream` row this writer has queued. The row has
    /// one key and value for every chunk of the stream, so only the first chunk queues
    /// it; were it dropped, it comes back from that chunk's object.
    stream_rows: BTreeSet<(u32, u32)>,
    /// The instances whose `ui/inv` list entry for this invocation committed with a
    /// header row. An instance not in it has its entry put with the next header row.
    listed: BTreeSet<String>,
}

/// The history writer core. See the module docs.
pub struct Writer<O> {
    objects: Scoped<O>,
    core: Core,
}

/// Everything of the writer but the object store and index calls: no I/O.
struct Core {
    root: HistoryRoot,
    index: HistoryIndex,
    writer: WriterId,
    minter: Arc<Minter>,
    queue: Arc<EventQueue>,
    limits: Limits,
    /// The next chunk's sequence. One count for every invocation the writer writes, so
    /// no chunk key or `ichunk` row is made twice in a writer's life, whatever it has
    /// forgotten and reopened.
    next_chunk: u32,
    invs: BTreeMap<InvKey, Inv>,
    ready: VecDeque<Cut>,
    /// Bytes in chunks not yet written: cut ones as encoded, building ones raw.
    held: usize,
    pending: Pending,
    kept: Kept,
    derived: Derived,
    drops: Drops,
    /// An index transaction failed this step: the index is not draining.
    index_failed: bool,
    /// The last stream block refill failed: the next step spends its budget on rows,
    /// so a counter that keeps failing never starves them.
    refill_failed: bool,
}

impl<O: ObjectStore> Writer<O> {
    /// A writer with id `writer` that writes objects under `root` in `objects` and rows
    /// to `index`, draining `queue` and refilling `minter`'s stream blocks.
    #[must_use]
    pub fn new(
        objects: O,
        root: HistoryRoot,
        index: HistoryIndex,
        writer: WriterId,
        minter: Arc<Minter>,
        queue: Arc<EventQueue>,
        limits: Limits,
    ) -> Writer<O> {
        Writer {
            objects: Scoped::history(objects, &root),
            core: Core {
                root,
                index,
                writer,
                minter,
                queue,
                limits,
                next_chunk: 0,
                invs: BTreeMap::new(),
                ready: VecDeque::new(),
                held: 0,
                pending: Pending::new(limits.pending),
                kept: Kept::new(limits.kept),
                derived: Derived::new(limits.derived),
                drops: Drops::default(),
                index_failed: false,
                refill_failed: false,
            },
        }
    }

    /// One turn of the writer at time `now_ms`: reserve a stream block if the minter
    /// needs one, take the queued events into chunks, cut the chunks that are due, write
    /// them, apply up to [`Limits::txns_per_step`] index transactions (the block's
    /// included), and, once nothing is pending, read back kept objects to restore their
    /// rows.
    pub async fn step(&mut self, now_ms: u64) {
        self.core.index_failed = false;
        let budget = self.core.limits.txns_per_step;
        let budget = budget - usize::from(self.refill_streams(budget).await);
        self.core.take_events(now_ms);
        self.read_headers().await;
        self.read_statuses().await;
        self.core.cut_due(now_ms);
        let mut failed = VecDeque::new();
        while let Some(cut) = self.core.ready.pop_front() {
            let body = Bytes::from(cut.encoded.bytes.clone());
            let result = self.objects.put_new(&cut.key, body, None).await;
            if let Some(cut) = self.core.written(cut, result, now_ms) {
                failed.push_back(cut);
            }
        }
        self.core.ready = failed;
        self.core.queue_derived();
        self.apply_pending(budget).await;
        for _ in 0..self.core.reinsert_budget() {
            let Some(object) = self.core.kept.pop() else {
                break;
            };
            let read = self.read_footer(&object).await;
            if !self.core.restore(object, read) {
                break;
            }
        }
        self.core.requeue_headers();
        self.core.queue_derived();
        self.core.prune(now_ms);
    }

    /// Reserves a stream block if the minter needs one and `budget` allows a
    /// transaction, and the last refill did not fail (that one waits a step). Returns
    /// whether it sent one: the refill is one of the step's [`Limits::txns_per_step`].
    async fn refill_streams(&mut self, budget: usize) -> bool {
        let core = &mut self.core;
        if budget == 0 || std::mem::take(&mut core.refill_failed) || !core.minter.needs_block() {
            return false;
        }
        let txn = Txn::new().next(core.index.root().streams_counter());
        match core.index.txn(txn).await {
            // An empty counter list is a backend bug; block 0 is refused, so it is safe.
            Ok(Ok(commit)) => {
                core.minter
                    .add_block(commit.counters.first().copied().unwrap_or(0));
            }
            _ => core.refill_failed = true,
        }
        true
    }

    /// Reads the stored headers of up to [`Limits::header_reads_per_step`] invocations
    /// opened since. A failed read leaves the rest for the next step: the index is not
    /// answering.
    async fn read_headers(&mut self) {
        for key in self.core.unread() {
            let row = self.core.index.root().invid(&key);
            let Ok(stored) = self.core.index.get(row).await else {
                return;
            };
            self.core.header_read(&key, stored);
        }
    }

    /// Reads the stored status rows of up to [`DerivedLimits::reads_per_step`] test
    /// keys with runs waiting, and queues each key's write. A failed read stops the
    /// reads of this step and writes nothing: the stored row is not known, and writing
    /// without it would reset the status.
    async fn read_statuses(&mut self) {
        for key in self.core.derived.to_read() {
            let Ok(stored) = self.core.index.get(key.clone()).await else {
                return;
            };
            self.core.status_read(&key, stored);
        }
    }

    /// Sends at most `budget` index transactions (what is left of
    /// [`Limits::txns_per_step`]), retries included.
    async fn apply_pending(&mut self, mut budget: usize) {
        let limits = self.core.limits;
        while budget > 0 {
            let batch = self.core.pending.take(limits.txn_rows, limits.txn_bytes);
            if batch.is_empty() {
                return;
            }
            budget = self.apply(batch, budget).await;
        }
    }

    /// Applies one batch of rows, one transaction of `budget` per try, and returns the
    /// budget left. A pointer row that already exists (a restored row, or a write
    /// retried after it landed) fails its `Absent` check; it is there, so it leaves the
    /// batch and the rest is tried again, or put back for the next step once the budget
    /// is spent. A failed transaction drops its rows, each counted and its object kept.
    async fn apply(&mut self, mut batch: Vec<(String, Row)>, mut budget: usize) -> usize {
        while !batch.is_empty() {
            if budget == 0 {
                for (key, row) in batch {
                    self.core.enqueue(key, row);
                }
                return 0;
            }
            budget -= 1;
            let (txn, owner) = Row::txn(&batch);
            match self.core.index.txn(txn).await {
                Ok(Ok(_)) => {
                    self.core.committed(&batch);
                    return budget;
                }
                Ok(Err(conflict)) => {
                    let at = owner[conflict.index.min(owner.len() - 1)];
                    let (key, _) = batch.remove(at);
                    self.core.conflicted(&key);
                }
                Err(_) => {
                    self.core.index_failed = true;
                    for (key, row) in batch {
                        self.core.dropped_row(&key, row);
                    }
                    return budget;
                }
            }
        }
        budget
    }

    /// Invocations the writer holds state for.
    #[must_use]
    pub fn open_invocations(&self) -> usize {
        self.core.invs.len()
    }

    async fn read_footer(&self, object: &KeptObject) -> Result<Bytes, ObjectStoreError> {
        let range = ByteRange::new(object.footer_offset, u64::from(object.footer_len));
        // A chunk's footer range holds at least the 24-byte trailer.
        let range = range.ok_or(ObjectStoreError::Unsupported("an empty footer range"))?;
        self.objects.get_range(&object.key, range).await
    }

    /// The writer's drop counts.
    #[must_use]
    pub fn drops(&self) -> Drops {
        self.core.drops
    }

    /// Index rows waiting to be applied.
    #[must_use]
    pub fn pending_rows(&self) -> usize {
        self.core.pending.len()
    }

    /// Shape and status keys the writer holds, each map's count: the keys it is
    /// bounded by ([`DerivedLimits::keys`]).
    #[must_use]
    pub fn index_keys(&self) -> (usize, usize) {
        self.core.derived.keys()
    }

    /// Catalog and status rows owed to the index: shape rows not known to exist and
    /// status keys with runs not yet written.
    #[must_use]
    pub fn index_owed(&self) -> usize {
        self.core.derived.owed()
    }

    /// Objects whose rows are waiting to be restored.
    #[must_use]
    pub fn kept_objects(&self) -> usize {
        self.core.kept.len()
    }

    /// Chunks cut and not yet written.
    #[must_use]
    pub fn unwritten_chunks(&self) -> usize {
        self.core.ready.len()
    }
}

impl Core {
    /// Takes the queued events into the invocations' chunks.
    fn take_events(&mut self, now_ms: u64) {
        self.minter.forget_before(Day::from_ms(now_ms));
        for event in self.queue.drain() {
            self.ingest(event, now_ms);
        }
    }

    /// Whether the writer may forget `inv`: nothing buffered or unwritten, its stored
    /// header read, and its last header row committed (a later record reloads the
    /// header from the index, so the index must have it).
    fn idle(&self, key: &InvKey, s: &Inv) -> bool {
        s.building.records() == 0
            && !s.unread
            && !s.unsaved
            && !self.ready.iter().any(|c| &c.inv == key)
            && !self.pending.contains(&self.index.root().invid(key))
    }

    /// Forgets idle invocations that are closed or were last seen before yesterday
    /// (UTC). A later record of one reloads its header from the index and continues it.
    fn prune(&mut self, now_ms: u64) {
        let yesterday = Day::from_ms(now_ms.saturating_sub(MS_PER_DAY));
        let forget: Vec<InvKey> = self
            .invs
            .iter()
            .filter(|(key, s)| {
                let done = s.header.state() == InvState::Closed
                    || Day::from_ms(s.header.last_seen_ms) < yesterday;
                done && self.idle(key, s)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in forget {
            self.invs.remove(&key);
        }
    }

    /// Forgets the idle invocation seen longest ago, to make room for a new one.
    /// Returns whether one was.
    fn evict_one(&mut self) -> bool {
        let oldest = self
            .invs
            .iter()
            .filter(|(key, s)| self.idle(key, s))
            .min_by_key(|(_, s)| s.header.last_seen_ms)
            .map(|(key, _)| key.clone());
        let Some(key) = oldest else {
            return false;
        };
        self.invs.remove(&key);
        true
    }

    /// The invocations whose stored header is to be read this step.
    fn unread(&self) -> Vec<InvKey> {
        self.invs
            .iter()
            .filter(|(_, s)| s.unread)
            .map(|(key, _)| key.clone())
            .take(self.limits.header_reads_per_step)
            .collect()
    }

    /// Takes `inv`'s stored header row (none, or one that does not decode: a fresh
    /// invocation) and merges it with what this writer added; a header row held back
    /// meanwhile is queued now.
    fn header_read(&mut self, inv: &InvKey, stored: Option<Versioned>) {
        let stored = stored.and_then(|v| InvocationHeader::decode(v.value.as_slice()).ok());
        let state = self.state(inv);
        if let Some(stored) = stored {
            state.header = merge(stored, &state.header);
        }
        state.unread = false;
        if std::mem::take(&mut state.held) {
            self.queue_header(inv, None);
        }
    }

    /// Queues `inv`'s header row, or holds it back until its stored header is read.
    fn write_header(&mut self, inv: &InvKey, origin: Option<KeptObject>) {
        let state = self.state(inv);
        if state.unread {
            state.held = true;
            return;
        }
        self.queue_header(inv, origin);
    }

    /// Queues `inv`'s header row as the writer holds it, unsaved until it commits. A
    /// header that lists no instance (a close with no records, and no stored header
    /// that had any) is not written: no caller could be allowed to read it, and it would
    /// only hold the client's record id.
    fn queue_header(&mut self, inv: &InvKey, origin: Option<KeptObject>) {
        let state = self.state(inv);
        if state.header.instances.is_empty() {
            return;
        }
        state.unsaved = true;
        let mut row = self.header_row(inv);
        row.origin = origin;
        self.enqueue(self.index.root().invid(inv), row);
    }

    /// `inv`'s header row as the writer holds it, with the list entry of each instance
    /// not yet listed: the entry commits in the header's transaction, so a header never
    /// lands without its entries and a restart that rewrites an existing one costs no
    /// transaction of its own. The entry's time is the header's `list_ms`, which a
    /// stored header fixed before the first write (the header rule), so it is the same
    /// entry whatever the writer has forgotten. An id-less pseudo-invocation is listed
    /// nowhere.
    fn header_row(&self, inv: &InvKey) -> Row {
        let state = &self.invs[inv];
        let root = self.index.root();
        let mut row = Row::new(state.header.encode_to_vec(), Cond::Any, None);
        if !state.header.idless {
            let list_ms = state.header.list_ms;
            let unlisted = state
                .header
                .instances
                .iter()
                .filter(|i| !state.listed.contains(*i));
            row.also = unlisted
                .map(|i| Also::Put(root.inv_list(i, list_ms, inv), Vec::new()))
                .collect();
        }
        row
    }

    /// Notes that the header rows (the [`Cond::Any`] rows) of `batch` committed.
    fn committed(&mut self, batch: &[(String, Row)]) {
        for (key, row) in batch {
            self.derived.committed(key);
            for also in &row.also {
                if let Also::Put(entry, _) = also {
                    self.listed(entry);
                }
            }
        }
        let saved: BTreeSet<&str> = batch
            .iter()
            .filter(|(_, row)| row.cond == Cond::Any)
            .map(|(key, _)| key.as_str())
            .collect();
        if saved.is_empty() {
            return;
        }
        let rows = self.index.root();
        for (key, s) in &mut self.invs {
            s.unsaved &= !saved.contains(rows.invid(key).as_str());
        }
    }

    /// Queues again the header of every invocation whose last header row did not
    /// commit, once the index is answering. One past the backlog's bounds is not
    /// counted again; it waits for the next step.
    fn requeue_headers(&mut self) {
        if self.index_failed {
            return;
        }
        let rows = self.index.root().clone();
        let unsaved: Vec<InvKey> = (self.invs.iter())
            .filter(|(_, s)| s.unsaved)
            .map(|(key, _)| key.clone())
            .collect();
        for key in unsaved {
            let row = rows.invid(&key);
            if !self.pending.contains(&row) {
                let _refused = self.pending.insert(row, self.header_row(&key));
            }
        }
    }

    /// Notes that the list entry `key`, if it is one, committed: its invocation, if the
    /// writer holds it, needs no more puts of it.
    fn listed(&mut self, key: &str) {
        let parsed = self.index.root().parse_inv_list(key);
        let held = parsed
            .and_then(|(instance, _, inv)| self.invs.get_mut(&inv).map(|state| (state, instance)));
        if let Some((state, instance)) = held {
            state.listed.insert(instance);
        }
    }

    /// The write of `key` failed its precondition. A pointer row and a shape row that
    /// exist already are the same row; a status row lost a race.
    fn conflicted(&mut self, key: &str) {
        self.derived.conflicted(key);
    }

    /// Queues the shape rows still owed, and takes the stored status rows read.
    fn queue_derived(&mut self) {
        if self.index_failed {
            return;
        }
        let owed: Vec<(String, Row)> = self
            .derived
            .unsaved_shapes()
            .filter(|(key, _)| !self.pending.contains(key))
            .map(|(key, row)| (key.to_owned(), row))
            .collect();
        for (key, row) in owed {
            let _refused = self.pending.insert(key, row);
        }
    }

    /// Takes the stored status row of `key` and queues the write of the row that the
    /// runs waiting make of it.
    fn status_read(&mut self, key: &str, stored: Option<Versioned>) {
        let root = self.index.root().clone();
        if let Some(row) = self.derived.read(&root, key, stored) {
            self.enqueue(key.to_owned(), row);
        }
    }

    /// Notes what `record` says of its shape and, for a test, of the test's status.
    fn observe(&mut self, inv: &InvRef, record: &RunRecord) {
        if record.shape == 0 {
            return;
        }
        let id = TestId {
            instance: record.instance.clone(),
            shape: record.shape,
            cfg: record.cfg.clone(),
        };
        let root = self.index.root().clone();
        let kind = if record.test {
            ShapeKind::Test
        } else {
            ShapeKind::Other
        };
        let shape = ShapeRow {
            kind: kind.into(),
            first_seen_ms: record.time_ms,
            last_seen_ms: record.time_ms,
            target: record.target.clone(),
        };
        let mut taken = self.derived.shape(&root, &id, &shape);
        let result = record.test.then(|| status::classify(record)).flatten();
        if let Some(status) = result {
            let run = Observed {
                status,
                run_id: record.run_id().to_bytes().to_vec(),
                time_ms: record.time_ms,
                execution_ms: u32::try_from(record.execution_us() / 1000).unwrap_or(u32::MAX),
                digest16: record.action.as_ref().map_or(0, |d| d.prefix16()),
                inv_key: inv
                    .record_id
                    .as_ref()
                    .map_or_else(String::new, |_| inv.key.to_string()),
            };
            taken &= self.derived.test(&root, &id, run);
        }
        self.drops.index += u64::from(!taken);
    }

    /// How many kept objects to read back this step: none while rows are pending or
    /// after an index transaction failed (their rows would only fail again).
    fn reinsert_budget(&self) -> usize {
        if self.pending.is_empty() && !self.index_failed {
            self.limits.reinserts_per_step
        } else {
            0
        }
    }

    /// Makes the writer's state for `inv` on first sight, with its stored header still
    /// to read (a restarted frontend continues the header a previous life wrote).
    /// Returns whether the writer holds it: at [`Limits::invocations`] with none idle,
    /// a new invocation is refused.
    fn open(&mut self, inv: &InvRef) -> bool {
        if self.invs.contains_key(&inv.key) {
            return true;
        }
        if self.invs.len() >= self.limits.invocations && !self.evict_one() {
            return false;
        }
        let header = InvocationHeader {
            record_id: inv.record_id.clone().unwrap_or_default(),
            idless: inv.record_id.is_none(),
            ..InvocationHeader::default()
        };
        let state = Inv {
            header,
            unread: true,
            ..Inv::default()
        };
        self.invs.insert(inv.key.clone(), state);
        true
    }

    /// The writer's state for an invocation it has already opened.
    fn state(&mut self, inv: &InvKey) -> &mut Inv {
        self.invs.entry(inv.clone()).or_default()
    }

    fn ingest(&mut self, event: Event, now_ms: u64) {
        let inv = match &event {
            Event::Run { inv, .. } | Event::Hit { inv, .. } | Event::Close { inv } => inv,
        };
        if !self.open(inv) {
            self.drops.invocations += 1;
            return;
        }
        match event {
            Event::Run {
                inv,
                record,
                failed,
                stream_opened_ms,
            } => {
                let day = Day::from_ms(record.time_ms);
                self.break_if(&inv, day, Some(record.stream), now_ms);
                self.observe(&inv, &record);
                let state = self.state(&inv.key);
                note(&mut state.header, &record.instance, record.time_ms);
                state.header.runs += 1;
                state.header.failed += u64::from(record.failed());
                rank(&mut state.slowest_execution, &record, record.execution_us());
                rank(&mut state.slowest_wall, &record, record.wall_us());
                state
                    .stream_first
                    .entry(record.stream)
                    .or_insert(stream_opened_ms);
                let mut size = record.encoded_len();
                let b = start(&mut state.building, day, now_ms);
                b.stream = Some(record.stream);
                if let Some(mut f) = failed {
                    f.seq = record.seq;
                    size += f.encoded_len();
                    b.failed.push(f);
                }
                b.runs.push(*record);
                self.grew(&inv.key, size, now_ms);
            }
            Event::Hit { inv, mut record } => {
                let day = Day::from_ms(record.time_ms);
                self.break_if(&inv, day, None, now_ms);
                let state = self.state(&inv.key);
                note(&mut state.header, &record.instance, record.time_ms);
                state.header.hits += 1;
                let b = start(&mut state.building, day, now_ms);
                // A chunk holds far fewer than 2^32 hits.
                record.row = b.hits.len() as u32;
                let size = record.encoded_len();
                b.hits.push(record);
                self.grew(&inv.key, size, now_ms);
            }
            Event::Close { inv } => {
                self.minter.close(&inv.key);
                let state = self.state(&inv.key);
                state.header.state = InvState::Closed.into();
                if state.building.records() > 0 {
                    self.cut(&inv.key, now_ms);
                } else {
                    self.write_header(&inv.key, None);
                }
            }
        }
    }

    /// Cuts `inv`'s chunk if a record of `day` and `stream` cannot join it.
    fn break_if(&mut self, inv: &InvRef, day: Day, stream: Option<u32>, now_ms: u64) {
        if self.state(&inv.key).building.breaks_on(day, stream) {
            self.cut(&inv.key, now_ms);
        }
    }

    /// Accounts `size` new raw bytes in `inv`'s chunk: cuts it at the chunk size, then
    /// sheds the oldest unwritten chunks while the writer holds too much.
    fn grew(&mut self, inv: &InvKey, size: usize, now_ms: u64) {
        let chunk_bytes = self.limits.chunk_bytes;
        self.held += size;
        let b = &mut self.state(inv).building;
        b.raw += size;
        if b.raw >= chunk_bytes {
            self.cut(inv, now_ms);
        }
        self.shed();
    }

    /// Drops the oldest chunks not yet written while the writer holds more than
    /// [`Limits::buffer_bytes`]: cut chunks first, in write order, then building chunks,
    /// longest waiting first. Each dropped record is counted, and its invocation's
    /// header counts the hole.
    fn shed(&mut self) {
        let limit = self.limits.buffer_bytes;
        if self.held <= limit {
            return;
        }
        while self.held > limit {
            let Some(cut) = self.ready.pop_front() else {
                break;
            };
            self.held -= cut.encoded.bytes.len();
            self.lost(&cut.inv, cut.records);
        }
        let mut waiting: Vec<(u64, InvKey)> = self
            .invs
            .iter()
            .filter(|(_, s)| s.building.records() > 0)
            .map(|(k, s)| (s.building.since_ms, k.clone()))
            .collect();
        waiting.sort();
        for (_, inv) in waiting {
            if self.held <= limit {
                break;
            }
            let b = std::mem::take(&mut self.state(&inv).building);
            self.held -= b.raw;
            self.lost(&inv, b.records());
        }
    }

    fn lost(&mut self, inv: &InvKey, records: usize) {
        self.drops.buffer += records as u64;
        self.state(inv).header.dropped += records as u64;
    }

    fn cut_due(&mut self, now_ms: u64) {
        let due: Vec<InvKey> = self
            .invs
            .iter()
            .filter(|(_, s)| {
                s.building.records() > 0
                    && now_ms.saturating_sub(s.building.since_ms) >= self.limits.chunk_age_ms
            })
            .map(|(k, _)| k.clone())
            .collect();
        for inv in due {
            self.cut(&inv, now_ms);
        }
    }

    /// Encodes `inv`'s building chunk and queues it for its write.
    fn cut(&mut self, inv: &InvKey, now_ms: u64) {
        let writer = self.writer;
        let seq = self.next_chunk;
        // 2^32 chunks in one life: at the design's rates (about 100,000 a day) that is
        // a century, so the wrap is never reached.
        self.next_chunk = seq.wrapping_add(1);
        let state = self.state(inv);
        let b = std::mem::take(&mut state.building);
        let day = b.day.unwrap_or(Day::from_ms(now_ms));
        let records = b.records();
        let raw = b.raw;
        let stream = b.stream.unwrap_or_default();
        let mut row = ChunkRow {
            day: day.number(),
            stream,
            first_seq: b.runs.iter().map(|r| r.seq).min().unwrap_or_default(),
            last_seq: b.runs.iter().map(|r| r.seq).max().unwrap_or_default(),
            // Counts in one chunk are bounded by its size.
            runs: b.runs.len() as u32,
            hits: b.hits.len() as u32,
            failed: b.runs.iter().filter(|r| r.failed()).count() as u32,
            ..ChunkRow::default()
        };
        let times = b
            .runs
            .iter()
            .map(|r| r.time_ms)
            .chain(b.hits.iter().map(|h| h.time_ms));
        row.first_ms = times.clone().min().unwrap_or_default();
        row.last_ms = times.max().unwrap_or_default();
        let stream_first_ms = state
            .stream_first
            .get(&stream)
            .copied()
            .unwrap_or(row.first_ms);
        let summary = ChunkSummary {
            inv_key: inv.as_str().to_owned(),
            writer: writer.0,
            seq,
            row: Some(row.clone()),
            stream_first_ms,
            header: Some(state.header.clone()),
            slowest_execution: state.slowest_execution.clone(),
            slowest_wall: state.slowest_wall.clone(),
            digests: Vec::new(),
        };
        let header = Header {
            family: Family::InvocationChunk,
            writer,
            day,
            seq,
            base_ms: row.first_ms,
        };
        let encoded = encode_chunk(&header, b, summary);
        row.bytes = encoded.bytes.len() as u64;
        row.footer_offset = encoded.footer_offset;
        row.footer_len = encoded.footer_len;
        self.held = self.held - raw + encoded.bytes.len();
        self.ready.push_back(Cut {
            inv: inv.clone(),
            key: self.root.chunk(day, inv, writer, seq),
            encoded,
            seq,
            row,
            stream_first_ms,
            records,
        });
    }

    /// Takes the answer to `cut`'s write: on success (or `AlreadyExists`, a retried
    /// write that had landed) its rows are queued; on failure it is counted and given
    /// back to be retried.
    fn written(
        &mut self,
        cut: Cut,
        result: Result<(), ObjectStoreError>,
        now_ms: u64,
    ) -> Option<Cut> {
        match result {
            Ok(()) | Err(ObjectStoreError::AlreadyExists(_)) => {}
            Err(_) => {
                self.drops.put += 1;
                return Some(cut);
            }
        }
        self.held -= cut.encoded.bytes.len();
        let origin = KeptObject {
            key: cut.key,
            footer_offset: cut.encoded.footer_offset,
            footer_len: cut.encoded.footer_len,
        };
        let writer = self.writer;
        let state = self.state(&cut.inv);
        state.header.chunks += 1;
        state.header.last_chunk_ms = now_ms;
        if cut.row.runs > 0 && state.stream_rows.insert((cut.row.day, cut.row.stream)) {
            let value = StreamRow {
                inv_key: cut.inv.as_str().to_owned(),
                writer: writer.0,
            };
            let day = Day::from_number(cut.row.day);
            let key = self
                .index
                .root()
                .stream(day, StreamId(cut.row.stream), cut.stream_first_ms);
            self.queue_row(
                key,
                value.encode_to_vec(),
                Cond::Absent,
                Some(origin.clone()),
            );
        }
        let key = self.index.root().ichunk(&cut.inv, writer, cut.seq);
        self.queue_row(
            key,
            cut.row.encode_to_vec(),
            Cond::Absent,
            Some(origin.clone()),
        );
        self.write_header(&cut.inv, Some(origin));
        None
    }

    fn queue_row(&mut self, key: String, value: Vec<u8>, cond: Cond, origin: Option<KeptObject>) {
        self.enqueue(key, Row::new(value, cond, origin));
    }

    /// Puts `row` in [`Pending`], or drops and counts it past its bounds.
    fn enqueue(&mut self, key: String, row: Row) {
        if let Err(row) = self.pending.insert(key.clone(), row) {
            self.dropped_row(&key, row);
        }
    }

    fn dropped_row(&mut self, key: &str, row: Row) {
        self.drops.index += 1;
        self.derived.dropped(key);
        if let Some(origin) = row.origin {
            self.kept.push(origin);
        }
    }

    /// Restores the rows of a kept object from its footer (`read`). Returns whether to
    /// go on with the next kept object: a failed read keeps the object and stops for
    /// this step; a footer that does not decode is left to the weekly sweep.
    fn restore(&mut self, object: KeptObject, read: Result<Bytes, ObjectStoreError>) -> bool {
        let Ok(bytes) = read else {
            self.kept.push(object);
            return false;
        };
        let parsed = container::decode_footer(&bytes)
            .ok()
            .and_then(|(_, footer)| ChunkSummary::decode(footer.summary.as_slice()).ok())
            .and_then(|s| {
                let inv = InvKey::parse(&s.inv_key).ok();
                inv.zip(s.row.clone()).map(|(inv, row)| (inv, row, s))
            });
        let Some((inv, mut row, summary)) = parsed else {
            return true;
        };
        // The summary was written before the object's length and footer position were
        // known; the footer and trailer run to the object's end.
        row.footer_offset = object.footer_offset;
        row.footer_len = object.footer_len;
        row.bytes = object.footer_offset + u64::from(object.footer_len);
        let writer = WriterId(summary.writer);
        // The chunk row first: with little room it is the row that matters most.
        let rows = self.index.root().clone();
        let key = rows.ichunk(&inv, writer, summary.seq);
        self.queue_row(key, row.encode_to_vec(), Cond::Absent, Some(object.clone()));
        // The footer's header is as of the cut. An invocation the writer still holds has
        // a newer header of its own, rewritten until it commits (or merged when its
        // stored header is read), so the footer's is put back only for one forgotten.
        if !self.invs.contains_key(&inv) {
            let header = summary.header.unwrap_or_default().encode_to_vec();
            self.queue_row(rows.invid(&inv), header, Cond::Absent, Some(object.clone()));
        }
        if row.runs > 0 {
            let value = StreamRow {
                inv_key: summary.inv_key,
                writer: writer.0,
            };
            let day = Day::from_number(row.day);
            let key = rows.stream(day, StreamId(row.stream), summary.stream_first_ms);
            self.queue_row(key, value.encode_to_vec(), Cond::Absent, Some(object));
        }
        true
    }
}

/// Encodes a chunk of `b`'s records with `summary`, adding the digest table.
fn encode_chunk(header: &Header, b: Building, mut summary: ChunkSummary) -> Encoded {
    let mut records = Vec::with_capacity(b.records());
    let bodies = b
        .runs
        .into_iter()
        .map(ChunkBody::Run)
        .chain(b.failed.into_iter().map(ChunkBody::Failed))
        .chain(b.hits.into_iter().map(ChunkBody::Hit));
    for body in bodies {
        let key = body.key().to_vec();
        let action = match &body {
            ChunkBody::Run(r) => r.action.as_ref(),
            ChunkBody::Hit(h) => h.action.as_ref(),
            ChunkBody::Failed(_) => None,
        };
        if let Some(d) = action {
            summary.digests.push(DigestRow {
                prefix: d.prefix16(),
                key: key.clone(),
            });
        }
        records.push((key, ChunkRecord { body: Some(body) }.encode_to_vec()));
    }
    summary
        .digests
        .sort_by(|a, b| (a.prefix, &a.key).cmp(&(b.prefix, &b.key)));
    container::encode(header, records, summary.encode_to_vec())
}

/// Starts `b` for a record of `day` at `now_ms` if it is empty, and returns it.
fn start(b: &mut Building, day: Day, now_ms: u64) -> &mut Building {
    if b.records() == 0 {
        b.since_ms = now_ms;
    }
    b.day = Some(day);
    b
}

/// Notes a record of `instance` at `time_ms` in an invocation's header, which reopens
/// it if it was closed.
fn note(h: &mut InvocationHeader, instance: &str, time_ms: u64) {
    h.state = InvState::Open.into();
    if h.list_ms == 0 {
        h.list_ms = time_ms;
    }
    add_instance(h, instance);
    h.first_seen_ms = earliest(h.first_seen_ms, time_ms);
    h.last_seen_ms = h.last_seen_ms.max(time_ms);
}

/// Lists `instance` in `h`, or, with [`MAX_INSTANCES`] listed, marks the list
/// truncated: the names are the client's, so the header stays bounded.
fn add_instance(h: &mut InvocationHeader, instance: &str) {
    if let Err(at) = h.instances.binary_search_by(|i| i.as_str().cmp(instance)) {
        if h.instances.len() >= MAX_INSTANCES {
            h.instances_truncated = true;
        } else {
            h.instances.insert(at, instance.to_owned());
        }
    }
}

/// The earlier of two times, where 0 is "none yet".
fn earliest(a: u64, b: u64) -> u64 {
    match (a, b) {
        (0, t) | (t, 0) => t,
        (a, b) => a.min(b),
    }
}

/// The stored header with what a writer added since it opened the invocation (`added`,
/// counted from zero): counts summed, instances joined, the time span widened, and
/// the state the writer saw last.
fn merge(stored: InvocationHeader, added: &InvocationHeader) -> InvocationHeader {
    let mut h = stored;
    for i in &added.instances {
        add_instance(&mut h, i);
    }
    h.instances_truncated |= added.instances_truncated;
    h.runs += added.runs;
    h.hits += added.hits;
    h.failed += added.failed;
    h.dropped += added.dropped;
    h.chunks += added.chunks;
    h.first_seen_ms = earliest(h.first_seen_ms, added.first_seen_ms);
    h.last_seen_ms = h.last_seen_ms.max(added.last_seen_ms);
    h.last_chunk_ms = h.last_chunk_ms.max(added.last_chunk_ms);
    h.state = added.state;
    // The stored time keys the entries that exist: it is never moved.
    if h.list_ms == 0 {
        h.list_ms = added.list_ms;
    }
    h
}

/// Puts `record` among the [`SLOWEST`] runs of `list` by `us`, slowest first.
fn rank(list: &mut Vec<Slow>, record: &RunRecord, us: u64) {
    let at = list.partition_point(|s| s.us >= us);
    if at >= SLOWEST {
        return;
    }
    list.insert(
        at,
        Slow {
            run_id: record.run_id().to_bytes().to_vec(),
            us,
        },
    );
    list.truncate(SLOWEST);
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
