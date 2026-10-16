//! [`Cache::read_blobs`]: many blobs read at once, as `BatchReadBlobs` asks for them.
//!
//! A batch of small blobs read one range GET after another costs one store round trip
//! per blob. Here the whole set is looked up and touched in one metadata query, blobs
//! that lie back to back in one segment are read with one range GET, and up to
//! [`BATCH_READS_IN_FLIGHT`] GETs run at once.

use std::collections::BTreeMap;

use bytes::Bytes;
use futures::{StreamExt, stream};
use kbf_meta::{BlobAnswer, Location, ObjectId, StoreId};
use kbf_objstore::{ByteRange, ObjectStore};
use kbf_types::Digest;

use super::{Cache, CacheError, is_empty_blob};
use crate::meta_log::MetaLog;

/// The most range GETs one [`Cache::read_blobs`] call keeps in flight.
pub const BATCH_READS_IN_FLIGHT: usize = 32;

/// Blobs that lie back to back in one object, from `start`, in offset order.
#[derive(Debug)]
struct Run {
    store: StoreId,
    object: ObjectId,
    start: u64,
    end: u64,
    blobs: Vec<(Digest, Location)>,
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Reads every blob in `digests` as [`Cache::read_blob`] reads one, with one answer
    /// per distinct digest.
    ///
    /// One metadata query and one touch cover the whole set. Blobs whose records lie
    /// back to back in one object are read with one range GET, and at most
    /// [`BATCH_READS_IN_FLIGHT`] GETs run at once. Each blob is checked against its
    /// digest on its own. A blob whose shared GET fails, or whose bytes in it fail
    /// their digest, is read again alone, so it fails (and marks its object) exactly as
    /// `read_blob` would.
    ///
    /// # Errors
    /// The metadata log is unavailable or kept changing: nothing is read. Otherwise each
    /// blob's own error, as `read_blob` gives it, is in its answer.
    pub async fn read_blobs(
        &self,
        digests: &[Digest],
    ) -> Result<BTreeMap<Digest, Result<Bytes, CacheError>>, CacheError> {
        let answers = self
            .touched(|s| {
                let answers: BTreeMap<Digest, BlobAnswer> =
                    digests.iter().map(|d| (*d, s.blob(d))).collect();
                (answers, s.touch_for(digests))
            })
            .await?;
        let mut out = BTreeMap::new();
        let mut held: BTreeMap<(StoreId, ObjectId), BTreeMap<u64, Digest>> = BTreeMap::new();
        let mut staged = Vec::new();
        for (digest, answer) in answers {
            match answer {
                _ if is_empty_blob(&digest) => {
                    out.insert(digest, Ok(Bytes::new()));
                }
                BlobAnswer::Absent => {
                    out.insert(digest, Err(CacheError::NotFound(digest)));
                }
                BlobAnswer::Unavailable => {
                    out.insert(digest, Err(CacheError::Unreachable(digest)));
                }
                BlobAnswer::Present {
                    location,
                    staged_on: Some(tier),
                } => staged.push((digest, location, tier)),
                BlobAnswer::Present {
                    location: at,
                    staged_on: None,
                } => {
                    let records = held.entry((at.store, at.object)).or_default();
                    records.insert(at.offset, digest);
                }
            }
        }
        // A staged blob is not in the store: each is read as `read_blob` reads it, from
        // the tier holding it, and never marks its object.
        for (digest, location, tier) in staged {
            let read = self.fetch_answer(&digest, location, Some(tier)).await;
            out.insert(digest, read);
        }
        let read: Vec<Vec<(Digest, Result<Bytes, CacheError>)>> = stream::iter(runs(held))
            .map(|run| self.read_run(run))
            .buffer_unordered(BATCH_READS_IN_FLIGHT)
            .collect()
            .await;
        out.extend(read.into_iter().flatten());
        Ok(out)
    }

    /// Each blob of `run`, from one GET when the run has more than one.
    async fn read_run(&self, run: Run) -> Vec<(Digest, Result<Bytes, CacheError>)> {
        let joined = if run.blobs.len() > 1 {
            self.read_joined(&run).await
        } else {
            None
        };
        let mut out = Vec::with_capacity(run.blobs.len());
        for (digest, at) in run.blobs {
            let sliced = joined
                .as_ref()
                .and_then(|bytes| record(bytes, run.start, &digest, at.offset));
            let read = match sliced {
                Some(bytes) => Ok(bytes),
                None => self.fetch_answer(&digest, at, None).await,
            };
            out.push((digest, read));
        }
        out
    }

    /// `run`'s records in one range GET, or `None` if that cannot be had (the blobs
    /// are then read alone, which gives each its own error).
    async fn read_joined(&self, run: &Run) -> Option<Bytes> {
        if run.store != StoreId::CONFIGURED {
            return None;
        }
        let key = self.object_key(run.object).ok()?;
        let range = ByteRange::new(run.start, run.end - run.start)?;
        match self.writer.get_range(&key, range).await {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                let blobs = run.blobs.len();
                tracing::debug!(%key, %error, blobs, "joined read failed; reading each alone");
                None
            }
        }
    }
}

/// The record of `digest` at `offset` within `joined` (read from `start`), if it is
/// all there and hashes to `digest`.
fn record(joined: &Bytes, start: u64, digest: &Digest, offset: u64) -> Option<Bytes> {
    let from = usize::try_from(offset - start).ok()?;
    let to = from.checked_add(usize::try_from(digest.size_bytes).ok()?)?;
    let bytes = joined.get(from..to).map(|_| joined.slice(from..to))?;
    (kbf_segments::sha256(&bytes) == *digest).then_some(bytes)
}

/// `held`'s records, split where one does not start where the previous one ends.
fn runs(held: BTreeMap<(StoreId, ObjectId), BTreeMap<u64, Digest>>) -> Vec<Run> {
    let mut out = Vec::new();
    for ((store, object), records) in held {
        let mut current: Option<Run> = None;
        for (offset, digest) in records {
            let at = Location {
                store,
                object,
                offset,
            };
            let end = offset.saturating_add(digest.size_bytes);
            match &mut current {
                Some(run) if run.end == offset => {
                    run.end = end;
                    run.blobs.push((digest, at));
                }
                _ => {
                    out.extend(current.take());
                    current = Some(Run {
                        store,
                        object,
                        start: offset,
                        end,
                        blobs: vec![(digest, at)],
                    });
                }
            }
        }
        out.extend(current);
    }
    out
}
