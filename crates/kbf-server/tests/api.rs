//! The operator API (`/v1`) over a real listener, and the fleet view behind it.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_caps::NodeCaps;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_meta::Retention;
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_proto::worker::{NodeStatus, ServerMessage, daemon_message};
use kbf_server::api::{DRAIN_DEADLINE, node_action};
use kbf_server::farm::NodeAction;
use kbf_server::fleet::SoftwareView;
use kbf_server::{Farm, Listeners, ServeError, bind_server_with_api};
use kbf_types::{Resources, WorkerId};
use serde_json::{Value, json};
use support::{Client, FakeDaemon, HELLO_WAIT, INTERVAL, Job, PROMPT, done, hello, output, ran};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

fn cache() -> Arc<MemoryCache> {
    Arc::new(Cache::new(
        MemoryMetaLog::new(Retention::default()),
        MemoryStore::new(Capabilities::default()),
        KeyPrefix::default(),
    ))
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

fn start() -> Server {
    let listeners = Listeners {
        reapi: loopback(),
        worker: loopback(),
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        let _ = stopped.await;
    };
    let bound = bind_server_with_api(cache(), listeners, Some(loopback()), shutdown).expect("bind");
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
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let length = body.len();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: {length}\r\n\
         Connection: close\r\n\r\n{body}"
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
    (code, body.to_owned())
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
        xcode_builds: Vec::new(),
    }
}

/// Catches: a `NodeStatus` the server drops (the worker stream ignores it), fields
/// mapped to the wrong JSON keys, a node without status left out of the list or
/// listed with an empty object instead of `null`, an older status kept over a newer
/// one, and a node still listed as connected after its stream ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_nodes_lists_each_nodes_newest_software() {
    let server = start();
    let (worker, api) = (server.worker, server.api);
    assert_eq!(nodes(api).await, json!({ "nodes": [] }));

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
    mac.send(daemon_message::Message::NodeStatus(NodeStatus {
        os_name: "macOS".to_owned(),
        os_version: "15.1".to_owned(),
        os_build: "24B83".to_owned(),
        kernel: String::new(),
        daemon_version: "0.1.0".to_owned(),
        xcode_builds: vec!["15F31d".to_owned(), "16C5032a".to_owned()],
    }));
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
        json!({ "nodes": [
            { "node_id": "linux-1", "connected": true, "software": {
                "os_name": "Ubuntu", "os_version": "24.04", "os_build": "",
                "kernel": "6.8.0-45-generic", "daemon_version": "0.1.0",
                "xcode_builds": [] },
              "placement": { "state": "serving" } },
            { "node_id": "mac-1", "connected": true, "software": {
                "os_name": "macOS", "os_version": "15.1", "os_build": "24B83",
                "kernel": "", "daemon_version": "0.1.0",
                "xcode_builds": ["15F31d", "16C5032a"] },
              "placement": { "state": "serving" } },
            { "node_id": "old-1", "connected": true, "software": null,
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
    };
    let refused = bind_server_with_api(cache(), listeners, Some(addr), std::future::pending());
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
    let farm = Farm::new(cache(), kbf_sched::UNSERVABLE_WAIT);
    let node = WorkerId::new("linux-1");
    let (old, _old_rx) = register(&farm, "linux-1");
    let (new, _new_rx) = register(&farm, "linux-1");
    farm.node_status(&node, new, linux_status("new"));
    farm.node_status(&node, old, linux_status("old"));
    farm.node_status(&WorkerId::new("ghost"), new, linux_status("ghost"));
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
    let farm = Farm::new(cache(), kbf_sched::UNSERVABLE_WAIT);
    let (_, responses) = register(&farm, "linux-1");
    assert!(farm.nodes().nodes[0].connected);
    drop(responses);
    assert!(!farm.nodes().nodes[0].connected);
}

async fn post(api: SocketAddr, target: &str, body: &str) -> (u16, Value) {
    let (code, text) = http(api, "POST", &format!("/v1/nodes/{target}"), body).await;
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
    assert!(
        (before + 1_000..=now_ms() + 1_000).contains(&deadline),
        "{deadline}"
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

/// Catches: the loopback check removed or inverted (anyone who reaches the API could
/// take nodes out of service), and an IPv4 loopback peer seen through an IPv6 socket
/// refused.
#[test]
fn writes_are_accepted_only_from_loopback() {
    for peer in ["127.0.0.1:4000", "[::1]:4000", "[::ffff:127.0.0.1]:4000"] {
        let peer: SocketAddr = peer.parse().expect("an address");
        let (node, action) = node_action(peer, "mac-1:cordon", b"").expect("allowed");
        assert_eq!((node.as_str(), action), ("mac-1", NodeAction::Cordon));
    }
    for peer in [
        "192.0.2.7:4000",
        "[2001:db8::7]:4000",
        "[::ffff:192.0.2.7]:4000",
    ] {
        let peer: SocketAddr = peer.parse().expect("an address");
        let (code, why) = node_action(peer, "mac-1:cordon", b"").expect_err("refused");
        assert_eq!(code, axum::http::StatusCode::FORBIDDEN, "{why}");
    }
    let local: SocketAddr = "127.0.0.1:1".parse().expect("an address");
    assert_eq!(
        node_action(local, "mac-1:drain", b"").map(|(_, a)| a),
        Ok(NodeAction::Drain(DRAIN_DEADLINE))
    );
    assert_eq!(
        node_action(local, "mac-1:drain", b"{\"deadline_secs\": 90}").map(|(_, a)| a),
        Ok(NodeAction::Drain(Duration::from_secs(90)))
    );
    assert_eq!(
        node_action(local, "a:b:uncordon", b"").map(|(n, a)| (n.as_str().to_owned(), a)),
        Ok(("a:b".to_owned(), NodeAction::Uncordon))
    );
}
