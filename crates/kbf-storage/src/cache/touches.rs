//! Touches recorded by reads, committed in batches off the reply path.
//!
//! A read that reports or serves an entry whose last touch is at least one touch
//! quantum old must touch it (`docs/design/storage.md#retention-and-touches`). Reads
//! do not commit that touch themselves: they record it here and answer. A background
//! task (the flusher, spawned by the first read that records anything) commits what is
//! pending as one [`Command::Touch`] of at most [`TouchConfig::batch`] entries, every
//! [`TouchConfig::every`], or as soon as a batch is full. A digest recorded twice before
//! a flush is committed once.
//!
//! **Pending touches pin their entries.** [`Cache::collect`] commits every pending
//! touch before its `Collect`, and holds the gate a read records under, so no read can
//! report an entry between that flush and the collection without the collection
//! seeing its touch. An entry reported present therefore stays held for at least
//! `min_ttl` after the report, within the life of the process. The flusher holds the
//! gate shared while it commits, so a collection cannot run between a batch leaving
//! the pending set and its commit. A stop (crash or graceful shutdown) loses the touches
//! still pending: one interval's, or more while commits keep failing; an entry whose
//! touch is lost keeps its last committed one. No collector runs in production yet, so
//! that costs nothing today.

use std::collections::BTreeSet;
use std::mem;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use kbf_meta::{Applied, Command, Touch};
use kbf_objstore::ObjectStore;
use kbf_types::Digest;
use tokio::sync::{Notify, RwLock};
use tokio::task::AbortHandle;

use super::write_group::Writer;
use super::{CacheError, unexpected};
use crate::meta_log::MetaLog;

/// How often, and in what batches, recorded touches are committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchConfig {
    /// The longest a recorded touch waits for its commit, unless a commit fails.
    pub every: Duration,
    /// The most entries (blobs and actions together) in one `Touch` commit. A batch
    /// this full is committed without waiting for [`TouchConfig::every`].
    pub batch: NonZeroUsize,
}

impl TouchConfig {
    /// One second, as often as the replicated log commits farm time; 4096 entries,
    /// about 180 KB of log entry.
    pub const DEFAULT: Self = Self {
        every: Duration::from_secs(1),
        batch: NonZeroUsize::MIN.saturating_add(4095),
    };
}

/// The touches recorded and not yet committed, the gate reads record under, and the
/// flusher.
#[derive(Debug)]
pub(super) struct Touches {
    config: TouchConfig,
    pending: Mutex<Touch>,
    /// Entries taken from `pending` whose commit has not answered yet.
    committing: AtomicUsize,
    /// Woken when a batch is full.
    full: Notify,
    /// Shared by a read (its query and its record) and by the flusher's commit;
    /// exclusive for a collection, which flushes and collects under it.
    gate: RwLock<()>,
    flusher: OnceLock<AbortHandle>,
}

impl Touches {
    pub(super) fn new(config: TouchConfig) -> Self {
        Self {
            config,
            pending: Mutex::new(Touch::default()),
            committing: AtomicUsize::new(0),
            full: Notify::new(),
            gate: RwLock::new(()),
            flusher: OnceLock::new(),
        }
    }

    pub(super) const fn gate(&self) -> &RwLock<()> {
        &self.gate
    }

    fn lock(&self) -> MutexGuard<'_, Touch> {
        // Only set inserts and removals run under the lock; a poisoned set is whole.
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many entries wait for their commit, those being committed included.
    pub(super) fn pending(&self) -> usize {
        let pending = self.lock();
        pending.blobs.len() + pending.actions.len() + self.committing.load(Ordering::SeqCst)
    }

    /// Adds `touch` to what the next flush commits; wakes the flusher when a batch is
    /// full. The caller holds the gate shared.
    pub(super) fn record(&self, touch: Touch) {
        let full = {
            let mut pending = self.lock();
            pending.blobs.extend(touch.blobs);
            pending.actions.extend(touch.actions);
            pending.blobs.len() + pending.actions.len() >= self.config.batch.get()
        };
        if full {
            self.full.notify_one();
        }
    }

