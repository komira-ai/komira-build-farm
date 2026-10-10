//! The operator API (`/v1`) over a real listener, and the fleet view behind it.

mod support;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use kbf_caps::NodeCaps;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::worker::{NodeStatus, ServerMessage, XcodeState, XcodeStatus, daemon_message};
use kbf_server::api::{DRAIN_DEADLINE, Write, write_request};
use kbf_server::farm::NodeAction;
use kbf_server::fleet::SoftwareView;
use kbf_server::token::ApiToken;
use kbf_server::{
    Api, BUILD_COMMIT, Farm, Listeners, SERVER_VERSION, ServeError, bind_server_with_api,
};
use kbf_types::{Resources, WorkerId};
use serde_json::{Value, json};
use support::{Client, FakeDaemon, HELLO_WAIT, INTERVAL, Job, PROMPT, done, hello, output, ran};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

fn cache() -> Arc<MemoryCache> {
    Arc::new(Cache::memory())
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// A server with the operator API in this process.
struct Server {
    reapi: SocketAddr,
    worker: SocketAddr,
    api: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Shuts the server down and waits until it has stopped.
    async fn stop(self) {
        self.stop.send(()).expect("the server is running");
        self.serving.await.expect("the server stops cleanly");
    }
}

/// The token the test servers' writes need.
const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

/// A token file holding `content`, mode 0600, unique to this call.
fn token_file(content: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-api");
    std::fs::create_dir_all(&dir).expect("a token directory");
    let path = dir.join(format!("token-{}-{n}", std::process::id()));
    std::fs::write(&path, content).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    path
}

fn token() -> ApiToken {
    ApiToken::from_file(&token_file(&format!("{TOKEN}\n"))).expect("a usable token")
}

fn start() -> Server {
    start_with(Some(token()))
}

fn start_with(token: Option<ApiToken>) -> Server {
    let listeners = Listeners {
        reapi: loopback(),
        worker: loopback(),
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        let _ = stopped.await;
    };
    let api = Api {
        listen: loopback(),
        token,
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
    };
    let bound = bind_server_with_api(cache(), listeners, Some(api), shutdown).expect("bind");
    let (reapi, worker, api) = (
        bound.reapi,
        bound.worker,
        bound.api.expect("an API address"),
    );
    let serving = tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        reapi,
        worker,
        api,
        stop,
        serving,
    }
}

/// One HTTP/1.1 request; the status code and the body.
async fn http(api: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let (code, _, body) = http_with(api, method, path, "", body).await;
    (code, body)
}

/// One HTTP/1.1 request with `headers` (each line ending `\r\n`); the status code,
/// the response head and the body.
async fn http_with(
    api: SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
    body: &str,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let length = body.len();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: {length}\r\n\
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
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json")
            || code == 404
            || code == 405,
        "{head}"
    );
    (code, head.to_owned(), body.to_owned())
}

async fn nodes(api: SocketAddr) -> Value {
    let (code, body) = http(api, "GET", "/v1/nodes", "").await;
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).expect("JSON")
}

