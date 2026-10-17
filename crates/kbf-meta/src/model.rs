//! The values the metadata core stores, takes and answers with.

use std::collections::BTreeSet;
use std::fmt;
use std::num::NonZeroU64;
use std::time::Duration;

use kbf_types::Digest;

use crate::tier::TierId;

/// A writer epoch: how an [`Writer::Epoch`] writer is named.
///
/// The metadata core hands out epochs through [`Command::AllocEpoch`](crate::Command::AllocEpoch),
/// strictly increasing and never twice. A writer (one cache in one process) takes one
/// at start and numbers its objects within it, so two writers, or one writer before
/// and after a restart, never name the same object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Epoch(u64);

impl Epoch {
    /// The epoch numbered `n`. Only an epoch the core allocated may be written under.
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A registered storage node's writer: the member id and boot counter the state store
/// assigned at its registration (`docs/design/architecture-v2.md`, section 6). The store
/// hands each pair to one registration only, and a member's boot counter grows with
/// every registration, so the pair names one process of the farm for ever: a node whose
/// metadata directory was wiped still never reuses a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeWriter {
    member: NonZeroU64,
    boot: NonZeroU64,
}

impl NodeWriter {
    /// Boot `boot` of member `member`.
    #[must_use]
    pub const fn new(member: NonZeroU64, boot: NonZeroU64) -> Self {
        Self { member, boot }
    }

    /// The member id.
    #[must_use]
    pub const fn member(self) -> NonZeroU64 {
        self.member
    }

    /// The boot counter.
    #[must_use]
    pub const fn boot(self) -> NonZeroU64 {
        self.boot
    }
}

/// A writer epoch of another log: what a rebuild of the index names an object by when
/// the object's key is `<root><log id>/cas/<epoch>/<seq>` under a log id that is not
/// this log's. The epoch alone would be ambiguous (every log numbers its epochs from
/// 1), so the id of the log that allocated it comes with it. Such an object is never
/// written by this log; it enters the index only by
/// [`Command::AdoptSegment`](crate::Command::AdoptSegment).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LegacyWriter {
    log: NonZeroU64,
    epoch: Epoch,
}

impl LegacyWriter {
    /// Epoch `epoch` of log `log`.
    #[must_use]
    pub const fn new(log: NonZeroU64, epoch: Epoch) -> Self {
        Self { log, epoch }
    }

    /// The id of the log that allocated the epoch.
    #[must_use]
    pub const fn log(self) -> NonZeroU64 {
        self.log
    }

    /// The epoch, in that log.
    #[must_use]
    pub const fn epoch(self) -> Epoch {
        self.epoch
    }
}

/// Who named an object: the first half of every [`ObjectId`].
///
/// Ordered with every epoch writer before every node writer, and every node writer
/// before every other log's epoch writer; within a kind, by number (member, then boot;
/// log, then epoch).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Writer {
    /// A writer epoch this log allocated ([`Command::AllocEpoch`](crate::Command::AllocEpoch)):
    /// a server, or a storage node that registers with no frontend.
    Epoch(Epoch),
    /// A registered storage node ([`Command::SetWriter`](crate::Command::SetWriter)).
    Node(NodeWriter),
    /// An epoch of another log ([`LegacyWriter`]): an object only a rebuild of the
    /// index ([`Command::AdoptSegment`](crate::Command::AdoptSegment)) names.
    Legacy(LegacyWriter),
}

impl Writer {
    /// Whether an object of `self` may have been named by `start` or by a writer that
    /// came after it in the same place: the same epoch or a later one, or the same
    /// member at the same boot or a later one. A tier refuses to start over a block of
    /// such an object, since no earlier process can have written it.
    #[must_use]
    pub fn at_or_after(self, start: Self) -> bool {
        match (self, start) {
            (Self::Epoch(e), Self::Epoch(s)) => e >= s,
            (Self::Node(n), Self::Node(s)) => n.member == s.member && n.boot >= s.boot,
            // Another log's writers never write again, and no writer starts after one.
            _ => false,
        }
    }
}

impl fmt::Display for Writer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Epoch(e) => write!(f, "{}", e.0),
            Self::Node(n) => write!(f, "member {} boot {}", n.member, n.boot),
            Self::Legacy(l) => write!(f, "log {:016x} epoch {}", l.log, l.epoch.0),
        }
    }
}

