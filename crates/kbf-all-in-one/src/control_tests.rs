//! What the binary's end-to-end run does not reach: a session refused by the server's
//! own checks, and a server half that is gone.

use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::unbounded;
use kbf_front::Cache;
use kbf_proto::worker::{
    Capability, DaemonMessage, Heartbeat, Hello, daemon_message, server_message,
};
use kbf_server::{Farm, Peers, WorkerService};
use tonic::Code;

use super::*;

/// A runtime standing in for the server half.
fn server() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime")
}

/// The plane over a farm on a new in-memory cache, its sessions opened on `server`.
fn plane(server: &tokio::runtime::Runtime) -> LocalControlPlane {
    let _context = server.enter();
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        Duration::from_secs(60),
        Duration::from_secs(60),
    );
    let service = WorkerService::new(
        Arc::new(farm),
        Peers::Unauthenticated,
        Duration::from_millis(100),
        Duration::from_secs(5),
    );
    LocalControlPlane::new(LocalWorker::new(service), server.handle().clone())
}

fn message(message: daemon_message::Message) -> DaemonMessage {
    DaemonMessage {
        message: Some(message),
    }
}

/// A Hello the server accepts, from `node-1`.
fn hello() -> Hello {
    let entry = |key: &str, value: &str| Capability {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    Hello {
        protocol_version: 1,
        node_id: "node-1".to_owned(),
        capabilities: vec![
            entry("cpus", "1"),
            entry("mem_gib", "1"),
            entry("arch", "x86_64"),
        ],
        ..Hello::default()
    }
}

/// Catches a session opened anywhere but in the server's own session handling: a
/// first message that is not `Hello` is refused INVALID_ARGUMENT by the server's check,
/// and a `Hello` it accepts is answered `Welcome` first, then a heartbeat on the same
/// session is acknowledged.
#[test]
fn a_session_is_the_servers_own() {
    let server = server();
    let plane = plane(&server);
    let daemon = tokio::runtime::Runtime::new().expect("runtime");
    daemon.block_on(async {
        let (tx, rx) = unbounded();
        let beat = Heartbeat {
            seq: 1,
            ..Heartbeat::default()
        };
        tx.unbounded_send(message(daemon_message::Message::Heartbeat(beat.clone())))
            .expect("send");
        let refused = plane.session(rx).await.err().expect("refused");
        assert_eq!(refused.code(), Code::InvalidArgument, "{refused}");

        let (tx, rx) = unbounded();
        tx.unbounded_send(message(daemon_message::Message::Hello(hello())))
            .expect("send");
        let mut inbound = plane.session(rx).await.expect("a session");
        let first = inbound
            .next()
            .await
            .expect("a message")
            .expect("not an error");
        assert!(
            matches!(first.message, Some(server_message::Message::Welcome(_))),
            "{first:?}"
        );
        tx.unbounded_send(message(daemon_message::Message::Heartbeat(beat)))
            .expect("send");
        let acked = loop {
            let next = inbound
                .next()
                .await
                .expect("a message")
                .expect("not an error");
            if let Some(server_message::Message::HeartbeatAck(ack)) = next.message {
                break ack;
            }
        };
        assert_eq!(acked.seq, 1);
    });
}

/// Catches a session opened on a server half whose runtime is gone panicking or
/// hanging, rather than being refused UNAVAILABLE, which the daemon half retries.
#[test]
fn a_gone_server_half_refuses_every_session() {
    let gone = server();
    let plane = plane(&gone);
    drop(gone);
    let daemon = tokio::runtime::Runtime::new().expect("runtime");
    let (_tx, rx) = unbounded();
    let refused = daemon.block_on(plane.session(rx)).err().expect("refused");
    assert_eq!(refused.code(), Code::Unavailable, "{refused}");
    assert!(
        refused.message().contains("the server half did not answer"),
        "{refused}"
    );
}
