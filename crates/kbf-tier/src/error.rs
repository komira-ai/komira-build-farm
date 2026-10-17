//! What the tier refuses, and why.

use std::io;

use kbf_meta::{ObjectId, TierId};

/// Why a tier call failed.
#[derive(Debug, thiserror::Error)]
pub enum TierError {
    /// The budget has no room for the range: write it by the direct path.
    #[error("write tier full: {want} bytes wanted, {used} of {budget} used")]
    Full {
        /// The bytes asked for.
        want: u64,
        /// The bytes already counted.
        used: u64,
        /// The hot store's budget.
        budget: u64,
    },
    /// A grouped range larger than a segment.
    #[error("a grouped range of {len} bytes passes the segment limit of {limit}")]
    TooLarge {
        /// The bytes asked for.
        len: u64,
        /// The segment limit.
        limit: u64,
    },
    /// A write outside its reservation.
    #[error("{len} bytes at {offset} pass a reservation of {reserved}")]
    OutOfRange {
        /// Where the write starts within the range.
        offset: u64,
        /// Its length.
        len: u64,
        /// The range's length.
        reserved: u64,
    },
    /// A read past a block's committed length: bytes never acknowledged.
    #[error("object {object}: bytes to {end} asked, {committed} committed")]
    NotCommitted {
        /// The block.
        object: ObjectId,
        /// The end of the read.
        end: u64,
        /// The committed length.
        committed: u64,
    },
    /// No such block in this tier, or no longer written.
    #[error("object {0} has no block being written in this tier")]
    UnknownBlock(ObjectId),
    /// An earlier write or sync of the block failed: nothing more of it may be
    /// acknowledged.
    #[error("object {0}: an earlier write or sync of its block failed")]
    Poisoned(ObjectId),
    /// A seal or unstage of a block not in the phase for it.
    #[error("object {0} is not ready for that step")]
    NotSealable(ObjectId),
    /// The directory's `TIER` file is missing or names another tier while objects
    /// are staged on this one: a wrong mount or flag. The start is refused.
    #[error(
        "write tier directory names {}, expected {expected}, and objects are staged on {expected}",
        found.map_or_else(|| "no tier".to_owned(), |t| t.to_string())
    )]
    WrongTier {
        /// The tier the server's objects are staged on.
        expected: TierId,
        /// What `TIER` names (`None`: missing or unreadable).
        found: Option<TierId>,
    },
    /// The directory belongs to another tier that still has objects staged.
    #[error("write tier directory belongs to {found}, which has objects staged")]
    OtherTier {
        /// What `TIER` names.
        found: TierId,
    },
    /// A block file of this start's writer or a later one exists at open (the same
    /// epoch or a later one, or the same member at this boot or a later one): the
    /// writer was reused.
    #[error("block file {name} is from this writer or a later one")]
    FutureBlock {
        /// The file.
        name: String,
    },
    /// The filesystem failed.
    #[error("{op} {name}: {source}")]
    Io {
        /// What was being done.
        op: &'static str,
        /// The file.
        name: String,
        /// The error.
        source: io::Error,
    },
}

impl TierError {
    pub(crate) fn io(op: &'static str, name: &str, source: io::Error) -> Self {
        Self::Io {
            op,
            name: name.to_owned(),
            source,
        }
    }
}