/// An object in the object store that holds blob bytes: a segment of one or more
/// records.
///
/// An id is its [`Writer`] and a sequence number the one writer counts. Writers are
/// never handed out twice (epochs by the core, node writers by the state store), so an
/// id names the same bytes for as long as any index entry points at it, across every
/// store of the farm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
    writer: Writer,
    seq: u64,
}

impl ObjectId {
    /// Object `seq` of writer epoch `epoch`.
    #[must_use]
    pub const fn new(epoch: Epoch, seq: u64) -> Self {
        Self::of(Writer::Epoch(epoch), seq)
    }

    /// Object `seq` of `writer`.
    #[must_use]
    pub const fn of(writer: Writer, seq: u64) -> Self {
        Self { writer, seq }
    }

    /// Who named the object.
    #[must_use]
    pub const fn writer(self) -> Writer {
        self.writer
    }

    /// The writer epoch, for an object of an epoch writer.
    #[must_use]
    pub const fn epoch(self) -> Option<Epoch> {
        match self.writer {
            Writer::Epoch(e) => Some(e),
            Writer::Node(_) | Writer::Legacy(_) => None,
        }
    }

    /// The sequence number within the writer.
    #[must_use]
    pub const fn seq(self) -> u64 {
        self.seq
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.writer, self.seq)
    }
}

/// Which object store holds an object. A farm has one store today,
/// [`StoreId::CONFIGURED`]; the id is in every [`Location`] so a second store (a
/// migration, a tier) needs no change to what the index records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreId(u16);

impl StoreId {
    /// The store the server is configured with.
    pub const CONFIGURED: Self = Self(0);

    /// Store number `n`.
    #[must_use]
    pub const fn new(n: u16) -> Self {
        Self(n)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Where a blob's bytes are: a record `offset` bytes into an object of a store.
///
/// Every object is a segment with a footer, a blob too large to pack with others
/// included (it is a segment of one record), so every object can be read back and
/// indexed from its own footer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    /// The store holding the object.
    pub store: StoreId,
    /// The segment object.
    pub object: ObjectId,
    /// The record's offset in the segment.
    pub offset: u64,
}

/// Why an object is marked unreachable.
///
/// Ordered by how sure the mark is: `Corrupt` is never downgraded to `Missing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnreachableReason {
    /// The store did not produce the object or the range: a 404, or an object that
    /// ends before the record starts. It may come back: a misrouted request or a store
    /// that recovers.
    Missing,
    /// The store produced bytes that fail their digest, including an object that ends
    /// inside the record. A later read of the same object cannot be trusted: no read
    /// clears this mark, and a re-upload heals the blobs in it by moving them to a new
    /// object. [`Command::ObjectReachable`](crate::Command::ObjectReachable) clears a
    /// mark of either reason, so whoever issues it vouches for the bytes.
    Corrupt,
}

/// The generation of a loss mark: the log index of the last
/// [`Command::ObjectUnreachable`](crate::Command::ObjectUnreachable) applied to the
/// object, whether or not it raised the reason.
///
/// [`Command::ObjectReachable`](crate::Command::ObjectReachable) names the generation
/// its sender read before it probed the store, and clears the mark only while that is
/// still the mark's generation. So a reachable-again that is delayed or retried past a
/// newer loss mark does nothing, instead of clearing the newer mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

impl Generation {
    /// The generation stamped by the entry at log index `index`.
    #[must_use]
    pub const fn new(index: u64) -> Self {
        Self(index)
    }

    /// The log index that stamped it.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// An object's loss mark: why it is unreachable, and the generation that last stamped
/// the mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LossMark {
    /// The highest reason any applied mark gave.
    pub reason: UnreachableReason,
    /// The log index of the last mark applied.
    pub generation: Generation,
}

/// Who is asking to write the action cache.
///
/// The front sets this from the caller's authenticated identity. Only a daemon, which
/// ran the action itself, may write a result; a client never can, so one client cannot
/// poison the cache for everyone else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A `kbf-daemon` reporting the result of an action it ran.
    Daemon,
    /// A build client (buck2, Bazel) calling `UpdateActionResult`.
    Client,
}

