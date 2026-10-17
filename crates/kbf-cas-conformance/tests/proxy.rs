//! The conformance checks against the frontend in proxy mode: [`Transfers`], a stock
//! REAPI client, over one HTTP/2 connection to the frontend's proxy
//! ([`kbf_front::Proxy`]), which passes ByteStream through to a real storage node (the
//! storage service over an in-memory cache) on another loopback port. The proxy does
//! not serve `Segments` (the daemon's), so the engine falls back to blob by blob, as
//! it does against any server without it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kbf_auth::Authorizers;
use kbf_cas_conformance::Subject;
use kbf_front::{Cache, CacheStorage, Proxy, SegmentsConfig};
use kbf_storage::service::{Admission, Config, StorageService};
use kbf_transfer::{TransferConfig, Transfers};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Endpoint, Server};

/// A frontend and a storage node per [`Subject::fresh`], each over empty stores.
struct Fronted {
    scratch: PathBuf,
}

impl Fronted {
    fn new() -> Self {
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("proxy-conformance");
        std::fs::create_dir_all(&scratch).expect("scratch directory");
        Self { scratch }
    }
}

/// Serves `routes` on a loopback port.
fn serve(routes: tonic::service::Routes) -> SocketAddr {
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming),
    );
    addr
}

impl Subject for Fronted {
    type Cas = Transfers;

    async fn fresh(&self) -> Arc<Transfers> {
        let cache = Arc::new(Cache::memory());
        let store = Arc::new(CacheStorage::new(cache, SegmentsConfig::DEFAULT));
        let config = Config {
            write_bytes_in_flight: 64 << 20,
        };
        let node = serve(StorageService::new(store, Admission::Open, config).routes());
        let node = Endpoint::from_shared(format!("http://{node}")).expect("endpoint");
        let proxy = Proxy::to_endpoint(&node, Arc::new(Authorizers::allow_all()));
        let front = serve(proxy.routes());
        let endpoint = Endpoint::from_shared(format!("http://{front}")).expect("endpoint");
        Arc::new(Transfers::new(endpoint, TransferConfig::default()))
    }

    fn scratch(&self) -> &Path {
        &self.scratch
    }
}

kbf_cas_conformance::tests!(Fronted::new());
