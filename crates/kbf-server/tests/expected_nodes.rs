//! `--expected-nodes`: nodes the server expects are listed in `GET /v1/nodes` as
//! `absent` until they register, the file is read again when its metadata changes, a
//! failed reload keeps the last list, and a bad file stops the server at start.

mod support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use kbf_caps::NodeCaps;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_meta::Retention;
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_proto::worker::ServerMessage;
use kbf_server::expected::{ExpectedNodes, ExpectedNodesError, MAX_EXPECTED_NODES_BYTES};
use kbf_server::token::ApiToken;
use kbf_server::{Api, Args, ConfigError, Farm, Listeners, bind_server_with_api};
use kbf_types::{Resources, WorkerId};
use serde_json::{Value, json};
use support::{FakeDaemon, HELLO_WAIT, INTERVAL, PROMPT, hello};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn cache() -> Arc<Cache<MemoryMetaLog, MemoryStore>> {
    Arc::new(Cache::new(
        MemoryMetaLog::new(Retention::default()),
        MemoryStore::new(Capabilities::default()),
        KeyPrefix::default(),
    ))
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn now_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970");
    u64::try_from(since.as_millis()).expect("fits")
}

/// A path unique to this call, in a directory of its own.
fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-server-expected")
        .join(format!("{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir.join(name)
}

/// Replaces `path` with `text` as an editor or config tool does: a new file renamed
/// over it.
fn replace(path: &Path, text: &str) {
    let next = path.with_extension("next");
    std::fs::write(&next, text).expect("write");
    std::fs::rename(&next, path).expect("rename");
}

struct Server {
    worker: SocketAddr,
    api: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

/// The token the test servers' writes need.
const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

fn token() -> ApiToken {
    use std::os::unix::fs::PermissionsExt;
    let path = scratch("token");
    std::fs::write(&path, TOKEN).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
}

/// A server with the operator API, expecting the nodes the file at `path` lists.
fn start(path: &Path) -> Server {
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
    let expected = ExpectedNodes::open(path).expect("a usable file");
    let api = Api {
        listen: loopback(),
        token: Some(token()),
        expected_nodes: Some(Arc::new(expected)),
    };
    let bound = bind_server_with_api(cache(), listeners, Some(api), shutdown).expect("bind");
    let (worker, api) = (bound.worker, bound.api.expect("an API address"));
    let serving = tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        worker,
        api,
        stop,
        serving,
    }
}

impl Server {
    async fn stop(self) {
        self.stop.send(()).expect("the server is running");
        self.serving.await.expect("the server stops cleanly");
    }
}

/// One HTTP/1.1 request; the status code and the body.
async fn http(api: SocketAddr, request: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, serde_json::from_str(body).expect("JSON"))
}

async fn nodes(api: SocketAddr) -> Value {
    let request = "GET /v1/nodes HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    let (code, body) = http(api, request).await;
    assert_eq!(code, 200, "{body}");
    body
}

/// An operator's cordon of `node`.
async fn cordon(api: SocketAddr, node: &str) -> (u16, Value) {
    let request = format!(
        "POST /v1/nodes/{node}:cordon HTTP/1.1\r\nHost: kbf\r\nContent-Length: 0\r\n\
         Authorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\n\
         Connection: close\r\n\r\n"
    );
    http(api, &request).await
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

/// The node `id` in a `GET /v1/nodes` body.
fn node<'a>(body: &'a Value, id: &str) -> &'a Value {
    let nodes = body["nodes"].as_array().expect("a list");
    nodes
        .iter()
        .find(|n| n["node_id"] == id)
        .unwrap_or_else(|| panic!("{id} is not listed: {body}"))
}

fn ids(body: &Value) -> Vec<&str> {
    let nodes = body["nodes"].as_array().expect("a list");
    nodes.iter().filter_map(|n| n["node_id"].as_str()).collect()
}

/// When the node `id` became absent.
fn since(body: &Value, id: &str) -> u64 {
    let placement = &node(body, id)["placement"];
    assert_eq!(placement["state"], "absent", "{body}");
    placement["since_unix_ms"].as_u64().expect("a since time")
}

