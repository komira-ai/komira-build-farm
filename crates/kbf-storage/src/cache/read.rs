//! Reading a blob in windows ([`Cache::read_windows`]), so a read of any size holds at
//! most [`READ_WINDOW_BYTES`] of it at once; and [`Cache::read_blob`], the whole-blob
//! read of the blobs the server decodes, over it.
//!
//! Every window is hashed as it is read, from the blob's first byte whatever range is
//! asked for, and the digest is checked after the last window, before that window is
//! handed over. So a blob of at most one window (most blobs) is checked whole before any
//! of it is; a larger blob's earlier windows are handed over before the check, and a
//! blob whose bytes turn out wrong ends its read with an error ([`CacheError::Unreachable`])
//! instead of its last window. REAPI clients check every blob against its digest, so
//! those bytes are never taken for the blob; the object is marked corrupt as a whole
//! read marks it.
//!
//! Windows are read where the index said the blob is: from this server's write tier
//! when the object is staged on it or has a flushed copy here, else from the store. A
//! window the tier cannot give (its copy evicted mid-read) is read from the store from
//! there on. A first window from a flushed copy that fails the digest is read again from
//! the store, as a whole read does.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use kbf_meta::{BlobAnswer, Location, ObjectId, StoreId, TierId, UnreachableReason};
use kbf_objstore::{ByteRange, ObjectStore, ObjectStoreError};
use kbf_segments::RecordHasher;
use kbf_types::Digest;

use super::tier::{Staging, blocking};
use super::write_group::Writer;
use super::{Cache, CacheError, is_empty_blob};
use crate::meta_log::MetaLog;

/// The most bytes of a blob one read holds at once, and the size of each range it
/// reads from the tier or the store: 8 MiB. A blob of at most this many bytes is read
/// with one range read and checked against its digest before any of it is handed over.
pub const READ_WINDOW_BYTES: u64 = 8 << 20;

/// The largest blob [`Cache::read_blob`] reads whole: 64 MiB. Whole reads are for the
/// blobs the server decodes (an `Action`, a `Command`, a `Directory`, a `Tree`, an
/// `ActionResult`); a larger one is refused as INVALID_ARGUMENT where it is decoded.
/// Clients read any blob, of any size, through ByteStream Read, in windows.
pub const MAX_DECODED_BLOB_BYTES: u64 = 64 << 20;

/// Where the next window of a read comes from.
enum Source {
    /// This server's write tier.
    Tier(Arc<Staging>),
    /// The store. A missing or damaged object is marked unreachable only if the blob
    /// was not staged when the index answered: the store does not hold a staged object
    /// yet (design section 6.1.5).
    Store,
}

