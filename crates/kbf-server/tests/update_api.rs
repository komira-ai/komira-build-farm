//! The desired release over the operator API (`docs/design/pull-updates.md` section 6.3):
//! `GET /v1/update`, `POST /v1/update/desired`, `pause` and `resume`, over real listeners and
//! the `update/desired` row of a state store.

mod support;

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_server::members::MembershipConfig;
use kbf_server::release::{Limits, ReleaseStore};
use kbf_server::token::ApiToken;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use serde_json::{Value, json};
use support::{HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "kbf-server-release-mount-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("a test directory");
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

struct Running {
    api: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn state() -> StateStore {
    let clock = Arc::new(ManualClock::default());
    let fake = Arc::new(Fake::with_flaw(clock, Flaw::None));
    StateStore {
        store: fake as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: MembershipConfig {
            ttl: Duration::from_secs(30),
            dwell: Duration::from_secs(120),
            replication: 1,
        },
        history: None,
    }
}

fn start(releases: Option<Arc<ReleaseStore>>, state: Option<StateStore>, dir: &Dir) -> Running {
    let path = dir.0.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: Some(ApiToken::from_file(&path).expect("a usable token")),
        local_cache: None,
        releases,
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
    Running {
        api,
        stop: Some(stop),
    }
}

/// One request; the status and the JSON body (`null` for none).
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    auth: bool,
    content_type: Option<&str>,
    body: &[u8],
) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if auth {
        head.push_str(&format!("Authorization: Bearer {TOKEN}\r\n"));
    }
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.expect("write");
    stream.write_all(body).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text[9..12].parse().expect("a status");
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

const A: &str = "0123456789abcdef0123456789abcdef01234567";
const B: &str = "fedcba9876543210fedcba9876543210fedcba98";

/// Offer `commit`: the status and body.
async fn offer(running: &Running, commit: &str) -> (u16, Value) {
    let body = json!({ "commit": commit }).to_string();
    http(
        running.api,
        "POST",
        "/v1/update/desired",
        true,
        Some("application/json"),
        body.as_bytes(),
    )
    .await
}

/// Store the tarball of `commit`, and its attestation when `attested`.
async fn put(running: &Running, commit: &str, attested: bool) {
    let path = format!("/v1/releases/{commit}/linux-x86_64/kbf");
    let (code, _) = http(running.api, "PUT", &path, true, None, b"a tarball").await;
    assert_eq!(code, 201);
    if attested {
        let (code, _) = http(
            running.api,
            "PUT",
            &format!("{path}.attestation"),
            true,
            None,
            b"{\"bundle\":1}",
        )
        .await;
        assert_eq!(code, 201);
    }
}

/// Catches a desired release set for something the hosts could not fetch and verify, a
/// repeated click starting a new run, and a rollback that does not outrank what it undoes:
/// a commit with no complete platform (no attestation yet, or nothing stored) is 409; the
/// first complete commit is generation 1; the same commit again keeps its generation; a
/// second commit is 2 and the first again, a rollback, is 3. The desired release is read
/// back by `GET /v1/update` with the stored releases.
///
/// Mutants seen red: the completeness check dropped (the unattested commit is accepted);
/// the generation not moved on a different commit.
#[tokio::test]
async fn a_complete_commit_is_offered_a_repeat_keeps_the_generation_a_rollback_outranks() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let running = start(Some(store), Some(state()), &dir);
    let (code, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(code, 200);
    assert_eq!(view["desired"], Value::Null);
    assert_eq!(offer(&running, A).await.0, 409, "nothing stored");
    put(&running, A, false).await;
    assert_eq!(offer(&running, A).await.0, 409, "no attestation yet");
    put(&running, B, true).await;
    let (code, first) = offer(&running, B).await;
    assert_eq!((code, first["generation"].clone()), (200, json!(1)));
    assert_eq!(first["commit"], B);
    assert_eq!(first["paused"], false);
    assert_eq!(
        offer(&running, B).await.1["generation"],
        1,
        "a repeat keeps it"
    );
    // A is attested at last; offered, it is a new run; B again is a rollback and outranks both.
    let path = format!("/v1/releases/{A}/linux-x86_64/kbf.attestation");
    assert_eq!(
        http(running.api, "PUT", &path, true, None, b"{\"bundle\":2}")
            .await
            .0,
        201
    );
    assert_eq!(offer(&running, A).await.1["generation"], 2);
    assert_eq!(offer(&running, B).await.1["generation"], 3);
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(view["desired"]["commit"], B);
    assert_eq!(view["desired"]["generation"], 3);
    assert_eq!(
        view["releases"]["releases"].as_array().map(Vec::len),
        Some(2),
        "{view}"
    );
}

/// Catches a write that skips a gate or a body that is not what it says: no token is 401, a
/// body that is not JSON-typed is 415, a commit that is not 40 lowercase hex or a body with
/// other fields is 400, and none of them moves the row.
#[tokio::test]
async fn the_desired_release_is_a_gated_json_write() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let running = start(Some(store), Some(state()), &dir);
    put(&running, A, true).await;
    let body = json!({ "commit": A }).to_string();
    let post = |auth: bool, ct: Option<&'static str>, body: Vec<u8>| {
        let api = running.api;
        async move {
            http(api, "POST", "/v1/update/desired", auth, ct, &body)
                .await
                .0
        }
    };
    assert_eq!(
        post(false, Some("application/json"), body.clone().into_bytes()).await,
        401
    );
    assert_eq!(post(true, None, body.clone().into_bytes()).await, 415);
    assert_eq!(post(true, Some("text/plain"), body.into_bytes()).await, 415);
    assert_eq!(
        post(
            true,
            Some("application/json"),
            b"{\"commit\":\"XYZ\"}".to_vec()
        )
        .await,
        400
    );
    assert_eq!(
        post(
            true,
            Some("application/json"),
            br#"{"commit":"0123456789abcdef0123456789abcdef01234567","x":1}"#.to_vec()
        )
        .await,
        400
    );
    assert_eq!(
        post(true, Some("application/json"), b"not json".to_vec()).await,
        400
    );
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(
        view["desired"],
        Value::Null,
        "no refused write moved the row"
    );
}

