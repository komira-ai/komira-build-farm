//! The write tier's flusher (`docs/design/write-path.md`, sections 6.1.4 and 6.4):
//! seals, then each sealed staged object read back from its block in pieces of
//! [`FLUSH_READ_BYTES`], each piece checked and handed to the store as it is read (a
//! streamed write, [`ObjectStore::put_stream`]: a multipart upload on S3), then its
//! footer; the footer read back, and `ObjectFlushed` committed, off the ack path. A
//! flush holds a piece of the block at a time, never the block.
//!
//! - **Same key, same bytes.** An object's key is fixed by its id, and its footer is
//!   built from the committed blob list (sorted by digest) and the bytes on disk, so
//!   every attempt, before or after a crash, PUTs identical content. A flush is never
//!   moved to another object.
//! - **`AlreadyExists`** (a store with conditional writes, after an earlier attempt
//!   whose answer was lost): the footer is read back; if it is ours the flush is done,
//!   else `FlushConflict` commits, the object stays staged and is read locally, and the
//!   flusher stops retrying it.
//! - **Every record is hashed** as its pieces are read, on the cache's hashing pool
//!   (off the runtime workers): the daemon's digests were trusted when the upload was
//!   acknowledged, so this is where they are checked. The footer is written only once
//!   every record hashed to its digest; a record that fails ends the write in its
//!   place, so the store keeps nothing (an S3 multipart upload is aborted). The record
//!   is counted (`kbf_tier_flush_digest_mismatch_total`), logged as an error with its
//!   digest and object, and dropped. The object is marked unreachable as corrupt, the
//!   records that did hash to their digests are stored again as an object of their
//!   own (read from the block and hashed again the same way, then indexed with one
//!   `PutBlobs`), and the block is discarded, so the rest of the segment still reaches
//!   the store and stays readable; only the bad digest is left unavailable, to be
//!   uploaded again.
//! - **Anything else** (the store or the log unavailable, a read-back that disagrees
//!   after our own PUT, a disk read error) is retried with backoff,
//!   [`FLUSH_BACKOFF_FIRST`] doubling to [`FLUSH_BACKOFF_MAX`].
//! - **Eviction is on committed state:** the block becomes a flushed copy
//!   ([`kbf_tier::WriteTier::flushed`]) only after `ObjectFlushed` applied.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt;
use kbf_meta::{Applied, Command, ObjectId, StagedObject, StagedState, UnreachableReason};
use kbf_objstore::{BodyStream, ObjectStore, ObjectStoreError};
use kbf_segments::layout::{encode_footer, footer_len};
use kbf_segments::{Footer, IndexEntry};
use kbf_tier::WriteTier;
use kbf_types::Digest;

use super::read::READ_WINDOW_BYTES;
use super::tier::{Staging, blocking};
use super::verify::{RecordCheck, Verified};
use super::write_group::{Group, Writer, placed, read_back};
use super::{Cache, CacheError, unexpected};
use crate::meta_log::MetaLog;

/// The wait after a flush's first failure.
pub const FLUSH_BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// The longest wait between attempts at one flush.
pub const FLUSH_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// The most objects one [`Cache::flush_tier`] round flushes, oldest first; the rest
/// wait for the next round.
pub const FLUSHES_PER_ROUND: usize = 4;

/// The wait before the next attempt after `failures` failures in a row.
pub(super) fn flush_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FLUSH_BACKOFF_FIRST
        .saturating_mul(1 << doublings)
        .min(FLUSH_BACKOFF_MAX)
}

/// What one [`Cache::flush_tier`] round did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushRound {
    /// Objects whose `SealStaged` committed.
    pub sealed: Vec<ObjectId>,
    /// Objects flushed: in the store, `ObjectFlushed` committed.
    pub flushed: Vec<ObjectId>,
    /// Objects whose key holds another object: `FlushConflict` committed.
    pub conflicts: Vec<ObjectId>,
    /// Objects with a record that failed its digest: marked unreachable as corrupt, the
    /// records that were sound staged again as an object of their own.
    pub corrupt: Vec<ObjectId>,
    /// The digests of the records that failed theirs, in the order they were found.
    pub dropped: Vec<Digest>,
    /// Objects whose flush failed this round, and how long until their next attempt.
    pub retrying: Vec<(ObjectId, Duration)>,
}

/// The most bytes of a block the flusher reads at once, and the pieces it hands to the
/// store: one read window.
pub const FLUSH_READ_BYTES: u64 = READ_WINDOW_BYTES;

