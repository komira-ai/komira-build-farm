//! [`CacheStorage`]: the cache as a [`Storage`], the boundary its callers
//! go through. A thin adapter: each call is the cache's own, unchanged; only the error
//! is converted ([`StorageError`] from [`CacheError`], answering the same gRPC code and
//! message).

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::{ObjectStore, ObjectStoreError};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_types::Digest;

use crate::cache::{Cache, CacheError, RecordStream, StreamedRecord, VerifiedBlob};
use crate::meta_log::MetaLog;
use crate::segments::{Connections, SegmentsConfig, SegmentsService};
use crate::{Answer, PutStream, READ_CHUNK_BYTES, ReadStream, Storage, StorageError, StreamedPut};

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

    /// The cache checks a blob against its digest only after its last window
    /// ([`Cache::read_windows`]), so this reads every window, gathers the blob, and cuts
    /// the answer: the blob is read and checked against its digest as a whole, for any
    /// `offset` and `limit`, before the first chunk is sent.
    ///
    /// Memory: the one blob is held once, until the stream is dropped or read to the
    /// end; each chunk is a zero-copy slice of it, at most [`READ_CHUNK_BYTES`], made
    /// when the stream is polled. A caller does not hold the blob, but this store does:
    /// a read in pieces from the object store, checked before it is sent, needs the
    /// cache to check a record in pieces, which it cannot.
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
            let mut read = self
                .cache
                .read_windows(digest, 0, digest.size_bytes)
                .await?;
            let mut blob = BytesMut::new();
            while let Some(window) = read.next().await {
                blob.extend_from_slice(&window?);
            }
            Ok(chunks(blob.freeze(), offset, limit))
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

    /// The cache's own stream onto its write tier ([`Cache::stream`]).
    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        let stream = self.cache.stream(size)?;
        Some(Box::new(stream))
    }

    /// The cache's own streamed write to the object store
    /// ([`Cache::store_streamed_claimed`]), each failed piece of `bytes` a body error.
    fn put_streamed(
        &self,
        size: u64,
        bytes: BoxStream<'static, std::io::Result<Bytes>>,
        claim: BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        let body = bytes
            .map(|piece| piece.map_err(|e| ObjectStoreError::Body(e.to_string())))
            .boxed();
        Box::pin(async move { Ok(self.cache.store_streamed_claimed(size, body, claim).await?) })
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

impl<M: MetaLog, O: ObjectStore + 'static> PutStream for RecordStream<M, O> {
    fn append(&mut self, chunk: &[u8]) -> Result<(), StorageError> {
        Ok(RecordStream::append(self, chunk)?)
    }

    fn finish(
        self: Box<Self>,
        digest: Digest,
        crc32c: u32,
    ) -> Result<Box<dyn StreamedPut>, StorageError> {
        let streamed = RecordStream::finish(*self, digest, crc32c)?;
        Ok(Box::new(streamed))
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> StreamedPut for StreamedRecord<M, O> {
    fn commit(self: Box<Self>) -> Answer<'static, ()> {
        Box::pin(async move { Ok(StreamedRecord::commit(*self).await?) })
    }
}

/// `blob[offset..]`, cut to `limit` bytes unless that is 0, as a stream of zero-copy
/// slices of at most [`READ_CHUNK_BYTES`]. `blob` is the whole blob, already checked.
fn chunks(blob: Bytes, offset: u64, limit: u64) -> ReadStream {
    let len = blob.len();
    let start = usize::try_from(offset).map_or(len, |o| o.min(len));
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let end = if limit == 0 {
        len
    } else {
        start.saturating_add(limit).min(len)
    };
    futures::stream::iter(
        (start..end)
            .step_by(READ_CHUNK_BYTES)
            .map(move |at| Ok(blob.slice(at..end.min(at + READ_CHUNK_BYTES)))),
    )
    .boxed()
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
