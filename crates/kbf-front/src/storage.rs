//! [`CacheStorage`]: the cache as a [`kbf_storage::Storage`], the boundary its callers
//! go through. A thin adapter: each call is the cache's own, unchanged; only the error
//! is converted ([`StorageError`] from [`CacheError`], answering the same gRPC code and
//! message).

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::ObjectStore;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;

use crate::cache::{Cache, CacheError, VerifiedBlob};
use crate::meta_log::MetaLog;
use crate::segments::{Connections, SegmentsConfig, SegmentsService};

/// A cache, and the `--segment-upload-*` limits its `Segments` services keep, as a
/// [`Storage`].
#[derive(Debug)]
pub struct CacheStorage<M, O> {
    cache: Arc<Cache<M, O>>,
    config: SegmentsConfig,
}

impl<M, O> CacheStorage<M, O> {
    /// `cache` as a [`Storage`], its `Segments` services keeping `config`.
    #[must_use]
    pub fn new(cache: Arc<Cache<M, O>>, config: SegmentsConfig) -> Self {
        Self { cache, config }
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> Storage for CacheStorage<M, O> {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async move { Ok(self.cache.find_missing(digests).await?) })
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async move { Ok(self.cache.read_blob(digest).await?) })
    }

    /// [`Cache::read_range`]: the blob is read from the object store (or, if it is only
    /// staged, the write tier) in pieces of at most [`kbf_storage::READ_CHUNK_BYTES`], hashed as it
    /// goes, and each piece's part of the range sent; the digest is compared before the
    /// last piece is handed out. The cache keeps one digest per blob, so a range cannot
    /// be checked alone: the bytes before `offset` are read and hashed without being
    /// sent, and so are the bytes after `offset + limit`, which the stream reads before
    /// it ends. A range costs the whole blob's read and hash, whatever its size, and
    /// holds one piece (plus the one the caller holds), never the blob. A resumed read
    /// pays it again.
    ///
    /// A blob found damaged before the first chunk is handed out (any blob of one
    /// piece) is [`StorageError::Unreachable`] in the answer; one found later is the
    /// last item of the stream, the same error. A stream that ends cleanly delivered
    /// only bytes of a blob that hashed to its digest; chunks that came before an error
    /// item were not yet vouched for, and a caller that gets one discards what it has.
    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            if offset > digest.size_bytes {
                return Err(StorageError::OutOfRange {
                    digest: *digest,
                    offset,
                });
            }
            let chunks = self.cache.read_range(digest, offset, limit).await?;
            Ok(chunks
                .map(|chunk| chunk.map_err(StorageError::from))
                .boxed())
        })
    }

    /// Hashed once here, admitted for its size as a ByteStream `Write` is
    /// ([`Cache::admit`]), and stored.
    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        Box::pin(async move {
            let size = bytes.len() as u64;
            let blob = VerifiedBlob::hashed(bytes);
            let digest = blob.digest();
            let admitted = self.cache.admit(size).await;
            self.cache.store_admitted(vec![blob], admitted).await?;
            Ok(digest)
        })
    }

    /// Always `None` until the cache can stream a record onto its write tier: this wires
    /// to the cache's `RecordStream` and `StreamedRecord` (`append`, `finish`, `commit`)
    /// when they land, and a caller stores the blob whole with [`Storage::put`] until
    /// then.
    fn stream(&self, _size: u64) -> Option<Box<dyn PutStream>> {
        None
    }

    /// Its own uploads and connections: no other caller can name its uploads, and
    /// having no connection, an upload it leaves is dropped when idle.
    fn segments(&self) -> Arc<dyn Segments> {
        Arc::new(SegmentsService::new(
            Arc::clone(&self.cache),
            self.config,
            Connections::new(),
        ))
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        Box::pin(async move { Ok(self.cache.action_result(action).await?) })
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        Box::pin(async move { Ok(self.cache.prepare_action_result(result).await?) })
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        Box::pin(async move {
            Ok(self
                .cache
                .commit_action_record(role, action, record)
                .await?)
        })
    }
}

/// The same gRPC code and message as the [`CacheError`] answers: what the store could
/// not do right now (its metadata log, object store, write tier) becomes
/// [`StorageError::Unavailable`], carrying the cache's error as it was. What fails the
/// same way on every retry becomes [`StorageError::Internal`]: a stored object that does
/// not parse ([`CacheError::Segment`]), a blob the cache's own packing refused
/// ([`CacheError::Pack`]; a blob over a segment is already refused as invalid), and an
/// object key it could not build ([`CacheError::Key`]: the prefix is configuration, the
/// rest is hex). The write tier stays unavailable: its failures are the disk's, and the
/// write falls back to the direct path.
impl From<CacheError> for StorageError {
    fn from(e: CacheError) -> Self {
        match e {
            CacheError::NotFound(d) => Self::NotFound(d),
            CacheError::Unreachable(d) => Self::Unreachable(d),
            CacheError::DigestMismatch { claimed, actual } => {
                Self::DigestMismatch { claimed, actual }
            }
            CacheError::Invalid(m) => Self::Invalid(m),
            CacheError::ActionWrite(w) => Self::ActionWrite(w),
            CacheError::Internal(m) => Self::Internal(m),
            e @ (CacheError::Segment(_) | CacheError::Pack(_) | CacheError::Key(_)) => {
                Self::Internal(e.to_string())
            }
            e @ (CacheError::Meta(_) | CacheError::Store(_) | CacheError::Tier(_)) => {
                Self::Unavailable(Arc::new(e))
            }
        }
    }
}
