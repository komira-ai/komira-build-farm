//! A registered storage node's rebuild of its index (`docs/design/architecture-v2.md`,
//! section 8): [`kbf_front::Cache::rebuild_index`] run in the background until it
//! completes, while the node serves as a catching-up one ([`CatchUp`]): it registers as
//! `CatchingUp`, answers a miss UNAVAILABLE rather than NOT_FOUND, and becomes `Active`
//! at the first heartbeat after the rebuild is done.
//!
//! The start-up pass over a `file://` cold store, and the evictor after it, wait for the
//! rebuild: a pass over a half-built index would forget the objects the rebuild had not
//! yet adopted ([`spawn`]).

use std::sync::Arc;

use kbf_front::{Cache, MetaLog, RebuildConfig};
use kbf_objstore::ObjectStore;
use kbf_storage::service::CatchUp;

use crate::join::{RETRY_BASE, backoff};
use crate::start::AbortOnDrop;
use crate::startup_pass::{self, StartPass};

/// The node's background tasks: the start-up pass and evictor, and the rebuild.
pub(crate) struct Background {
    _pass: Option<AbortOnDrop>,
    _rebuild: Option<AbortOnDrop>,
}

/// Runs the rebuild of `cache`'s index until it completes, then lifts `catch_up`. A
/// failed attempt is logged and tried again after [`backoff`], from the key the log holds:
/// the node stays catching up, and serves what it indexes, meanwhile.
pub(crate) async fn run<M: MetaLog, O: ObjectStore>(
    cache: Arc<Cache<M, O>>,
    catch_up: CatchUp,
    config: RebuildConfig,
) {
    let mut failures = 0_u32;
    loop {
        match cache.rebuild_index(config).await {
            Ok(report) => {
                tracing::info!(
                    resumed_after = report.resumed_after.as_deref().unwrap_or(""),
                    objects = report.objects,
                    segments = report.segments,
                    inserted = report.inserted,
                    kept = report.kept,
                    own = report.own,
                    skipped = report.skipped,
                    foreign = report.foreign,
                    gone = report.gone,
                    duration_ms = report.duration.as_millis() as u64,
                    "rebuild of the index from the store done"
                );
                catch_up.finish();
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "rebuild of the index from the store failed; trying again");
                tokio::time::sleep(backoff(RETRY_BASE, failures)).await;
                failures = failures.saturating_add(1);
            }
        }
    }
}

/// Starts what runs beside the node's serving: the rebuild, if `catch_up` says one is
/// owed, read and committed as `config` says, and the start-up pass with its evictor, if
/// there is one, started only once the rebuild is done.
pub(crate) fn spawn<M: MetaLog, O: ObjectStore + 'static>(
    cache: &Arc<Cache<M, O>>,
    catch_up: &CatchUp,
    pass: Option<StartPass<M>>,
    config: RebuildConfig,
) -> Background {
    let rebuild = catch_up
        .is_catching_up()
        .then(|| run(Arc::clone(cache), catch_up.clone(), config));
    let spawn_pass = |pass| AbortOnDrop(startup_pass::spawn(Arc::clone(cache), pass));
    match (pass, rebuild) {
        (Some(pass), Some(rebuild)) => Background {
            _pass: Some(spawn_pass(pass.held(rebuild))),
            _rebuild: None,
        },
        (pass, rebuild) => Background {
            _pass: pass.map(spawn_pass),
            _rebuild: rebuild.map(|rebuild| AbortOnDrop(tokio::spawn(rebuild))),
        },
    }
}
