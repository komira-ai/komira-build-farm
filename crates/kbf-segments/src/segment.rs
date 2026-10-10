//! Packing blobs into a segment, and reading them back by digest.

use std::collections::BTreeMap;

use kbf_types::Digest;

use crate::crc32c::crc32c;
use crate::error::{SegmentError, WriteError};
use crate::layout::{ENTRY_LEN, Footer, IndexEntry, footer_len, write_footer};

/// The largest segment the farm writes (`docs/design/storage.md#writes`).
pub const MAX_SEGMENT_BYTES: u64 = 128 << 20;

/// Packs blobs into one segment of at most `limit` bytes, footer included.
///
/// Each blob is hashed as it is pushed, so a record can never sit under the wrong
/// digest. Pushing a blob the segment already holds is a no-op.
#[derive(Debug)]
pub struct SegmentWriter {
    limit: u64,
    records: Vec<u8>,
    index: BTreeMap<Digest, IndexEntry>,
}

impl SegmentWriter {
    /// A writer for a segment of at most `limit` bytes.
    #[must_use]
    pub const fn new(limit: u64) -> Self {
        Self {
            limit,
            records: Vec::new(),
            index: BTreeMap::new(),
        }
    }

    /// Adds `blob` and returns its digest.
    ///
    /// # Errors
    /// [`WriteError::Full`] if the blob does not fit in the space left (finish this
    /// segment and start another); [`WriteError::TooLarge`] if it would not fit in an
    /// empty segment either. The writer is unchanged by an error.
    pub fn push(&mut self, blob: &[u8]) -> Result<Digest, WriteError> {
        let digest = crate::sha256(blob);
        if self.index.contains_key(&digest) {
            return Ok(digest);
        }
        let size = digest.size_bytes;
        if size.saturating_add(footer_len(1)) > self.limit {
            return Err(WriteError::TooLarge {
                size,
                limit: self.limit,
            });
        }
        if self.len().saturating_add(size + ENTRY_LEN as u64) > self.limit {
            return Err(WriteError::Full { size });
        }
        let entry = IndexEntry {
            digest,
            offset: self.records.len() as u64,
            crc32c: crc32c(blob),
        };
        self.records.extend_from_slice(blob);
        self.index.insert(digest, entry);
        Ok(digest)
    }

    /// The length the segment would have if finished now.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.records.len() as u64 + footer_len(self.index.len() as u64)
    }

    /// Whether no blob has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The finished segment: the records, then the footer.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        let mut out = self.records;
        write_footer(&mut out, self.index.values());
        out
    }
}

/// A whole segment held in memory, with its footer checked.
#[derive(Debug)]
pub struct SegmentReader<'a> {
    bytes: &'a [u8],
    footer: Footer,
}

impl<'a> SegmentReader<'a> {
    /// Checks the footer of `bytes`, a complete segment.
    ///
    /// # Errors
    /// Any refusal of [`Footer::parse`].
    pub fn open(bytes: &'a [u8]) -> Result<Self, SegmentError> {
        let footer = Footer::parse(bytes, bytes.len() as u64)?;
        Ok(Self { bytes, footer })
    }

    /// The blob for `digest`, checked against its CRC-32C and digest, or `None` if the
    /// segment does not hold it.
    ///
    /// # Errors
    /// [`SegmentError::RecordCrc`] or [`SegmentError::RecordDigest`] if the record's
    /// bytes are damaged.
    pub fn get(&self, digest: &Digest) -> Result<Option<&'a [u8]>, SegmentError> {
        let Some(entry) = self.footer.find(digest) else {
            return Ok(None);
        };
        // The footer checked that every range lies inside the records, which lie inside
        // `bytes`, so these casts and the slice are in bounds.
        let range = entry.range();
        let record = &self.bytes[range.start as usize..range.end as usize];
        entry.verify(record)?;
        Ok(Some(record))
    }

    /// The segment's checked footer.
    #[must_use]
    pub const fn footer(&self) -> &Footer {
        &self.footer
    }
}

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;