/// A blob being read in windows ([`Cache::read_windows`]).
pub struct BlobRead<M, O> {
    writer: Arc<Writer<M, O>>,
    digest: Digest,
    location: Location,
    staged_on: Option<TierId>,
    source: Source,
    /// The range asked for.
    from: u64,
    to: u64,
    /// The next byte to read.
    at: u64,
    hasher: RecordHasher,
    /// Whether a window was handed over.
    handed: bool,
    done: bool,
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Opens a read of bytes `[from, to)` of `digest` (`to` at most its size), after
    /// touching it, as [`Cache::read_blob`] reads a whole one.
    ///
    /// # Errors
    /// [`CacheError::NotFound`] only if the blob is not held;
    /// [`CacheError::Unreachable`] if it is held but its object cannot be read;
    /// [`CacheError::Invalid`] for a range past its end. A window that cannot be read,
    /// or a blob whose bytes fail their digest, is an error of [`BlobRead::next`].
    pub async fn read_windows(
        &self,
        digest: &Digest,
        from: u64,
        to: u64,
    ) -> Result<BlobRead<M, O>, CacheError> {
        if from > to || to > digest.size_bytes {
            return Err(CacheError::Invalid(format!(
                "bytes {from}..{to} are not within {digest}"
            )));
        }
        let unread = Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(self.writer.epoch, 0),
            offset: 0,
        };
        if is_empty_blob(digest) {
            let mut read = self.located(digest, unread, None, from, to)?;
            read.done = true;
            return Ok(read);
        }
        let answer = self
            .touched(|s| (s.blob(digest), s.touch_for([digest])))
            .await?;
        match answer {
            BlobAnswer::Absent => Err(CacheError::NotFound(*digest)),
            BlobAnswer::Unavailable => Err(CacheError::Unreachable(*digest)),
            BlobAnswer::Present {
                location,
                staged_on,
            } => self.located(digest, location, staged_on, from, to),
        }
    }

    /// A read of bytes `[from, to)` of `digest` at `location`, staged on `staged_on`,
    /// as the index answered: from this server's tier when the object is staged on it
    /// or has a flushed copy here, else from the store. A use of the object.
    ///
    /// # Errors
    /// [`CacheError::Internal`] for a location in another store than this cache's.
    pub(super) fn located(
        &self,
        digest: &Digest,
        location: Location,
        staged_on: Option<TierId>,
        from: u64,
        to: u64,
    ) -> Result<BlobRead<M, O>, CacheError> {
        if location.store != StoreId::CONFIGURED {
            return Err(CacheError::Internal(format!(
                "blob {digest} is in store {}, and this cache has only store {}",
                location.store.get(),
                StoreId::CONFIGURED.get()
            )));
        }
        self.used([location.object]);
        let source = match self.writer.tier.get() {
            Some(staging) if staged_on.is_none_or(|t| t == staging.id) => {
                Source::Tier(Arc::clone(staging))
            }
            _ => Source::Store,
        };
        Ok(BlobRead {
            writer: Arc::clone(&self.writer),
            digest: *digest,
            location,
            staged_on,
            source,
            from,
            to,
            at: 0,
            hasher: RecordHasher::new(),
            handed: false,
            done: false,
        })
    }

    /// The whole of `digest` at `location` as the index answered it, read as
    /// [`Cache::located`] reads: for a batch read's blobs, which are small.
    pub(super) async fn fetch_answer(
        &self,
        digest: &Digest,
        location: Location,
        staged_on: Option<TierId>,
    ) -> Result<Bytes, CacheError> {
        let size = digest.size_bytes;
        self.located(digest, location, staged_on, 0, size)?
            .gather()
            .await
    }

    /// Reads a whole blob, verified against its digest, after touching it: for the
    /// blobs the server decodes. Read in windows as [`Cache::read_windows`] reads, and
    /// gathered.
    ///
    /// # Errors
    /// [`CacheError::NotFound`] only if the blob is not held;
    /// [`CacheError::Unreachable`] if it is held but cannot be read;
    /// [`CacheError::Invalid`] if it is over [`MAX_DECODED_BLOB_BYTES`].
    pub async fn read_blob(&self, digest: &Digest) -> Result<Bytes, CacheError> {
        let read = self.read_windows(digest, 0, digest.size_bytes).await?;
        if digest.size_bytes > MAX_DECODED_BLOB_BYTES {
            return Err(CacheError::Invalid(format!(
                "{digest} is over the {MAX_DECODED_BLOB_BYTES} bytes the server reads whole \
                 to decode; any blob can be read in windows through ByteStream Read"
            )));
        }
        read.gather().await
    }
}

impl<M: MetaLog, O: ObjectStore> BlobRead<M, O> {
    /// Every byte the read hands over, gathered: one window's bytes as they are, more
    /// joined.
    async fn gather(mut self) -> Result<Bytes, CacheError> {
        let Some(first) = self.next().await.transpose()? else {
            return Ok(Bytes::new());
        };
        if first.len() as u64 == self.range_len() {
            return Ok(first);
        }
        // The caller bounds the blob's size: a decoded blob, or a batch read's.
        let mut whole = BytesMut::with_capacity(usize::try_from(self.range_len()).unwrap_or(0));
        whole.extend_from_slice(&first);
        while let Some(window) = self.next().await.transpose()? {
            whole.extend_from_slice(&window);
        }
        Ok(whole.freeze())
    }

    /// The bytes the read hands over in all: the range asked for.
    #[must_use]
    pub fn range_len(&self) -> u64 {
        self.to - self.from
    }

