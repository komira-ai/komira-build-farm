//! The running tier: reservations, positioned writes, syncs, answers and seals.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use kbf_meta::{ObjectId, Writer};
use kbf_segments::MAX_SEGMENT_BYTES;
use kbf_store::{Fs, FsFile};

use crate::block::{block_name, seg_name};
use crate::error::TierError;

/// How a tier is sized and when it seals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierOptions {
    /// The hot store's budget: the most bytes the tier's block files may hold before
    /// they are flushed: every live block's reserved extent, open uploads' included.
    pub budget: u64,
    /// How long the open grouped block takes reservations.
    pub seal_after: Duration,
    /// The size at which the open grouped block is sealed: [`MAX_SEGMENT_BYTES`].
    pub segment_limit: u64,
    /// Whether a flushed block is kept as a flushed copy (`.seg`) for local reads. Off
    /// on a node whose one cache keeps the local copies (`kbf_storage::local_cache`):
    /// a flushed block is then deleted, and a start deletes the copies an earlier run
    /// kept.
    pub flushed_copies: bool,
}

impl TierOptions {
    /// The seal delay of the design (30 s).
    pub const SEAL_AFTER: Duration = Duration::from_secs(30);

    /// A tier of `budget` bytes, sealing after [`TierOptions::SEAL_AFTER`] or at
    /// [`MAX_SEGMENT_BYTES`].
    #[must_use]
    pub const fn new(budget: u64) -> Self {
        Self {
            budget,
            seal_after: Self::SEAL_AFTER,
            segment_limit: MAX_SEGMENT_BYTES,
            flushed_copies: true,
        }
    }

    /// These options with flushed copies off ([`TierOptions::flushed_copies`]).
    #[must_use]
    pub const fn without_flushed_copies(self) -> Self {
        Self {
            flushed_copies: false,
            ..self
        }
    }
}

/// How full the tier is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// [`TierOptions::budget`].
    pub budget: u64,
    /// The bytes counted against it: the reserved extent of every block not yet
    /// unstaged or removed.
    pub used: u64,
    /// The bytes of flushed copies (`.seg` files) kept for local reads. They fill
    /// what the budget leaves over and are evicted, least recently read first, before
    /// a reservation is refused.
    pub flushed: u64,
    /// Flushed copies evicted since the tier opened.
    pub evicted: u64,
}

/// What a disk loss would take now: acknowledged bytes not yet in the object store
/// (design section 6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unflushed {
    /// Committed bytes of every block not yet flushed.
    pub bytes: u64,
    /// Blocks holding committed bytes not yet flushed.
    pub blocks: u64,
    /// The age of the oldest such block: since it was created, or, for a block
    /// recovered at a restart, since that restart. `None` when there is none.
    pub oldest: Option<Duration>,
}

/// A byte range reserved in one block file. Write it ([`WriteTier::write`]), sync it
/// ([`WriteTier::sync`]), and answer it: [`WriteTier::committed`] once its
/// `PutStaged` committed, or drop it (or its [`Synced`]) if that failed. Dropping it
/// unanswered is a failure: a seal never waits on a range whose caller went away.
#[derive(Debug)]
#[must_use = "an unanswered reservation is answered as failed when dropped"]
pub struct Reservation {
    object: ObjectId,
    offset: u64,
    len: u64,
    tier: Option<Weak<dyn Answer>>,
}

impl Reservation {
    /// The block (and so the object) the range is in.
    #[must_use]
    pub const fn object(&self) -> ObjectId {
        self.object
    }

    /// Where the range starts in the block: the offset of its first record in the
    /// object the block becomes.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// How many bytes it holds.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether it holds none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The end of the range: `PutStaged`'s `end` for this object.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.offset + self.len
    }

    fn answer(&mut self, committed: bool) {
        if let Some(tier) = self.tier.take().and_then(|t| t.upgrade()) {
            tier.answer(self.object, self.end(), committed);
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.answer(false);
    }
}

