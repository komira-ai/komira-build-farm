//! A blob written to the store as its bytes arrive ([`Cache::store_streamed`]): how a
//! ByteStream `Write` of a blob over
//! [`MAX_UNSTAGED_BLOB_BYTES`](crate::MAX_UNSTAGED_BLOB_BYTES) is stored when there is
//! no write tier, or it has no room for the blob, whatever its size, holding a piece of
//! it at a time; and how a daemon in the server's own process stores an output over it
//! when the write tier is missing or has no room ([`Cache::store_streamed_claimed`]),
//! its digest named once the bytes ended.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::StreamExt;
use futures::future::{BoxFuture, FutureExt as _};
use kbf_objstore::{BodyStream, ObjectStore, ObjectStoreError};
use kbf_segments::layout::{encode_footer, footer_len};
use kbf_segments::{IndexEntry, RecordHasher};
use kbf_types::Digest;

use super::write_group::{Group, placed, read_back};
use super::{Cache, CacheError};
use crate::MAX_UNSTAGED_BLOB_BYTES;
use crate::meta_log::MetaLog;

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Stores the blob `digest`, whose bytes come in `bytes`, as an object of its own:
    /// each piece hashed (on the hashing pool) and handed to the store as it comes
    /// ([`ObjectStore::put_stream`]: a multipart upload on S3), then the footer of its
    /// one record, only once the bytes hashed to `digest`. The footer is read back and
    /// the blob indexed with one `PutBlobs`: it is present only once this answers.
    ///
    /// Not staged on the write tier, and not under a `--write-puts-in-flight` slot: the
    /// write lasts as long as its sender takes. The caller's admission bounds how many
    /// run at once.
    ///
    /// # Errors
    /// [`CacheError::DigestMismatch`] if the bytes do not hash to `digest` (or are not
    /// as many): nothing is stored. The body's own error, or the store's or the log's.
    pub async fn store_streamed(
        &self,
        digest: Digest,
        bytes: BodyStream,
    ) -> Result<(), CacheError> {
        let claim = futures::future::ready(Some(digest)).boxed();
        self.store_claimed(digest.size_bytes, bytes, claim)
            .await
            .map(drop)
    }

    /// [`Cache::store_streamed`] for a blob of `size` bytes whose digest is known only
    /// once its bytes are read: an output file a daemon in the server's own process
    /// hashes as it reads it. `claim` names the digest once the bytes ended, or `None`
    /// to withdraw the blob (the file changed, or the blob is held already): then the
    /// upload is aborted, nothing is stored, and this answers `None`. Otherwise the
    /// bytes must hash to the claimed digest, as for [`Cache::store_streamed`], and
    /// this answers it once the blob is present.
    ///
    /// Admitted for [`MAX_UNSTAGED_BLOB_BYTES`], as a ByteStream `Write` that streams to
    /// the store is: a piece in hand and a part in the store, whatever `size` is.
    ///
    /// # Errors
    /// As for [`Cache::store_streamed`].
    pub async fn store_streamed_claimed(
        &self,
        size: u64,
        bytes: BodyStream,
        claim: BoxFuture<'static, Option<Digest>>,
    ) -> Result<Option<Digest>, CacheError> {
        let admitted = self.admit(MAX_UNSTAGED_BLOB_BYTES).await;
        let stored = self.store_claimed(size, bytes, claim).await;
        drop(admitted);
        stored
    }

    /// Streams `bytes` (`size` of them) to the store as the object of one record, the
    /// digest `claim` names checked before its footer; `None` if the claim was
    /// withdrawn (nothing stored).
    async fn store_claimed(
        &self,
        size: u64,
        bytes: BodyStream,
        claim: BoxFuture<'static, Option<Digest>>,
    ) -> Result<Option<Digest>, CacheError> {
        let object = self.writer.allocate();
        let key = self.object_key(object)?;
        let total = size.saturating_add(footer_len(1));
        let ended = Arc::new(Mutex::new(Ended::Running));
        let hashing = Hashing {
            group: Arc::clone(&self.group),
            bytes,
            hasher: Some(RecordHasher::new()),
            size,
            claim,
            ended: Arc::clone(&ended),
        };
        let body = futures::stream::unfold(hashing, Hashing::step).boxed();
        let put = self.writer.objects.put_stream(&key, total, body).await;
        let ended = std::mem::replace(
            &mut *ended.lock().unwrap_or_else(PoisonError::into_inner),
            Ended::Running,
        );
        let digest = match (ended, put) {
            (Ended::Withdrawn, _) => return Ok(None),
            (Ended::Mismatch { claimed, actual }, _) => {
                return Err(CacheError::DigestMismatch { claimed, actual });
            }
            (Ended::Claimed(digest), Ok(())) => digest,
            // The store's own failure; a store that took the object without its end
            // (which no store does) stores nothing either.
            (_, put) => {
                let unended = CacheError::Invalid(UNENDED.to_owned());
                return Err(put.err().map_or(unended, CacheError::from));
            }
        };
        self.writer.recency.written(object, total);
        let footer = self
            .writer
            .read_footer(&key, total)
            .await
            .map_err(read_back)?;
        let at = placed(&footer, &key, object, std::iter::once(digest))?;
        self.writer.commit_segment(at).await?;
        Ok(Some(digest))
    }
}