    /// The next bytes of the range asked for, at most [`READ_WINDOW_BYTES`]; `None`
    /// after the last. Every window of the blob is read and hashed, the ones outside
    /// the range too, and the last is handed over only once the blob's digest checked.
    ///
    /// # Errors
    /// A window could not be read, or the blob's bytes fail its digest: the read ends.
    pub async fn next(&mut self) -> Option<Result<Bytes, CacheError>> {
        loop {
            if self.done {
                return None;
            }
            let start = self.at;
            let len = READ_WINDOW_BYTES.min(self.digest.size_bytes - start);
            let window = match self.window(start, len).await {
                Ok(window) => window,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            self.hasher.update(&window);
            self.at += len;
            if self.at == self.digest.size_bytes {
                let (actual, _) = std::mem::take(&mut self.hasher).finish();
                if actual != self.digest {
                    match self.damaged().await {
                        Ok(()) => continue,
                        Err(e) => {
                            self.done = true;
                            return Some(Err(e));
                        }
                    }
                }
                self.done = true;
            }
            let (from, to) = (self.from.max(start), self.to.min(start + len));
            if from < to {
                self.handed = true;
                // Both within the window, which is in memory.
                let piece = window.slice((from - start) as usize..(to - start) as usize);
                return Some(Ok(piece));
            }
        }
    }

    /// `len` bytes of the blob from its byte `start`.
    async fn window(&mut self, start: u64, len: u64) -> Result<Bytes, CacheError> {
        let Location { object, offset, .. } = self.location;
        if let Source::Tier(staging) = &self.source {
            let tier = Arc::clone(&staging.tier);
            let n = usize::try_from(len).unwrap_or(usize::MAX);
            let read = blocking(
                &self.writer.metrics,
                Box::new(move || tier.read(object, offset + start, n)),
            )
            .await;
            match read {
                // The tier reads all of `len` or fails.
                Ok(bytes) => {
                    staging.counters.local_reads.add(1);
                    return Ok(Bytes::from(bytes));
                }
                Err(kbf_tier::TierError::UnknownBlock(_)) => {}
                Err(e) => {
                    tracing::warn!(digest = %self.digest, error = %e, "write tier read failed; reading the store");
                }
            }
            self.source = Source::Store;
        }
        let mark = self.staged_on.is_none();
        let key = self.writer.object_key(object)?;
        let digest = self.digest;
        let range = ByteRange::new(offset + start, len).ok_or(CacheError::NotFound(digest))?;
        match self.writer.get_range(&key, range).await {
            Ok(bytes) if bytes.len() as u64 == len => Ok(bytes),
            Ok(_) => {
                tracing::error!(%digest, %key, mark, "the store read a held blob short");
                self.mark(UnreachableReason::Missing, mark).await
            }
            Err(e @ (ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. })) => {
                tracing::error!(%digest, %key, error = %e, mark, "held blob missing from the store");
                self.mark(UnreachableReason::Missing, mark).await
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The blob's bytes failed its digest. A blob from a flushed copy on the tier, none
    /// of it handed over yet, is read again from the store (`Ok`); otherwise the
    /// object is marked corrupt as a whole read marks it and the read fails.
    async fn damaged(&mut self) -> Result<(), CacheError> {
        let (digest, object) = (self.digest, self.location.object);
        tracing::error!(%digest, %object, "stored bytes fail their digest");
        let failed = match &self.source {
            Source::Tier(_) if self.staged_on.is_none() && !self.handed => {
                // Nothing handed over yet: read it all again from the store.
                self.source = Source::Store;
                self.at = 0;
                return Ok(());
            }
            Source::Tier(staging) if self.staged_on.is_some() => {
                // The local copy is the only one: its block takes no more ranges, the
                // mark commits, then a sealed block is deleted.
                staging.tier.close(object);
                let marked = self
                    .writer
                    .mark_unreachable(object, UnreachableReason::Corrupt)
                    .await;
                let _ = staging.tier.discard(object);
                marked
            }
            Source::Tier(_) => Ok(()),
            Source::Store => {
                let mark = self.staged_on.is_none();
                self.mark(UnreachableReason::Corrupt, mark).await.map(drop)
            }
        };
        failed.and(Err(CacheError::Unreachable(digest)))
    }

    /// Marks the object unreachable for `reason` if `mark`, and fails the read.
    async fn mark(&self, reason: UnreachableReason, mark: bool) -> Result<Bytes, CacheError> {
        if mark {
            self.writer
                .mark_unreachable(self.location.object, reason)
                .await?;
        }
        Err(CacheError::Unreachable(self.digest))
    }
}
