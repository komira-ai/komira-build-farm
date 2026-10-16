//! The server's blob store as calls in this process: [`LocalStore`], what a worker
//! daemon linked into the server's own process (`kbf-all-in-one`) reads and writes blobs
//! through, with no listener, TLS or protobuf encoding between them.
//!
//! Each call is one the worker listener serves a remote daemon, on the same [`Cache`]:
//! FindMissing ([`LocalStore::find_missing`]), a whole-blob read of a blob the daemon
//! decodes ([`LocalStore::read_blob`]) and a streamed one of any blob
//! ([`LocalStore::read_stream`]), a whole-blob write hashed here once, under the same
//! admission as a ByteStream `Write` ([`LocalStore::put`]), and the `Segments` service
//! itself ([`LocalStore::segments`]), whose `Begin`, `PutRange` and `Commit` are the
//! handlers a remote upload runs: with a write tier the daemon's digests are trusted at
//! the commit and the flusher hashes every record before its PUT, dropping one that
//! does not match; without one each record is hashed as its ranges arrive. Nothing here
//! skips a check the listener's path makes.
//!
//! Two calls have no remote counterpart. A record streamed onto the write tier as its
//! bytes are read ([`LocalStore::stream`], over `kbf_front::RecordStream`), so a large
//! output is never held in memory on its way into the cache: its digest is trusted at
//! the commit as a segment upload's is with a tier, and checked by the flusher. And,
//! without a tier or when it has no room, a record streamed straight to the object
//! store ([`LocalStore::put_streamed`]), hashed as it passes and checked against the
//! digest named at its end before the store completes it, as a ByteStream `Write` of a
//! large blob is.
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
use kbf_storage::{PutStream, ReadStream, Storage, StorageError, StreamedPut};
use kbf_types::Digest;

/// The server's cache, as calls a daemon in the same process makes, through the
/// storage boundary ([`kbf_storage::Storage`]). Clones share the store.
#[derive(Clone)]
pub struct LocalStore {
    inner: Arc<dyn Storage>,
}

/// One record being streamed onto the server's write tier ([`LocalStore::stream`]).
/// Dropped before it is committed, nothing is stored.
pub struct LocalStream(Box<dyn PutStream>);

impl LocalStream {
    /// Writes `chunk` after the bytes written so far. Blocking: call it off the async
    /// runtime.
    ///
    /// # Errors
    /// More bytes than the stream was made for, or the write tier failed.
    pub fn append(&mut self, chunk: &[u8]) -> Result<(), StorageError> {
        self.0.append(chunk)
    }

    /// Ends the stream as the blob `digest` with CRC-32C `crc32c`.
    ///
    /// # Errors
    /// The bytes written are not as many as `digest` names, or their CRC-32C is not
    /// `crc32c`: nothing is stored.
    pub fn finish(self, digest: Digest, crc32c: u32) -> Result<LocalStreamed, StorageError> {
        self.0.finish(digest, crc32c).map(LocalStreamed)
    }
}

/// A record whose bytes are on the write tier, checked, not yet committed.
pub struct LocalStreamed(Box<dyn StreamedPut>);

impl LocalStreamed {
    /// Syncs the record and commits it: present once this answers.
    ///
    /// # Errors
    /// The write tier's sync or the metadata log's commit failed.
    pub async fn commit(self) -> Result<(), StorageError> {
        self.0.commit().await
    }
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

    /// The blob's bytes as a stream of chunks, checked as [`Storage::read_range`]
    /// says: a stream that ends cleanly delivered only bytes of a blob that hashed to
    /// its digest.
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

    /// A stream for one blob of `size` bytes onto the write tier, in a block of its own,
    /// its bytes written as they are appended and never held here; `None` without a
    /// write tier, when the tier has no room for it, or for a size of 0. Blocking: call
    /// it off the async runtime.
    #[must_use]
    pub fn stream(&self, size: u64) -> Option<LocalStream> {
        self.inner.stream(size).map(LocalStream)
    }

    /// Stores a blob of `size` bytes straight to the object store as `bytes` come (a
    /// multipart upload on S3), holding a piece of it at a time, whatever its size: how
    /// an output over `kbf_front::MAX_UNSTAGED_BLOB_BYTES` is stored when the write
    /// tier is missing or has no room for it. `claim` names the blob's digest once its
    /// bytes ended (the bytes must hash to it, or nothing is stored), or `None` to
    /// withdraw it: the upload is aborted and this answers `None`. Answers the digest
    /// once the blob is present. Admitted as a ByteStream `Write` streamed to the store
    /// is.
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
