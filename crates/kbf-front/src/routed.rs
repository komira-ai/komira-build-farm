//! The frontend's storage calls routed by the storage map (`docs/design/architecture-v2.md`,
//! sections 6 to 8): one [`Router`] over the frontend's own map of the members, which the
//! proxy ([`crate::Owners::routed`]) and the backend Execute and the scheduler use
//! ([`crate::Backend::routed`]) share, so a blob is written and read where the daemons
//! put it.
//!
//! **When it routes.** [`Routed::active`] is true once the map has had two members or
//! more, and stays true. Before the first map, and for a farm that has only ever had one
//! member, the frontend uses the node `--storage` names, as a frontend without members
//! always has: a one-member map is that same node, and its advertised address need not be
//! the URL the frontend was given. A farm that has had two members and shrinks to one is
//! not that case (`--storage` may be a node that left), so it is routed by its map still.
//! Blobs written to `--storage` before the map first has two members are not moved when
//! the map places them elsewhere.
//!
//! **What routing gives.** The rules of `kbf-route`: every call is tagged with the
//! generation it was routed by; a node that holds a newer map answers `STALE_MAP`, the map
//! is fetched once and the call run again, and a second answer is UNAVAILABLE; a blob read
//! whole is hashed against its digest at every owner; a read fails over to the next owner;
//! a write goes to the first owner that is not down.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_map::Member;
use kbf_route::{MapSource, Nodes, Placement, Router, TlsNodes};
use kbf_storage::StorageError;
use kbf_storage::service::RemoteStorage;
use kbf_types::Digest;
use tonic::transport::Channel;

/// The connection to a member, for a caller that speaks to a node without a
/// [`kbf_storage::Storage`] (the proxy's raw pass-through of a ByteStream). A trait so a
/// test can give lazy plain channels.
pub trait Channels: Send + Sync {
    /// The connection to `member`.
    ///
    /// # Errors
    /// The member's address is not one.
    fn channel(&self, member: &Member) -> Result<Channel, StorageError>;
}

impl Channels for TlsNodes {
    fn channel(&self, member: &Member) -> Result<Channel, StorageError> {
        TlsNodes::channel(self, member)
    }
}

/// The map, the nodes it names and the router over them. Shared by everything in the
/// frontend that reaches storage.
pub struct Routed {
    router: Router,
    maps: Arc<dyn MapSource>,
    channels: Arc<dyn Channels>,
    /// A map with two members or more has been seen.
    settled: AtomicBool,
    /// The handles to the `--storage` node the frontend calls while it is not routing by the
    /// map ([`Routed::bootstrapped`]).
    bootstrap: Vec<RemoteStorage>,
}

impl std::fmt::Debug for Routed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Routed(..)")
    }
}

impl Routed {
    /// Calls routed by `maps`, to the nodes `nodes` gives and, for the raw pass-through,
    /// the connections `channels` gives.
    #[must_use]
    pub fn new(
        maps: Arc<dyn MapSource>,
        nodes: Arc<dyn Nodes>,
        channels: Arc<dyn Channels>,
    ) -> Self {
        Self {
            router: Router::new(Arc::clone(&maps), nodes),
            maps,
            channels,
            settled: AtomicBool::new(false),
            bootstrap: Vec::new(),
        }
    }

    /// The same, also tagging the calls made through `storage`, the handle to the
    /// `--storage` node, with the generation of the map held ([`Routed::tag_bootstrap`]).
    #[must_use]
    pub fn bootstrapped(mut self, storage: RemoteStorage) -> Self {
        self.bootstrap.push(storage);
        self
    }

    /// The generation of the map held now; 0 before the first.
    #[must_use]
    pub fn bootstrap_generation(&self) -> u64 {
        self.maps.current().map_or(0, |view| view.map.generation())
    }

    /// Tags the handles of [`Routed::bootstrapped`] with the map's generation now. A
    /// node that holds a map judges every call by it and refuses 0 as behind, and while
    /// the map has one member the frontend calls only the `--storage` node, so each call
    /// that does is tagged first and follows every bump.
    pub fn tag_bootstrap(&self) {
        let generation = self.bootstrap_generation();
        for storage in &self.bootstrap {
            storage.set_map_generation(generation);
        }
    }

    /// Calls routed by `maps` to the members over mutual TLS (`nodes`).
    #[must_use]
    pub fn over_tls(maps: Arc<dyn MapSource>, nodes: Arc<TlsNodes>) -> Self {
        Self::new(maps, nodes.clone(), nodes)
    }

    /// Whether to route by the map: it has had two members or more. Once true it stays
    /// true, so that a farm which shrinks to one member (or whose `--storage` node is
    /// drained) is not sent back to a node that may no longer be in it, and so that the
    /// two halves of an action-cache write cannot be routed differently by a member count
    /// that changes between them.
    #[must_use]
    pub fn active(&self) -> bool {
        if self.settled.load(Ordering::Acquire) {
            return true;
        }
        let several = self
            .maps
            .current()
            .is_some_and(|view| view.map.members().len() >= 2);
        if several {
            self.settled.store(true, Ordering::Release);
        }
        several
    }

    /// The router over the map, for a caller outside the crate that asks the farm a
    /// question of its own (the frontend's readiness check). Whether to use it is
    /// [`Routed::active`].
    #[must_use]
    pub fn router(&self) -> &Router {
        &self.router
    }

    /// Who holds `digest` in the map as it is now.
    pub(crate) fn place(&self, digest: &Digest) -> Result<Placement, StorageError> {
        self.router.place(digest)
    }

    pub(crate) fn channel(&self, member: &Member) -> Result<Channel, StorageError> {
        self.channels.channel(member)
    }
}