/// What a flush's body found when it had read every record.
type Found = Arc<Mutex<Option<Verified>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A block being read into an object's body ([`Cache::block_body`]).
struct Reading<M, O> {
    writer: Arc<Writer<M, O>>,
    group: Arc<Group>,
    tier: Arc<dyn WriteTier>,
    object: ObjectId,
    /// The block's ranges left to read.
    spans: VecDeque<(u64, u64)>,
    check: RecordCheck,
    /// Whether the body ended (its footer, or an error, was handed over).
    done: bool,
    /// The bytes of the body so far.
    written: u64,
    found: Found,
}

impl<M: MetaLog, O: ObjectStore> Reading<M, O> {
    /// The next piece of the body, and the reading after it.
    async fn step(mut self) -> Option<(Result<Bytes, ObjectStoreError>, Self)> {
        if self.done {
            return None;
        }
        let Some(&(start, end)) = self.spans.front() else {
            // Every record was read: the footer, or the error that ends the write.
            self.done = true;
            let checked = std::mem::take(&mut self.check).finish();
            let footer = checked
                .bad
                .is_empty()
                .then(|| encode_footer(self.written, &checked.good));
            *lock(&self.found) = Some(checked);
            let piece = footer.map(Bytes::from).ok_or_else(|| {
                ObjectStoreError::Body("records of the block fail their digests".to_owned())
            });
            return Some((piece, self));
        };
        let len = FLUSH_READ_BYTES.min(end - start);
        let piece = self.piece(start, len).await;
        // A piece that failed ends the body; the spans left are never read.
        self.done = piece.is_err();
        // The front span, read just above, moves past the piece; once read, it goes.
        self.spans[0].0 += len;
        if self.spans[0].0 == end {
            self.spans.pop_front();
        }
        Some((piece, self))
    }

    /// `len` bytes of the block from `start`, read and hashed into the check.
    async fn piece(&mut self, start: u64, len: u64) -> Result<Bytes, ObjectStoreError> {
        let (tier, object) = (Arc::clone(&self.tier), self.object);
        let n = usize::try_from(len).unwrap_or(usize::MAX);
        let read = blocking(
            &self.writer.metrics,
            Box::new(move || tier.read(object, start, n)),
        )
        .await;
        // The tier reads all of `len` or fails.
        let read = read.map_err(|e| format!("reading the block of {object}: {e}"));
        let piece = Bytes::from(read.map_err(ObjectStoreError::Body)?);
        let mut check = std::mem::take(&mut self.check);
        let (at, hashed) = (self.written, piece.clone());
        let check = self
            .group
            .hashing
            .compute(move || {
                check.update(at, &hashed);
                check
            })
            .await?;
        self.check = check;
        self.written += len;
        Ok(piece)
    }
}

/// How one flush ended, short of an error to retry.
enum Flushed {
    Done,
    Conflict,
    /// Records failed their digests; these are their digests.
    Corrupt(Vec<Digest>),
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// One round of the write tier's background work at `now`; without a tier, none.
    ///
    /// 1. Every block whose seal is due and whose ranges are all answered gets its
    ///    `SealStaged` ([`kbf_tier::WriteTier::poll_seals`]).
    /// 2. Up to [`FLUSHES_PER_ROUND`] sealed objects staged on this tier, in id order
    ///    (oldest first), whose backoff has passed, are flushed. The queue is the
    ///    committed state's, so a restart resumes every flush it had not finished.
    ///
    /// # Errors
    ///
    /// The metadata log could not answer a seal or the queue's query. A failed flush
    /// is not an error: it is in [`FlushRound::retrying`].
    pub async fn flush_tier(&self, now: Instant) -> Result<FlushRound, CacheError> {
        let Some(staging) = self.writer.tier.get() else {
            return Ok(FlushRound::default());
        };
        let mut round = FlushRound::default();
        for object in staging.tier.poll_seals(now) {
            self.commit_tier(Command::SealStaged { object }).await?;
            // Named by `poll_seals` just now, so the tier takes it.
            let _ = staging.tier.sealed(object);
            round.sealed.push(object);
        }
        let (tier, waiting) = (staging.id, staging.waiting(now));
        let due = self
            .writer
            .query(move |s| {
                let ours = |o: &StagedObject| (o.tier(), o.state()) == (tier, StagedState::Sealed);
                let sealed = s.staged_objects().filter(|(_, o)| ours(o));
                let sealed = sealed.filter(|(object, _)| !waiting.contains(object));
                let due = sealed.take(FLUSHES_PER_ROUND).map(|(object, o)| {
                    let blobs: Vec<(Digest, u64)> = o.blobs().map(|(d, at)| (*d, at)).collect();
                    (object, o.committed_len(), blobs)
                });
                due.collect::<Vec<_>>()
            })
            .await?;
        for (object, len, blobs) in due {
            match self.flush_one(staging, object, len, &blobs).await {
                Ok(Flushed::Done) => {
                    staging.done(object);
                    round.flushed.push(object);
                }
                Ok(Flushed::Conflict) => {
                    staging.done(object);
                    round.conflicts.push(object);
                }
                Ok(Flushed::Corrupt(digests)) => {
                    staging.done(object);
                    round.corrupt.push(object);
                    round.dropped.extend(digests);
                }
                Err(e) => {
                    staging.counters.flush_failures.add(1);
                    let wait = staging.failed(object, now);
                    tracing::warn!(%object, error = %e, ?wait, "flush failed; retrying after the wait");
                    round.retrying.push((object, wait));
                }
            }
        }
        Ok(round)
    }