/// Catches (the mutant this file exists for): absent nodes omitted from the list, so a
/// node that does not come back after a restart vanishes; an absent node shown as
/// connected, with software, or without the time it has been expected since; a node
/// that registers still shown as absent; `expected` set on a node the file does not
/// list; a node that registered and then disconnected turned back into `absent`,
/// which would hide its placement; a write's answer not marked `expected`; and a
/// write accepted for an absent node, which the scheduler has never seen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nodes_listed_but_not_registered_are_absent() {
    let path = scratch("expected");
    std::fs::write(&path, "# hosts\nmac-9\nlinux-1   # the build host\n").expect("write");
    let before = now_ms();
    let server = start(&path);
    let after = now_ms();
    let (worker, api) = (server.worker, server.api);

    let got = nodes(api).await;
    let mac_since = since(&got, "mac-9");
    assert!((before..=after).contains(&mac_since), "{got}");
    assert_eq!(
        got,
        json!({ "nodes": [
            { "node_id": "linux-1", "connected": false, "expected": true,
              "last_seen_unix_ms": null, "software": null,
              "placement": { "state": "absent", "since_unix_ms": since(&got, "linux-1") } },
            { "node_id": "mac-9", "connected": false, "expected": true,
              "last_seen_unix_ms": null, "software": null,
              "placement": { "state": "absent", "since_unix_ms": mac_since } },
        ], "expected_nodes_error": null })
    );

    let linux = FakeDaemon::connect(worker, hello("linux-1", 8, 16))
        .await
        .expect("registered");
    let stray = FakeDaemon::connect(worker, hello("stray-1", 8, 16))
        .await
        .expect("registered");
    let got = nodes_until(api, |v| v["nodes"].as_array().is_some_and(|n| n.len() == 3)).await;
    assert_eq!(ids(&got), ["linux-1", "mac-9", "stray-1"], "{got}");
    let registered = node(&got, "linux-1");
    assert_eq!(registered["connected"], true, "{got}");
    assert_eq!(registered["expected"], true, "{got}");
    assert_eq!(registered["placement"], json!({ "state": "serving" }));
    assert!(registered["last_seen_unix_ms"].as_u64().is_some(), "{got}");
    assert_eq!(node(&got, "stray-1")["expected"], false, "{got}");
    assert_eq!(
        since(&got, "mac-9"),
        mac_since,
        "the since time moved: {got}"
    );

    linux.close();
    let got = nodes_until(api, |v| node(v, "linux-1")["connected"] == false).await;
    let gone = node(&got, "linux-1");
    assert_eq!(gone["placement"], json!({ "state": "serving" }), "{got}");
    assert!(gone["last_seen_unix_ms"].as_u64().is_some(), "{got}");

    let (code, view) = cordon(api, "linux-1").await;
    assert_eq!(code, 200, "{view}");
    assert_eq!(
        view["expected"], true,
        "a write's answer is marked too: {view}"
    );
    assert_eq!(view["placement"], json!({ "state": "cordoned" }));
    let (code, refused) = cordon(api, "mac-9").await;
    assert_eq!(code, 404, "an absent node has never registered: {refused}");
    drop(stray);
    server.stop().await;
}

/// Catches: a file read only at start (an edit never seen), a reload that resets the
/// since time of a node it still lists, a removed node still listed, and a failed
/// reload (the file gone, or a line that does not parse) that empties the list, which
/// would silently drop every absent node, or that is not reported; and an error kept
/// after the file is fixed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_is_read_again_and_a_failed_read_keeps_the_last_list() {
    let path = scratch("expected");
    std::fs::write(&path, "linux-1\n").expect("write");
    let server = start(&path);
    let api = server.api;
    let first = since(&nodes(api).await, "linux-1");

    tokio::time::sleep(Duration::from_millis(5)).await;
    let edited = now_ms();
    replace(&path, "linux-1\nlinux-2\n");
    let got = nodes(api).await;
    assert_eq!(ids(&got), ["linux-1", "linux-2"], "{got}");
    assert_eq!(since(&got, "linux-1"), first, "{got}");
    assert!(since(&got, "linux-2") >= edited, "{got}");
    let second = since(&got, "linux-2");

    // Edited in place: a longer file.
    std::fs::write(&path, "linux-2\nlinux-3\n# linux-1 is retired\n").expect("write");
    let got = nodes(api).await;
    assert_eq!(ids(&got), ["linux-2", "linux-3"], "{got}");
    assert_eq!(since(&got, "linux-2"), second, "{got}");

    std::fs::remove_file(&path).expect("remove");
    // Twice: the second read fails the same way.
    for _ in 0..2 {
        let got = nodes(api).await;
        assert_eq!(ids(&got), ["linux-2", "linux-3"], "{got}");
        let why = got["expected_nodes_error"].as_str().expect("an error");
        assert!(why.starts_with("read "), "{why}");
    }

    replace(&path, "linux-2\nlinux-3 linux-4\n");
    let got = nodes(api).await;
    assert_eq!(ids(&got), ["linux-2", "linux-3"], "{got}");
    let why = got["expected_nodes_error"].as_str().expect("an error");
    assert!(why.ends_with(":2: a line holds one node id"), "{why}");

    replace(&path, "linux-2\nlinux-4\n");
    let got = nodes(api).await;
    assert_eq!(ids(&got), ["linux-2", "linux-4"], "{got}");
    assert_eq!(got["expected_nodes_error"], Value::Null, "{got}");
    assert_eq!(since(&got, "linux-2"), second, "{got}");
    server.stop().await;
}

