//! The storage map on a daemon's session: `Welcome` carries the map the server holds, a
//! `MapUpdate` follows each change on every open session, and a `MapRequest` is
//! answered with the current map. A server with no map source sends and answers nothing.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_mapsync::{FakeMapSource, MapSource};
use kbf_objstore::MemoryStore;
use kbf_proto::worker::{MapRequest, daemon_message};
use kbf_server::members::{Members, MembershipConfig, Progress, Registrant};
use kbf_server::{
    EpochPolicy, Listeners, StateStore, bind_server_with_map, bind_server_with_state,
};
use kbf_state::Store;
use support::{FakeDaemon, HELLO_WAIT, INTERVAL, PROMPT, QUIET, hello};

fn map(generation: u64, members: u64) -> Map {
    let members = (1..=members)
        .map(|id| Member {
            id: MemberId(id),
            addr: format!("127.0.0.1:{id}"),
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
        })
        .collect();
    Map::new(generation, members, Rule::Rendezvous, 1, 1, Vec::new()).expect("a valid map")
}

struct Server {
    worker: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn listeners() -> Listeners {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

fn start(source: Option<Arc<dyn MapSource>>) -> Server {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let cache = Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory());
    let bound = bind_server_with_map(
        cache,
        listeners(),
        None,
        kbf_auth::Policy::allow_all(),
        None,
        source,
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let worker = bound.worker;
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        worker,
        stop: Some(stop),
    }
}

async fn connect(server: &Server, node: &str) -> FakeDaemon {
    FakeDaemon::connect(server.worker, hello(node, 4, 8))
        .await
        .expect("registered")
}

/// Catches a Welcome that does not carry the map (a daemon would route by none until
/// the first change) or carries another map than the source holds now.
#[tokio::test]
async fn welcome_carries_the_current_map() {
    let source = Arc::new(FakeMapSource::new(map(4, 3)));
    let server = start(Some(Arc::clone(&source) as Arc<dyn MapSource>));
    let daemon = connect(&server, "node-1").await;
    let sent = daemon.map.expect("a map in Welcome");
    assert_eq!(Map::try_from(sent).expect("a valid map"), map(4, 3));
    source.set(map(5, 3));
    let later = connect(&server, "node-2").await;
    assert_eq!(later.map.expect("a map").generation, 5, "the current one");
}

/// Catches a change that reaches the first session only (or none): each open session
/// gets a `MapUpdate` of the map that changed, and a session that ended is not sent
/// to (its task ends, which drops the server's hold on the source).
#[tokio::test]
async fn a_map_update_reaches_every_connected_session() {
    let source = Arc::new(FakeMapSource::new(map(1, 1)));
    let server = start(Some(Arc::clone(&source) as Arc<dyn MapSource>));
    let idle = Arc::strong_count(&source);
    let mut sessions = Vec::new();
    for node in ["a", "b", "c"] {
        sessions.push(connect(&server, node).await);
    }
    source.set(map(2, 2));
    for session in &mut sessions {
        assert_eq!(session.map_update_within(PROMPT).await, Some(2));
    }
    source.set(map(3, 3));
    for session in &mut sessions {
        assert_eq!(session.map_update_within(PROMPT).await, Some(3));
    }
    assert!(
        Arc::strong_count(&source) > idle,
        "sessions hold the source"
    );
    drop(sessions);
    // Each session's tasks end with its stream and let go of the source.
    let deadline = tokio::time::Instant::now() + PROMPT;
    while Arc::strong_count(&source) > idle {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a closed session still holds the source"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Catches a `MapRequest` that is not answered (a node that lost a push would wait for
/// the next change), and one answered with an older map than the server holds.
#[tokio::test]
async fn a_map_request_is_answered_with_the_current_map() {
    let source = Arc::new(FakeMapSource::new(map(7, 2)));
    let server = start(Some(Arc::clone(&source) as Arc<dyn MapSource>));
    let mut daemon = connect(&server, "node-1").await;
    daemon.send(daemon_message::Message::MapRequest(MapRequest {
        at_least: 9,
    }));
    // The server holds 7, which may be older than asked: it answers what it has.
    assert_eq!(daemon.map_update_within(PROMPT).await, Some(7));
    source.set(map(9, 2));
    assert_eq!(daemon.map_update_within(PROMPT).await, Some(9));
    daemon.send(daemon_message::Message::MapRequest(MapRequest {
        at_least: 9,
    }));
    assert_eq!(daemon.map_update_within(PROMPT).await, Some(9));
}

/// Catches a server with no map that sends one anyway, or answers a request with an
/// empty one: it names none in Welcome and does not answer.
#[tokio::test]
async fn a_server_without_a_map_sends_none() {
    let server = start(None);
    let mut daemon = connect(&server, "node-1").await;
    assert!(daemon.map.is_none());
    daemon.send(daemon_message::Message::MapRequest(MapRequest {
        at_least: 1,
    }));
    assert_eq!(daemon.map_update_within(QUIET).await, None);
}

/// The same through the cell the other server tests run on (a different instantiation of
/// the server's generic session code than `bind_server_with_map` over memory): Welcome
/// carries the map, a change is pushed, a request is answered.
#[tokio::test]
async fn a_cell_sends_the_map_too() {
    let source = Arc::new(FakeMapSource::new(map(4, 2)));
    let cell = support::Cell::start_with_map(Arc::clone(&source) as Arc<dyn MapSource>).await;
    let mut daemon = cell.daemon("node-1", 4, 8).await;
    assert_eq!(daemon.map.take().expect("a map in Welcome").generation, 4);
    source.set(map(5, 2));
    assert_eq!(daemon.map_update_within(PROMPT).await, Some(5));
    daemon.send(daemon_message::Message::MapRequest(MapRequest {
        at_least: 5,
    }));
    assert_eq!(daemon.map_update_within(PROMPT).await, Some(5));
}

fn registrant(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("10.0.0.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: kbf_server::members::PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// Catches a server with a state store that sends daemons no map (the members' map is
/// the one it holds, with no source given): a daemon that connects before any storage node
/// registered is welcomed with none and is sent the first map when a node registers,
/// and a daemon that connects later is welcomed with the members' map. Mutant: a server
/// that takes no map from its members.
#[tokio::test]
async fn a_server_with_a_state_store_sends_daemons_its_members_map() {
    let store = hooked::Hooked::new();
    let config = MembershipConfig {
        ttl: Duration::from_millis(60),
        dwell: Duration::from_secs(1),
        replication: 1,
    };
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config,
        history: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        None,
        kbf_auth::Policy::allow_all(),
        Some(state),
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let server = Server {
        worker: bound.worker,
        stop: Some(stop),
    };
    tokio::spawn(async move { bound.serving.await.expect("serve") });

    let mut early = connect(&server, "early").await;
    assert!(early.map.is_none(), "no member, no map to welcome with");
    let members = Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config);
    let first = members.register(&registrant(1)).expect("registers");
    assert_eq!(
        early.map_update_within(PROMPT).await,
        Some(first.map.generation()),
        "the first map reaches a daemon that was already connected"
    );
    let late = connect(&server, "late").await;
    let welcomed = Map::try_from(late.map.expect("a map in Welcome")).expect("valid");
    assert_eq!(welcomed.members().len(), 1);
}
