//! [`FarmCas`]: the data plane of a daemon that may or may not be told a storage map.
//!
//! With a map (`Welcome.map`, kept in a [`MapHolder`]) every blob goes straight to the
//! storage nodes that own it, through a [`RoutedCas`] over a [`Router`] fed by the held
//! maps ([`HeldMaps`]), each node verified against its own name from the map. The
//! frontend then carries work and results over the session and no blob bytes. Without a
//! map (a server that sends none, or one-box all-in-one) every blob goes through the
//! frontend's CAS, the transfer engine over `--cas`, as before the map existed.
//!
//! A daemon in the same process as a storage node (`kbf storage-and-daemon` with a
//! frontend) has no server leg: [`member_cas`] is the map leg alone, with the one member
//! that is this process's own reached in-process ([`LocalMember`]) and every other over
//! TLS.
//!
//! The choice is made again for each lease and for each call made outside one, from
//! whether a map is held, and is logged each time it changes: once at the first call,
//! and again if a map arrives later. A lease keeps the side it started on, so its reads
//! and its writes are never split between the two.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use kbf_map::Member;
use kbf_mapsync::MapHolder;
use kbf_proto::reapi::Digest;
use kbf_route::{Liveness, Nodes, Router, TlsNodes};
use kbf_storage::service::InProcess;
use kbf_storage::{Storage, StorageError};
use kbf_transfer::cas::{Cas, CasError, Chunks, Intake};
use kbf_transfer::{Record, RoutedCas, Scope, TransferConfig, Transfers};
use kbf_types::LeaseId;

use crate::config::{ConfigError, TlsFiles};
use crate::map::{HeldMaps, REFETCH_WITHIN};

/// Storage nodes dialled over mutual TLS with the daemon's CA and certificate, each
/// verified against its own host from the map. `files.server_name` (the frontend's
/// name) is not used: [`TlsNodes`] has no way to be given one name for every node. A
/// node that goes silent is given up on as `liveness` says.
///
/// # Errors
/// A TLS file cannot be read.
pub fn storage_nodes(files: &TlsFiles, liveness: Liveness) -> Result<Arc<TlsNodes>, ConfigError> {
    let (ca, identity) = files.trust()?;
    Ok(Arc::new(
        TlsNodes::new(ca, identity).with_liveness(liveness),
    ))
}

/// Which side a call went to, last time one was made.
const UNSAID: u8 = 0;
const SERVER: u8 = 1;
const ROUTED: u8 = 2;

/// The daemon's data plane: the storage owners when a map is held, the frontend's CAS
/// when none is.
pub struct FarmCas {
    server: Arc<Transfers>,
    routed: Arc<RoutedCas>,
    maps: Arc<MapHolder>,
    said: AtomicU8,
}

impl FarmCas {
    /// A plane that goes through `server` until `maps` holds a map, and then to the
    /// nodes `nodes` dials, within the limits `config` names.
    #[must_use]
    pub fn new(
        server: Arc<Transfers>,
        maps: Arc<MapHolder>,
        nodes: Arc<dyn Nodes>,
        config: &TransferConfig,
    ) -> Self {
        let held = Arc::new(HeldMaps::new(Arc::clone(&maps), REFETCH_WITHIN));
        let routed = RoutedCas::new(Router::new(held, nodes), config);
        Self {
            server,
            routed: Arc::new(routed),
            maps,
            said: AtomicU8::new(UNSAID),
        }
    }

    /// The side the next call goes to, logged when it differs from the last call's.
    fn side(&self) -> Side<&Arc<Transfers>, &Arc<RoutedCas>> {
        let generation = self.maps.generation();
        let now = if generation > 0 { ROUTED } else { SERVER };
        if self.said.swap(now, Ordering::Relaxed) != now {
            if now == ROUTED {
                tracing::info!(
                    generation,
                    "data plane: blobs go to the storage nodes that own them; \
                     none cross the frontend"
                );
            } else {
                tracing::info!(
                    "data plane: no storage map held; blobs go through the frontend's CAS"
                );
            }
        }
        if now == ROUTED {
            Side::Routed(&self.routed)
        } else {
            Side::Server(&self.server)
        }
    }
}

/// The two sides of the plane. A macro, not a generic function, delegates a call to
/// whichever it is (see [`via`]).
enum Side<S, R> {
    Server(S),
    Routed(R),
}