/// `GET /v1/nodes` until `done` holds for its body.
async fn nodes_until(api: SocketAddr, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + PROMPT;
    loop {
        let got = nodes(api).await;
        if done(&got) {
            return got;
        }
        assert!(tokio::time::Instant::now() < deadline, "never: {got}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_millis(),
    )
    .expect("fits")
}

fn linux_status(kernel: &str) -> NodeStatus {
    NodeStatus {
        os_name: "Ubuntu".to_owned(),
        os_version: "24.04".to_owned(),
        os_build: String::new(),
        kernel: kernel.to_owned(),
        daemon_version: "0.1.0".to_owned(),
        ..NodeStatus::default()
    }
}

/// A Mac with Xcode 16.2 ready and 16.1 installed but its licence not accepted.
fn mac_status() -> NodeStatus {
    NodeStatus {
        os_name: "macOS".to_owned(),
        os_version: "15.1".to_owned(),
        os_build: "24B83".to_owned(),
        kernel: String::new(),
        daemon_version: "0.1.0".to_owned(),
        xcode_builds: vec!["16C5032a".to_owned()],
        xcodes: vec![
            XcodeStatus {
                app: "/Applications/Xcode_16.1.app".to_owned(),
                build: "16B40".to_owned(),
                state: XcodeState::LicenseNotAccepted.into(),
                reason: "not agreed".to_owned(),
                fix: "sudo x -license accept".to_owned(),
            },
            XcodeStatus {
                app: "/Applications/Xcode_16.2.app".to_owned(),
                build: "16C5032a".to_owned(),
                state: XcodeState::Ready.into(),
                reason: String::new(),
                fix: String::new(),
            },
        ],
    }
}

/// What `/v1/nodes` and the log say of `mac_status`'s Xcode 16.1.
const NOT_READY: &str = "Xcode 16B40 (/Applications/Xcode_16.1.app) installed but not \
    ready: not agreed; fix: sudo x -license accept";

/// Catches: a body without the `server` field, or one that does not name this build
/// and its commit (a deploy could not check which commit runs); a `NodeStatus` the
/// server drops (the worker stream ignores it), fields mapped to the wrong JSON keys,
/// a node without status left out of the list or listed with an empty object instead
/// of `null`, an older status kept over a newer one, a node still listed as connected
/// after its stream ended, and an installed Xcode that is not ready missing from
/// `xcodes` or from `needs_attention` (issue #164).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_nodes_lists_each_nodes_newest_software() {
    let server = start();
    let (worker, api) = (server.worker, server.api);
    let this_build = json!({ "version": SERVER_VERSION, "commit": BUILD_COMMIT });
    assert_eq!(
        SERVER_VERSION,
        format!("{}+{BUILD_COMMIT}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        nodes(api).await,
        json!({ "server": this_build, "nodes": [] })
    );

    let before = now_ms();
    let linux = FakeDaemon::connect_without_status(worker, hello("linux-1", 8, 16))
        .await
        .expect("registered");
    linux.send(daemon_message::Message::NodeStatus(linux_status(
        "6.8.0-45-generic",
    )));
    let mac = FakeDaemon::connect_without_status(worker, hello("mac-1", 8, 16))
        .await
        .expect("registered");
    mac.send(daemon_message::Message::NodeStatus(mac_status()));
    // A daemon that predates NodeStatus.
    let old = FakeDaemon::connect_without_status(worker, hello("old-1", 8, 16))
        .await
        .expect("registered");

    let mut got = nodes_until(api, |v| {
        v["nodes"][0]["software"].is_object() && v["nodes"][1]["software"].is_object()
    })
    .await;
    let after = now_ms();
    for node in got["nodes"].as_array_mut().expect("a list").iter_mut() {
        if let Some(software) = node["software"].as_object_mut() {
            let at = software
                .remove("received_at_unix_ms")
                .and_then(|v| v.as_u64())
                .expect("a receive time");
            assert!(
                (before..=after).contains(&at),
                "{at} not in {before}..={after}"
            );
        }
    }
    assert_eq!(
        got,
        json!({ "server": this_build, "nodes": [
            { "node_id": "linux-1", "connected": true, "software": {
                "os_name": "Ubuntu", "os_version": "24.04", "os_build": "",
                "kernel": "6.8.0-45-generic", "daemon_version": "0.1.0",
                "xcode_builds": [], "xcodes": [] },
              "needs_attention": [],
              "placement": { "state": "serving" } },
            { "node_id": "mac-1", "connected": true, "software": {
                "os_name": "macOS", "os_version": "15.1", "os_build": "24B83",
                "kernel": "", "daemon_version": "0.1.0",
                "xcode_builds": ["16C5032a"],
                "xcodes": [
                    { "app": "/Applications/Xcode_16.1.app", "build": "16B40",
                      "state": "license_not_accepted", "reason": "not agreed",
                      "fix": "sudo x -license accept" },
                    { "app": "/Applications/Xcode_16.2.app", "build": "16C5032a",
                      "state": "ready", "reason": "", "fix": "" } ] },
              "needs_attention": [NOT_READY],
              "placement": { "state": "serving" } },
            { "node_id": "old-1", "connected": true, "software": null,
              "needs_attention": [],
              "placement": { "state": "serving" } },
        ] })
    );

    linux.send(daemon_message::Message::NodeStatus(linux_status(
        "6.8.0-50-generic",
    )));
    nodes_until(api, |v| {
        v["nodes"][0]["software"]["kernel"] == "6.8.0-50-generic"
    })
    .await;
    old.close();
    nodes_until(api, |v| v["nodes"][2]["connected"] == false).await;
    drop((linux, mac));
    server.stop().await;
}

/// Catches: an API address that cannot be bound being ignored (a server that starts
/// without the API it was asked for) instead of refused at start.
#[tokio::test]
async fn an_api_address_in_use_is_refused() {
    let taken = std::net::TcpListener::bind(loopback()).expect("bind a port");
    let addr = taken.local_addr().expect("its address");
    let listeners = Listeners {
        reapi: loopback(),
        worker: loopback(),
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
    };
    let api = Api {
        listen: addr,
        token: None,
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
    };
    let refused = bind_server_with_api(cache(), listeners, Some(api), std::future::pending());
    let Err(ServeError::Bind { addr: at, .. }) = refused else {
        panic!("an API address in use was not refused");
    };
    assert_eq!(at, addr);
}

fn register(
    farm: &Farm<MemoryMetaLog, MemoryStore>,
    node: &str,
) -> (
    kbf_server::farm::StreamId,
    mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>,
) {
    let (outbound, responses) = mpsc::unbounded_channel();
    let caps = NodeCaps::from_report([("arch", "x86_64")]).expect("caps");
    let resources = Resources::new(8_000, 16 << 30);
    let stream = farm.register(
        &WorkerId::new(node),
        kbf_sched::DaemonInstance::new(node),
        resources,
        caps,
        outbound,
        ServerMessage::default(),
    );
    (stream, responses)
}

/// Catches: a status from a replaced stream overwriting the newest stream's (a daemon
/// that restarted onto a new OS would be shown with its old software), and a status
/// for a node that never registered creating a node.
#[tokio::test]
async fn a_status_from_a_replaced_stream_is_ignored() {
    let farm = Farm::new(
        cache(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let node = WorkerId::new("linux-1");
    let (old, _old_rx) = register(&farm, "linux-1");
    let (new, _new_rx) = register(&farm, "linux-1");
    assert_eq!(farm.node_status(&node, new, linux_status("new")), []);
    assert_eq!(farm.node_status(&node, old, mac_status()), [], "not kept");
    let ghost = farm.node_status(&WorkerId::new("ghost"), new, mac_status());
    assert_eq!(ghost, []);
    let nodes = farm.nodes().nodes;
    assert_eq!(nodes.len(), 1);
    let software = nodes[0].software.clone().expect("a status");
    assert_eq!(
        software,
        SoftwareView::new(linux_status("new"), software.received_at_unix_ms)
    );
}

/// Catches: `connected` read from something other than the stream's response side
/// (a node whose stream is gone listed as connected).
#[tokio::test]
async fn a_node_whose_stream_ended_is_listed_as_disconnected() {
    let farm = Farm::new(
        cache(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let (_, responses) = register(&farm, "linux-1");
    assert!(farm.nodes().nodes[0].connected);
    drop(responses);
    assert!(!farm.nodes().nodes[0].connected);
}

/// The headers an operator's write carries.
fn operator() -> String {
    format!("Authorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\n")
}

/// An operator's write.
async fn post(api: SocketAddr, target: &str, body: &str) -> (u16, Value) {
    post_with(api, target, &operator(), body).await
}

async fn post_with(api: SocketAddr, target: &str, headers: &str, body: &str) -> (u16, Value) {
    let path = format!("/v1/nodes/{target}");
    let (code, _, text) = http_with(api, "POST", &path, headers, body).await;
    (code, serde_json::from_str(&text).expect("JSON"))
}

/// Catches: a cordon or drain that does not reach placement (new work still starts on
/// the node), a drain that cancels or gives up the running lease (its result would be
/// refused), a drain that never pauses at its deadline or resumes by itself, a drained
/// node that is not reported drained, an uncordon that leaves queued work stuck, and
/// writes to an unknown node or with an unknown verb or body that are not refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cordon_drain_and_uncordon_over_http() {
    let server = start();
    let api = server.api;
    let cell = Client::connect(server.reapi, server.worker).await;
    let mut node = FakeDaemon::connect(server.worker, hello("linux-1", 8, 16))
        .await
        .expect("registered");

    for (target, body, want) in [
        ("ghost:cordon", "", 404),
        ("linux-1:frobnicate", "", 404),
        ("linux-1", "", 404),
        ("linux-1:drain", "{\"deadline\": 3}", 400),
        ("linux-1:drain", "soon", 400),
    ] {
        let (code, body) = post(api, target, body).await;
        assert_eq!(code, want, "{target}: {body}");
        assert!(body["error"].is_string(), "{body}");
    }
    let (code, _) = http(api, "GET", "/v1/nodes/linux-1:cordon", "").await;
    assert_eq!(code, 405, "a write is a POST");

    let first = Job::new("first", &[]);
    let second = Job::new("second", &[]);
    for job in [&first, &second] {
        cell.upload(&job.blobs()).await;
    }
    let mut first_ops = cell.execute(&first.action).await;
    let lease = node.start().await.lease_id;
    let lease_name = lease
        .map(|l| format!("{}.{}", l.term, l.seq))
        .expect("a lease");

    let before = now_ms();
    let (code, view) = post(api, "linux-1:drain", "{\"deadline_secs\": 1}").await;
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["placement"]["state"], "draining", "{view}");
    assert_eq!(view["placement"]["leases"], json!([lease_name]));
    let deadline = view["placement"]["deadline_unix_ms"]
        .as_u64()
        .expect("a deadline");
    // The farm shows times as its wall-clock start plus farm time, each truncated to
    // the millisecond: allow those two milliseconds.
    assert!(
        (before + 1_000 - 2..=now_ms() + 1_000).contains(&deadline),
        "{deadline} not in {before} + 1 s"
    );

    // The node keeps the lease alive; past the deadline the drain pauses, kills nothing.
    let mut second_ops = cell.execute(&second.action).await;
    let paused = async {
        loop {
            assert!(node.heartbeat(&[lease.expect("a lease")]).await);
            let got = nodes(api).await;
            if got["nodes"][0]["placement"]["state"] == "drain_paused" {
                return got;
            }
        }
    };
    let got = tokio::time::timeout(PROMPT, paused)
        .await
        .expect("the drain pauses");
    assert_eq!(got["nodes"][0]["placement"]["leases"], json!([lease_name]));
    assert_eq!(got["nodes"][0]["placement"]["deadline_unix_ms"], deadline);
    assert_eq!(node.cancelled().await, None, "a drain never cancels");
    node.no_work().await;

    let built = output(&cell, "built", 0).await;
    let ack = node.report(ran(lease, &built)).await;
    assert!(ack.accepted, "the drained lease's result counts");
    done(&mut first_ops).await;
    let got = nodes(api).await;
    assert_eq!(
        got["nodes"][0]["placement"]["state"], "drain_paused",
        "nothing proceeds by itself: {got}"
    );

    let (_, view) = post(api, "linux-1:drain", "").await;
    assert_eq!(view["placement"], json!({ "state": "drained" }));
    node.no_work().await;

    let (_, view) = post(api, "linux-1:uncordon", "").await;
    assert_eq!(view["placement"], json!({ "state": "serving" }));
    let start = node.start().await;
    assert_eq!(start.action_digest.as_ref(), Some(&second.action.proto));
    let ack = node.report(ran(start.lease_id, &built)).await;
    assert!(ack.accepted);
    done(&mut second_ops).await;

    let (_, view) = post(api, "linux-1:cordon", "").await;
    assert_eq!(view["placement"], json!({ "state": "cordoned" }));
    server.stop().await;
}

/// Catches: the loopback check removed or inverted (a remote caller could send the
/// token in clear, or act without the proxy), and an IPv4 loopback peer seen through
/// an IPv6 socket refused.
#[test]
fn writes_are_accepted_only_from_loopback() {
    let token = token();
    let headers = headers(&[("authorization", &format!("Bearer {TOKEN}")), JSON]);
    let write = |peer: &str, target: &'static str, body: &'static [u8]| {
        let peer: SocketAddr = peer.parse().expect("an address");
        let request = Write {
            peer,
            headers: &headers,
            target,
            body,
        };
        write_request(Some(&token), &request)
    };
    for peer in ["127.0.0.1:4000", "[::1]:4000", "[::ffff:127.0.0.1]:4000"] {
        let (node, action) = write(peer, "mac-1:cordon", b"").expect("allowed");
        assert_eq!((node.as_str(), action), ("mac-1", NodeAction::Cordon));
    }
    for peer in [
        "192.0.2.7:4000",
        "[2001:db8::7]:4000",
        "[::ffff:192.0.2.7]:4000",
    ] {
        let (code, why) = write(peer, "mac-1:cordon", b"").expect_err("refused");
        assert_eq!(code, StatusCode::FORBIDDEN, "{why}");
    }
    let local = "127.0.0.1:1";
    assert_eq!(
        write(local, "mac-1:drain", b"").map(|(_, a)| a),
        Ok(NodeAction::Drain(DRAIN_DEADLINE))
    );
    assert_eq!(
        write(local, "mac-1:drain", b"{\"deadline_secs\": 90}").map(|(_, a)| a),
        Ok(NodeAction::Drain(Duration::from_secs(90)))
    );
    assert_eq!(
        write(local, "a:b:uncordon", b"").map(|(n, a)| (n.as_str().to_owned(), a)),
        Ok(("a:b".to_owned(), NodeAction::Uncordon))
    );
}

const JSON: (&str, &str) = ("content-type", "application/json");

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for &(name, value) in pairs {
        let name: axum::http::HeaderName = name.parse().expect("a header name");
        map.append(name, HeaderValue::from_str(value).expect("a header value"));
    }
    map
}

/// Catches, gate by gate, a write let through without what it needs, each on its own
/// so that removing any one gate turns this red:
/// - the token check removed (any local process, such as a build action on this host,
///   could cordon the fleet), a wrong, empty or scheme-less token accepted, or a prefix or
///   extension of the token accepted;
/// - an `Origin` header allowed (a page in a browser on the host could POST);
/// - any content type accepted (a browser's cross-origin `text/plain` or form POST
///   needs no preflight);
/// - writes allowed when the server has no token.
#[test]
fn a_write_passes_every_gate_or_is_refused() {
    let token = token();
    let bearer = format!("Bearer {TOKEN}");
    let check = |token: Option<&ApiToken>, pairs: &[(&str, &str)]| {
        let headers = headers(pairs);
        let request = Write {
            peer: "127.0.0.1:4000".parse().expect("an address"),
            headers: &headers,
            target: "mac-1:cordon",
            body: b"",
        };
        write_request(token, &request).map_err(|(code, _)| code)
    };
    let allowed = Ok((WorkerId::new("mac-1"), NodeAction::Cordon));
    assert_eq!(
        check(Some(&token), &[("authorization", &bearer), JSON]),
        allowed
    );
    let lower = format!("bearer   {TOKEN}");
    let charset = ("content-type", "Application/JSON ; charset=utf-8");
    assert_eq!(
        check(Some(&token), &[("authorization", &lower), charset]),
        allowed,
        "the scheme in any case, a JSON type with parameters"
    );

    let short = &bearer[..bearer.len() - 1];
    let longer = format!("{bearer}0");
    let basic = format!("Basic {TOKEN}");
    for presented in [
        None,
        Some(""),
        Some("Bearer"),
        // An empty token: HTTP/1 strips the trailing space, h2c can deliver it.
        Some("Bearer "),
        Some(short),
        Some(&longer),
        Some(&basic),
        Some(TOKEN),
    ] {
        let mut pairs = vec![JSON];
        pairs.extend(presented.map(|p| ("authorization", p)));
        assert_eq!(
            check(Some(&token), &pairs),
            Err(StatusCode::UNAUTHORIZED),
            "{presented:?}"
        );
    }
    for origin in ["http://localhost:3000", "null"] {
        let pairs = [("authorization", bearer.as_str()), JSON, ("origin", origin)];
        assert_eq!(check(Some(&token), &pairs), Err(StatusCode::FORBIDDEN));
    }
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
        Some("application/jsonp"),
    ] {
        let mut pairs = vec![("authorization", bearer.as_str())];
        pairs.extend(content_type.map(|c| ("content-type", c)));
        assert_eq!(
            check(Some(&token), &pairs),
            Err(StatusCode::UNSUPPORTED_MEDIA_TYPE),
            "{content_type:?}"
        );
    }
    assert_eq!(
        check(None, &[("authorization", &bearer), JSON]),
        Err(StatusCode::FORBIDDEN),
        "no token: writes are off"
    );
}

/// Catches the same gates over a real listener: the handler not passing the headers
/// to them, a 401 without `WWW-Authenticate: Bearer`, a server given no token that
/// accepts writes, and a refused write that changed the node anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_over_http_need_the_token_no_origin_and_json() {
    let server = start();
    let api = server.api;
    let node = FakeDaemon::connect(server.worker, hello("linux-1", 8, 16))
        .await
        .expect("registered");
    let bearer = format!("Authorization: Bearer {TOKEN}\r\n");
    let json = "Content-Type: application/json\r\n";
    let wrong = "Authorization: Bearer kbf-test-token-wrong-0123456789abcdef\r\n";
    let origin = "Origin: http://localhost:3000\r\n";
    for (headers, want) in [
        (json.to_owned(), 401),
        (format!("{wrong}{json}"), 401),
        (format!("{bearer}{json}{origin}"), 403),
        (format!("{bearer}Content-Type: text/plain\r\n"), 415),
        (bearer.clone(), 415),
    ] {
        let path = "/v1/nodes/linux-1:cordon";
        let (code, head, body) = http_with(api, "POST", path, &headers, "").await;
        assert_eq!(code, want, "{headers}: {body}");
        if code == 401 {
            let head = head.to_ascii_lowercase();
            assert!(head.contains("www-authenticate: bearer"), "{head}");
        }
        let got = nodes(api).await;
        assert_eq!(got["nodes"][0]["placement"]["state"], "serving", "{got}");
    }
    let (code, view) = post_with(api, "linux-1:cordon", &format!("{bearer}{json}"), "").await;
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["placement"]["state"], "cordoned");
    server.stop().await;
    drop(node);

    let server = start_with(None);
    let (code, body) = post(server.api, "ghost:cordon", "").await;
    assert_eq!(code, 403, "{body}");
    let why = body["error"].as_str().unwrap_or_default();
    assert!(why.contains("--api-token-file"), "{why}");
    server.stop().await;
}

