//! Why a segment was refused, and why a blob could not be added to one.

use kbf_types::Digest;

/// Why a segment, its footer or one of its records was refused.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SegmentError {
    /// Fewer bytes than the trailer or the footer it describes.
    #[error("segment footer truncated: needed {needed} bytes, had {available}")]
    Truncated {
        /// Bytes the trailer or footer needs.
        needed: u64,
        /// Bytes that were supplied.
        available: u64,
    },
    /// The last eight bytes are not the segment magic: not a segment, or cut short.
    #[error("not a segment: wrong magic")]
    BadMagic,
    /// A layout version this build does not read.
    #[error("unsupported segment version {0}")]
    UnsupportedVersion(u32),
    /// A digest function code this build does not read.
    #[error("unsupported digest function code {0}")]
    UnsupportedDigestFunction(u8),
    /// The trailer's reserved bytes are not zero.
    #[error("segment trailer reserved bytes are not zero")]
    ReservedNotZero,
    /// The trailer's record length plus the footer length is not the object's length.
    #[error("segment length is {actual} bytes but its trailer describes {recorded}")]
    LengthMismatch {
        /// The length the trailer implies.
        recorded: u64,
        /// The length of the object.
        actual: u64,
    },
    /// The footer's CRC-32C does not match its bytes.
    #[error("segment footer CRC mismatch: recorded {recorded:#010x}, computed {computed:#010x}")]
    FooterCrc {
        /// The CRC in the trailer.
        recorded: u32,
        /// The CRC of the footer bytes.
        computed: u32,
    },
    /// The index entry at `position` does not sort strictly after the one before it.
    #[error("segment index out of order at entry {position}")]
    IndexOrder {
        /// The entry's position in the index.
        position: usize,
    },
    /// The fan-out count for `byte` disagrees with the index.
    #[error("segment fan-out disagrees with the index at byte {byte:#04x}")]
    FanOut {
        /// The first hash byte whose cumulative count is wrong.
        byte: u8,
    },
    /// A record's range ends beyond the record area.
    #[error("record {digest} lies outside the segment's records")]
    RecordOutOfBounds {
        /// The record's digest.
        digest: Digest,
    },
    /// A record's bytes are not as long as its digest's size.
    #[error("record {digest} has {actual} bytes")]
    RecordLength {
        /// The record's digest.
        digest: Digest,
        /// The length supplied.
        actual: u64,
    },
    /// A record's CRC-32C does not match its index entry: the bytes were damaged.
    #[error("record {digest} fails its CRC-32C")]
    RecordCrc {
        /// The record's digest.
        digest: Digest,
    },
    /// A record's CRC matches but its SHA-256 does not.
    #[error("record {digest} fails its digest check")]
    RecordDigest {
        /// The record's digest.
        digest: Digest,
    },
}

/// Why a blob was not added to a segment.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    /// The blob does not fit in what is left of this segment. Finish it and push the
    /// blob into a new one.
    #[error("segment full: {size}-byte blob does not fit in the remaining space")]
    Full {
        /// The blob's size.
        size: u64,
    },
    /// The blob does not fit even in an empty segment. Chunk it first.
    #[error("{size}-byte blob exceeds the {limit}-byte segment limit")]
    TooLarge {
        /// The blob's size.
        size: u64,
        /// The writer's segment size limit.
        limit: u64,
    },
}
