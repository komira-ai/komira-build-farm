//! The local write tier's metadata (docs/design/write-path.md, section 6): which tier
//! each server writes to, and the staged objects, whose only copy is a block file on
//! one tier until it is flushed to the object store.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use kbf_types::Digest;

use crate::model::ObjectId;
use crate::state::ForeignWriter;

/// A local write tier: one directory on one server's disk. The server draws the id the
/// first time it opens the directory, writes it to the directory's `TIER` file and
/// registers it with [`Command::AddTier`](crate::Command::AddTier).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TierId(u64);

impl TierId {
    /// Tier `n`.
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

impl fmt::Display for TierId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tier {}", self.0)
    }
}

/// A server of the farm, as the tier registry names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerId(u64);

impl ServerId {
    /// Server `n`.
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

impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "server {}", self.0)
    }
}

/// One object of a [`Command::PutStaged`](crate::Command::PutStaged): records synced to
/// the object's block file below `end`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedPut {
    /// The object, named as the block file is.
    pub object: ObjectId,
    /// The end of the bytes this commit covers. The object's committed length becomes
    /// the larger of its length and `end`, so commits applied out of order never
    /// shorten it.
    pub end: u64,
    /// Seals the object in the same commit (a segment upload's block, written whole).
    pub seal: bool,
    /// The records: each blob and its offset in the block. Every record ends at or
    /// before `end`.
    pub blobs: Vec<(Digest, u64)>,
}

/// Where a staged object is in its life. It only moves forward: `Open`, then
/// `Sealed`, then `Conflict` or out of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StagedState {
    /// Still taking [`Command::PutStaged`](crate::Command::PutStaged)s.
    Open,
    /// Sealed: its blob list is final and the flusher may upload it.
    Sealed,
    /// The store holds a different object under its key
    /// ([`Command::FlushConflict`](crate::Command::FlushConflict)). It stays staged,
    /// read from its block, and the flusher stops retrying it until an operator acts.
    Conflict,
}

/// A staged object: its tier, committed length, state and blob list, and the objects
/// its flush waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedObject {
    pub(crate) tier: TierId,
    pub(crate) len: u64,
    pub(crate) state: StagedState,
    pub(crate) blobs: BTreeMap<Digest, u64>,
    /// Objects on the same tier holding a closure blob of an action-cache entry whose
    /// result blob this object holds, as they were when the entry was written
    /// ([`Command::PutAction`](crate::Command::PutAction)). Kept while they are staged;
    /// [`MetaState::flush_waits`](crate::MetaState::flush_waits) answers which still are.
    pub(crate) after: BTreeSet<ObjectId>,
}

impl StagedObject {
    /// The tier holding the block.
    #[must_use]
    pub const fn tier(&self) -> TierId {
        self.tier
    }

    /// The committed length: bytes past it were never acknowledged.
    #[must_use]
    pub const fn committed_len(&self) -> u64 {
        self.len
    }

    /// The state.
    #[must_use]
    pub const fn state(&self) -> StagedState {
        self.state
    }

    /// Every record committed into the object, sorted by digest: the order of the
    /// footer the flusher builds. A digest committed twice is listed once, at the offset
    /// first committed, which is where the index points.
    pub fn blobs(&self) -> impl Iterator<Item = (&Digest, u64)> {
        self.blobs.iter().map(|(d, o)| (d, *o))
    }
}

/// Why a tier command was refused. A refused command changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TierError {
    /// An object is not of a writer the log takes for the command.
    #[error(transparent)]
    Foreign(#[from] ForeignWriter),
    /// The tier is not the current tier of any server.
    #[error("{0} is not registered to any server")]
    UnknownTier(TierId),
    /// The tier is registered to another server.
    #[error("{tier} is registered to {server}")]
    TierTaken {
        /// The tier.
        tier: TierId,
        /// The server it belongs to.
        server: ServerId,
    },
    /// The tier was replaced earlier; an id is never registered again.
    #[error("{0} was replaced and cannot be registered again")]
    TierRetired(TierId),
    /// The server's current tier still holds staged objects. Each must be flushed or
    /// marked unreachable before the server registers another tier.
    #[error("{server}'s {tier} still holds staged objects")]
    StillStaged {
        /// The server.
        server: ServerId,
        /// Its current tier.
        tier: TierId,
    },
    /// The object is staged on another tier.
    #[error("object {object} is staged on {tier}")]
    OtherTier {
        /// The object.
        object: ObjectId,
        /// The tier holding it.
        tier: TierId,
    },
    /// The object is sealed and the command would add to it.
    #[error("object {0} is sealed")]
    Sealed(ObjectId),
    /// The object left the staged table (flushed, or marked unreachable); it is never
    /// staged again.
    #[error("object {0} is no longer staged and cannot be staged again")]
    Left(ObjectId),
    /// The object is marked unreachable.
    #[error("object {0} is marked unreachable")]
    Unreachable(ObjectId),
    /// One command names the object twice.
    #[error("object {0} is named twice in one command")]
    Repeated(ObjectId),
    /// A record ends past the commit's `end`, or past `u64::MAX`.
    #[error("record {digest} ends past the end of object {object}'s commit")]
    PastEnd {
        /// The object.
        object: ObjectId,
        /// The record.
        digest: Digest,
    },
    /// The object is not staged and never left the table (within the retention
    /// horizon).
    #[error("object {0} is not staged")]
    NotStaged(ObjectId),
    /// The object is still open; only a sealed object is flushed.
    #[error("object {0} is not sealed")]
    NotSealed(ObjectId),
}