/// Runs `$call` on the side `$side` names, bound to `$c`.
macro_rules! via {
    ($side:expr, $c:ident => $call:expr) => {
        match $side {
            Side::Server($c) => $call,
            Side::Routed($c) => $call,
        }
    };
}

impl Cas for FarmCas {
    type Lease = FarmLease;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> FarmLease {
        let side = match self.side() {
            Side::Server(server) => Side::Server(server.scope(lease)),
            Side::Routed(routed) => Side::Routed(Arc::clone(routed)),
        };
        FarmLease {
            side,
            farm: Arc::clone(self),
        }
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        via!(self.side(), c => c.get(digest).await)
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        via!(self.side(), c => c.put(bytes).await)
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        via!(self.side(), c => c.put_chunks(digest, chunks).await)
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        via!(self.side(), c => c.read_to(digest, file).await)
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        via!(self.side(), c => c.missing(digests).await)
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        via!(self.side(), c => c.put_records(records).await)
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        via!(self.side(), c => c.intake())
    }

    fn in_flight(&self) -> usize {
        via!(self.side(), c => c.in_flight())
    }
}

/// One lease's view of a [`FarmCas`]: the side the plane was on when the lease began.
pub struct FarmLease {
    side: Side<Scope, Arc<RoutedCas>>,
    farm: Arc<FarmCas>,
}

impl Cas for FarmLease {
    type Lease = Self;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> Self {
        self.farm.lease(lease)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        via!(&self.side, c => c.get(digest).await)
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        via!(&self.side, c => c.put(bytes).await)
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        via!(&self.side, c => c.put_chunks(digest, chunks).await)
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        via!(&self.side, c => c.read_to(digest, file).await)
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        via!(&self.side, c => c.missing(digests).await)
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        via!(&self.side, c => c.put_records(records).await)
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        via!(&self.side, c => c.intake())
    }

    fn in_flight(&self) -> usize {
        via!(&self.side, c => c.in_flight())
    }
}

/// What the storage half of a `kbf storage-and-daemon` process gives its daemon half: the
/// node's own storage service called in-process and the address it registered under (the
/// member of the map that is this process).
#[derive(Clone)]
pub struct LocalMember {
    addr: String,
    service: InProcess,
}

impl LocalMember {
    /// The member registered as `addr`, served in-process by `service`.
    ///
    /// `service` must be the view of the very [`StorageService`](kbf_storage::service::StorageService)
    /// the node's listener serves (`StorageHalf::in_process`), not the storage under it: a
    /// write the daemon half makes to a blob this node is the primary of must take the path
    /// a call from another node takes, which stores the blob here and sends it on to the
    /// replica (the chain, `kbf_storage::replicate`) and judges the call by the map
    /// generation the daemon routed by. Calling the inner storage would store one copy and
    /// judge nothing.
    #[must_use]
    pub fn new(addr: String, service: InProcess) -> Self {
        Self { addr, service }
    }

    /// The address the member is registered under: the one of the map's member that is
    /// this process.
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.addr
    }
}

/// The member `local` is, called in-process; any other through `rest`.
///
/// A call to the local node carries the generation of the map the daemon routed by, as a
/// call to any other member does (`generation`), and the service judges it as it judges
/// theirs, with the one check it makes on every call: UNAVAILABLE until the node holds a
/// map (a node without one would take a write the map places elsewhere), STALE_MAP for a
/// daemon behind the node. The refusal is the node's alone; reads and writes owned by
/// other members go on.
struct WithLocal {
    local: LocalMember,
    rest: Arc<dyn Nodes>,
}

impl Nodes for WithLocal {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        if member.addr != self.local.addr {
            return self.rest.node(member, generation);
        }
        Ok(self.local.service.at(generation))
    }
}

/// The data plane of a daemon beside the storage node `local`: every blob goes to the
/// members that own it by the map `maps` holds (the one the daemon's frontend sends),
/// the one that is `local` in-process and the others through `rest`. With no map held,
/// every call fails UNAVAILABLE and nothing is written anywhere: there is no server leg
/// to fall back on.
#[must_use]
pub fn member_cas(
    maps: Arc<MapHolder>,
    local: LocalMember,
    rest: Arc<dyn Nodes>,
    config: &TransferConfig,
) -> Arc<RoutedCas> {
    let held = Arc::new(HeldMaps::new(maps, REFETCH_WITHIN));
    let nodes = Arc::new(WithLocal { local, rest });
    Arc::new(RoutedCas::new(Router::new(held, nodes), config))
}
