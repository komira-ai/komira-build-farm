//! Where a process learns the storage map: the seam av2-08 fills (the daemon's holder
//! of the pushed map, and `kbf-server`'s `FrontMaps` over the members).
//!
//! A node holds the newest map it has been sent (`Welcome.map`, then each `MapUpdate`)
//! and the one before it while a change settles. The router asks for them with
//! [`MapSource::current`] on every call, and for a newer one with [`MapSource::refetch`]
//! when a storage node answers `STALE_MAP`.

use std::sync::Arc;

use futures::future::BoxFuture;
use kbf_map::Map;

/// The map a call routes by, and the generation before it.
#[derive(Clone, Debug)]
pub struct MapView {
    /// The newest map the process has.
    pub map: Arc<Map>,
    /// The generation before it, while a join or a drain is settling: a blob written
    /// before the change is found where it was. `None` once the change has settled.
    pub previous: Option<Arc<Map>>,
}

/// Why a newer map could not be fetched.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("could not fetch map generation {at_least}: {reason}")]
pub struct RefetchError {
    /// The generation that was needed.
    pub at_least: u64,
    /// What went wrong, in words for a log.
    pub reason: String,
}

/// A process's view of the storage map.
///
/// The frontend implements it over its state store, a daemon over the stream it holds to
/// a frontend; a test implements it over a cell it sets.
pub trait MapSource: Send + Sync {
    /// The map the process has now, or `None` before the first one arrived: nothing can
    /// be routed until a map has.
    fn current(&self) -> Option<MapView>;

    /// A map of generation `at_least` or newer, fetched from a frontend, and the view
    /// after it. Called when a storage node answers `STALE_MAP`: a push that was lost
    /// is harmless, because the next call that reaches a node that has seen the newer
    /// map lands here.
    ///
    /// # Errors
    /// No frontend could be reached or none had a map that new.
    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>>;
}
