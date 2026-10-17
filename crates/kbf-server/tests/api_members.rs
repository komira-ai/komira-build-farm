//! `GET /v1/members` over a real listener: the storage members of the current map, read
//! from the same `Members` the `Membership` service registers into.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::{MemberId, MemberState};
use kbf_objstore::MemoryStore;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Cond, ManualClock, Store, Txn};
use serde_json::{Value, json};
use support::{HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TTL: Duration = Duration::from_secs(30);
const DWELL: Duration = Duration::from_secs(120);

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: TTL,
        dwell: DWELL,
        replication: 1,
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

/// A server with the operator API, writes off (no token), over `state` if given.
struct Server {
    api: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn start(state: Option<StateStore>) -> Server {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: None,
        local_cache: None,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        kbf_auth::Policy::allow_all(),
        state,
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let api = bound.api.expect("an API address");
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        api,
        stop: Some(stop),
    }
}

/// A store on a clock the test moves, and the state a server over it is given.
fn store() -> (Arc<ManualClock>, Arc<Fake>, StateStore) {
    let clock = Arc::new(ManualClock::default());
    let fake = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let state = StateStore {
        store: Arc::clone(&fake) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    (clock, fake, state)
}

/// Another frontend over the same store: what storage nodes register with.
fn other_frontend(fake: &Arc<Fake>) -> Members {
    Members::new(Arc::clone(fake) as Arc<dyn Store>, config())
}

fn registrant(i: u32, weight: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// One HTTP/1.1 request; the status code, the response head and the body.
async fn http(api: SocketAddr, method: &str, path: &str, headers: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: 0\r\n\
         {headers}Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, head.to_owned(), body.to_owned())
}

async fn members(api: SocketAddr) -> Value {
    let (code, head, body) = http(api, "GET", "/v1/members", "").await;
    assert_eq!(code, 200, "{body}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{head}"
    );
    serde_json::from_str(&body).expect("a JSON body")
}

/// Catches a members view that invents a map before the first member registers (the
/// script that waits for "active" must see an empty list, not an error), and one that
/// answers a server with no state store as if it had an empty farm: that server keeps
/// no members at all, and says so.
#[tokio::test]
async fn no_map_is_empty_and_no_state_store_is_not_found() {
    let (_clock, _fake, state) = store();
    let with = start(Some(state));
    assert_eq!(
        members(with.api).await,
        json!({ "generation": 0, "members": [] })
    );

    let without = start(None);
    let (code, _, body) = http(without.api, "GET", "/v1/members", "").await;
    assert_eq!(code, 404, "{body}");
    let why = serde_json::from_str::<Value>(&body).expect("JSON")["error"]
        .as_str()
        .expect("an error")
        .to_owned();
    assert!(why.contains("--state-store"), "{why}");
}

/// Catches a view that reads another source than the members the `Membership` service
/// writes (a registered member must appear, with the id the registration answered), one
/// that drops or renames a field (`id`, `addr`, `state`, `weight`, `generation`), and one
/// that names a state other than the map's: a member the operator set `joining` is
/// `joining` until promoted, and `active` after (a registration that is caught up is
/// `active` at once).
#[tokio::test]
async fn a_registered_member_is_listed_and_active_once_promoted() {
    let (_clock, fake, state) = store();
    let server = start(Some(state));
    let frontend = other_frontend(&fake);
    let a = frontend.register(&registrant(1, 1)).expect("a");
    let b = frontend.register(&registrant(2, 3)).expect("b");
    let registered = members(server.api).await;
    assert_eq!(registered["members"][0]["state"], json!("active"));
    assert_eq!(registered["members"][1]["state"], json!("active"));
    frontend.set_state(a.id, MemberState::Joining).expect("a");
    let b_map = frontend.set_state(b.id, MemberState::Joining).expect("b");

    let joined = members(server.api).await;
    assert_eq!(joined["generation"], json!(b_map.generation()));
    assert_eq!(
        joined["members"],
        json!([
            { "id": a.id.0, "addr": "192.0.2.1:8982", "state": "joining", "weight": 1 },
            { "id": b.id.0, "addr": "192.0.2.2:8982", "state": "joining", "weight": 3 },
        ])
    );

    let promoted = frontend
        .set_state(a.id, MemberState::Active)
        .expect("promoted");
    let after = members(server.api).await;
    assert_eq!(after["generation"], json!(promoted.generation()));
    assert_eq!(after["members"][0]["state"], json!("active"));
    assert_eq!(after["members"][1]["state"], json!("joining"));
}

/// Catches a state spelled other than the map's five (`catching_up` and `draining` are
/// the ones a hand-written mapping gets wrong), and a Down member dropped from the view
/// at once: it is in the map for the dwell, and leaves only when the map drops it.
#[tokio::test]
async fn a_draining_or_expired_member_shows_its_state_until_it_leaves() {
    let (clock, fake, state) = store();
    let server = start(Some(state));
    let frontend = other_frontend(&fake);
    let a = frontend.register(&registrant(1, 1)).expect("a");
    let b = frontend.register(&registrant(2, 1)).expect("b");
    let c = frontend.register(&registrant(3, 1)).expect("c");
    frontend.set_state(c.id, MemberState::Joining).expect("c");

    let state_of = |view: &Value, id: MemberId| -> Option<String> {
        view["members"]
            .as_array()
            .expect("a list")
            .iter()
            .find(|m| m["id"] == json!(id.0))
            .map(|m| m["state"].as_str().expect("a state").to_owned())
    };

    frontend
        .set_state(a.id, MemberState::CatchingUp)
        .expect("a");
    frontend.set_state(b.id, MemberState::Draining).expect("b");
    let view = members(server.api).await;
    assert_eq!(state_of(&view, a.id).as_deref(), Some("catching_up"));
    assert_eq!(state_of(&view, b.id).as_deref(), Some("draining"));
    assert_eq!(state_of(&view, c.id).as_deref(), Some("joining"));

    // Only `a` and `b` keep beating: `c` lapses, and is Down in the map.
    clock.advance(TTL - Duration::from_secs(1));
    frontend
        .heartbeat(a.id, a.boot, Progress::CatchingUp, None)
        .expect("beat a");
    frontend
        .heartbeat(b.id, b.boot, Progress::CaughtUp, None)
        .expect("beat b");
    clock.advance(Duration::from_secs(2));
    frontend.refresh().expect("refreshed");
    let view = members(server.api).await;
    assert_eq!(state_of(&view, c.id).as_deref(), Some("down"));
    assert_eq!(state_of(&view, a.id).as_deref(), Some("catching_up"));

    // Past the dwell the map drops it, and so does the view.
    clock.advance(DWELL + Duration::from_secs(1));
    frontend.refresh().expect("refreshed");
    let view = members(server.api).await;
    assert_eq!(state_of(&view, c.id), None, "{view}");
}

/// Catches `/v1/members` behind a gate `/v1/nodes` does not have (the operator scripts
/// call both the same way), and a write verb answered on a read path. The server has no
/// token, so a read that needed one could not succeed.
#[tokio::test]
async fn members_is_read_like_nodes_and_takes_no_write() {
    let (_clock, _fake, state) = store();
    let server = start(Some(state));
    let (nodes, _, _) = http(server.api, "GET", "/v1/nodes", "").await;
    let (members, _, _) = http(server.api, "GET", "/v1/members", "").await;
    assert_eq!((nodes, members), (200, 200));
    let (code, _, _) = http(
        server.api,
        "POST",
        "/v1/members",
        "Content-Type: application/json\r\n",
    )
    .await;
    assert_eq!(code, 405);
}

/// A server whose state store is the hooked fake, which the test makes fail.
fn failing() -> (Arc<Hooked>, Server) {
    let hooked = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&hooked) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    (hooked, start(Some(state)))
}

/// The error `GET /v1/members` answers once `arm` has set a fault. The server reads the
/// store on its own timers too, and one of those reads can take the armed fault first;
/// the fault is armed again until the request itself meets it.
async fn error_of(api: SocketAddr, arm: impl Fn()) -> (u16, String) {
    for _ in 0..200 {
        arm();
        let (code, _, body) = http(api, "GET", "/v1/members", "").await;
        if code != 200 {
            let why = serde_json::from_str::<Value>(&body).expect("JSON")["error"]
                .as_str()
                .expect("an error")
                .to_owned();
            return (code, why);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the fault never reached the request");
}

/// Catches a store that cannot be read answered as an empty farm or as a server bug:
/// it is `503` (try again), a stored map that does not decode is `500` (the store is
/// up and its content is wrong), and a store that panics inside the blocking task is
/// `500` naming the task, not a dropped connection. Each one recovers once the fault
/// is gone.
#[tokio::test]
async fn a_store_that_fails_is_503_and_a_map_that_is_wrong_or_a_panic_is_500() {
    let (hooked, server) = failing();

    let (code, why) = error_of(server.api, || {
        hooked
            .fail_gets
            .store(1, std::sync::atomic::Ordering::SeqCst);
    })
    .await;
    assert_eq!(code, 503, "{why}");
    assert!(why.contains("injected"), "{why}");
    assert_eq!(
        members(server.api).await,
        json!({ "generation": 0, "members": [] })
    );

    let (code, why) = error_of(server.api, || {
        hooked
            .panic_gets
            .store(1, std::sync::atomic::Ordering::SeqCst);
    })
    .await;
    assert_eq!(code, 500, "{why}");
    assert!(why.contains("membership task"), "{why}");

    hooked
        .fake
        .txn(Txn::new().put("map", b"not a map".to_vec(), Cond::Absent))
        .expect("store")
        .expect("no conflict");
    let (code, why) = error_of(server.api, || ()).await;
    assert_eq!(code, 500, "{why}");
}