    /// Up to one batch of what is pending, moved to `committing` until
    /// [`Touches::committed`] or [`Touches::restore`].
    fn take_batch(&self) -> Touch {
        let limit = self.config.batch.get();
        let mut pending = self.lock();
        let batch = if pending.blobs.len() + pending.actions.len() <= limit {
            mem::take(&mut *pending)
        } else {
            let blobs = take_first(&mut pending.blobs, limit);
            let actions = take_first(&mut pending.actions, limit - blobs.len());
            Touch { blobs, actions }
        };
        self.committing.fetch_add(len(&batch), Ordering::SeqCst);
        batch
    }

    /// A batch's commit applied.
    fn committed(&self, batch: &Touch) {
        self.committing.fetch_sub(len(batch), Ordering::SeqCst);
    }

    /// Puts back a batch whose commit failed, so a later flush commits it.
    fn restore(&self, batch: Touch) {
        let mut pending = self.lock();
        self.committing.fetch_sub(len(&batch), Ordering::SeqCst);
        pending.blobs.extend(batch.blobs);
        pending.actions.extend(batch.actions);
    }

    /// Spawns the flusher on the current Tokio runtime, once. Outside a runtime it
    /// spawns nothing; [`Cache::collect`](super::Cache::collect) still commits what is
    /// pending before it collects.
    pub(super) fn start<M, O>(self: &Arc<Self>, writer: &Arc<Writer<M, O>>)
    where
        M: MetaLog,
        O: ObjectStore,
    {
        if self.flusher.get().is_some() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (touches, writer) = (Arc::clone(self), Arc::clone(writer));
        self.flusher
            .get_or_init(|| runtime.spawn(flush_every(touches, writer)).abort_handle());
    }

    /// Stops the flusher. Pending touches are dropped with the cache.
    pub(super) fn stop(&self) {
        if let Some(flusher) = self.flusher.get() {
            flusher.abort();
        }
    }
}

fn len(touch: &Touch) -> usize {
    touch.blobs.len() + touch.actions.len()
}

/// The first `n` digests of `set` (all of them if it holds fewer), removed from it.
fn take_first(set: &mut BTreeSet<Digest>, n: usize) -> BTreeSet<Digest> {
    let first_kept = set.iter().nth(n).copied();
    let rest = match first_kept {
        Some(d) => set.split_off(&d),
        None => BTreeSet::new(),
    };
    mem::replace(set, rest)
}

/// Commits every pending touch, a batch at a time. The caller holds the gate (shared
/// for the flusher, exclusive for a collection). A batch whose commit fails is put
/// back, and the error returned.
///
/// # Errors
/// The metadata log is unavailable, or answered with another command's outcome.
pub(super) async fn flush<M: MetaLog, O: ObjectStore>(
    touches: &Touches,
    writer: &Writer<M, O>,
) -> Result<(), CacheError> {
    loop {
        let batch = touches.take_batch();
        if batch.is_empty() {
            return Ok(());
        }
        let failed = match writer.commit(Command::Touch(batch.clone())).await {
            Ok(Applied::Touched { lost }) => {
                touches.committed(&batch);
                if !lost.is_empty() {
                    // Only a collection removes entries, and it flushes first under
                    // the gate; a lost entry means something else committed `Collect`.
                    tracing::error!(?lost, "a recorded touch found its entry gone");
                }
                // Lets other tasks on this worker run between batches.
                tokio::task::yield_now().await;
                continue;
            }
            Ok(other) => unexpected(&other),
            Err(e) => e.into(),
        };
        touches.restore(batch);
        return Err(failed);
    }
}

/// The flusher: commits what is pending every [`TouchConfig::every`], or when a batch
/// is full. A failed commit is logged and retried at the next wake.
async fn flush_every<M: MetaLog, O: ObjectStore>(touches: Arc<Touches>, writer: Arc<Writer<M, O>>) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(touches.config.every) => {}
            () = touches.full.notified() => {}
        }
        let _shared = touches.gate.read().await;
        if let Err(e) = flush(&touches, &writer).await {
            tracing::warn!(error = %e, pending = touches.pending(), "touch commit failed");
        }
    }
}
