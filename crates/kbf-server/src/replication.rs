//! What a storage node adds to its storage service when it is one of several
//! (`docs/design/architecture-v2.md`, sections 6 and 8): the map it judges calls by, the
//! chain that copies a write to the blob's second owner, and the worker that makes a copy
//! the chain could not.
//!
//! A node started with `--frontend` registers with it and holds the maps it is sent
//! ([`NodeMap`]). Its service judges every call by that map ([`kbf_storage::replicate`]):
//! until the node holds one it serves nothing (UNAVAILABLE), a caller behind it is
//! answered STALE_MAP, and a caller ahead of it makes it fetch the map. That check is the
//! storage service's own and the only one; a listener has no second gate in front of it.
//!
//! What a node can do with the map depends on how it admits callers:
//! - **Mutual TLS** (the farm's mode): the node is a link of the chain. A write goes to the
//!   blob's first owner, which copies it to the second over the same certificate it
//!   registers with (`Peers`), and answers when both hold it. A copy the second owner
//!   could not take is written to the node's repair journal, in `repair/` under the node's
//!   metadata directory, before the write is answered, and the repair worker started here
//!   makes it when the owner returns. The worker stops when the node is told to shut down.
//! - **Plain text** (one machine, loopback only): the node judges calls by the map and
//!   keeps every blob it is given, but copies nothing, because a call over plain text
//!   carries no role to tell a node's copy from a client's. The node says so in its log.
//!
//! A node with no `--frontend` is a one-member farm of itself and judges nothing.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use futures::future::BoxFuture;
use kbf_mapsync::MapHolder;
use kbf_storage::Storage;
use kbf_storage::StorageError;
use kbf_storage::repair::{JournalError, NoLedger, Options, RepairConfig, RepairWorker, Repairs};
use kbf_storage::replicate::{Chain, ChainConfig, MapSource, Peers};
use kbf_storage::service::{CatchUp, InProcess, StorageService};
use kbf_store::StdFs;
use tonic::transport::ClientTlsConfig;

use crate::join::{Joined, Joiner};
use crate::node_map::NodeMap;
use crate::serve::{ServeError, StorageListener, StorageSocket, serve_service};
use crate::start::AbortOnDrop;
use crate::storage_node::StorageFlags;

/// Where the repair journal is kept, under the node's metadata directory. The metadata
/// log claims its directory when it is opened and refuses one that holds files it did not
/// write and no log record, so this is made after it.
pub const REPAIR_DIR: &str = "repair";