/// A reservation whose bytes are durable (the file synced through its handle after
/// the writes, and the block's name synced): what a `PutStaged` may name. Only
/// [`WriteTier::sync`] makes one.
///
/// It takes no more writes: [`WriteTier::write`] takes a [`Reservation`], which
/// [`WriteTier::sync`] consumed, and a `Synced` hands none out. A byte written after
/// the sync would be acknowledged without being durable. What a caller does with one:
///
/// ```
/// # use kbf_tier::{Synced, WriteTier};
/// fn commit(tier: &dyn WriteTier, synced: Synced) {
///     let _put_staged = (synced.object(), synced.offset(), synced.end());
///     tier.committed(synced);
/// }
/// ```
///
/// and what it cannot:
///
/// ```compile_fail
/// # use kbf_tier::{Synced, WriteTier};
/// fn rewrite(tier: &dyn WriteTier, synced: &Synced) {
///     let _ = tier.write(synced.reservation(), 0, b"late");
/// }
/// ```
#[derive(Debug)]
#[must_use = "a synced reservation is answered as failed when dropped"]
pub struct Synced(Reservation);

impl Synced {
    /// The block (and so the object) the range is in: `PutStaged`'s object.
    #[must_use]
    pub const fn object(&self) -> ObjectId {
        self.0.object
    }

    /// Where the range starts in the block.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.0.offset
    }

    /// The end of the range: `PutStaged`'s `end` for this object.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.0.end()
    }
}

/// A sync that failed: the reservation (drop it to answer it as failed) and why.
#[derive(Debug)]
pub struct Unsynced {
    /// The reservation, still unanswered.
    pub reservation: Reservation,
    /// What failed.
    pub error: TierError,
}

/// The local write tier's disk layer, as the write group, segment uploads, the
/// flusher and the metadata (rollout stages 3 and 4b) use it. [`Tier`] is the one
/// implementation; the trait keeps the filesystem type out of its callers.
///
/// A caller's sequence, per range: reserve, write, sync, commit `PutStaged` naming
/// [`Synced::object`] and [`Synced::end`], then [`WriteTier::committed`]; on any
/// failure drop the reservation. Sealing: [`WriteTier::poll_seals`] names the blocks
/// whose `SealStaged` may be committed now; after that commit,
/// [`WriteTier::sealed`].
pub trait WriteTier: Send + Sync {
    /// Reserves `len` bytes in the open grouped block, opening a new block (and
    /// sealing the old one) if the range would pass the segment limit or the block's
    /// seal delay has passed at `now`.
    ///
    /// # Errors
    ///
    /// [`TierError::Full`] when the budget has no room (use the direct path);
    /// [`TierError::TooLarge`] for a range over the segment limit;
    /// [`TierError::Io`] if a new block file could not be created.
    fn reserve_group(&self, len: u64, now: Instant) -> Result<Reservation, TierError>;

    /// Reserves a block of its own for one segment of a segment upload, of `len`
    /// bytes (its records, up to the declared `index_offset`).
    ///
    /// # Errors
    ///
    /// [`TierError::Full`] when the budget has no room; [`TierError::Io`] if the
    /// block file could not be created.
    fn reserve_upload(&self, len: u64) -> Result<Reservation, TierError>;

    /// Writes `bytes` at `offset` within `reservation`'s range.
    ///
    /// # Errors
    ///
    /// [`TierError::OutOfRange`] for bytes outside the range;
    /// [`TierError::Poisoned`] after an earlier I/O error on the block;
    /// [`TierError::Io`], which poisons the block.
    fn write(&self, reservation: &Reservation, offset: u64, bytes: &[u8]) -> Result<(), TierError>;

    /// Makes the range's bytes and the block's name durable: `fdatasync` on the
    /// block's handle, then `sync_dir` once per block.
    ///
    /// # Errors
    ///
    /// An [`Unsynced`] holding the reservation: [`TierError::Poisoned`] if any I/O
    /// error on the block came before or during this sync (a failed sync on another
    /// range of the block may have taken this range's write-back error), or
    /// [`TierError::Io`], which poisons the block. The syncs of one block run one at
    /// a time, so none succeeds while another sync of the block is failing.
    fn sync(&self, reservation: Reservation) -> Result<Synced, Unsynced>;