/// Catches pause and resume: a run that does not exist cannot be paused (409), a paused run
/// says so in `GET /v1/update`, a new offer keeps it paused, and resume clears it; both need
/// the token.
#[tokio::test]
async fn pause_and_resume_set_the_run_flag() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let running = start(Some(store), Some(state()), &dir);
    let pause = |auth: bool, which: &'static str| {
        let api = running.api;
        async move { http(api, "POST", &format!("/v1/update/{which}"), auth, None, b"").await }
    };
    assert_eq!(pause(true, "pause").await.0, 409, "no run yet");
    put(&running, A, true).await;
    assert_eq!(offer(&running, A).await.0, 200);
    assert_eq!(pause(false, "pause").await.0, 401);
    assert_eq!(pause(true, "pause").await, (200, json!({ "paused": true })));
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(view["desired"]["paused"], true);
    assert_eq!(
        pause(true, "resume").await,
        (200, json!({ "paused": false }))
    );
}

/// Catches an answer made up for a server that cannot keep the row or hold the releases:
/// without a state store the desired release is 503 naming `--state-store`, without a
/// release store it is 503 naming `--releases-dir`.
#[tokio::test]
async fn without_a_state_store_or_a_release_store_the_update_routes_say_so() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let no_rows = start(Some(Arc::clone(&store)), None, &dir);
    let (code, body) = http(no_rows.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(code, 503);
    assert!(
        body["error"]
            .as_str()
            .expect("text")
            .contains("--state-store"),
        "{body}"
    );
    assert_eq!(offer(&no_rows, A).await.0, 503);
    let no_store = start(None, Some(state()), &dir);
    let (code, body) = http(no_store.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(code, 503);
    assert!(
        body["error"]
            .as_str()
            .expect("text")
            .contains("--releases-dir"),
        "{body}"
    );
}
