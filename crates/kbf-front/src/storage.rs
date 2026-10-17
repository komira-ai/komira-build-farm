//! [`CacheStorage`]: the cache as a [`kbf_storage::Storage`], the boundary its callers
//! go through. A thin adapter: each call is the cache's own, unchanged; only the error
//! is converted ([`StorageError`] from [`CacheError`], answering the same gRPC code and
//! message).

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::{ObjectStore, ObjectStoreError};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_segments::RecordHasher;
use kbf_storage::service::OpenWrites;
use kbf_storage::{
    Answer, PutStream, READ_CHUNK_BYTES, ReadStream, Storage, StorageError, StreamedPut,
};
use kbf_types::Digest;

use crate::cache::{BlobRead, Cache, CacheError, RecordStream, StreamedRecord, VerifiedBlob};
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

    /// The cache's windowed read ([`Cache::read_windows`]): the blob is read and hashed
    /// a window at a time (at most [`crate::READ_WINDOW_BYTES`] in memory, whatever the
    /// blob's size), the last window handed over only once the whole blob checked against
    /// its digest, so a stream that ends cleanly delivered a verified blob and a damaged
    /// one ends in an error (the first window is read before the answer, so a blob
    /// that fails there is the call's error). The windows are cut to chunks of at most
    /// [`READ_CHUNK_BYTES`], zero-copy.
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
            let end = if limit == 0 {
                digest.size_bytes
            } else {
                digest.size_bytes.min(offset.saturating_add(limit))
            };
            let read = self.cache.read_windows(digest, offset, end).await?;
            let mut chunks = chunks(read);
            // The first window is read before the answer, so a blob that is gone or
            // damaged (found by its first window, or by the whole of a one-window blob) is
            // the call's error, where its gRPC code is still open.
            match chunks.next().await {
                Some(Err(e)) => Err(e),
                Some(Ok(first)) => Ok(futures::stream::once(async move { Ok(first) })
                    .chain(chunks)
                    .boxed()),
                None => Ok(futures::stream::empty().boxed()),
            }
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

    /// The cache's own stream ([`Cache::stream`]): onto the write tier when it has room,
    /// else to the object store as the bytes come. `None` only for a size of 0 or a cache
    /// made outside a runtime. Each append is forwarded as it comes and hashed here, so
    /// `finish` refuses bytes that are not the digest's (see `Writing`).
    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        let stream = self.cache.stream(size)?;
        Some(Box::new(Writing {
            stream,
            hasher: RecordHasher::new(),
        }))
    }

    /// The cache's streamed store ([`Cache::store_streamed_claimed`]), admitted as a
    /// ByteStream `Write` that streams to the store is.
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

    /// As [`Storage::segments`], each upload counted in `open` while it is open.
    fn segments_counted(&self, open: &OpenWrites) -> Arc<dyn Segments> {
        Arc::new(
            SegmentsService::new(Arc::clone(&self.cache), self.config, Connections::new())
                .with_open_writes(open.clone()),
        )
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

/// A record being written ([`Storage::stream`]): the cache's [`RecordStream`], and the
/// SHA-256 of the bytes appended to it.
///
/// [`RecordStream::finish`] checks the size and the CRC-32C, not the hash: onto the
/// tier the cache trusts the digest at the commit (the flusher hashes the record later),
/// which is right for the in-process plane, whose digests are its own hashes, and wrong
/// for a caller of this trait, whose bytes come from another process. So the hash is
/// checked here, as the cache's own `stage_streamed` checks a ByteStream `Write` before
/// its commit: a blob is never present under a digest its bytes do not have. The cost
/// is one SHA-256 pass over the bytes, on the appending thread; to the store the cache
/// hashes them again before it completes the upload.
struct Writing<M, O> {
    stream: RecordStream<M, O>,
    hasher: RecordHasher,
}

impl<M: MetaLog, O: ObjectStore + 'static> PutStream for Writing<M, O> {
    /// Forwards `chunk` to the cache's stream, then hashes it: a chunk the stream
    /// refused is not counted.
    fn append(&mut self, chunk: &[u8]) -> Result<(), StorageError> {
        self.stream.append(chunk)?;
        self.hasher.update(chunk);
        Ok(())
    }

    /// The cache's checks first (the size `digest` names, `crc32c`: `Invalid`), then the
    /// hash (`DigestMismatch`). Refused either way, the stream is dropped uncommitted and
    /// nothing is stored.
    fn finish(
        self: Box<Self>,
        digest: Digest,
        crc32c: u32,
    ) -> Result<Box<dyn StreamedPut>, StorageError> {
        let Self { stream, hasher } = *self;
        let streamed = stream.finish(digest, crc32c)?;
        let (actual, _) = hasher.finish();
        if actual != digest {
            return Err(StorageError::DigestMismatch {
                claimed: digest,
                actual,
            });
        }
        Ok(Box::new(Written(streamed)))
    }
}

/// A record whose bytes are all written and checked ([`StreamedRecord`]).
struct Written<M, O>(StreamedRecord<M, O>);

impl<M: MetaLog, O: ObjectStore + 'static> StreamedPut for Written<M, O> {
    fn commit(self: Box<Self>) -> Answer<'static, ()> {
        Box::pin(async move { Ok(self.0.commit().await?) })
    }
}

/// The windows `read` hands over, cut to chunks of at most [`READ_CHUNK_BYTES`] as
/// zero-copy slices. A window that fails is the stream's last item.
fn chunks<M: MetaLog, O: ObjectStore + 'static>(read: BlobRead<M, O>) -> ReadStream {
    futures::stream::unfold((read, Bytes::new()), |(mut read, mut held)| async move {
        if held.is_empty() {
            match read.next().await? {
                Ok(window) => held = window,
                Err(e) => return Some((Err(e.into()), (read, Bytes::new()))),
            }
        }
        let chunk = held.split_to(held.len().min(READ_CHUNK_BYTES));
        Some((Ok(chunk), (read, held)))
    })
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