    /// Answers a range whose `PutStaged` committed: the block's committed length
    /// becomes at least its end. An upload's block is sealed by the same commit.
    fn committed(&self, synced: Synced);

    /// Starts the seal of the open grouped block if its delay has passed at `now`,
    /// and names every block whose `SealStaged` may be committed: sealing, with every
    /// reservation answered and at least one committed. A sealing block whose
    /// reservations all failed is removed instead. A block is named on every call
    /// until [`WriteTier::sealed`].
    fn poll_seals(&self, now: Instant) -> Vec<ObjectId>;

    /// Records that `object`'s `SealStaged` committed.
    ///
    /// # Errors
    ///
    /// [`TierError::NotSealable`] unless [`WriteTier::poll_seals`] named it.
    fn sealed(&self, object: ObjectId) -> Result<(), TierError>;

    /// `len` bytes of `object`'s block from `offset`, all below its committed length.
    ///
    /// # Errors
    ///
    /// [`TierError::NotCommitted`] for bytes past the committed length (never
    /// acknowledged, so never served); [`TierError::UnknownBlock`]; [`TierError::Io`].
    fn read(&self, object: ObjectId, offset: u64, len: usize) -> Result<Vec<u8>, TierError>;

    /// Stops counting a sealed block against the budget once its object is flushed
    /// (`ObjectFlushed` committed). Its file is left as it is, for the flusher.
    ///
    /// # Errors
    ///
    /// [`TierError::NotSealable`] unless the block is sealed.
    fn unstage(&self, object: ObjectId) -> Result<(), TierError>;

    /// Records that `object`'s `ObjectFlushed` committed: the block stops counting
    /// against the budget and, with [`TierOptions::flushed_copies`], its file is
    /// renamed `<epoch>-<seq>.seg` (and the directory synced), a flushed copy
    /// [`WriteTier::read`] still serves until it is evicted; without, its file is
    /// deleted and a read of it is [`TierError::UnknownBlock`]. Call it only once the
    /// commit applied: eviction then never touches a block whose only copy is local.
    ///
    /// # Errors
    ///
    /// [`TierError::NotSealable`] unless the block is sealed (or ready to be: its
    /// `SealStaged` may have committed without the tier hearing); [`TierError::Io`]
    /// if the rename, the delete or the directory sync failed. The block stops counting either
    /// way: the object is in the store, and a `.blk` left behind is an orphan the
    /// next start deletes.
    fn flushed(&self, object: ObjectId) -> Result<(), TierError>;

    /// Takes no more ranges into `object`'s block if it is the open grouped block: it
    /// starts sealing, as a full one does, and the next range opens a new block. For
    /// an object marked unreachable while its block was open (a read found a damaged
    /// record), whose later `PutStaged` would be refused. Any other block is left as
    /// it is.
    fn close(&self, object: ObjectId);

    /// Forgets a sealed block whose object was marked unreachable (a record failed its
    /// digest) and deletes its file.
    ///
    /// # Errors
    ///
    /// [`TierError::NotSealable`] unless the block is sealed or ready to be.
    fn discard(&self, object: ObjectId) -> Result<(), TierError>;

    /// How full the tier is.
    fn usage(&self) -> Usage;

    /// The acknowledged bytes not yet flushed, and the oldest block's age at `now`.
    fn unflushed(&self, now: Instant) -> Unflushed;
}

/// What a dropped or committed reservation tells its tier.
trait Answer: Send + Sync + std::fmt::Debug {
    fn answer(&self, object: ObjectId, end: u64, committed: bool);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Grouped,
    Upload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The grouped block taking reservations, or an upload's block being written.
    Open,
    /// No new reservations; waiting for the outstanding ones to be answered.
    Sealing,
    /// Every reservation answered and one committed: `SealStaged` may be committed.
    SealReady,
    /// Sealed in the committed state: for the flusher.
    Sealed,
}

