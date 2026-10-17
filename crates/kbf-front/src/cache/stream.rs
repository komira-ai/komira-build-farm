//! A blob read in pieces, so no process holds the whole of it ([`Cache::read_range`]).
//!
//! The cache keeps one digest per blob and nothing finer, so a byte can be vouched for
//! only by hashing the blob from its first byte to its last. This read does that as it
//! goes: it reads the blob from where the index holds it in pieces of at most
//! [`READ_CHUNK_BYTES`], hashes each, and sends the part of each that the caller asked
//! for. Memory is one piece, plus the piece the caller still holds. The price of a
//! range is the rest of the blob, read and hashed but not sent: the bytes before the
//! offset, and the bytes after the limit.
//!
//! **What the caller can rely on.** The digest is compared the moment the last piece is
//! read, before that piece is handed out. A stream that ends cleanly therefore
//! delivered only bytes of a blob that hashed to its digest, from its first byte to its
//! last. A blob that does not ends in an error: in the answer, if it was found before
//! the first byte was handed out (a blob of one piece always is), else as the stream's
//! last item. Chunks sent before that error were not yet vouched for, so a reader that
//! gets an error discards what it has. A read that stops polling stops reading; the
//! rest of the blob is then never checked.
//!
//! **Where it reads.** From the write tier when the blob is staged on it, and the tier
//! holds the only copy; else from the store. A flushed copy on the tier is read by
//! [`Cache::read_blob`] and not by this read: a copy found damaged only at the end
//! could not be told from a damaged store object, and the tier has no way to drop a
//! flushed copy, so a damaged one would fail every stream until it was evicted.
//!
//! **A damaged blob** is handled as [`Cache::read_blob`] handles it: the object is
//! marked unreachable (corrupt, or missing if the store lost it), so the blob is
//! reported absent until a write heals it, and the error is
//! [`CacheError::Unreachable`]. A staged record is marked only when the tier is where
//! the damage was found; a blob that is only staged is never marked missing because the
//! store does not hold it yet.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use kbf_meta::{Location, UnreachableReason};
use kbf_objstore::{ByteRange, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_segments::RecordHasher;
use kbf_tier::WriteTier;
use kbf_types::Digest;

use super::tier::{Staging, blocking};
use super::{Cache, CacheError, is_empty_blob};
use crate::READ_CHUNK_BYTES;
use crate::meta_log::MetaLog;

/// The bytes of a ranged read ([`Cache::read_range`]), in order, in chunks of at most
/// [`READ_CHUNK_BYTES`]. An item is an error if the read failed after the answer began;
/// the stream ends after one.
pub type BlobStream = BoxStream<'static, Result<Bytes, CacheError>>;

impl<M: MetaLog, O: ObjectStore + 'static> Cache<M, O> {
    /// The bytes `[offset, offset + limit)` of the blob `digest` (to its end when
    /// `limit` is 0), after touching it, read from the store in pieces and verified as
    /// the module doc says. An `offset` past the end is the end: an empty stream.
    ///
    /// Nothing is read when no byte is asked for (an `offset` at the end, or the empty
    /// blob), so such a read does not check the blob.
    ///
    /// # Errors
    /// [`CacheError::NotFound`] only if the blob is not held;
    /// [`CacheError::Unreachable`] if it is held but cannot be read, or if a
    /// damaged blob is found before the first byte is handed out; the store failed.
    /// The stream's last item is [`CacheError::Unreachable`] if the blob is found
    /// damaged later.
    pub async fn read_range(
        self: &Arc<Self>,
        digest: &Digest,
        offset: u64,
        limit: u64,
    ) -> Result<BlobStream, CacheError> {
        if is_empty_blob(digest) {
            return Ok(stream::empty().boxed());
        }
        let (location, staged_on) = self.find_blob(digest).await?;
        let size = digest.size_bytes;
        let start = offset.min(size);
        let end = match limit {
            0 => size,
            n => size.min(offset.saturating_add(n)),
        };
        if start >= end {
            return Ok(stream::empty().boxed());
        }
        let key = self.store_key(digest, location)?;
        let tier = self
            .writer
            .tier
            .get()
            .filter(|staging| staged_on == Some(staging.id))
            .map(|staging| Arc::clone(&staging.tier));
        let mut pieces = Pieces {
            cache: Arc::clone(self),
            digest: *digest,
            location,
            key,
            tier,
            staged: staged_on.is_some(),
            hasher: RecordHasher::new(),
            pos: 0,
            start,
            end,
        };
        let first = pieces.next().await?;
        let rest = stream::try_unfold(pieces, |mut pieces| async move {
            Ok(pieces.next().await?.map(|chunk| (chunk, pieces)))
        });
        Ok(stream::iter(first.map(Ok)).chain(rest).boxed())
    }
}