/// Catches: a missing, unparsable, non-UTF-8, oversized or non-regular file accepted
/// at start (a server that runs believing it expects nodes it does not), a file of
/// exactly the limit refused, a FIFO waited on instead of refused, and the flag
/// accepted without the API that shows it.
#[test]
fn a_bad_file_stops_the_server_at_start() {
    let missing = scratch("missing");
    let refused = |path: &Path| {
        let args = Args::try_parse_from([
            "kbf-server",
            "--api-listen",
            "127.0.0.1:0",
            "--expected-nodes",
            path.to_str().expect("UTF-8"),
        ])
        .expect("flags");
        match args.api() {
            Err(ConfigError::ExpectedNodes(e)) => e,
            other => panic!("{} was not refused: {other:?}", path.display()),
        }
    };
    assert!(matches!(refused(&missing), ExpectedNodesError::Read { .. }));

    let two = scratch("two-words");
    std::fs::write(&two, "linux-1\n\nlinux-2 linux-3\n").expect("write");
    let ExpectedNodesError::Parse { line, .. } = refused(&two) else {
        panic!("a line of two ids was not refused as a parse error");
    };
    assert_eq!(line, 3);

    let large = scratch("large");
    std::fs::write(&large, "#".repeat(MAX_EXPECTED_NODES_BYTES + 1)).expect("write");
    assert!(matches!(
        refused(&large),
        ExpectedNodesError::NotAFile { .. }
    ));
    let largest = scratch("largest");
    std::fs::write(&largest, "#".repeat(MAX_EXPECTED_NODES_BYTES)).expect("write");
    assert!(
        ExpectedNodes::open(&largest).is_ok(),
        "a file of the limit is refused"
    );

    let binary = scratch("binary");
    std::fs::write(&binary, b"linux-1\n\xff\n").expect("write");
    assert!(matches!(
        refused(&binary),
        ExpectedNodesError::NotText { .. }
    ));

    let dir = scratch("dir");
    std::fs::create_dir_all(&dir).expect("a directory");
    assert!(matches!(refused(&dir), ExpectedNodesError::NotAFile { .. }));

    let fifo = scratch("fifo");
    let name = std::ffi::CString::new(fifo.to_str().expect("UTF-8")).expect("a C path");
    // SAFETY: `name` is a NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0, "mkfifo");
    let (done, finished) = std::sync::mpsc::channel();
    let fifo_path = fifo.clone();
    std::thread::spawn(move || {
        let _ = done.send(matches!(
            ExpectedNodes::open(&fifo_path),
            Err(ExpectedNodesError::NotAFile { .. })
        ));
    });
    let refused_fifo = finished
        .recv_timeout(PROMPT)
        .expect("a FIFO is not waited on");
    assert!(refused_fifo, "a FIFO was not refused as not a regular file");

    let alone = Args::try_parse_from(["kbf-server", "--expected-nodes", "nodes.txt"]);
    assert!(
        alone.is_err(),
        "--expected-nodes was accepted without --api-listen"
    );
}

/// Catches: a last-seen time set at registration and never moved, so an operator
/// reading a disconnected node's `last_seen_unix_ms` would see when it first
/// connected rather than when it was last heard from.
#[tokio::test]
async fn last_seen_moves_with_each_heartbeat() {
    let farm = Farm::new(
        cache(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let (outbound, _responses) = tokio::sync::mpsc::unbounded_channel();
    let node = WorkerId::new("linux-1");
    let stream = farm.register(
        &node,
        kbf_sched::DaemonInstance::new("linux-1"),
        Resources::new(8_000, 16 << 30),
        NodeCaps::from_report([("arch", "x86_64")]).expect("caps"),
        outbound,
        ServerMessage::default(),
    );
    let seen = || farm.nodes().nodes[0].last_seen_unix_ms.expect("seen");
    let registered = seen();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(farm.heartbeat(&node, stream, 1, Vec::new()), "taken");
    let beat = seen();
    assert!(beat >= registered + 20, "{beat} is not after {registered}");
}