/// Catches: `docs/api.md` wrong about the protocols the listener speaks. It answers
/// HTTP/2 in clear text (h2c, prior knowledge) as well as HTTP/1.1, so the write
/// gates must hold for both; they read only the request's peer and headers, which
/// both carry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_api_listener_also_speaks_h2c() {
    let server = start();
    let mut stream = TcpStream::connect(server.api).await.expect("connect");
    // The HTTP/2 connection preface and an empty SETTINGS frame.
    let mut preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    preface.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
    stream.write_all(&preface).await.expect("send");
    let mut head = [0u8; 9];
    tokio::time::timeout(PROMPT, stream.read_exact(&mut head))
        .await
        .expect("an answer")
        .expect("a frame header");
    assert_eq!(head[3], 4, "the server's first frame is SETTINGS: {head:?}");
    drop(stream);
    server.stop().await;
}

/// Catches (issue #164): an Xcode that is not ready raised on every status that repeats
/// it (each new stream sends one), or never raised; one that became ready never
/// cleared; and an item compared against another node's status.
#[tokio::test]
async fn an_attention_item_is_logged_once_per_change() {
    let farm = Farm::new(
        cache(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let node = WorkerId::new("mac-1");
    let (first, _first_rx) = register(&farm, "mac-1");
    let raised = vec![(true, format!("node mac-1: {NOT_READY}"))];
    assert_eq!(farm.node_status(&node, first, mac_status()), raised);
    assert_eq!(farm.node_status(&node, first, mac_status()), []);
    let (other, _other_rx) = register(&farm, "mac-2");
    let other_raised = farm.node_status(&WorkerId::new("mac-2"), other, mac_status());
    assert_eq!(other_raised, [(true, format!("node mac-2: {NOT_READY}"))]);
    // A new stream sends its status again: nothing new.
    let (second, _second_rx) = register(&farm, "mac-1");
    assert_eq!(farm.node_status(&node, second, mac_status()), []);
    let mut accepted = mac_status();
    accepted.xcodes[0].state = XcodeState::Ready.into();
    let cleared = vec![(false, format!("node mac-1: resolved: {NOT_READY}"))];
    assert_eq!(farm.node_status(&node, second, accepted), cleared);
    let listed = farm.node_view(&node).expect("listed");
    assert_eq!(listed.needs_attention, Vec::<String>::new());
    assert_eq!(farm.node_status(&node, second, mac_status()), raised);
}

/// `mac_status` as a daemon sends it from its start until its first survey ends: every
/// Xcode found, none asked yet (no build, no fix).
fn not_surveyed_status() -> NodeStatus {
    let mut status = mac_status();
    status.xcode_builds.clear();
    for xcode in &mut status.xcodes {
        xcode.build.clear();
        xcode.fix.clear();
        xcode.state = XcodeState::NotSurveyed.into();
        xcode.reason = "not surveyed yet".to_owned();
    }
    status
}

/// Catches (review of PR #258): a restarted daemon, whose first status lists every
/// Xcode as not surveyed yet, logging a false "resolved" for an Xcode that is still
/// not ready, dropping it from `needs_attention` until its survey ends, and raising it
/// again when the survey reports it unchanged; and an Xcode fixed while its daemon was
/// down not cleared once its survey says so.
#[tokio::test]
async fn a_restarted_daemon_neither_clears_nor_raises_an_unchanged_item() {
    let farm = Farm::new(
        cache(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let node = WorkerId::new("mac-1");
    let (first, _first_rx) = register(&farm, "mac-1");
    let raised = vec![(true, format!("node mac-1: {NOT_READY}"))];
    assert_eq!(farm.node_status(&node, first, mac_status()), raised);
    // The daemon restarts: a new stream, whose first status precedes its survey.
    let (second, _second_rx) = register(&farm, "mac-1");
    assert_eq!(farm.node_status(&node, second, not_surveyed_status()), []);
    let listed = farm.node_view(&node).expect("listed");
    assert_eq!(
        listed.needs_attention,
        [NOT_READY],
        "kept while not surveyed"
    );
    let software = listed.software.expect("software");
    assert_eq!(software.xcodes[0].state, "not_surveyed", "shown as sent");
    assert_eq!(farm.node_status(&node, second, not_surveyed_status()), []);
    assert_eq!(
        farm.node_status(&node, second, mac_status()),
        [],
        "same item"
    );
    let listed = farm.node_view(&node).expect("listed");
    assert_eq!(listed.needs_attention, [NOT_READY]);
    // Restarted again, and fixed while it was down: cleared once its survey says so.
    let (third, _third_rx) = register(&farm, "mac-1");
    assert_eq!(farm.node_status(&node, third, not_surveyed_status()), []);
    let mut accepted = mac_status();
    accepted.xcodes[0].state = XcodeState::Ready.into();
    let cleared = vec![(false, format!("node mac-1: resolved: {NOT_READY}"))];
    assert_eq!(farm.node_status(&node, third, accepted), cleared);
    let listed = farm.node_view(&node).expect("listed");
    assert_eq!(listed.needs_attention, Vec::<String>::new());
}