#[derive(Debug)]
pub(crate) struct Block<File> {
    kind: Kind,
    pub(crate) phase: Phase,
    /// The handle every write and sync of the block goes through; `None` once nothing
    /// will write it again.
    file: Option<Arc<Handle<File>>>,
    opened: Option<Instant>,
    /// When the block was created, or recovered at a restart: the start of its
    /// exposure ([`Unflushed::oldest`]).
    born: Instant,
    /// The bytes reserved: the end of its last range. Counted against the budget.
    pub(crate) extent: u64,
    /// The committed length: the largest end of a committed range.
    pub(crate) committed: u64,
    committed_any: bool,
    outstanding: u64,
    dir_synced: bool,
    poisoned: bool,
}

impl<File> Block<File> {
    /// A block recovered at open, at `now`: committed, never written again.
    pub(crate) const fn recovered(len: u64, phase: Phase, now: Instant) -> Self {
        Self {
            kind: Kind::Grouped,
            phase,
            file: None,
            opened: None,
            born: now,
            extent: len,
            committed: len,
            committed_any: true,
            outstanding: 0,
            dir_synced: true,
            poisoned: false,
        }
    }
}

/// A block's open file, and the lock its syncs take one at a time.
#[derive(Debug)]
struct Handle<File> {
    file: File,
    /// Held from the `fdatasync` through the poison check and the `sync_dir`. The
    /// kernel reports a write-back error to one sync of the shared handle only: a
    /// sync that overlapped a failing one could otherwise succeed before that one
    /// poisoned the block, or before the block's name was durable.
    sync: Mutex<()>,
}

/// A block's handle, and whether the block is poisoned.
type Opened<File> = (Arc<Handle<File>>, bool);

/// A flushed copy: a `.seg` file kept for local reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seg {
    pub(crate) len: u64,
    /// The read clock at its last read (or its rename): the least is evicted first.
    pub(crate) last_read: u64,
}

#[derive(Debug)]
pub(crate) struct State<File> {
    pub(crate) blocks: BTreeMap<ObjectId, Block<File>>,
    /// The grouped block taking reservations.
    open: Option<ObjectId>,
    pub(crate) used: u64,
    /// Flushed copies, and their total length.
    pub(crate) segs: BTreeMap<ObjectId, Seg>,
    pub(crate) seg_bytes: u64,
    /// Counts reads of flushed copies, for least-recently-read eviction.
    reads: u64,
    evicted: u64,
}

impl<File> State<File> {
    pub(crate) const fn new(used: u64) -> Self {
        Self {
            blocks: BTreeMap::new(),
            open: None,
            used,
            segs: BTreeMap::new(),
            seg_bytes: 0,
            reads: 0,
            evicted: 0,
        }
    }

    /// Takes a flushed copy of `len` bytes into the accounting. An object is flushed
    /// once, and a start lists each name once, so it is never there already.
    pub(crate) fn add_seg(&mut self, object: ObjectId, len: u64) {
        let last_read = self.reads;
        self.segs.insert(object, Seg { len, last_read });
        self.seg_bytes += len;
    }
}

/// Allocates the sequence numbers of the tier's objects within its writer, shared with
/// the writer that numbers the direct path's objects so the two never collide.
pub type NextSeq = Box<dyn Fn() -> u64 + Send + Sync>;

struct Shared<F: Fs> {
    fs: F,
    options: TierOptions,
    writer: Writer,
    state: Mutex<State<F::File>>,
    next_seq: NextSeq,
}

impl<F: Fs + std::fmt::Debug> std::fmt::Debug for Shared<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tier")
            .field("fs", &self.fs)
            .field("options", &self.options)
            .field("writer", &self.writer)
            .finish_non_exhaustive()
    }
}