/// How long the core keeps what it holds, in farm time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// A blob reported present or served stays held for at least this long after the
    /// report.
    pub min_ttl: Duration,
    /// A read touches a blob or an action-cache entry (a log commit) only when its last
    /// touch is at least this old. Entries are held this much longer than their TTL, so
    /// a read that skips the touch is still covered for the full TTL.
    pub touch_quantum: Duration,
    /// An action-cache entry without a hit for this long is a miss and is collected.
    pub action_ttl: Duration,
}

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

impl Default for Retention {
    /// The defaults (`docs/design/storage.md#retention-and-touches`): 7 days, touched at
    /// most once a day, actions kept 30 days.
    fn default() -> Self {
        Self {
            min_ttl: DAY.saturating_mul(7),
            touch_quantum: DAY,
            action_ttl: DAY.saturating_mul(30),
        }
    }
}

/// The blobs an action-cache entry needs besides its `ActionResult` blob: every output
/// file, stdout and stderr, every output directory's `Tree`, and every file and
/// directory the tree names.
///
/// The caller derives it from the `ActionResult` and its trees (it holds the protos;
/// this crate does not). A closure that leaves a child out is a closure check that
/// cannot see the child go missing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Closure(BTreeSet<Digest>);

impl Closure {
    /// An empty closure.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `digest` to the closure.
    pub fn insert(&mut self, digest: Digest) {
        self.0.insert(digest);
    }

    /// The digests, in order.
    pub fn iter(&self) -> impl Iterator<Item = &Digest> {
        self.0.iter()
    }

    /// The number of distinct digests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the closure is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Digest> for Closure {
    fn from_iter<I: IntoIterator<Item = Digest>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Extend<Digest> for Closure {
    fn extend<I: IntoIterator<Item = Digest>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

/// An action-cache entry as written: the `ActionResult` blob and its closure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRecord {
    /// The digest of the serialized `ActionResult`, stored as a CAS blob.
    pub result: Digest,
    /// Every other blob the result needs.
    pub closure: Closure,
}

/// The answer for one blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobAnswer {
    /// Held, at a reachable location.
    Present {
        /// Where the bytes are.
        location: Location,
        /// The tier whose block holds the object while it is staged
        /// (docs/design/write-path.md, section 6.1), from the same query: a staged
        /// object is read from its block, and a reader never marks it unreachable
        /// because the store lacks it. `None`: the object is in the store.
        staged_on: Option<TierId>,
    },
    /// Held, but the object holding it is unreachable. The read fails with
    /// UNAVAILABLE; it is never NOT_FOUND, because the farm holds the blob.
    Unavailable,
    /// Not held. Only this answer becomes NOT_FOUND.
    Absent,
}

/// Index entries a reader must touch before it answers, committed as
/// [`Command::Touch`](crate::Command::Touch).
///
/// Only entries whose last touch is at least the touch quantum old are listed, so most
/// reads list nothing and commit nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Touch {
    /// CAS blobs to touch.
    pub blobs: BTreeSet<Digest>,
    /// Action-cache entries to touch, by action digest.
    pub actions: BTreeSet<Digest>,
}

impl Touch {
    /// Whether there is nothing to touch, so nothing needs committing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty() && self.actions.is_empty()
    }
}

/// The answer to `FindMissingBlobs`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindMissing {
    /// Digests to report missing, in request order, duplicates kept. A blob held at an
    /// unreachable object is listed: the client re-uploads it, and the upload moves the
    /// entry to the new, reachable copy.
    pub missing: Vec<Digest>,
    /// Present blobs that must be touched before the answer is sent.
    pub touch: Touch,
    /// The objects the present blobs are in, each once: a server that evicts the
    /// least recently used objects counts the answer as a use of each.
    pub objects: BTreeSet<ObjectId>,
}

/// Why an action-cache lookup is a miss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Miss {
    /// No entry for the action.
    NoEntry,
    /// The entry has had no hit for longer than the action TTL.
    Expired,
    /// A blob the entry needs (the result or a closure member) is not held.
    Absent(Digest),
    /// A blob the entry needs is held at an unreachable object.
    Unreachable(Digest),
}

/// The answer to `GetActionResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionAnswer {
    /// Every blob the entry needs is present: serve `result`, after committing `touch`
    /// if it is not empty.
    Hit {
        /// The `ActionResult` blob.
        result: Digest,
        /// What to touch before serving.
        touch: Touch,
    },
    /// Not served; the action runs again.
    Miss(Miss),
}
