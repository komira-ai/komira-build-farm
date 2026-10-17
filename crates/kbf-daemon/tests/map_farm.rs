//! The storage map from a real server to a real daemon over mutual TLS: the map in
//! `Welcome`, a `MapUpdate` for a change, and a push that is lost, which is harmless
//! because a daemon that finds it is behind asks the server for the map.

mod support;

use std::future::pending;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Daemon, DaemonConfig, FakeRuntime, NodeReport, Runtime};
use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_mapsync::{FakeMapSource, MapHolder, MapSource};
use kbf_objstore::MemoryStore;
use kbf_server::{Listeners, WorkerTls, bind_server_with_map};
use support::{PROMPT, pki};
use tokio::sync::watch;
use tokio::time::timeout;

fn map(generation: u64) -> Map {
    let member = Member {
        id: MemberId(1),
        addr: "127.0.0.1:1".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    Map::new(generation, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("map")
}

/// A source whose changes never announce: every push the server would make is lost.
struct Silent {
    inner: FakeMapSource,
    never: watch::Sender<u64>,
}

impl MapSource for Silent {
    fn current(&self) -> Option<Arc<Map>> {
        self.inner.current()
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.never.subscribe()
    }
}

/// A server holding `source` and a daemon putting what it is sent in the returned holder.
async fn farm(name: &str, source: Arc<dyn MapSource>) -> Arc<MapHolder> {
    let pki = pki(name);
    let listeners = Listeners {
        reapi: SocketAddr::from(([127, 0, 0, 1], 0)),
        worker: SocketAddr::from(([127, 0, 0, 1], 0)),
        reapi_tls: None,
        worker_tls: Some(WorkerTls {
            server: pki.worker_tls(),
            deny_list: None,
        }),
        heartbeat_interval: Duration::from_millis(100),
        hello_wait: Duration::from_secs(2),
        tick: Duration::from_millis(50),
        unservable_wait: Duration::from_secs(300),
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: Duration::from_secs(60),
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    };
    let cache = Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory());
    let bound = bind_server_with_map(
        cache,
        listeners,
        None,
        kbf_auth::Policy::allow_all(),
        None,
        Some(source),
        pending(),
    )
    .expect("bind");
    let worker = bound.worker;
    tokio::spawn(async move { bound.serving.await.expect("serve") });

    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let report = NodeReport::detect(&[runtime.driver()]).expect("detect this node");
    let mut config = DaemonConfig::new(
        format!("https://127.0.0.1:{}", worker.port()),
        pki.client,
        "node-1".to_owned(),
    );
    config.reconnect_after = Duration::from_millis(100);
    let holder = Arc::new(MapHolder::new());
    let daemon = Daemon::new(config, runtime, report)
        .expect("daemon config")
        .with_map(Arc::clone(&holder));
    tokio::spawn(daemon.run(pending()));
    holder
}

async fn taken(holder: &MapHolder, generation: u64) {
    let mut changes = holder.changes();
    timeout(PROMPT, changes.wait_for(|held| *held >= generation))
        .await
        .expect("the map is taken")
        .expect("the holder is open");
}

/// Catches a farm whose daemon routes by no map: it holds the one in `Welcome`, and then
/// each change the server pushes, in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_follows_the_map_the_server_holds() {
    let source = Arc::new(FakeMapSource::new(map(2)));
    let holder = farm("map-farm-follow", Arc::clone(&source) as Arc<dyn MapSource>).await;
    taken(&holder, 2).await;
    source.set(map(3));
    taken(&holder, 3).await;
    source.set(map(4));
    taken(&holder, 4).await;
    assert_eq!(holder.previous().expect("replaced").generation(), 3);
}

/// Catches a farm that depends on the push: with every `MapUpdate` lost, a daemon that
/// finds it is behind (a storage node named generation 5) asks, and the server answers
/// with the map it holds. Mutants: the server not answering a `MapRequest`; the daemon
/// not sending one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_map_update_is_harmless() {
    let source = Arc::new(Silent {
        inner: FakeMapSource::new(map(2)),
        never: watch::Sender::new(0),
    });
    let holder = farm("map-farm-lost", Arc::clone(&source) as Arc<dyn MapSource>).await;
    taken(&holder, 2).await;
    source.inner.set(map(5));
    // The source announced nothing, so nothing was pushed: the daemon still holds 2.
    assert_eq!(holder.generation(), 2);
    let reached = timeout(PROMPT, holder.reach(5))
        .await
        .expect("asked and answered");
    assert_eq!(reached.generation(), 5);
}