/// The local write tier over the block files of one directory ([`Fs`]): see
/// [`WriteTier`]. Made by [`Tier::open`](crate::Tier::open).
pub struct Tier<F: Fs> {
    shared: Arc<Shared<F>>,
}

impl<F: Fs + std::fmt::Debug> std::fmt::Debug for Tier<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.shared.fmt(f)
    }
}

impl<F> Tier<F>
where
    F: Fs + Send + Sync + std::fmt::Debug + 'static,
    F::File: Send + Sync + std::fmt::Debug + 'static,
{
    pub(crate) fn from_parts(
        fs: F,
        options: TierOptions,
        writer: Writer,
        state: State<F::File>,
        next_seq: NextSeq,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                fs,
                options,
                writer,
                state: Mutex::new(state),
                next_seq,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<F::File>> {
        self.shared.lock()
    }

    /// Evicts flushed copies found at open that do not fit beside the staged blocks.
    pub(crate) fn make_room_at_open(&self) {
        let mut state = self.lock();
        self.shared.make_room(&mut state, 0);
    }

    /// Creates a block of `kind` with one reservation of `len` bytes.
    fn new_block(
        &self,
        state: &mut State<F::File>,
        kind: Kind,
        len: u64,
        now: Option<Instant>,
    ) -> Result<Reservation, TierError> {
        let object = ObjectId::of(self.shared.writer, (self.shared.next_seq)());
        let name = block_name(object);
        let file = self
            .shared
            .fs
            .create_positioned(&name)
            .map_err(|e| TierError::io("create", &name, e))?;
        state.blocks.insert(
            object,
            Block {
                kind,
                phase: Phase::Open,
                file: Some(Arc::new(Handle {
                    file,
                    sync: Mutex::new(()),
                })),
                opened: now,
                born: now.unwrap_or_else(Instant::now),
                extent: 0,
                committed: 0,
                committed_any: false,
                outstanding: 0,
                dir_synced: false,
                poisoned: false,
            },
        );
        Ok(self.take_range(state, object, len))
    }

    fn take_range(&self, state: &mut State<F::File>, object: ObjectId, len: u64) -> Reservation {
        let block = state.blocks.get_mut(&object).expect("block just looked up");
        let offset = block.extent;
        block.extent += len;
        block.outstanding += 1;
        state.used += len;
        let tier: Weak<dyn Answer> = Arc::downgrade(&self.shared) as Weak<dyn Answer>;
        Reservation {
            object,
            offset,
            len,
            tier: Some(tier),
        }
    }

    fn check_budget(&self, state: &State<F::File>, len: u64) -> Result<(), TierError> {
        let budget = self.shared.options.budget;
        if state.used.saturating_add(len) > budget {
            return Err(TierError::Full {
                want: len,
                used: state.used,
                budget,
            });
        }
        Ok(())
    }

    /// The handle of the block a range is in, and whether the block is poisoned.
    fn handle(&self, object: ObjectId) -> Result<Opened<F::File>, TierError> {
        let state = self.lock();
        let block = state
            .blocks
            .get(&object)
            .ok_or(TierError::UnknownBlock(object))?;
        let file = block.file.clone().ok_or(TierError::UnknownBlock(object))?;
        Ok((file, block.poisoned))
    }

    fn poison(&self, object: ObjectId, error: TierError) -> TierError {
        let mut state = self.lock();
        let block = state
            .blocks
            .get_mut(&object)
            .expect("block of a reservation");
        block.poisoned = true;
        error
    }

    /// A flushed copy's bytes. A copy evicted since the lookup is
    /// [`TierError::UnknownBlock`], so the reader goes to the store.
    fn read_seg(&self, object: ObjectId, offset: u64, len: usize) -> Result<Vec<u8>, TierError> {
        let name = seg_name(object);
        match self.shared.fs.read_at(&name, offset, len) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Deleted under the tier, or evicted since the lookup (then not listed).
                let mut state = self.lock();
                let len = state.segs.remove(&object).map_or(0, |seg| seg.len);
                state.seg_bytes -= len;
                Err(TierError::UnknownBlock(object))
            }
            read => read.map_err(|e| TierError::io("read", &name, e)),
        }
    }

    /// The extent of a block [`WriteTier::flushed`] or [`WriteTier::discard`] may
    /// take: one sealed, or ready to be.
    fn flushable(&self, state: &State<F::File>, object: ObjectId) -> Result<u64, TierError> {
        match state.blocks.get(&object) {
            Some(block) if matches!(block.phase, Phase::SealReady | Phase::Sealed) => {
                Ok(block.extent)
            }
            _ => Err(TierError::NotSealable(object)),
        }
    }

    fn sync_range(&self, r: &Reservation) -> Result<(), TierError> {
        // Poisoning is checked after the sync, not before: see below.
        let (handle, _) = self.handle(r.object)?;
        let _one_at_a_time = handle.sync.lock().unwrap_or_else(PoisonError::into_inner);
        let name = block_name(r.object);
        handle
            .file
            .sync()
            .map_err(|e| self.poison(r.object, TierError::io("sync", &name, e)))?;
        let mut state = self.lock();
        let block = state
            .blocks
            .get_mut(&r.object)
            .expect("block of a reservation");
        // A failed write or sync of another range before this sync (a failing sync
        // cannot overlap it: the lock) may have reported, and so cleared, a
        // write-back error of this range's pages: this sync's success proves nothing
        // then.
        if block.poisoned {
            return Err(TierError::Poisoned(r.object));
        }
        if !block.dir_synced {
            drop(state);
            self.shared
                .fs
                .sync_dir()
                .map_err(|e| self.poison(r.object, TierError::io("sync_dir", &name, e)))?;
            let mut state = self.lock();
            let block = state
                .blocks
                .get_mut(&r.object)
                .expect("block of a reservation");
            block.dir_synced = true;
        }
        Ok(())
    }
}

