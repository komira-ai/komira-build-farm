//! The storage map on the session: the map of `Welcome` and of each `MapUpdate` goes into
//! the holder the routing layer reads, never backwards, and a generation the holder waits
//! for is asked of the server.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::FakeRuntime;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_mapsync::MapHolder;
use kbf_proto::kbf::map::v1 as wire;
use kbf_proto::worker::daemon_message::Message;
use support::{Harness, PROMPT};
use tokio::time::timeout;

fn map(generation: u64) -> wire::Map {
    let member = Member {
        id: MemberId(1),
        addr: "127.0.0.1:1".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let map = Map::new(generation, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("map");
    wire::Map::from(&map)
}

/// Waits until the holder holds generation `generation` or newer, without asking for it.
async fn taken(holder: &MapHolder, generation: u64) {
    let mut changes = holder.changes();
    timeout(PROMPT, changes.wait_for(|held| *held >= generation))
        .await
        .expect("the map is taken")
        .expect("the holder is open");
}

async fn daemon(name: &str) -> (Harness, Arc<MapHolder>) {
    let holder = Arc::new(MapHolder::new());
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let h = Harness::with_map(name, runtime, Arc::clone(&holder)).await;
    (h, holder)
}

/// Catches a Welcome whose map the daemon drops (it would route by no map until the next
/// push), and a daemon that reads a Welcome without a map as a map of generation 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_map_in_welcome_is_taken() {
    let (mut h, holder) = daemon("map-welcome").await;
    let mut peer = h.session().await;
    peer.hello().await;
    peer.welcome_map(Some(map(3)));
    taken(&holder, 3).await;
    assert_eq!(holder.generation(), 3);
    // The next session's Welcome names no map: what is held stays.
    peer.close();
    let mut next = h.session().await;
    next.hello().await;
    next.welcome_map(None);
    next.heartbeat().await;
    assert_eq!(holder.generation(), 3);
}

/// Catches a receiver that goes backwards: pushes of an older and of the same generation,
/// in any order, change nothing, so a map that arrives late never replaces a newer one.
/// The newest one after them is a barrier: the map it replaced is the newest of the
/// earlier ones, 5, and not 4. Mutant: the holder takes any map.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_map_never_replaces_a_newer_one() {
    let (mut h, holder) = daemon("map-monotone").await;
    let peer = h.welcomed().await;
    for generation in [5, 3, 5, 2, 4, 7] {
        peer.map_update(map(generation));
    }
    taken(&holder, 7).await;
    assert_eq!(holder.generation(), 7);
    assert_eq!(holder.previous().expect("replaced").generation(), 5);
}

/// Catches a daemon that ends its session, or loses the map it has, on a map it cannot
/// read (here generation 0, which the domain refuses).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_map_the_daemon_cannot_read_is_ignored() {
    let (mut h, holder) = daemon("map-unreadable").await;
    let mut peer = h.welcomed().await;
    peer.map_update(map(4));
    taken(&holder, 4).await;
    peer.map_update(wire::Map {
        generation: 0,
        ..map(1)
    });
    peer.map_update(map(6));
    taken(&holder, 6).await;
    assert_eq!(holder.previous().expect("replaced").generation(), 4);
    peer.heartbeat().await;
}

/// Catches a daemon that waits for a push that was lost: a holder asked to reach a
/// generation it does not have gets it by asking the server, which answers with a
/// `MapUpdate`. A generation asked for before the stream was up is asked after its
/// Welcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wanted_generation_is_asked_of_the_server() {
    let (mut h, holder) = daemon("map-request").await;
    let mut peer = h.welcomed().await;
    peer.map_update(map(2));
    taken(&holder, 2).await;
    let waiting = tokio::spawn({
        let holder = Arc::clone(&holder);
        async move { holder.reach(9).await.generation() }
    });
    assert_eq!(peer.map_request().await, 9);
    peer.map_update(map(9));
    assert_eq!(
        timeout(PROMPT, waiting)
            .await
            .expect("reached")
            .expect("task"),
        9
    );

    // Wanted while the stream is down: asked on the next one, after its Welcome.
    peer.close();
    let waiting = tokio::spawn({
        let holder = Arc::clone(&holder);
        async move { holder.reach(12).await.generation() }
    });
    let mut next = h.session().await;
    next.hello().await;
    next.welcome_map(Some(map(9)));
    assert_eq!(next.map_request().await, 12);
    next.map_update(map(12));
    assert_eq!(
        timeout(PROMPT, waiting)
            .await
            .expect("reached")
            .expect("task"),
        12
    );
}

/// Catches a daemon that asks again on every wake for a generation it has asked for on
/// this stream (the server may answer with an older map, and a heartbeat every interval
/// would be a request every interval): one request, then heartbeats. Mutant: `request`
/// without the generation last asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wanted_generation_is_asked_once_on_a_stream() {
    let (mut h, holder) = daemon("map-asked-once").await;
    let mut peer = h.welcomed().await;
    let _waiting = tokio::spawn({
        let holder = Arc::clone(&holder);
        async move { holder.reach(9).await.generation() }
    });
    assert_eq!(peer.map_request().await, 9);
    let mut seen = Vec::new();
    for _ in 0..3 {
        let (_, m) = peer
            .expect(PROMPT, |m| {
                matches!(m, Message::Heartbeat(_) | Message::MapRequest(_)).then(|| m.clone())
            })
            .await
            .expect("a message");
        seen.push(m);
    }
    assert!(
        seen.iter().all(|m| matches!(m, Message::Heartbeat(_))),
        "{seen:?}"
    );
}

/// Catches a request for a generation the daemon already has (every `reach` that finds
/// its map held would cost the server a round trip).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generation_already_held_is_not_asked_for() {
    let (mut h, holder) = daemon("map-no-request").await;
    let mut peer = h.welcomed().await;
    peer.map_update(map(5));
    taken(&holder, 5).await;
    holder.reach(4).await;
    // A heartbeat follows the request, if one was sent: none was among the messages
    // before it.
    let mut seen = Vec::new();
    for _ in 0..3 {
        let (_, m) = peer
            .expect(PROMPT, |m| {
                matches!(m, Message::Heartbeat(_) | Message::MapRequest(_)).then(|| m.clone())
            })
            .await
            .expect("a message");
        seen.push(m);
    }
    assert!(
        seen.iter().all(|m| matches!(m, Message::Heartbeat(_))),
        "{seen:?}"
    );
}

/// Catches a daemon built without a holder that fails on the map a server sends it (in
/// Welcome and in a `MapUpdate`): it ignores both and goes on with its session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_without_a_holder_ignores_the_map() {
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let mut h = Harness::with_runtime("map-no-holder", runtime, Duration::from_secs(40)).await;
    let mut peer = h.session().await;
    peer.hello().await;
    peer.welcome_map(Some(map(3)));
    peer.map_update(map(4));
    peer.heartbeat().await;
}
