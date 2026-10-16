//! The local write tier as the cache uses it (`docs/design/write-path.md`, section 6):
//! packed segments staged in block files instead of PUT, staged and flushed blobs read
//! from the local disk, and the start that ties a tier directory to the committed
//! state. The flusher is in `flush.rs`; the disk layer is `kbf-tier`.
//!
//! **Writes.** The write group's settle (`write_group.rs`) offers every segment it
//! seals, and a `Segments` upload (`upload.rs`) every segment it built, to
//! [`Writer::stage`] first: staged when a tier is attached and takes it, else PUT as
//! before. A `Segments` range read is served from the tier too, while the object is
//! staged or has a flushed copy.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kbf_meta::{Applied, Command, ObjectId, ServerId, StagedPut, TierId, UnreachableReason};
use kbf_objstore::ObjectStore;
use kbf_segments::{Footer, IndexEntry};
use kbf_store::Fs;
use kbf_tier::{
    NextSeq, Recovery, StagedSnapshot, Synced, Tier, TierError, TierOptions, TierStart, WriteTier,
};
use kbf_types::Digest;

use super::write_group::Writer;
use super::{Cache, CacheError, unexpected};
use crate::meta_log::MetaLog;
use crate::metrics::{CacheMetrics, TierCounters};

/// The tier a cache stages its writes on, what it counted, and the flusher's retries.
pub(super) struct Staging {
    pub(super) tier: Arc<dyn WriteTier>,
    pub(super) id: TierId,
    pub(super) counters: TierCounters,
    retry: Mutex<BTreeMap<ObjectId, Retry>>,
}

/// A flush that failed: how many times in a row, and when the next may start.
#[derive(Clone, Copy, Debug)]
struct Retry {
    failures: u32,
    next: Instant,
}

impl fmt::Debug for Staging {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Staging")
            .field("id", &self.id)
            .field("usage", &self.tier.usage())
            .finish_non_exhaustive()
    }
}

impl Staging {
    fn new(tier: Arc<dyn WriteTier>, id: TierId) -> Self {
        Self {
            tier,
            id,
            counters: TierCounters::default(),
            retry: Mutex::new(BTreeMap::new()),
        }
    }

    fn retries(&self) -> MutexGuard<'_, BTreeMap<ObjectId, Retry>> {
        self.retry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The objects whose next flush attempt is still in the future at `now`.
    pub(super) fn waiting(&self, now: Instant) -> BTreeSet<ObjectId> {
        let retries = self.retries();
        let waiting = retries.iter().filter(|(_, r)| r.next > now);
        waiting.map(|(object, _)| *object).collect()
    }

    /// Records a failed flush of `object` at `now`: the next attempt waits
    /// [`flush_backoff`] of the failures in a row.
    pub(super) fn failed(&self, object: ObjectId, now: Instant) -> Duration {
        let mut retries = self.retries();
        let failures = retries.get(&object).map_or(0, |r| r.failures) + 1;
        let wait = super::flush::flush_backoff(failures);
        let next = now + wait;
        retries.insert(object, Retry { failures, next });
        wait
    }

    /// Forgets `object`'s failures: it left the flusher's queue.
    pub(super) fn done(&self, object: ObjectId) {
        self.retries().remove(&object);
    }
}

/// What [`Cache::attach_tier`] found and did at this start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierStartup {
    /// The tier this server stages on now.
    pub tier: TierId,
    /// Whether this start registered it (`AddTier`): a directory used for the first
    /// time, or the one after `--hot-store-abandon`.
    pub registered: bool,
    /// The objects `--hot-store-abandon` marked unreachable as missing.
    pub abandoned: Vec<ObjectId>,
    /// What the disk layer's open found and did. Its `lost` objects are marked
    /// unreachable as missing by the time this is returned.
    pub recovery: Recovery,
}