/// Why a node could not be set up as a link of a chain.
#[derive(Debug, thiserror::Error)]
pub enum ReplicationError {
    /// The repair journal's directory could not be made.
    #[error("repair journal directory {dir}: {source}")]
    Dir {
        /// The directory.
        dir: PathBuf,
        /// What stopped it.
        source: io::Error,
    },
    /// The repair journal could not be opened: it is damaged (which is refused, since
    /// guessing past it could forget a debt) or the disk failed.
    #[error("{0}")]
    Journal(#[from] JournalError),
    /// The service refused to be a link of a chain.
    #[error("{0}")]
    Service(#[from] StorageError),
}

/// A service set up for a farm: the chain's worker and the journal it works on, when the
/// node can be a link of the chain.
pub(crate) struct Replicated {
    pub service: StorageService,
    pub worker: Option<RepairWorker>,
    pub repairs: Option<Arc<Repairs>>,
}

/// `service` judging calls by `node`'s map, and, with `peer_tls` (the certificate the node
/// calls its peers with: a node admitted by certificates), as a link of the chain with a
/// repair journal in `repair_dir` and the worker that settles it (not started).
///
/// # Errors
/// The journal's directory cannot be made or its journal opened, or `service` does not
/// admit callers by certificate (a chain's copy must be shown to come from a storage
/// node).
pub(crate) fn replicate(
    service: StorageService,
    storage: Arc<dyn Storage>,
    node: &Arc<NodeMap>,
    peer_tls: Option<ClientTlsConfig>,
    repair_dir: &Path,
    config: RepairConfig,
) -> Result<Replicated, ReplicationError> {
    let source: Arc<dyn MapSource> = Arc::clone(node) as Arc<dyn MapSource>;
    let Some(tls) = peer_tls else {
        tracing::warn!("plain text: this node judges calls by the map and copies no blob");
        return Ok(Replicated {
            service: service.with_map(source),
            worker: None,
            repairs: None,
        });
    };
    let fs = StdFs::new(repair_dir).map_err(|source| ReplicationError::Dir {
        dir: repair_dir.to_owned(),
        source,
    })?;
    // The frontend is not told of what is owed yet (`RepairLedger` is the seam for it).
    let repairs = Arc::new(Repairs::open(fs, Options::default(), Arc::new(NoLedger))?);
    let chain = Arc::new(Chain::new(
        source,
        Peers::tls(tls),
        Arc::clone(&repairs),
        ChainConfig::default(),
    ));
    let service = service.with_chain(Arc::clone(&chain))?;
    let worker = RepairWorker::new(storage, chain, Arc::clone(&repairs), config);
    Ok(Replicated {
        service,
        worker: Some(worker),
        repairs: Some(repairs),
    })
}

/// Resolves `stop` when `shutdown` has resolved (or its future is gone), and `shutdown`
/// itself as it was. Not generic, so the lines count once.
fn stopping(shutdown: BoxFuture<'static, ()>) -> (BoxFuture<'static, ()>, BoxFuture<'static, ()>) {
    let (told, stop) = tokio::sync::watch::channel(false);
    let shutdown = Box::pin(async move {
        shutdown.await;
        told.send_replace(true);
    });
    let stop = Box::pin(async move {
        let mut stop = stop;
        // Err: the sender is gone, which is also the end of the node.
        let _ = stop.wait_for(|stopped| *stopped).await;
    });
    (shutdown, stop)
}

type Serving = Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>;

/// A storage node that is bound, with what runs beside its listener.
pub(crate) struct Node {
    pub addr: SocketAddr,
    pub serving: Serving,
    pub in_process: InProcess,
    pub maps: Arc<MapHolder>,
    pub repairs: Option<Arc<Repairs>>,
    pub joiner: Option<AbortOnDrop>,
    pub repair: Option<AbortOnDrop>,
}

/// A node that is bound and, with `--frontend`, registered: what [`bind_node`] serves
/// once the node has opened its cache. The cache is opened between the two so that it
/// writes as the registration's member and boot ([`crate::storage_node::open_cache`]).
pub(crate) struct Registered {
    /// The listener's socket, bound and not yet serving.
    pub socket: StorageSocket,
    /// The map the node keeps and the member it learned it is.
    pub node: Arc<NodeMap>,
    /// The registration, and what it gave, for the heartbeats ([`Joiner::keep`]).
    pub joiner: Option<(Joiner, Joined)>,
    /// Whether the node owes a rebuild of its index: the service answers a miss
    /// UNAVAILABLE while it does, and the registration and heartbeats say so.
    pub catch_up: CatchUp,
}

/// Serves `storage` on the socket of `registered` as the node `flags` describe: a
/// one-member farm of itself, or, with `--frontend`, a node that judges calls by the map
/// it was sent, and replicates ([`replicate`]). The repair worker and the heartbeats run
/// beside the listener; the worker stops when `shutdown` resolves, the calls in flight
/// being drained by the listener as they are for any node.
///
/// # Errors
/// The TLS files cannot be read; the node cannot be made a link of a chain
/// ([`ReplicationError`]); the listener's TLS is refused.
pub(crate) fn bind_node(
    flags: &StorageFlags,
    listener: StorageListener,
    registered: Registered,
    storage: Arc<dyn Storage>,
    repair_dir: &Path,
    shutdown: BoxFuture<'static, ()>,
) -> Result<Node, Box<dyn Error>> {
    let Registered {
        socket,
        node,
        joiner,
        catch_up,
    } = registered;
    let maps = node.maps();
    let service = listener
        .service(Arc::clone(&storage))
        .with_catch_up(catch_up);
    let (shutdown, stop) = stopping(shutdown);
    let (service, worker, repairs) = if flags.frontend.is_some() {
        let peer_tls = flags.peer_tls()?;
        let config = flags.repair;
        let farm = replicate(service, storage, &node, peer_tls, repair_dir, config)?;
        (farm.service, farm.worker, farm.repairs)
    } else {
        (service, None, None)
    };
    let (bound, in_process) = serve_service(service, socket, listener.tls, shutdown)?;
    let addr = bound.addr;
    let joiner = joiner.map(|(joiner, joined)| AbortOnDrop(tokio::spawn(joiner.keep(joined))));
    let repair = worker.map(|worker| AbortOnDrop(tokio::spawn(run_repairs(worker, stop))));
    Ok(Node {
        addr,
        serving: Box::pin(bound.serving),
        in_process,
        maps,
        repairs,
        joiner,
        repair,
    })
}

/// The repair worker until `stop`.
async fn run_repairs(worker: RepairWorker, stop: BoxFuture<'static, ()>) {
    worker.run(stop).await;
}

#[cfg(test)]
mod tests;
