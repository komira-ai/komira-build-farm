//! The plane's log line about the side it is on. Alone in its test binary: a tracing
//! callsite that another test's thread reached first, with no subscriber installed, is
//! cached as uninteresting, and a subscriber installed after would miss it.

mod support;

use std::sync::Arc;

use kbf_daemon::cas::Cas as _;
use kbf_daemon::{FarmCas, MapHolder, storage_nodes};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_proto::reapi::Digest;
use kbf_route::Liveness;
use kbf_transfer::{RecordHasher, TransferConfig, Transfers, proto_digest};
use support::storage_node::{Node, serve_plain, serve_tls};
use support::{Pki, pki};
use tonic::transport::Endpoint;

fn plane(pki: &Pki, server: &Node) -> (Arc<FarmCas>, Arc<MapHolder>) {
    let channel = Endpoint::from_shared(format!("http://127.0.0.1:{}", server.port))
        .expect("endpoint")
        .connect_lazy();
    let engine = Arc::new(Transfers::over(channel, TransferConfig::default()));
    let holder = Arc::new(MapHolder::new());
    let nodes = storage_nodes(&pki.client, Liveness::default()).expect("TLS files");
    let cas = FarmCas::new(
        engine,
        Arc::clone(&holder),
        nodes,
        &TransferConfig::default(),
    );
    (Arc::new(cas), holder)
}

fn digest(bytes: &[u8]) -> Digest {
    let mut hasher = RecordHasher::new();
    hasher.update(bytes);
    proto_digest(&hasher.finish().0)
}

fn map(generation: u64, owner: &Node) -> Map {
    let member = Member {
        id: MemberId(1),
        addr: owner.name(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    Map::new(generation, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("a map")
}

/// Catches a plane that does not say which side it is on, or says it at every call: the
/// choice is logged at the first call, not again while it holds, and again when a map
/// arrives. (A current-thread runtime, so the test's subscriber sees the plane's calls.)
#[tokio::test]
async fn the_plane_logs_its_side_when_it_changes_and_only_then() {
    let log = Log::default();
    let _installed = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    let pki_ = pki("farm-cas-log");
    let (frontend, owner) = (serve_plain(), serve_tls(&pki_));
    let (cas, holder) = plane(&pki_, &frontend);
    let blob = digest(b"asked, never held");

    assert!(cas.get(&blob).await.is_err());
    assert!(cas.get(&blob).await.is_err());
    assert_eq!(log.count("no storage map held"), 1);
    assert_eq!(log.count("none cross the frontend"), 0);

    holder.offer(map(2, &owner));
    assert!(cas.get(&blob).await.is_err());
    assert!(cas.get(&blob).await.is_err());
    assert_eq!(log.count("no storage map held"), 1);
    assert_eq!(log.count("none cross the frontend"), 1);
    assert_eq!(log.count("generation=2"), 1);
}

/// What a subscriber wrote, for the test above.
#[derive(Clone, Default)]
struct Log(Arc<std::sync::Mutex<Vec<u8>>>);

impl Log {
    fn count(&self, needle: &str) -> usize {
        let text = String::from_utf8(self.0.lock().expect("lock").clone()).expect("UTF-8");
        text.matches(needle).count()
    }
}

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
