//! One flusher (`docs/design/architecture-v2.md`, section 8): of the two owners of a
//! digest, only the primary (rank 1 of [`kbf_map::Map::write_owners`]) PUTs it to the
//! cold store. Otherwise both write the same blobs into different objects and the PUT
//! bytes and the store's cost double.
//!
//! [`OneFlusher`] is the node's answer to "do I flush this digest": a [`FlushGate`] the
//! cache asks before it flushes a staged object. It is also the node's [`MapSource`]
//! (it wraps the real one), because that is where a node learns it is behind.
//!
//! - **Rank 1 under the map the node holds.** A `Down` primary keeps its slot in the map
//!   for the dwell, so its replica holds; when the member leaves the map, the replica is
//!   rank 1 under the new map and flushes what the primary never did. The blob is not
//!   lost: the replica's hot copy is still staged.
//! - **Fail closed.** A node with no map, or that does not know its own member id, flushes
//!   nothing: a late flush costs hot-store room, a wrong one costs a second PUT.
//! - **A stale map does not make a second flusher.** A caller routed by a generation the
//!   node has not been told of makes the node fetch it ([`crate::replicate`]); that
//!   request names a generation, and until the node holds a map at least that new it
//!   does not flush.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::BoxFuture;
use kbf_map::{Map, MemberId};
use kbf_types::Digest;

use crate::replicate::MapSource;

/// Whether this node flushes a digest to the cold store.
pub trait FlushGate: Send + Sync + fmt::Debug {
    /// Whether this node is the one that flushes `digest` now.
    fn flushes(&self, digest: &Digest) -> bool;
}

/// The gate of a node that is a member of a farm: it flushes the digests it is rank 1 for.
pub struct OneFlusher {
    source: Arc<dyn MapSource>,
    /// The newest generation any caller has routed by, as far as the node was told.
    newest: AtomicU64,
}

impl fmt::Debug for OneFlusher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OneFlusher")
            .field("newest", &self.newest.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl OneFlusher {
    /// The gate over the node's map source. Give the node this, as its [`MapSource`],
    /// in place of `source`.
    #[must_use]
    pub fn new(source: Arc<dyn MapSource>) -> Arc<Self> {
        Arc::new(Self {
            source,
            newest: AtomicU64::new(0),
        })
    }
}

impl FlushGate for OneFlusher {
    fn flushes(&self, digest: &Digest) -> bool {
        let (Some(map), Some(me)) = (self.source.current(), self.source.me()) else {
            return false;
        };
        map.generation() >= self.newest.load(Ordering::SeqCst) && primary(&map, digest) == Some(me)
    }
}

/// The rank-1 owner of `digest` under `map`.
fn primary(map: &Map, digest: &Digest) -> Option<MemberId> {
    map.write_owners(digest).first().copied()
}

impl MapSource for OneFlusher {
    fn current(&self) -> Option<Arc<Map>> {
        self.source.current()
    }

    fn me(&self) -> Option<MemberId> {
        self.source.me()
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Option<Arc<Map>>> {
        self.newest.fetch_max(at_least, Ordering::SeqCst);
        self.source.refetch(at_least)
    }
}

#[cfg(test)]
mod tests;