impl<F: Fs> Shared<F> {
    fn lock(&self) -> MutexGuard<'_, State<F::File>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Moves a sealing block on once its reservations are all answered: to
    /// [`Phase::SealReady`] if one committed, else removes it.
    fn settle(&self, state: &mut State<F::File>, object: ObjectId) {
        let block = state.blocks.get_mut(&object).expect("block to settle");
        if block.phase != Phase::Sealing || block.outstanding > 0 {
            return;
        }
        block.file = None;
        if block.committed_any {
            block.phase = Phase::SealReady;
        } else {
            self.remove(state, object);
        }
    }

    /// Forgets a block nothing committed and deletes its file. A failed removal leaves
    /// an orphan, which the next start's sweep deletes.
    fn remove(&self, state: &mut State<F::File>, object: ObjectId) {
        let block = state.blocks.remove(&object).expect("block to remove");
        state.used -= block.extent;
        let _ = self.fs.remove(&block_name(object));
    }

    /// Evicts flushed copies, least recently read first, until `want` more bytes fit
    /// in the budget beside the blocks and the copies left, or no copy is left. A
    /// removal that fails leaves a file the next start lists again.
    pub(crate) fn make_room(&self, state: &mut State<F::File>, want: u64) {
        let budget = self.options.budget;
        while state
            .used
            .saturating_add(state.seg_bytes)
            .saturating_add(want)
            > budget
        {
            let Some(object) = state
                .segs
                .iter()
                .min_by_key(|(object, seg)| (seg.last_read, **object))
                .map(|(object, _)| *object)
            else {
                return;
            };
            let seg = state.segs.remove(&object).expect("copy just found");
            state.seg_bytes -= seg.len;
            state.evicted += 1;
            let _ = self.fs.remove(&seg_name(object));
        }
    }

    /// Whether `block`'s seal delay has passed at `now`.
    fn aged(&self, block: &Block<F::File>, now: Instant) -> bool {
        block
            .opened
            .is_some_and(|t| now.saturating_duration_since(t) >= self.options.seal_after)
    }

