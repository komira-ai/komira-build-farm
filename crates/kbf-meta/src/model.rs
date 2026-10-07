//! The values the metadata core stores, takes and answers with.

use std::collections::BTreeSet;
use std::time::Duration;

use kbf_types::Digest;

/// An object in the object store that holds blob bytes: a packed segment, or one large
/// blob stored whole.
///
/// The store assigns ids and never reuses one, so an id names the same bytes for as
/// long as any index entry points at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(u64);

impl ObjectId {
    /// The object with id `id`.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Where a blob's bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Location {
    /// A record inside a packed segment, starting `offset` bytes into it.
    Segment {
        /// The segment object.
        segment: ObjectId,
        /// The record's offset in the segment.
        offset: u64,
    },
    /// A large blob stored whole as its own object.
    Object(ObjectId),
}

impl Location {
    /// The object that holds the bytes, which is what becomes reachable or unreachable.
    #[must_use]
    pub const fn object(self) -> ObjectId {
        match self {
            Self::Segment { segment, .. } => segment,
            Self::Object(object) => object,
        }
    }
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
    /// The RFC constants: 7 days, touched at most once a day, actions kept 30 days.
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
    Present(Location),
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
