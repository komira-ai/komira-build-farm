//! The map a storage node holds, as its storage service reads it.
//!
//! A node learns the farm's map from its frontend: the registration's answer and every
//! heartbeat that finds it behind put the map in a [`MapHolder`] ([`crate::join`]). The
//! storage service and its chain ([`kbf_storage::replicate`]) read it through
//! [`kbf_storage::replicate::MapSource`]; this is the one implementation of that trait for
//! a node, over that holder, so the map the service judges calls by is the map the node
//! holds and nothing else. It adds what the holder does not know: which member the node
//! is, which only the registration tells.

use std::sync::{Arc, OnceLock};

use futures::future::BoxFuture;
use kbf_map::{Map, MemberId};
use kbf_mapsync::MapHolder;
use kbf_storage::replicate::MapSource;

/// The map a storage node holds, and which member of it the node is.
#[derive(Debug)]
pub struct NodeMap {
    maps: Arc<MapHolder>,
    me: OnceLock<MemberId>,
}

impl NodeMap {
    /// A node that holds what `maps` holds and has not registered.
    #[must_use]
    pub fn new(maps: Arc<MapHolder>) -> Self {
        Self {
            maps,
            me: OnceLock::new(),
        }
    }

    /// The holder the frontend's maps are put in.
    #[must_use]
    pub fn maps(&self) -> Arc<MapHolder> {
        Arc::clone(&self.maps)
    }

    /// The member id a registration gave the node. The state store keeps one id per
    /// instance, so a later registration (after the row lapsed) names the same member and
    /// changes nothing.
    pub(crate) fn registered(&self, id: MemberId) {
        let _ = self.me.set(id);
    }
}

impl MapSource for NodeMap {
    fn current(&self) -> Option<Arc<Map>> {
        self.maps.current()
    }

    fn me(&self) -> Option<MemberId> {
        self.me.get().copied()
    }

    /// [`MapHolder::reach`]: records the generation as wanted and waits for a map that new.
    /// The caller bounds the wait.
    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Option<Arc<Map>>> {
        Box::pin(async move { Some(self.maps.reach(at_least).await) })
    }
}
