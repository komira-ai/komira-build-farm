//! A daemon in its server's own process (`Daemon::in_process`): every session is opened
//! through the `ControlPlane` it is given, with no server URL, connection or TLS file,
//! and is the session a dialled daemon holds. The plane here is the fake worker service
//! of `support`, reached by calls.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_daemon::{
    ControlPlane, Daemon, DaemonConfig, DaemonMessages, Event, FakeRuntime, IN_PROCESS, NodeReport,
    ServerMessages, TlsFiles,
};
use support::{PROMPT, Peer, peer_over};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tonic::Status;

/// A plane that refuses its first session UNAVAILABLE, then hands each session to the
/// test as a [`Peer`].
struct Plane {
    sessions: mpsc::UnboundedSender<Peer>,
    refuse: AtomicBool,
}

impl ControlPlane for Plane {
    fn session(&self, outbound: DaemonMessages) -> BoxFuture<'_, Result<ServerMessages, Status>> {
        if self.refuse.swap(false, Ordering::SeqCst) {
            return Box::pin(async { Err(Status::unavailable("not serving yet")) });
        }
        let (peer, stream) = peer_over(outbound);
        let _ = self.sessions.send(peer);
        Box::pin(async move { Ok(stream.boxed()) })
    }
}

/// The next event `pick` accepts, skipping others.
async fn event<T>(
    events: &mut mpsc::UnboundedReceiver<Event>,
    mut pick: impl FnMut(Event) -> Option<T>,
) -> T {
    timeout(PROMPT, async {
        loop {
            let event = events.recv().await.expect("the daemon runs");
            if let Some(t) = pick(event) {
                return t;
            }
        }
    })
    .await
    .expect("the event in time")
}

/// Catches an in-process daemon that reads a server URL or TLS file (it is given none
/// that exist: building it would fail, and so would a dial), that stops after a session
/// it could not open or one the server ended, or whose sessions differ from a dialled
/// daemon's. A first session refused is reported as a failed attempt on `in-process`
/// and followed by a wait; the next is welcomed, and a Start on it runs the lease,
/// whose Result comes back on the same session; when the server ends that session the
/// daemon opens another and says Hello again, naming the same process.
#[tokio::test]
async fn every_session_is_opened_through_the_plane() {
    let (sessions_tx, mut sessions) = mpsc::unbounded_channel();
    let plane = Arc::new(Plane {
        sessions: sessions_tx,
        refuse: AtomicBool::new(true),
    });
    let tls = TlsFiles {
        ca_cert: "/nonexistent/ca.pem".into(),
        cert: "/nonexistent/node.pem".into(),
        key: "/nonexistent/node.key".into(),
        server_name: None,
    };
    let mut config = DaemonConfig::new("http://not-a-server".to_owned(), tls, "node-1".to_owned());
    config.reconnect_after = Duration::from_millis(20);
    let report = NodeReport::new([("cpus", "1"), ("drivers", "fake"), ("mem_gib", "1")]);
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let daemon = Daemon::in_process(config, runtime, report, plane).with_events(events_tx);
    let running = tokio::spawn(daemon.run(std::future::pending()));

    let failed = event(&mut events, |e| match e {
        Event::ConnectFailed { server, reason } => Some((server, reason)),
        _ => None,
    })
    .await;
    assert_eq!(failed.0, IN_PROCESS);
    assert!(failed.1.contains("not serving yet"), "{failed:?}");
    let rounds = event(&mut events, |e| match e {
        Event::Retrying { failed_rounds, .. } => Some(failed_rounds),
        _ => None,
    })
    .await;
    assert_eq!(rounds, 1);

    let mut peer = timeout(PROMPT, sessions.recv())
        .await
        .expect("a session in time")
        .expect("a session");
    let hello = peer.hello().await;
    assert_eq!(hello.node_id, "node-1");
    peer.welcome();
    event(&mut events, |e| {
        matches!(e, Event::Welcomed { .. }).then_some(())
    })
    .await;
    peer.start(1, 1, "action");
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    let lease = result.lease_id.expect("a lease");
    assert_eq!((lease.term, lease.seq), (1, 1));

    peer.close();
    let reason = event(&mut events, |e| match e {
        Event::Disconnected(reason) => Some(reason),
        _ => None,
    })
    .await;
    assert_eq!(reason, "the server ended the stream");
    let mut again = timeout(PROMPT, sessions.recv())
        .await
        .expect("another session in time")
        .expect("another session");
    assert_eq!(again.hello().await.instance_id, hello.instance_id);
    running.abort();
}