    /// Flushes `object`, staged with committed length `len` and `blobs` (sorted by
    /// digest, as the committed list is): its block read in pieces of
    /// [`FLUSH_READ_BYTES`], each hashed on the hashing pool and handed to the store as
    /// it is read ([`ObjectStore::put_stream`]), then the footer, built once every
    /// record has hashed. A record that fails its digest ends the write before the
    /// footer: the store keeps nothing.
    async fn flush_one(
        &self,
        staging: &Staging,
        object: ObjectId,
        len: u64,
        blobs: &[(Digest, u64)],
    ) -> Result<Flushed, CacheError> {
        let key = self.object_key(object)?;
        let total = len + footer_len(blobs.len() as u64);
        let found = Found::default();
        let body = self.block_body(
            staging,
            object,
            vec![(0, len)],
            RecordCheck::new(blobs.to_vec()),
            &found,
        );
        let put = self.writer.objects.put_stream(&key, total, body).await;
        // A store that answers without reading the body found nothing: its footer is
        // then not ours, and the flush is tried again.
        let checked = lock(&found).take().unwrap_or_default();
        if !checked.bad.is_empty() {
            return self.drop_mismatched(staging, object, &checked).await;
        }
        let existed = match put {
            Ok(()) => false,
            Err(ObjectStoreError::AlreadyExists(_)) => true,
            Err(e) => return Err(e.into()),
        };
        let footer = encode_footer(len, &checked.good);
        let ours = Footer::parse(&footer, total)?;
        self.writer.recency.written(object, total);
        let stored = match self.writer.read_footer(&key, total).await {
            Ok(footer) => Some(footer),
            // The key holds an object this footer does not describe (another length,
            // or no footer where ours ends).
            Err(
                CacheError::Segment(_)
                | CacheError::Internal(_)
                | CacheError::Store(ObjectStoreError::InvalidRange { .. }),
            ) => None,
            Err(e) => return Err(e),
        };
        if stored.as_ref() == Some(&ours) {
            self.commit_tier(Command::ObjectFlushed { object }).await?;
            if let Err(e) = staging.tier.flushed(object) {
                tracing::warn!(%object, error = %e, "flushed object's block not kept as a local copy");
            }
            staging.counters.flushes.add(1);
            return Ok(Flushed::Done);
        }
        if !existed {
            // Our own PUT read back as something else: a store fault. The next attempt
            // PUTs the same bytes again (or meets them, or a conflict, as AlreadyExists).
            return Err(CacheError::Internal(format!(
                "object {key} read back with another footer after its PUT"
            )));
        }
        tracing::error!(%object, %key, "the store holds another object under a staged object's key; it stays staged, read locally, until an operator acts");
        self.commit_tier(Command::FlushConflict { object }).await?;
        staging.counters.conflicts.add(1);
        Ok(Flushed::Conflict)
    }

    /// Handles a staged object with records that fail their digests: counts and logs
    /// each (never uploaded), marks the object unreachable as corrupt (its blobs are
    /// unavailable, and a re-upload heals them), stores the records that hashed to
    /// their digests again as an object of their own, read from the block as the flush
    /// reads it, and frees the block. A failure of the restore is logged, not retried:
    /// the object has left the flusher's queue, and those blobs are unavailable until
    /// uploaded again.
    async fn drop_mismatched(
        &self,
        staging: &Staging,
        object: ObjectId,
        found: &Verified,
    ) -> Result<Flushed, CacheError> {
        for bad in &found.bad {
            tracing::error!(%object, digest = %bad.claimed, found = %bad.actual, "a staged record fails its digest; it is dropped and never flushed");
        }
        staging.counters.digest_mismatch.add(found.bad.len() as u64);
        staging.counters.corrupt.add(1);
        self.mark_unreachable(object, UnreachableReason::Corrupt)
            .await?;
        if let Err(e) = self.restage(staging, object, &found.good).await {
            tracing::error!(%object, error = %e, sound = found.good.len(), "the sound records of a corrupt staged object could not be stored again; they are unavailable until uploaded again");
        }
        let _ = staging.tier.discard(object);
        let dropped = found.bad.iter().map(|m| m.claimed).collect();
        Ok(Flushed::Corrupt(dropped))
    }