/// Why a streamed blob the store took before its body ended is not stored.
const UNENDED: &str = "the store took a streamed blob before its end";

/// How a streamed blob's bytes ended, as [`Hashing`] saw it.
enum Ended {
    /// Not yet.
    Running,
    /// The claim was withdrawn: nothing is to be stored.
    Withdrawn,
    /// The bytes did not hash to the claimed digest.
    Mismatch { claimed: Digest, actual: Digest },
    /// The bytes hashed to this claimed digest; its footer followed.
    Claimed(Digest),
}

/// A blob's bytes on their way to the store, hashed as they pass.
struct Hashing {
    group: Arc<Group>,
    bytes: BodyStream,
    /// `None` once the bytes ended.
    hasher: Option<RecordHasher>,
    /// The bytes the blob has.
    size: u64,
    /// The digest the bytes must hash to, named once they ended; taken then.
    claim: BoxFuture<'static, Option<Digest>>,
    /// How the bytes ended.
    ended: Arc<Mutex<Ended>>,
}

impl Hashing {
    /// The next piece of the object: a piece of the blob, then its footer (or, if the
    /// bytes were not the blob's, an error in its place).
    async fn step(mut self) -> Option<(Result<Bytes, ObjectStoreError>, Self)> {
        let hasher = self.hasher.take()?;
        let Some(piece) = self.bytes.next().await else {
            let (actual, crc32c) = hasher.finish();
            // Taken once: the hasher is gone after this step.
            let claim = std::mem::replace(&mut self.claim, futures::future::ready(None).boxed());
            let claimed = claim.await;
            let Some(claimed) = claimed else {
                self.end(Ended::Withdrawn);
                let why = "the blob was withdrawn before its end".to_owned();
                return Some((Err(ObjectStoreError::Body(why)), self));
            };
            if (actual != claimed) | (claimed.size_bytes != self.size) {
                self.end(Ended::Mismatch { claimed, actual });
                let why = format!("{claimed} hashes to {actual}");
                return Some((Err(ObjectStoreError::Body(why)), self));
            }
            self.end(Ended::Claimed(claimed));
            let entry = IndexEntry {
                digest: claimed,
                offset: 0,
                crc32c,
            };
            let footer = encode_footer(claimed.size_bytes, &[entry]);
            return Some((Ok(Bytes::from(footer)), self));
        };
        let piece = match piece {
            Ok(piece) => self.hash(hasher, piece).await,
            Err(e) => Err(e),
        };
        Some((piece, self))
    }

    /// Records how the bytes ended.
    fn end(&self, ended: Ended) {
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner) = ended;
    }

    /// Hashes `piece` into `hasher` on the hashing pool, and keeps the hasher.
    async fn hash(
        &mut self,
        mut hasher: RecordHasher,
        piece: Bytes,
    ) -> Result<Bytes, ObjectStoreError> {
        let hashed = piece.clone();
        let job = self.group.hashing.compute(move || {
            hasher.update(&hashed);
            hasher
        });
        self.hasher = Some(job.await?);
        Ok(piece)
    }
}
