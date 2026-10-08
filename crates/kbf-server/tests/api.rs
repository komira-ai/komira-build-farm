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
use kbf_server::fleet::SoftwareView;
use kbf_server::{Farm, Listeners, ServeError, bind_server_with_api};
use kbf_types::{Resources, WorkerId};
use serde_json::{Value, json};
use support::{FakeDaemon, HELLO_WAIT, INTERVAL, PROMPT, hello};
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
    let (worker, api) = (bound.worker, bound.api.expect("an API address"));
    let serving = tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        worker,
        api,
        stop,
        serving,
    }
}

/// One HTTP/1.1 request without a body; the status code and the body.
async fn http(api: SocketAddr, method: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
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
            || code == 404,
        "{head}"
    );
    (code, body.to_owned())
}

async fn nodes(api: SocketAddr) -> Value {
    let (code, body) = http(api, "GET", "/v1/nodes").await;
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
                "xcode_builds": [] } },
            { "node_id": "mac-1", "connected": true, "software": {
                "os_name": "macOS", "os_version": "15.1", "os_build": "24B83",
                "kernel": "", "daemon_version": "0.1.0",
                "xcode_builds": ["15F31d", "16C5032a"] } },
            { "node_id": "old-1", "connected": true, "software": null },
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