/// Why [`Cache::attach_tier`] refused the start.
#[derive(Debug, thiserror::Error)]
pub enum TierStartError {
    /// The metadata log could not answer.
    #[error(transparent)]
    Cache(#[from] CacheError),
    /// The directory is not this server's tier, or the disk failed: a wrong mount or
    /// flag must not destroy the index.
    #[error(transparent)]
    Tier(#[from] TierError),
    /// `--hot-store-abandon` names a tier that is not this server's.
    #[error("--hot-store-abandon names {named}, but this server's tier is {}", current.map_or_else(|| "none".to_owned(), |t| t.to_string()))]
    AbandonOther {
        /// The tier the flag names.
        named: TierId,
        /// The server's registered tier.
        current: Option<TierId>,
    },
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Opens the tier directory `fs` as `server`'s write tier for this start
    /// (`docs/design/write-path.md`, section 6.4), and stages every later write on it.
    ///
    /// 1. With `abandon`, which must name `server`'s registered tier, every object
    ///    staged there is marked unreachable as missing (its blobs answer
    ///    `Unavailable` until re-uploaded), and a new tier is registered.
    /// 2. The tier is the registered one, or, for a server without one, a new id: this
    ///    start's writer epoch, which no tier was ever registered under.
    /// 3. The disk layer opens the directory against the committed staged table
    ///    ([`Tier::open`]): the `TIER` check (a wrong or missing `TIER` while objects
    ///    are staged refuses the start), each staged block cut to its committed length,
    ///    and the orphan sweep. A new tier's `TIER` is synced before its `AddTier`
    ///    commits.
    /// 4. Each staged object whose block is missing or short is marked unreachable as
    ///    missing, and logged as an error: only that object, not the tier.
    ///
    /// Unsealed recovered blocks are sealed, and sealed ones flushed, by
    /// [`Cache::flush_tier`].
    ///
    /// # Errors
    ///
    /// [`TierStartError`]; the cache is dropped and the server must not start.
    pub async fn attach_tier<F>(
        self,
        fs: F,
        options: TierOptions,
        server: ServerId,
        abandon: Option<TierId>,
    ) -> Result<(Self, TierStartup), TierStartError>
    where
        F: Fs + Send + Sync + fmt::Debug + 'static,
        F::File: Send + Sync + fmt::Debug + 'static,
    {
        let mut current = self
            .writer
            .query(move |s| s.tier(server))
            .await
            .map_err(CacheError::from)?;
        let mut abandoned = Vec::new();
        if let Some(named) = abandon {
            if current != Some(named) {
                return Err(TierStartError::AbandonOther { named, current });
            }
            abandoned = self
                .writer
                .query(move |s| {
                    let on_it = s.staged_objects().filter(|(_, o)| o.tier() == named);
                    on_it.map(|(object, _)| object).collect::<Vec<_>>()
                })
                .await
                .map_err(CacheError::from)?;
            for object in &abandoned {
                self.mark_unreachable(*object, UnreachableReason::Missing)
                    .await?;
            }
            tracing::warn!(%named, objects = abandoned.len(), "write tier abandoned");
            current = None;
        }
        let tier = current.unwrap_or_else(|| TierId::new(self.writer.epoch.get()));
        let snapshot = self
            .writer
            .query(StagedSnapshot::of)
            .await
            .map_err(CacheError::from)?;
        let start = TierStart {
            tier,
            epoch: self.writer.epoch,
            options,
        };
        let (opened, recovery) = Tier::open(fs, start, &snapshot, self.seq_source())?;
        if current.is_none() {
            let added = self.writer.commit(Command::AddTier { server, tier }).await;
            match added.map_err(CacheError::from)? {
                Applied::Tier(Ok(())) => {}
                other => return Err(unexpected(&other).into()),
            }
        }
        for object in &recovery.lost {
            tracing::error!(%object, %tier, "a staged block is missing or short; its blobs are unavailable until uploaded again");
            self.mark_unreachable(*object, UnreachableReason::Missing)
                .await?;
        }
        // A cache takes one tier: attached before it stores anything, and only once.
        let _ = self
            .writer
            .tier
            .set(Arc::new(Staging::new(Arc::new(opened), tier)));
        let startup = TierStartup {
            tier,
            registered: current.is_none(),
            abandoned,
            recovery,
        };
        Ok((self, startup))
    }

    /// The tier this cache stages on, if one is attached.
    pub fn tier_id(&self) -> Option<TierId> {
        self.writer.tier.get().map(|s| s.id)
    }

    /// The attached tier's disk layer, if one is attached: its usage and exposure.
    pub fn write_tier(&self) -> Option<&dyn WriteTier> {
        self.writer.tier.get().map(|s| &*s.tier)
    }

    /// Every metric family: [`CacheMetrics::render`](crate::metrics::CacheMetrics::render),
    /// then, with a tier attached, the tier's (its counters, its budget and usage, and
    /// the unflushed bytes and oldest unflushed block's age, measured now).
    #[must_use]
    pub fn render_metrics(&self) -> String {
        let mut out = self.writer.metrics.render();
        if let Some(staging) = self.writer.tier.get() {
            let usage = staging.tier.usage();
            let unflushed = staging.tier.unflushed(Instant::now());
            staging.counters.render(&mut out, usage, unflushed);
        }
        out
    }

    /// Numbers the tier's objects from this cache's sequence, so a staged object and a
    /// PUT one never share an id.
    fn seq_source(&self) -> NextSeq {
        let seq = Arc::clone(&self.writer.next_seq);
        Box::new(move || seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    /// `len` bytes of `object` from `offset` on this tier (a staged block's committed
    /// bytes, or a flushed copy), unchecked; `None` when there is no tier or it does
    /// not hold the whole range, and the caller reads the store.
    pub(super) async fn read_staged_range(
        &self,
        object: ObjectId,
        offset: u64,
        len: u64,
    ) -> Option<Bytes> {
        let staging = self.writer.tier.get()?;
        let tier = Arc::clone(&staging.tier);
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let read = blocking(
            self.metrics(),
            Box::new(move || tier.read(object, offset, len)),
        )
        .await;
        read.ok().map(|bytes| {
            staging.counters.local_reads.add(1);
            Bytes::from(bytes)
        })
    }
}

impl<M: MetaLog, O: ObjectStore> Writer<M, O> {
    /// Stages one finished segment on the write tier, if one is attached and takes it:
    /// its records written to a reserved range (the open grouped block, or, for
    /// `own`, a block of its own sealed by its commit), synced, then one `PutStaged`
    /// for every record. The footer is not written; the flusher builds it. Answers
    /// whether it did: without a tier, at the budget, or after a disk error before the
    /// commit, the caller PUTs the segment instead (backpressure, never loss).
    ///
    /// # Errors
    ///
    /// The `PutStaged` commit failed. The range is answered as failed unless the
    /// committed state shows the commit applied anyway (`answer_lost_put`).
    pub(super) async fn stage(&self, body: Bytes, own: bool) -> Result<bool, CacheError> {
        let Some(staging) = self.tier.get() else {
            return Ok(false);
        };
        let footer = Footer::parse(&body, body.len() as u64)?;
        let records = body.slice(..usize::try_from(footer.records_len()).unwrap_or(body.len()));
        let len = records.len() as u64;
        let tier = Arc::clone(&staging.tier);
        let synced = blocking(
            &self.metrics,
            Box::new(move || {
                let reservation = if own {
                    tier.reserve_upload(len)?
                } else {
                    tier.reserve_group(len, Instant::now())?
                };
                tier.write(&reservation, 0, &records)?;
                tier.sync(reservation).map_err(|unsynced| unsynced.error)
            }),
        )
        .await;
        let synced = match synced {
            Ok(synced) => synced,
            Err(e) => {
                staging.counters.direct(&e);
                tracing::debug!(error = %e, len, "the write tier did not take a segment; writing it to the store");
                return Ok(false);
            }
        };
        let at = |e: &IndexEntry| (e.digest, synced.offset() + e.offset);
        let blobs = footer.entries().iter().map(at).collect();
        self.put_staged(staging, synced, blobs, own).await?;
        Ok(true)
    }

    /// Commits one `PutStaged` for `blobs` (each at its offset in the block) in the
    /// synced range `synced`, sealing its block with the commit when `seal` (a block
    /// of its own), and answers the range committed.
    ///
    /// # Errors
    ///
    /// The commit failed. The range is answered as failed unless the committed state
    /// shows the commit applied anyway (`answer_lost_put`).
    pub(super) async fn put_staged(
        &self,
        staging: &Staging,
        synced: Synced,
        blobs: Vec<(Digest, u64)>,
        seal: bool,
    ) -> Result<(), CacheError> {
        let put = StagedPut {
            object: synced.object(),
            end: synced.end(),
            seal,
            blobs,
        };
        let command = Command::PutStaged {
            tier: staging.id,
            objects: vec![put],
        };
        let applied = match self.commit(command).await {
            Ok(applied) => applied,
            Err(e) => {
                self.answer_lost_put(staging, synced).await;
                return Err(e.into());
            }
        };
        match applied {
            Applied::Staged(Ok(_)) => {
                staging.tier.committed(synced);
                staging.counters.staged.add(1);
                Ok(())
            }
            other => Err(unexpected(&other)),
        }
    }

    /// Answers a range whose `PutStaged` commit failed. A log can apply a command and
    /// still fail to answer it (a Raft log that stopped after the entry was durable), so
    /// the committed state is asked: if it holds the object through the range's end,
    /// the range is answered committed, and the tier's committed length follows the
    /// index (its blobs are read from the block, and the block is sealed and flushed,
    /// not removed). Otherwise, or if the state cannot be read, `synced` is dropped:
    /// the range is answered as failed, and a commit that did apply is recovered
    /// against the committed state at the next start.
    async fn answer_lost_put(&self, staging: &Staging, synced: Synced) {
        let (object, end) = (synced.object(), synced.end());
        let held = self
            .query(move |s| s.staged(object).is_some_and(|o| o.committed_len() >= end))
            .await;
        if held == Ok(true) {
            staging.tier.committed(synced);
        }
    }
}

/// Runs a tier call on the blocking pool: the tier's file I/O (creates, writes,
/// `fdatasync`, reads of up to a segment) must not hold a runtime thread. A call that
/// panicked is a failed call. The call is counted while it is queued or running
/// (`kbf_tier_in_flight`), and its wait for a pool thread and its whole time are
/// observed, so a pool that is full shows as wait, not as a slow disk.
pub(super) async fn blocking<T: Send + 'static>(
    metrics: &CacheMetrics,
    call: Box<dyn FnOnce() -> Result<T, TierError> + Send>,
) -> Result<T, TierError> {
    let _in_flight = metrics.tier_in_flight.raise();
    let handed = Instant::now();
    let joined = tokio::task::spawn_blocking(move || {
        let waited = handed.elapsed();
        (waited, call())
    })
    .await;
    metrics.tier_call.since(handed);
    match joined {
        Ok((waited, answer)) => {
            metrics.tier_wait.observe_duration(waited);
            answer
        }
        Err(e) => Err(TierError::Io {
            op: "run",
            name: "a write tier call".to_owned(),
            source: std::io::Error::other(e.to_string()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a tier call that panicked on the blocking pool taken for a success, or
    /// tearing down the caller, instead of failing that one call.
    #[tokio::test]
    async fn a_tier_call_that_panics_is_a_failed_call() {
        let metrics = CacheMetrics::default();
        let ok = blocking::<u8>(&metrics, Box::new(|| Ok(7))).await;
        assert!(matches!(ok, Ok(7)), "{ok:?}");
        let panicked = blocking::<u8>(&metrics, Box::new(|| panic!("a tier call panicked"))).await;
        assert!(
            matches!(panicked, Err(TierError::Io { op: "run", .. })),
            "{panicked:?}"
        );
    }

    /// Catches: a blocking pool that is full showing as a slow disk, or not at all: the
    /// second call waits for the only pool thread while the first holds it, so it counts
    /// as in flight and its wait is the time the first took (300 ms here, observed by
    /// the second's `kbf_tier_wait_seconds`, not 0), and the gauge is back to 0 after.
    /// A wait taken from the closure's end instead of its start, or a gauge raised
    /// inside the closure (it would read 1, not 2, while one call waits), is caught.
    #[test]
    fn a_call_waiting_for_a_pool_thread_is_counted_and_its_wait_observed() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let metrics = Arc::new(CacheMetrics::default());
        runtime.block_on(async {
            let first = {
                let metrics = Arc::clone(&metrics);
                tokio::spawn(async move {
                    blocking(
                        &metrics,
                        Box::new(|| {
                            std::thread::sleep(Duration::from_millis(300));
                            Ok(1_u8)
                        }),
                    )
                    .await
                })
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            let second = {
                let metrics = Arc::clone(&metrics);
                tokio::spawn(async move { blocking(&metrics, Box::new(|| Ok(2_u8))).await })
            };
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(metrics.tier_in_flight.get(), 2, "one running, one queued");
            assert!(matches!(first.await.expect("joined"), Ok(1)));
            assert!(matches!(second.await.expect("joined"), Ok(2)));
        });
        assert_eq!(metrics.tier_in_flight.get(), 0);
        assert_eq!(metrics.tier_wait.count(), 2);
        assert_eq!(metrics.tier_call.count(), 2);
        // The second waited about 200 ms (300 ms less the 100 ms head start); the first
        // did not wait.
        let waited = Duration::from_nanos(metrics.tier_wait.sum());
        assert!(
            (Duration::from_millis(150)..Duration::from_millis(280)).contains(&waited),
            "{waited:?}"
        );
    }
}
