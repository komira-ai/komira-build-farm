//! The local write tier in the server (`docs/design/write-path.md`, section 6): the
//! start that attaches the hot store to the cache over the Raft metadata log, and
//! the background task that seals and flushes its blocks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_front::{Cache, FlushRound, MetaLog, MetaLogError, TierStartError, TierStartup};
use kbf_log::MachineVersion;
use kbf_objstore::ObjectStore;
use kbf_store::{Fs, StdFs};
use kbf_tier::TierOptions;

use crate::RaftMetaLog;
use crate::config::SERVER;
use crate::stores::Hot;

/// How often the flusher runs a round ([`Cache::flush_tier`]).
pub const FLUSH_EVERY: Duration = Duration::from_secs(1);

/// Why the write tier could not be attached; the server does not start.
#[derive(Debug, thiserror::Error)]
pub enum WriteTierError {
    /// The metadata log could not be raised to machine version 2, which carries the
    /// tier's commands.
    #[error("--hot-store: {0}")]
    Log(MetaLogError),
    /// The directory could not be created.
    #[error("--hot-store: {0}")]
    Dir(std::io::Error),
    /// The start was refused (a wrong or missing `TIER` while objects are staged, an
    /// abandon flag naming another tier, a disk or log failure).
    #[error("--hot-store: {0}")]
    Start(#[from] TierStartError),
}

/// `cache` with the hot store's tier attached, and what its start found; the cache as it
/// was when there is no hot store.
///
/// With a tier, the Raft log is first raised to machine version 2, whose format carries
/// the tier's commands ([`RaftMetaLog::raise_machine_version`]; a log already there
/// commits nothing; from then on a binary that runs only version 1 refuses the log), then
/// the tier's start recovers the directory against the committed staged table, which
/// outlives the process ([`Cache::attach_tier`]). Without a tier the log is not touched,
/// so it stays one an older binary reads.
///
/// # Errors
/// [`WriteTierError`].
pub async fn attach<F, O>(
    cache: Cache<RaftMetaLog<F>, O>,
    hot: Option<&Hot>,
) -> Result<(Cache<RaftMetaLog<F>, O>, Option<TierStartup>), WriteTierError>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
    O: ObjectStore,
{
    attach_with(cache, hot, true).await
}

/// [`attach`], the tier keeping flushed copies or not ([`TierOptions::flushed_copies`]):
/// not on a node whose one cache keeps its local copies (`kbf_storage::local_cache`).
///
/// # Errors
/// [`WriteTierError`].
pub async fn attach_with<F, O>(
    cache: Cache<RaftMetaLog<F>, O>,
    hot: Option<&Hot>,
    flushed_copies: bool,
) -> Result<(Cache<RaftMetaLog<F>, O>, Option<TierStartup>), WriteTierError>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
    O: ObjectStore,
{
    let Some(hot) = hot else {
        return Ok((cache, None));
    };
    let fs = StdFs::new(&hot.dir).map_err(WriteTierError::Dir)?;
    let options = TierOptions {
        flushed_copies,
        ..TierOptions::new(hot.budget)
    };
    cache
        .meta()
        .raise_machine_version(MachineVersion::V2)
        .map_err(WriteTierError::Log)?;
    let (cache, startup) = cache.attach_tier(fs, options, SERVER, hot.abandon).await?;
    tracing::info!(
        tier = %startup.tier,
        registered = startup.registered,
        abandoned = startup.abandoned.len(),
        lost = startup.recovery.lost.len(),
        orphans = startup.recovery.removed.len(),
        unsealed = startup.recovery.unsealed.len(),
        sealed = startup.recovery.sealed.len(),
        "write tier attached"
    );
    Ok((cache, Some(startup)))
}

/// Runs a flusher round every `every`, for as long as the task lives. A round that
/// fails (the metadata log unavailable) is logged and the next one tries again.
pub async fn run_flusher<M: MetaLog, O: ObjectStore>(cache: Arc<Cache<M, O>>, every: Duration) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match cache.flush_tier(Instant::now()).await {
            Ok(round) => report_round(&round),
            Err(error) => tracing::warn!(%error, "write tier flusher round failed"),
        }
    }
}

/// Logs, as one WARN line, the objects a flusher round found that need an operator: a
/// flush whose key holds another object (`conflicts`), and objects with a record that
/// failed its digest (`corrupt`, with `dropped_blobs` such records). A round that only
/// sealed and flushed logs nothing (that is every second of a busy server), and a
/// flush that failed is logged where it fails, with its retry wait, by `kbf-front`.
pub fn report_round(round: &FlushRound) {
    if round.conflicts.len() + round.corrupt.len() == 0 {
        return;
    }
    tracing::warn!(
        conflicts = round.conflicts.len(),
        corrupt = round.corrupt.len(),
        dropped_blobs = round.dropped.len(),
        "write tier flush found objects that conflict with the store or fail their digests"
    );
}
