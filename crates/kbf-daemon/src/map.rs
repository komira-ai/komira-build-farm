//! The storage map on the session (`docs/design/architecture-v2.md`, section 6): the map
//! of `Welcome` and of each `MapUpdate` goes into a [`MapHolder`] the routing layer reads,
//! and a generation the holder is waiting for ([`MapHolder::reach`]) is asked of the
//! frontend in a `MapRequest`.
//!
//! A map the daemon cannot read (a value this build does not name, or one the domain
//! refuses) is logged and not taken: the daemon goes on with the map it has, and a
//! frontend that sends such a map has a bug a farm of daemons should not each fail on.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_map::Map;
use kbf_mapsync::MapHolder;
use kbf_proto::kbf::map::v1 as wire;
use kbf_proto::worker::MapRequest;
use kbf_route::{MapView, RefetchError};
use tokio::sync::watch;

/// The daemon's end of the map: where pushed maps go, and what the holder asks for.
pub(crate) struct MapLink {
    holder: Arc<MapHolder>,
    asks: watch::Receiver<u64>,
    /// The generation last asked for on this stream; 0 before any.
    sent: u64,
}

impl MapLink {
    pub(crate) fn new(holder: Arc<MapHolder>) -> Self {
        let asks = holder.asks();
        Self {
            holder,
            asks,
            sent: 0,
        }
    }

    /// Takes the map a `Welcome` or a `MapUpdate` carried. A holder keeps the newest, so
    /// a late or repeated map changes nothing; `None` (a server with no map) changes
    /// nothing either.
    pub(crate) fn take(&self, map: Option<wire::Map>) {
        let Some(map) = map else { return };
        match Map::try_from(map) {
            Ok(map) => {
                let generation = map.generation();
                if self.holder.offer(map) {
                    tracing::info!(generation, "storage map taken");
                }
            }
            Err(error) => tracing::warn!(%error, "a storage map the daemon cannot read ignored"),
        }
    }

    /// A new stream: what the holder waits for is asked for again on it.
    pub(crate) fn new_stream(&mut self) {
        self.sent = 0;
    }

    /// The request to send now, if the holder waits for a generation it does not have and
    /// this stream has not asked for it. Marks the wait seen, so [`Self::asked`]
    /// completes again only for a new one.
    pub(crate) fn request(&mut self) -> Option<MapRequest> {
        let at_least = *self.asks.borrow_and_update();
        let wanted = at_least > self.holder.generation() && at_least > self.sent;
        wanted.then(|| {
            self.sent = at_least;
            MapRequest { at_least }
        })
    }

    /// Completes when the holder is asked to reach a generation it was not before.
    async fn asked(&mut self) {
        // The link holds the holder, whose sender lives as long as it does: the wait
        // cannot fail.
        let _ = self.asks.changed().await;
    }
}

/// Completes when `link` has a new wait; never without a link. (A plain future rather
/// than a `Some(()) =` pattern in the select: a pattern arm adds a path nothing runs.)
pub(crate) async fn map_asked(link: Option<&mut MapLink>) {
    match link {
        Some(link) => link.asked().await,
        None => std::future::pending().await,
    }
}

/// How long a call that found itself behind waits for the frontend's answer to its
/// `MapRequest`: a bound against a frontend that never answers, not a schedule.
pub const REFETCH_WITHIN: Duration = Duration::from_secs(10);

/// The daemon's map holder as the routing library's map source: the seam between the
/// two `MapSource` traits (`kbf_mapsync`'s is the frontend's end, which holds a map and
/// announces changes; `kbf_route`'s is a client's, which asks for the map to route by and
/// for a newer one).
///
/// `current` is what the session pushed. `refetch` is [`MapHolder::reach`]: it records the
/// generation as wanted, which the session turns into a `MapRequest`
/// ([`MapLink::request`]), and waits for a map that new, so a push that was lost costs a
/// round trip and no more.
pub struct HeldMaps {
    holder: Arc<MapHolder>,
    within: Duration,
}

impl HeldMaps {
    /// The holder's maps, a refetch waiting at most `within`.
    #[must_use]
    pub fn new(holder: Arc<MapHolder>, within: Duration) -> Self {
        Self { holder, within }
    }

    /// `map` with the generation it replaced, if the holder still has it. The holder is
    /// read twice and may move between the reads: a previous map that is not older than
    /// `map` is the same map, and is left out.
    fn view(&self, map: Arc<Map>) -> MapView {
        let previous = self
            .holder
            .previous()
            .filter(|previous| previous.generation() < map.generation());
        MapView { map, previous }
    }
}

impl kbf_route::MapSource for HeldMaps {
    fn current(&self) -> Option<MapView> {
        self.holder.current().map(|map| self.view(map))
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            let reached = tokio::time::timeout(self.within, self.holder.reach(at_least)).await;
            match reached {
                Ok(map) => Ok(self.view(map)),
                Err(_) => Err(RefetchError {
                    at_least,
                    reason: format!(
                        "the frontend sent no map of that generation within {:?}",
                        self.within
                    ),
                }),
            }
        })
    }
}
