//! The server's blob store as calls in this process: [`LocalStore`], what a worker
//! daemon linked into the server's own process (`kbf-all-in-one`) reads and writes blobs
//! through, with no listener, TLS or protobuf encoding between them.
//!
//! Each call is one the worker listener serves a remote daemon, on the same [`Cache`]:
//! FindMissing ([`LocalStore::find_missing`]), a whole-blob read of a blob the daemon
//! decodes ([`LocalStore::read_blob`]) and a read of any blob in windows
//! ([`LocalStore::read_stream`], ByteStream Read's path), a whole-blob write hashed here
//! once, under the same
//! admission as a ByteStream `Write` ([`LocalStore::put`]), and the `Segments` service
//! itself ([`LocalStore::segments`]), whose `Begin`, `PutRange` and `Commit` are the
//! handlers a remote upload runs: with a write tier the daemon's digests are trusted at
//! the commit and the flusher hashes every record before its PUT, dropping one that
//! does not match; without one each record is hashed as its ranges arrive. Nothing here
//! skips a check the listener's path makes.
//!
//! Two calls have no remote counterpart. A record streamed as its bytes are read
//! ([`LocalStore::stream`]), so a large output is never held in memory on its way into
//! the cache: onto the write tier when it has room (its digest trusted at the commit as
//! a segment upload's is with a tier, and checked by the flusher), else straight to the
//! object store (hashed as it passes and checked against the digest claimed at the
//! commit before the store completes it). And a blob of known digest streamed to the
//! object store ([`LocalStore::put_streamed`]), as a ByteStream `Write` of a large blob
//! with no room on a tier is.
//!
//! Every call goes through the storage boundary, an `Arc<dyn kbf_storage::Storage>`
//! ([`LocalStore::over`]); [`LocalStore::new`] puts the cache behind it with
//! `kbf-front`'s adapter ([`CacheStorage`]), which calls the cache's own code unchanged.
//!
//! Not here yet: admission. The worker listener admits every remote call by the
//! daemon's certificate and the deny list; a local call is admitted by nothing.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kbf_front::{Cache, CacheStorage, MetaLog, SegmentsConfig};
use kbf_objstore::ObjectStore;
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;

/// The server's cache, as calls a daemon in the same process makes, through the
/// storage boundary ([`kbf_storage::Storage`]). Clones share the store.
#[derive(Clone)]
pub struct LocalStore {
    inner: Arc<dyn Storage>,
}

impl LocalStore {
    /// The calls over `cache` ([`CacheStorage`]), its `Segments` services keeping
    /// `config`.
    pub fn new<M: MetaLog, O: ObjectStore + 'static>(
        cache: Arc<Cache<M, O>>,
        config: SegmentsConfig,
    ) -> Self {
        Self::over(Arc::new(CacheStorage::new(cache, config)))
    }

    /// The calls over any `storage`.
    #[must_use]
    pub fn over(storage: Arc<dyn Storage>) -> Self {
        Self { inner: storage }
    }

    /// The digests among `digests` the cache does not hold, in the order asked, as
    /// FindMissingBlobs answers; the held ones are touched.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, StorageError> {
        self.inner.find_missing(digests).await
    }

    /// The blob's bytes, checked against its digest by the cache, read whole: for a
    /// blob the daemon decodes (at most `kbf_front::MAX_DECODED_BLOB_BYTES`).
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if the blob is not held, or why it cannot be read;
    /// [`StorageError::Invalid`] for a blob too large to read whole.
    pub async fn read_blob(&self, digest: &Digest) -> Result<Vec<u8>, StorageError> {
        self.inner.read_blob(digest).await.map(Vec::from)
    }

    /// The blob's bytes as a stream of chunks, a window of the blob in memory at a time
    /// whatever its size, checked as [`Storage::read_range`] says: a stream that ends
    /// cleanly delivered only bytes of a blob that hashed to its digest.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if the blob is not held, or why it cannot be read.
    pub async fn read_stream(&self, digest: &Digest) -> Result<ReadStream, StorageError> {
        self.inner.read_range(digest, 0, 0).await
    }

    /// Hashes `bytes` once, stores them under that digest within
    /// `--write-bytes-in-flight` (waiting for room, as a ByteStream `Write` does), and
    /// returns the digest once they are durable.
    ///
    /// # Errors
    /// The blob is over the largest the cache accepts, or the store or the metadata
    /// log failed.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<Digest, StorageError> {
        self.inner.put(bytes.into()).await
    }

    /// A stream for one blob of `size` bytes, its bytes written as they are appended
    /// and never held whole: onto the write tier, in a block of its own, when it has
    /// room for it; else straight to the object store ([`Storage::stream`]). `None`
    /// only for a size of 0, or a cache made outside a runtime. Blocking: call it off
    /// the async runtime.
    #[must_use]
    pub fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    /// Stores a blob of `size` bytes straight to the object store as `bytes` come (a
    /// multipart upload on S3), holding a piece of it at a time, whatever its size: how
    /// an output over `kbf_front::MAX_UNSTAGED_BLOB_BYTES` is stored when the write
    /// tier is missing or has no room for it. `claim` names the blob's digest once its
    /// bytes ended (the bytes must hash to it, or nothing is stored), or `None` to
    /// withdraw it: the upload is aborted and this answers `None`. Answers the digest
    /// once the blob is present.
    ///
    /// # Errors
    /// [`StorageError::DigestMismatch`] if the bytes do not hash to the claimed digest;
    /// a piece of `bytes` failed; the store or the metadata log failed.
    pub async fn put_streamed(
        &self,
        size: u64,
        bytes: BoxStream<'static, std::io::Result<Bytes>>,
        claim: BoxFuture<'static, Option<Digest>>,
    ) -> Result<Option<Digest>, StorageError> {
        self.inner.put_streamed(size, bytes, claim).await
    }

    /// A `Segments` service of its own over the cache: the one a remote upload's calls
    /// are served by, with uploads no other caller can name. One per caller; an upload
    /// is held until it commits, or until it has had no call for
    /// `--segment-upload-idle-secs`.
    #[must_use]
    pub fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }
}