/// One blob being read: how far, and the hash of what was read.
struct Pieces<M, O> {
    cache: Arc<Cache<M, O>>,
    digest: Digest,
    location: Location,
    key: ObjectKey,
    /// The write tier, while every piece so far came from it.
    tier: Option<Arc<dyn WriteTier>>,
    /// Whether the index says the blob is only staged: the store may not hold it yet.
    staged: bool,
    /// The hash of the blob's bytes `[0, pos)`.
    hasher: RecordHasher,
    /// The next byte of the blob to read.
    pos: u64,
    /// The bytes `[start, end)` are the ones asked for.
    start: u64,
    end: u64,
}

impl<M: MetaLog, O: ObjectStore + 'static> Pieces<M, O> {
    /// The next chunk of `[start, end)`, or `None` after the last, once the whole blob
    /// has been read and checked. Reads (and hashes) the pieces before `start` and,
    /// after the last chunk, the pieces after `end`.
    async fn next(&mut self) -> Result<Option<Bytes>, CacheError> {
        let size = self.digest.size_bytes;
        while self.pos < size {
            let piece = self.read_piece().await?;
            let from = self.pos;
            self.pos += piece.len() as u64;
            self.hasher.update(&piece);
            if self.pos == size {
                self.check().await?;
            }
            let (lo, hi) = (self.start.max(from), self.end.min(self.pos));
            if lo < hi {
                return Ok(Some(
                    piece.slice((lo - from) as usize..(hi - from) as usize),
                ));
            }
        }
        Ok(None)
    }

    /// The piece of the blob at `pos`: at most [`READ_CHUNK_BYTES`], and never past the
    /// blob's end. From the tier if every piece so far was, and it still has the
    /// range; else from the store, where an object it does not have, or has too short,
    /// is marked as [`Cache::fetch`] marks one.
    async fn read_piece(&mut self) -> Result<Bytes, CacheError> {
        let Location { object, offset, .. } = self.location;
        let size = self.digest.size_bytes;
        let len = READ_CHUNK_BYTES.min(usize::try_from(size - self.pos).unwrap_or(usize::MAX));
        let range = offset
            .checked_add(self.pos)
            .and_then(|at| ByteRange::new(at, len as u64))
            .ok_or_else(|| {
                let digest = self.digest;
                CacheError::Internal(format!("blob {digest} has no readable range at {offset}"))
            })?;
        if let Some(tier) = self.tier.clone() {
            let metrics = self.cache.metrics();
            let at = range.offset();
            match blocking(metrics, Box::new(move || tier.read(object, at, len))).await {
                Ok(bytes) => return Ok(Bytes::from(bytes)),
                Err(e) => {
                    tracing::warn!(digest = %self.digest, error = %e, "write tier read failed; reading the store");
                    self.tier = None;
                }
            }
        }
        let got = self.cache.writer.get_range(&self.key, range).await;
        match got {
            Ok(bytes) if bytes.len() == len => Ok(bytes),
            // A short read cannot hash to the digest, whatever follows it.
            Ok(_) => {
                tracing::error!(digest = %self.digest, key = %self.key, "stored object is shorter than its blob");
                Err(self.damaged(UnreachableReason::Corrupt).await)
            }
            Err(e @ (ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. })) => {
                tracing::error!(digest = %self.digest, key = %self.key, error = %e, "held blob missing from the store");
                Err(self.damaged(UnreachableReason::Missing).await)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The tier, and its counters, while every piece so far came from it.
    fn local(&self) -> Option<(&Arc<dyn WriteTier>, &Staging)> {
        self.tier.as_ref().zip(self.cache.writer.tier.get())
    }

    /// Compares the hash of the whole blob with its digest; the blob is damaged if it
    /// differs.
    async fn check(&self) -> Result<(), CacheError> {
        let (actual, _) = self.hasher.clone().finish();
        if actual == self.digest {
            if let Some((_, staging)) = self.local() {
                staging.counters.local_reads.add(1);
            }
            return Ok(());
        }
        tracing::error!(digest = %self.digest, %actual, key = %self.key, "stored bytes fail their digest");
        Err(self.damaged(UnreachableReason::Corrupt).await)
    }

    /// The error for a blob found damaged, after marking its object unreachable for
    /// `reason`. The tier's copy of a staged blob is the only one, so damage found there
    /// is always marked: its block first takes no more ranges, and once the mark
    /// committed a sealed block is deleted. The store not having a staged blob says
    /// nothing (it is not there yet), so damage found in the store is marked only for a
    /// blob that is not staged. That the mark could not be committed is the error
    /// instead.
    async fn damaged(&self, reason: UnreachableReason) -> CacheError {
        let object = self.location.object;
        let local = self.local();
        if let Some((tier, _)) = local {
            tier.close(object);
        }
        let mark = local.is_some() || !self.staged;
        let marked = if mark {
            self.cache.mark_unreachable(object, reason).await
        } else {
            Ok(())
        };
        if let Some((tier, _)) = local {
            let _ = tier.discard(object);
        }
        marked.err().unwrap_or(CacheError::Unreachable(self.digest))
    }
}