    /// Stores `good` (records of `object`'s block, each at its offset there) again as an
    /// object of their own, back to back, copied from the block a piece at a time: on
    /// the write tier, a block of its own staged with one `PutStaged` (flushed, and so
    /// hashed again, by a later round), when the tier has room; else in the store,
    /// hashed again as the pieces go, and indexed with one `PutBlobs`.
    async fn restage(
        &self,
        staging: &Staging,
        object: ObjectId,
        good: &[IndexEntry],
    ) -> Result<(), CacheError> {
        if good.is_empty() {
            return Ok(());
        }
        let mut spans = Vec::with_capacity(good.len());
        let mut listed = Vec::with_capacity(good.len());
        let mut at = 0u64;
        for entry in good {
            let size = entry.digest.size_bytes;
            spans.push((entry.offset, entry.offset + size));
            listed.push((entry.digest, at));
            at += size;
        }
        if let Some(range) = self.reserve_range(at).await {
            let (tier, copied) = (Arc::clone(&staging.tier), spans.clone());
            let copy = move || {
                let mut to = 0u64;
                for (start, end) in copied {
                    for from in (start..end).step_by(FLUSH_READ_BYTES as usize) {
                        let n = FLUSH_READ_BYTES.min(end - from);
                        let piece = tier.read(object, from, n as usize)?;
                        range.write(to, &piece)?;
                        to += n;
                    }
                }
                Ok(range)
            };
            let range = blocking(&self.writer.metrics, Box::new(copy)).await?;
            return self.stage_range(range, listed).await;
        }
        let total = at + footer_len(good.len() as u64);
        let found = Found::default();
        let body = self.block_body(staging, object, spans, RecordCheck::new(listed), &found);
        let sound = self.writer.allocate();
        let key = self.object_key(sound)?;
        self.writer.objects.put_stream(&key, total, body).await?;
        self.writer.recency.written(sound, total);
        let footer = self
            .writer
            .read_footer(&key, total)
            .await
            .map_err(read_back)?;
        let placed = placed(&footer, &key, sound, good.iter().map(|e| e.digest))?;
        self.writer.commit_segment(placed).await
    }

    /// The body of an object read from `object`'s block: the bytes of `spans` (ranges
    /// of the block, in order) in pieces of at most [`FLUSH_READ_BYTES`], each read on
    /// the blocking pool and hashed into `check` on the hashing pool (the check's
    /// offsets are the new object's), then the footer of the records, or, if any
    /// failed its digest, an error in its place, which ends the write with nothing
    /// stored. What the check found is left in `found` once every record was read.
    fn block_body(
        &self,
        staging: &Staging,
        object: ObjectId,
        spans: Vec<(u64, u64)>,
        check: RecordCheck,
        found: &Found,
    ) -> BodyStream {
        let reading = Reading {
            writer: Arc::clone(&self.writer),
            group: Arc::clone(&self.group),
            tier: Arc::clone(&staging.tier),
            object,
            spans: spans.into(),
            check,
            done: false,
            written: 0,
            found: Arc::clone(found),
        };
        futures::stream::unfold(reading, Reading::step).boxed()
    }

    /// Commits a tier command whose refusal would mean the flusher and the committed
    /// state disagree: anything but its success is an internal error.
    async fn commit_tier(&self, command: Command) -> Result<(), CacheError> {
        match self.writer.commit(command).await? {
            Applied::Tier(Ok(())) => Ok(()),
            other => Err(unexpected(&other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a backoff that does not grow (a store outage met by a retry every
    /// round), one that grows without bound (a flush that waits hours after the store
    /// came back), and an overflow at many failures.
    #[test]
    fn backoff_doubles_from_the_first_wait_to_the_cap() {
        assert_eq!(flush_backoff(1), FLUSH_BACKOFF_FIRST);
        assert_eq!(flush_backoff(2), FLUSH_BACKOFF_FIRST * 2);
        assert_eq!(flush_backoff(3), FLUSH_BACKOFF_FIRST * 4);
        assert_eq!(flush_backoff(7), FLUSH_BACKOFF_MAX);
        assert_eq!(flush_backoff(u32::MAX), FLUSH_BACKOFF_MAX);
    }
}
