//! The minimal API of an S3-compatible store, as this crate uses it.

use async_trait::async_trait;
use bytes::Bytes;

use crate::Error;

/// A byte range to read: `len` bytes from `offset`.
///
/// A read returns the bytes of `[offset, offset + len)` that exist, so a range running past
/// the end of the object is cut at the end, and a range of length zero gives no bytes. An
/// `offset` beyond the end of the object is an [`Error::Other`]; an `offset` at the end
/// gives no bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteRange {
    offset: u64,
    len: u64,
}

impl ByteRange {
    /// The range `[offset, offset + len)`.
    #[must_use]
    pub fn new(offset: u64, len: u64) -> Self {
        Self { offset, len }
    }

    /// The first byte.
    #[must_use]
    pub fn offset(self) -> u64 {
        self.offset
    }

    /// The number of bytes asked for.
    #[must_use]
    pub fn size(self) -> u64 {
        self.len
    }

    /// The bytes of `body` this range covers.
    ///
    /// # Errors
    /// [`Error::Other`] if `offset` is beyond the end of `body`.
    pub fn slice(self, body: &Bytes) -> Result<Bytes, Error> {
        let size = body.len() as u64;
        if self.offset > size {
            return Err(Error::Other(format!(
                "range starting at {} is past the end of the {size}-byte object",
                self.offset
            )));
        }
        let end = size.min(self.offset.saturating_add(self.len));
        // Both bounds are at most `size`, which came from a `usize`.
        Ok(body.slice(self.offset as usize..end as usize))
    }
}

/// What a HEAD says about an object: its size and a content tag.
///
/// Two objects are taken to hold the same bytes when their `Meta` are equal, so a backend's
/// tag must be a function of the content (a checksum, or an entity tag that the backend
/// computes from the bytes) and the same function on both sides of a comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// The object's size in bytes.
    pub size: u64,
    /// The object's content tag, compared as it is.
    pub tag: String,
}

/// One object in a listing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Listed {
    /// The object's key.
    pub key: String,
    /// The object's size in bytes.
    pub size: u64,
}

/// An S3-compatible object store with the five calls this crate needs, and no more.
///
/// Objects are immutable: a key is written once. The trait is used as `dyn Backend`, so a
/// tiered store holds its two tiers without a type parameter.
///
/// An implementation over a network bounds each call with a timeout of its own: the tiered
/// store waits for the calls it makes, and a call that never returns stops its write-back and
/// the deletes behind it.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Writes an object, only if the key does not exist (`If-None-Match: *`).
    ///
    /// # Errors
    /// [`Error::AlreadyExists`] if the key exists; the stored object is left as it was.
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error>;

    /// Reads the object, or the bytes of `range` of it (see [`ByteRange`]).
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key.
    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error>;

    /// The object's size and content tag.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key.
    async fn head(&self, key: &str) -> Result<Meta, Error>;

    /// Deletes the object. Deleting a key that does not exist succeeds, so a retried delete
    /// is safe.
    async fn delete(&self, key: &str) -> Result<(), Error>;

    /// Every object whose key starts with `prefix`, in ascending key order. For recovery and
    /// garbage collection only: the listing need not be consistent with recent writes.
    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error>;
}