    /// Seals the open grouped block `object`: it takes no new range.
    fn begin_seal(&self, state: &mut State<F::File>, object: ObjectId) {
        state.open = None;
        let block = state.blocks.get_mut(&object).expect("open block");
        block.phase = Phase::Sealing;
        self.settle(state, object);
    }
}

impl<F> Answer for Shared<F>
where
    F: Fs + Send + Sync + std::fmt::Debug,
    F::File: Send + Sync + std::fmt::Debug,
{
    fn answer(&self, object: ObjectId, end: u64, committed: bool) {
        let mut state = self.lock();
        let block = state
            .blocks
            .get_mut(&object)
            .expect("block of a reservation");
        block.outstanding -= 1;
        if committed {
            block.committed = block.committed.max(end);
            block.committed_any = true;
        }
        if block.kind == Kind::Upload {
            if committed {
                block.phase = Phase::Sealed;
                block.file = None;
            } else {
                self.remove(&mut state, object);
            }
            return;
        }
        self.settle(&mut state, object);
    }
}

impl<F> WriteTier for Tier<F>
where
    F: Fs + Send + Sync + std::fmt::Debug + 'static,
    F::File: Send + Sync + std::fmt::Debug + 'static,
{
    fn reserve_group(&self, len: u64, now: Instant) -> Result<Reservation, TierError> {
        let limit = self.shared.options.segment_limit;
        if len > limit {
            return Err(TierError::TooLarge { len, limit });
        }
        let mut state = self.lock();
        let done =
            |b: &Block<F::File>| b.poisoned || self.shared.aged(b, now) || b.extent + len > limit;
        if let Some(object) = state.open.filter(|o| done(&state.blocks[o])) {
            self.shared.begin_seal(&mut state, object);
        }
        self.shared.make_room(&mut state, len);
        self.check_budget(&state, len)?;
        let r = match state.open {
            Some(object) => self.take_range(&mut state, object, len),
            None => {
                let r = self.new_block(&mut state, Kind::Grouped, len, Some(now))?;
                state.open = Some(r.object);
                r
            }
        };
        if state.blocks[&r.object].extent >= limit {
            self.shared.begin_seal(&mut state, r.object);
        }
        Ok(r)
    }

    fn reserve_upload(&self, len: u64) -> Result<Reservation, TierError> {
        let mut state = self.lock();
        self.shared.make_room(&mut state, len);
        self.check_budget(&state, len)?;
        self.new_block(&mut state, Kind::Upload, len, None)
    }

    fn write(&self, r: &Reservation, offset: u64, bytes: &[u8]) -> Result<(), TierError> {
        let len = bytes.len() as u64;
        if offset.checked_add(len).is_none_or(|end| end > r.len) {
            return Err(TierError::OutOfRange {
                offset,
                len,
                reserved: r.len,
            });
        }
        let (handle, poisoned) = self.handle(r.object)?;
        if poisoned {
            return Err(TierError::Poisoned(r.object));
        }
        handle.file.write_at(r.offset + offset, bytes).map_err(|e| {
            let error = TierError::io("write", &block_name(r.object), e);
            self.poison(r.object, error)
        })
    }

    fn sync(&self, reservation: Reservation) -> Result<Synced, Unsynced> {
        match self.sync_range(&reservation) {
            Ok(()) => Ok(Synced(reservation)),
            Err(error) => Err(Unsynced { reservation, error }),
        }
    }

    fn committed(&self, mut synced: Synced) {
        synced.0.answer(true);
    }

    fn poll_seals(&self, now: Instant) -> Vec<ObjectId> {
        let mut state = self.lock();
        let aged = state
            .open
            .filter(|o| self.shared.aged(&state.blocks[o], now));
        if let Some(object) = aged {
            self.shared.begin_seal(&mut state, object);
        }
        state
            .blocks
            .iter()
            .filter(|(_, b)| b.phase == Phase::SealReady)
            .map(|(object, _)| *object)
            .collect()
    }

    fn sealed(&self, object: ObjectId) -> Result<(), TierError> {
        let mut state = self.lock();
        match state.blocks.get_mut(&object) {
            Some(block) if block.phase == Phase::SealReady => {
                block.phase = Phase::Sealed;
                Ok(())
            }
            _ => Err(TierError::NotSealable(object)),
        }
    }

    fn read(&self, object: ObjectId, offset: u64, len: usize) -> Result<Vec<u8>, TierError> {
        let committed = {
            let mut state = self.lock();
            match state.blocks.get(&object) {
                Some(block) => Some(block.committed),
                None => {
                    state.reads += 1;
                    let now = state.reads;
                    let seg = state.segs.get_mut(&object);
                    seg.ok_or(TierError::UnknownBlock(object))?.last_read = now;
                    None
                }
            }
        };
        let Some(committed) = committed else {
            return self.read_seg(object, offset, len);
        };
        let end = offset.saturating_add(len as u64);
        if end > committed {
            return Err(TierError::NotCommitted {
                object,
                end,
                committed,
            });
        }
        let name = block_name(object);
        match self.shared.fs.read_at(&name, offset, len) {
            // Flushed and renamed since the lookup: the copy has the same bytes.
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.read_seg(object, offset, len),
            read => read.map_err(|e| TierError::io("read", &name, e)),
        }
    }

    fn unstage(&self, object: ObjectId) -> Result<(), TierError> {
        let mut state = self.lock();
        let extent = match state.blocks.get(&object) {
            Some(block) if block.phase == Phase::Sealed => block.extent,
            _ => return Err(TierError::NotSealable(object)),
        };
        state.blocks.remove(&object);
        state.used -= extent;
        Ok(())
    }

    fn flushed(&self, object: ObjectId) -> Result<(), TierError> {
        let mut state = self.lock();
        let extent = self.flushable(&state, object)?;
        state.blocks.remove(&object);
        state.used -= extent;
        let (from, to) = (block_name(object), seg_name(object));
        if !self.shared.options.flushed_copies {
            // Unsynced: a block left by a crash is an orphan at the next start.
            return (self.shared.fs.remove(&from)).map_err(|e| TierError::io("remove", &from, e));
        }
        self.shared
            .fs
            .rename(&from, &to)
            .map_err(|e| TierError::io("rename", &from, e))?;
        // Moving the extent from the blocks to the copies keeps their sum, which every
        // reservation kept within the budget: nothing to evict here.
        state.add_seg(object, extent);
        self.shared
            .fs
            .sync_dir()
            .map_err(|e| TierError::io("sync_dir", &to, e))
    }

    fn close(&self, object: ObjectId) {
        let mut state = self.lock();
        if state.open == Some(object) {
            self.shared.begin_seal(&mut state, object);
        }
    }

    fn discard(&self, object: ObjectId) -> Result<(), TierError> {
        let mut state = self.lock();
        let extent = self.flushable(&state, object)?;
        state.blocks.remove(&object);
        state.used -= extent;
        let name = block_name(object);
        // A removal that fails leaves an orphan the next start deletes.
        let _ = self.shared.fs.remove(&name);
        Ok(())
    }

    fn usage(&self) -> Usage {
        let state = self.lock();
        Usage {
            budget: self.shared.options.budget,
            used: state.used,
            flushed: state.seg_bytes,
            evicted: state.evicted,
        }
    }

    fn unflushed(&self, now: Instant) -> Unflushed {
        let state = self.lock();
        let held = state.blocks.values().filter(|b| b.committed > 0);
        let (mut bytes, mut blocks, mut oldest) = (0, 0, None::<Instant>);
        for block in held {
            bytes += block.committed;
            blocks += 1;
            oldest = Some(oldest.map_or(block.born, |o| o.min(block.born)));
        }
        Unflushed {
            bytes,
            blocks,
            oldest: oldest.map(|born| now.saturating_duration_since(born)),
        }
    }
}
