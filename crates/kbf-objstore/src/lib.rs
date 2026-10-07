//! The `ObjectStore` trait with an in-memory fake and an S3 backend, and the conformance
//! suite every backend must pass.
//!
//! kbf names every key and records every location in its own replicated log, so it never
//! asks a store whether something exists, and needs no consistent listing and no
//! read-after-write. What it needs of a store is in [`ObjectStore`]; what a store may
//! additionally claim (conditional writes, Object Lock) is in [`Capabilities`], and
//! [`conformance`] checks the base contract and every claim.
//!
//! One store value addresses one bucket.

pub mod conformance;
mod memory;
pub mod s3;
mod types;

use std::future::Future;
use std::time::SystemTime;

use bytes::Bytes;

pub use memory::MemoryStore;
pub use types::{
    ByteRange, Capabilities, KeyError, KeyPrefix, ListPage, ListToken, ObjectInfo, ObjectKey,
    ObjectStoreError, PageSize,
};

/// An object store bucket, as kbf uses it.
///
/// The futures are `Send` so a store can be driven from any runtime thread.
pub trait ObjectStore: Send + Sync {
    /// What this store claims beyond the base contract.
    fn capabilities(&self) -> Capabilities;

    /// Writes an immutable object.
    ///
    /// With [`Capabilities::conditional_put`], an existing key is refused with
    /// [`ObjectStoreError::AlreadyExists`] and left unchanged. With `retain_until`, the
    /// object may not be deleted before that time; a store without
    /// [`Capabilities::object_lock`] refuses that with [`ObjectStoreError::Unsupported`].
    fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> impl Future<Output = Result<(), ObjectStoreError>> + Send;

    /// Reads the bytes of `range` that exist (see [`ByteRange`]).
    fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> impl Future<Output = Result<Bytes, ObjectStoreError>> + Send;

    /// Deletes an object. Deleting an absent key succeeds, so a retried delete is safe.
    /// An object under retention is refused and stays readable.
    fn delete(&self, key: &ObjectKey) -> impl Future<Output = Result<(), ObjectStoreError>> + Send;

    /// One page of the keys under `prefix`, in ascending order, starting where `after`
    /// says (or at the beginning). For rebuild and the weekly sweep only: the listing
    /// need not be consistent with recent writes.
    fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> impl Future<Output = Result<ListPage, ObjectStoreError>> + Send;
}
