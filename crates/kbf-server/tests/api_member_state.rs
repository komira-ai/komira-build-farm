//! `POST /v1/members/{id}` over a real listener: an operator sets a storage member's
//! state through the `Members` the `Membership` service registers into, behind the same
//! gates as every other write.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::{Map, MemberId, MemberState};
use kbf_objstore::MemoryStore;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::token::ApiToken;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use support::{HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const OPERATOR: &str = "Authorization: Bearer kbf-test-token-0123456789abcdef0123456789\r\nContent-Type: application/json\r\n";

/// One replica, rows living 30 s.
fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 1,
    }
}

fn token() -> ApiToken {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-api-members");
    std::fs::create_dir_all(&dir).expect("a token directory");
    let path = dir.join(format!("token-{}-{n}", std::process::id()));
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
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
        token: Some(token()),
        local_cache: None,
        releases: None,
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

/// A server over a store with one `Joining` member (id 1) set to `Joining` through another
/// frontend, and that frontend to read the result with.
fn joined() -> (Server, Members, MemberId) {
    let clock = Arc::new(ManualClock::default());
    let fake = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let state = StateStore {
        store: Arc::clone(&fake) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let other = Members::new(fake as Arc<dyn Store>, config());
    let registered = other
        .register(&Registrant {
            instance_id: "node-1".to_owned(),
            addr: "192.0.2.1:8982".to_owned(),
            zone: String::new(),
            weight: 1,
            protocol: PROTOCOL,
            progress: Progress::CaughtUp,
        })
        .expect("register");
    // A registration is Active; the operator promotes a Joining member, so make one.
    let map = other
        .set_state(registered.id, MemberState::Joining)
        .expect("joining");
    assert_eq!(
        map.member(registered.id).map(|m| m.state),
        Some(MemberState::Joining)
    );
    (start(Some(state)), other, registered.id)
}

fn map_of(members: &Members) -> Map {
    members.current_map().expect("read").expect("a map")
}

/// One HTTP/1.1 request; the status code and the body.
async fn post(api: SocketAddr, path: &str, headers: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let length = body.len();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: {length}\r\n\
         {headers}Connection: close\r\n\r\n{body}"
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
    (code, body.to_owned())
}

/// Catches: a promotion that answers 200 without reaching the store, one that does not
/// bump the map's generation, and a body field that is read wrongly (`draining` taken as
/// `active`): a joiner is set `active`, then `draining`, and the map read through another
/// frontend says so each time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operator_promotes_a_joining_member() {
    let (server, other, id) = joined();
    let before = map_of(&other).generation();
    let (code, body) = post(
        server.api,
        "/v1/members/1",
        OPERATOR,
        r#"{"state":"active"}"#,
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let map = map_of(&other);
    assert_eq!(map.member(id).map(|m| m.state), Some(MemberState::Active));
    assert!(map.generation() > before);
    assert!(body.contains(r#""state":"active""#), "{body}");
    let (code, body) = post(
        server.api,
        "/v1/members/1",
        OPERATOR,
        r#"{"state":"draining"}"#,
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        map_of(&other).member(id).map(|m| m.state),
        Some(MemberState::Draining)
    );
}

/// Catches: the member write skipping a gate the node writes pass: no token, a wrong
/// token, a browser's `Origin`, or a body that is not JSON must each be refused, and the
/// member must stay `Joining`. Mutant: the `gate` call removed from `set_member_state`
/// turns the first three red.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_write_is_refused_without_the_gates() {
    let (server, other, id) = joined();
    let want = r#"{"state":"active"}"#;
    let wrong = "Authorization: Bearer not-the-token-not-the-token-000000000\r\n\
                 Content-Type: application/json\r\n";
    let origin = format!("{OPERATOR}Origin: http://evil.example\r\n");
    let plain = "Authorization: Bearer kbf-test-token-0123456789abcdef0123456789\r\n";
    for (headers, refused) in [
        ("Content-Type: application/json\r\n", 401),
        (wrong, 401),
        (origin.as_str(), 403),
        (plain, 415),
    ] {
        let (code, body) = post(server.api, "/v1/members/1", headers, want).await;
        assert_eq!(code, refused, "{headers}: {body}");
    }
    assert_eq!(
        map_of(&other).member(id).map(|m| m.state),
        Some(MemberState::Joining)
    );
}

/// Catches: `Down` or a registration-only state being settable over HTTP, an id that is
/// not a member answering 200, and a malformed body not answering 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_write_names_a_settable_state_of_a_live_member() {
    let (server, other, id) = joined();
    for (path, body, want) in [
        ("/v1/members/1", r#"{"state":"down"}"#, 400),
        ("/v1/members/1", r#"{"state":"joining"}"#, 400),
        ("/v1/members/1", r#"{"state":"active","extra":1}"#, 400),
        ("/v1/members/1", "", 400),
        ("/v1/members/9", r#"{"state":"active"}"#, 404),
        ("/v1/members/one", r#"{"state":"active"}"#, 404),
    ] {
        let (code, text) = post(server.api, path, OPERATOR, body).await;
        assert_eq!(code, want, "{path} {body}: {text}");
    }
    assert_eq!(
        map_of(&other).member(id).map(|m| m.state),
        Some(MemberState::Joining)
    );
}

/// Catches: a server without a state store answering a member write as if it had members.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_without_members_answers_404() {
    let server = start(None);
    let (code, body) = post(
        server.api,
        "/v1/members/1",
        OPERATOR,
        r#"{"state":"active"}"#,
    )
    .await;
    assert_eq!(code, 404, "{body}");
}
