//! The conformance checks against the routed implementation: [`RoutedCas`] over a
//! [`Router`] over three storage nodes (kbf-front's caches served by the storage service
//! on loopback ports), replication 2. Every blob goes to its primary and is read from the
//! owners in rank order, through the real gRPC service and client.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use kbf_cas_conformance::Subject;
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_route::{MapSource, MapView, Nodes, RefetchError, Router};
use kbf_storage::service::{Admission, Config, RemoteStorage, StorageService};
use kbf_storage::{Storage, StorageError};
use kbf_transfer::{RoutedCas, TransferConfig};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};

/// A map that never changes.
struct Fixed(MapView);

impl MapSource for Fixed {
    fn current(&self) -> Option<MapView> {
        Some(self.0.clone())
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            Err(RefetchError {
                at_least,
                reason: "the test map does not change".to_owned(),
            })
        })
    }
}

/// Plain-text connections to the members of the test map.
#[derive(Default)]
struct Plain {
    channels: Mutex<HashMap<MemberId, Channel>>,
}

impl Nodes for Plain {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        let mut channels = self.channels.lock().expect("lock");
        let channel = channels.entry(member.id).or_insert_with(|| {
            Endpoint::from_shared(format!("http://{}", member.addr))
                .expect("endpoint")
                .connect_lazy()
        });
        Ok(Arc::new(RemoteStorage::new(channel.clone(), generation)))
    }
}

/// A storage node over a new in-memory cache, on a loopback port.
async fn serve() -> SocketAddr {
    let cache = Arc::new(Cache::memory());
    let storage: Arc<dyn Storage> = Arc::new(CacheStorage::new(cache, SegmentsConfig::DEFAULT));
    let service = StorageService::new(
        storage,
        Admission::Open,
        Config {
            write_bytes_in_flight: 1 << 30,
        },
    );
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        Server::builder()
            .add_routes(service.routes())
            .serve_with_incoming(incoming),
    );
    addr
}

/// A data plane over a farm of three nodes.
async fn farm() -> Arc<RoutedCas> {
    let mut members = Vec::new();
    for id in 1..=3 {
        members.push(Member {
            id: MemberId(id),
            addr: serve().await.to_string(),
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
        });
    }
    let map = Map::new(1, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("a map");
    let view = MapView {
        map: Arc::new(map),
        previous: None,
    };
    let router = Router::new(Arc::new(Fixed(view)), Arc::new(Plain::default()));
    Arc::new(RoutedCas::new(router, &TransferConfig::default()))
}

/// A farm per [`Subject::fresh`].
struct Routed {
    scratch: PathBuf,
}

impl Routed {
    fn new() -> Self {
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("routed-conformance");
        std::fs::create_dir_all(&scratch).expect("scratch directory");
        Self { scratch }
    }
}

impl Subject for Routed {
    type Cas = RoutedCas;

    async fn fresh(&self) -> Arc<RoutedCas> {
        farm().await
    }

    fn scratch(&self) -> &Path {
        &self.scratch
    }
}

kbf_cas_conformance::tests!(Routed::new());
