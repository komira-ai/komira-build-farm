//! The storage map the frontend routes its own storage calls by: the members' map
//! ([`Members`]) as the routing library's map source (`docs/design/architecture-v2.md`,
//! section 7). Daemons are sent the same map over their sessions; this is the frontend
//! holding it for itself.

use std::sync::Arc;

use futures::future::BoxFuture;
use kbf_route::{MapSource, MapView, RefetchError};

use crate::members::Members;

/// [`Members`] as a [`MapSource`].
///
/// `current` is the newest map this process has seen and the generation before it
/// ([`Members::view`]); no store is read. `refetch` is what a `STALE_MAP` answer from a
/// storage node asks for: it re-reads the map from the state store, which brings in a bump
/// another frontend committed, and answers the view if that holds a map of the generation
/// wanted. It does not rebuild the map ([`Members::refresh`] does, on the frontend's
/// timer), so a node that is ahead of every frontend's store is a failed fetch, and the
/// router then answers UNAVAILABLE.
pub struct FrontMaps {
    members: Arc<Members>,
}

impl FrontMaps {
    /// The map of `members`.
    #[must_use]
    pub fn new(members: Arc<Members>) -> Self {
        Self { members }
    }
}

impl MapSource for FrontMaps {
    fn current(&self) -> Option<MapView> {
        self.members
            .view()
            .map(|(map, previous)| MapView { map, previous })
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            let failed = |reason: String| RefetchError { at_least, reason };
            // The store's calls block (its trait says so).
            let members = Arc::clone(&self.members);
            tokio::task::spawn_blocking(move || members.current_map())
                .await
                .map_err(|e| failed(format!("the map read did not finish: {e}")))?
                .map_err(|e| failed(e.to_string()))?;
            self.current()
                .filter(|view| view.map.generation() >= at_least)
                .ok_or_else(|| failed("the state store holds no map that new".to_owned()))
        })
    }
}
