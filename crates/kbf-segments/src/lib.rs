//! The segment format that packs blobs into stored objects, with integrity checks,
//! and FastCDC content-defined chunking.
//!
//! - [`SegmentWriter`] packs blobs into one segment up to a size limit.
//! - [`SegmentReader`] finds a blob in a whole segment by digest and checks it;
//!   [`Footer`] does the same for a reader that fetches byte ranges.
//! - [`chunks`] and [`Manifest`] split a large blob into content-defined chunks.
//!
//! The layout is documented in [`layout`]. This is a pure crate: no async runtime,
//! network, clock, randomness or hashed collections. The layering test in `kbf-it` and
//! the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod crc32c;
mod error;
mod fastcdc;
pub mod layout;
mod segment;

use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

pub use crate::crc32c::crc32c;
pub use crate::error::{SegmentError, WriteError};
pub use crate::fastcdc::{
    AVG_CHUNK, CHUNKING_THRESHOLD, Chunks, MAX_CHUNK, MIN_CHUNK, Manifest, chunks,
};
pub use crate::layout::{Footer, IndexEntry};
pub use crate::segment::{MAX_SEGMENT_BYTES, SegmentReader, SegmentWriter};

/// The SHA-256 digest of `bytes`.
#[must_use]
pub fn sha256(bytes: &[u8]) -> Digest {
    Digest::new(
        DigestFunction::Sha256,
        Sha256::digest(bytes).into(),
        bytes.len() as u64,
    )
}
