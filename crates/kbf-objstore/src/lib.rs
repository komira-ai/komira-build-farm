//! The `ObjectStore` trait with an in-memory fake, a local-directory store and an S3
//! backend, the conformance suite every backend must pass, every object under a prefix
//! as a stream that holds one page at a time ([`listing`]), and the reader that gets
//! every segment's index back from the segments' footers ([`footers`]).
//!
//! kbf names every key and records every location in its own replicated log, so it never
//! asks a store whether something exists, and needs no consistent listing and no
//! read-after-write. What it needs of a store is in [`ObjectStore`]; what a store may
//! additionally claim (conditional writes, Object Lock) is in [`Capabilities`], and
//! [`conformance`] checks the base contract and every claim.
//!
//! One store value addresses one bucket.

mod body;
pub mod conformance;
pub mod footers;
mod fs_store;
pub mod listing;
mod memory;
pub mod s3;
mod types;

use std::future::Future;
use std::time::SystemTime;

use bytes::Bytes;

pub use body::{BodyStream, gather};
pub use fs_store::{
    Floor, FsStore, FsStoreError, Room, Scanned, ShardScan, SpaceSource, objects_source,
    statvfs_source,
};
pub use kbf_threads::disk::Space;
pub use memory::MemoryStore;
pub use types::{
    ByteRange, Capabilities, KeyError, KeyPrefix, ListPage, ListToken, ObjectInfo, ObjectKey,
    ObjectStoreError, PageSize,
};

/// An object store bucket, as kbf uses it.
///
/// The futures are `Send` so a store can be driven from any runtime thread, and a store
/// is `'static` so a task spawned to write a segment can own a handle on it.
pub trait ObjectStore: Send + Sync + 'static {
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

    /// Writes an immutable object of exactly `len` bytes, taken from `body` as it is
    /// sent, so the writer never holds the object whole.
    ///
    /// The object is written only if `body` gives `len` bytes and then ends. A body that
    /// yields an error, or more or fewer bytes, leaves nothing under `key` and fails the
    /// write (with the body's error, or [`ObjectStoreError::Body`]). With
    /// [`Capabilities::conditional_put`], an existing key is refused with
    /// [`ObjectStoreError::AlreadyExists`] and left unchanged, as by
    /// [`ObjectStore::put_new`]. No retention.
    ///
    /// The default gathers the body whole and calls [`ObjectStore::put_new`]: for the
    /// in-memory fake and the test stores that wrap another. The S3 store sends a large
    /// object as a multipart upload ([`s3::MULTIPART_THRESHOLD`]), a part at a time, and
    /// aborts it on any failure; the local-directory store writes the pieces to its
    /// file as they come.
    fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> impl Future<Output = Result<(), ObjectStoreError>> + Send {
        async move {
            let bytes = gather(body, len).await?;
            self.put_new(key, bytes, None).await
        }
    }

    /// Reads the bytes of `range` that exist (see [`ByteRange`]).
    fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> impl Future<Output = Result<Bytes, ObjectStoreError>> + Send;

    /// Whether the store may hold `key`: `false` only when the store knows, without a
    /// request over the network, that it does not (a local store's file is gone, or is
    /// not a whole object). A store that would need a request answers `true`, the
    /// default, and a read finds out. kbf asks before it serves an action-cache hit,
    /// once per object the hit needs.
    fn may_hold(
        &self,
        key: &ObjectKey,
    ) -> impl Future<Output = Result<bool, ObjectStoreError>> + Send {
        let _ = key;
        std::future::ready(Ok(true))
    }

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
