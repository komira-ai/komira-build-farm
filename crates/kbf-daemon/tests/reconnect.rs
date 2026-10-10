//! The daemon never stops trying to reach a server (`kbf_daemon::connect`): it tries
//! every address of every `--server` in turn, resolves the names again each round,
//! waits a bounded, growing time between rounds, and starts over at the shortest wait
//! after a session.
//!
//! Each test runs real mutual TLS on loopback ports against the fake worker service
//! of `support`; a server that is "down" is a port nothing listens on.

mod support;

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_daemon::connect::Resolve;
use kbf_daemon::{Daemon, DaemonConfig, Event, FakeRuntime, NodeReport};
use support::{PROMPT, Peer, Pki, pki, serve_unavailable, serve_worker};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

/// A loopback port that nothing listens on (bound, then released).
fn dead_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

async fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    (listener, addr)
}

/// A resolver whose answer for every name the test sets, and which counts the times
/// it was asked.
#[derive(Default)]
struct Switchable {
    answer: Mutex<Vec<SocketAddr>>,
    asked: AtomicUsize,
}

impl Switchable {
    fn new(answer: Vec<SocketAddr>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            asked: AtomicUsize::new(0),
        })
    }

    fn set(&self, answer: Vec<SocketAddr>) {
        *self.answer.lock().expect("answer lock") = answer;
    }
}

impl Resolve for Switchable {
    fn resolve(&self, _: String, _: u16) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let answer = self.answer.lock().expect("answer lock").clone();
        Box::pin(async move { Ok(answer) })
    }
}

/// A running daemon and what it reports.
struct Running {
    events: mpsc::UnboundedReceiver<Event>,
    daemon: JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.daemon.abort();
    }
}

impl Running {
    /// Starts a daemon of `servers`, waiting `base` after a session and doubling
    /// that up to `max` between failed rounds.
    fn start(
        pki: &Pki,
        servers: &[String],
        base: Duration,
        max: Duration,
        resolver: Option<Arc<dyn Resolve>>,
        tls_server_name: bool,
    ) -> Self {
        let mut tls = pki.client.clone();
        if !tls_server_name {
            tls.server_name = None;
        }
        let mut config = DaemonConfig::new(servers[0].clone(), tls, "node-1".to_owned());
        config.servers = servers.to_vec();
        config.reconnect_after = base;
        config.reconnect_max = max;
        let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
        let report = NodeReport::new([("cpus", "1"), ("drivers", "fake"), ("mem_gib", "1")]);
        let (events_tx, events) = mpsc::unbounded_channel();
        let mut daemon = Daemon::new(config, runtime, report)
            .expect("daemon config")
            .with_events(events_tx);
        if let Some(resolver) = resolver {
            daemon = daemon.with_resolver(resolver);
        }
        let daemon = tokio::spawn(daemon.run(std::future::pending()));
        Self { events, daemon }
    }

    /// The next event `pick` accepts within `within`, skipping others.
    async fn event<T>(
        &mut self,
        within: Duration,
        mut pick: impl FnMut(&Event) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + within;
        loop {
            let e = timeout(deadline - Instant::now(), self.events.recv())
                .await
                .ok()??;
            if let Some(t) = pick(&e) {
                return Some(t);
            }
        }
    }

    /// Every `Retrying` wait until the `rounds`th failed round in a row.
    async fn retried(&mut self, rounds: u32, within: Duration) -> Vec<Duration> {
        let mut waits = Vec::new();
        while waits.len() < rounds as usize {
            let (n, wait) = self
                .event(within, |e| match e {
                    Event::Retrying {
                        failed_rounds,
                        wait,
                    } => Some((*failed_rounds, *wait)),
                    _ => None,
                })
                .await
                .unwrap_or_else(|| panic!("the daemon stopped retrying after {waits:?}"));
            waits.push(wait);
            assert_eq!(n as usize, waits.len(), "rounds counted one by one");
        }
        waits
    }
}

/// A session the fake server takes, past Hello and Welcome.
async fn welcomed(sessions: &mut mpsc::UnboundedReceiver<Peer>, within: Duration) -> Peer {
    let mut peer = timeout(within, sessions.recv())
        .await
        .expect("the daemon connects")
        .expect("server running");
    peer.hello().await;
    peer.welcome();
    peer
}

/// Catches: a daemon that gives up after some number of attempts (it never connects
/// to a server that comes up late), a wait that does not grow from round to round, and
/// one that grows past `--reconnect-max-ms`.
///
/// The server is down for 40 rounds, each wait at most 40 ms; then it comes up on the
/// port the daemon dials, and the daemon connects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_keeps_trying_until_a_server_comes_up() {
    let pki = pki("reconnect-forever");
    let port = dead_port();
    let url = format!("https://127.0.0.1:{port}");
    let max = Duration::from_millis(40);
    let mut d = Running::start(&pki, &[url], Duration::from_millis(5), max, None, true);

    let failure = d
        .event(PROMPT, |e| match e {
            Event::ConnectFailed { server, reason } => Some(format!("{server}: {reason}")),
            _ => None,
        })
        .await
        .expect("a failed attempt is reported");
    assert!(failure.to_lowercase().contains("refused"), "{failure}");
    let waits = d.retried(40, PROMPT).await;
    assert!(waits.iter().all(|w| *w <= max), "{waits:?}");
    assert!(waits[0] <= Duration::from_millis(5), "{waits:?}");
    assert!(waits[39] >= max / 2, "the wait grew: {waits:?}");

    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("rebind");
    let (mut sessions, server) = serve_worker(pki.server_tls(), listener);
    welcomed(&mut sessions, PROMPT).await;
    server.abort();
}

/// Catches: a daemon that dials only the first address of a name (with three
/// servers, it would never leave a dead one), one that waits out a backoff between
/// the addresses of one round, and one that stops at a server answering `UNAVAILABLE`
/// (a follower that knows no leader) or at a TLS failure instead of trying the next.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_address_of_a_name_is_tried_in_turn() {
    let pki = pki("reconnect-rotate");
    let stranger = support::pki("reconnect-rotate-stranger");
    let dead: SocketAddr = format!("127.0.0.1:{}", dead_port()).parse().expect("addr");
    let (follower, follower_addr) = listener().await;
    let follower = serve_unavailable(pki.server_tls(), follower, "no leader known");
    let (impostor, impostor_addr) = listener().await;
    let (_impostor_sessions, impostor) = serve_worker(stranger.server_tls(), impostor);
    let (leader, leader_addr) = listener().await;
    let (mut sessions, leader) = serve_worker(pki.server_tls(), leader);

    let resolver = Switchable::new(vec![dead, follower_addr, impostor_addr, leader_addr]);
    let long = Duration::from_secs(60);
    let urls = ["https://farm.test:7070".to_owned()];
    let resolve: Arc<dyn Resolve> = resolver.clone();
    let mut d = Running::start(&pki, &urls, long, long, Some(resolve), true);

    let mut failed = Vec::new();
    while failed.len() < 3 {
        let event = d.event(PROMPT, |e| match e {
            Event::ConnectFailed { server, reason } => Some((server.clone(), reason.clone())),
            Event::Retrying { .. } => panic!("a wait inside a round"),
            _ => None,
        });
        failed.push(event.await.expect("a failed attempt"));
    }
    welcomed(&mut sessions, PROMPT).await;

    assert_eq!(failed[0].0, format!("https://farm.test:7070 ({dead})"));
    assert!(failed[0].1.to_lowercase().contains("refused"), "{failed:?}");
    assert_eq!(
        failed[1].0,
        format!("https://farm.test:7070 ({follower_addr})")
    );
    assert!(failed[1].1.contains("no leader known"), "{failed:?}");
    assert_eq!(
        failed[2].0,
        format!("https://farm.test:7070 ({impostor_addr})")
    );
    assert!(
        failed[2].1.to_lowercase().contains("certificate"),
        "{failed:?}"
    );
    assert_eq!(resolver.asked.load(Ordering::SeqCst), 1, "one round");
    for task in [follower, impostor, leader] {
        task.abort();
    }
}

/// Catches: a daemon that resolves its servers' names once, at start (a server that
/// moved, or came up under the name later, is never reached without a restart).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_round_resolves_the_names_again() {
    let pki = pki("reconnect-resolve");
    let dead: SocketAddr = format!("127.0.0.1:{}", dead_port()).parse().expect("addr");
    let resolver = Switchable::new(vec![dead]);
    let urls = ["https://farm.test:7070".to_owned()];
    let resolve: Arc<dyn Resolve> = resolver.clone();
    let ms = Duration::from_millis;
    let mut d = Running::start(&pki, &urls, ms(5), ms(20), Some(resolve), true);
    d.retried(3, PROMPT).await;

    let (listener, addr) = listener().await;
    let (mut sessions, server) = serve_worker(pki.server_tls(), listener);
    resolver.set(vec![addr]);
    welcomed(&mut sessions, PROMPT).await;
    assert!(
        resolver.asked.load(Ordering::SeqCst) >= 4,
        "asked every round"
    );
    server.abort();
}

/// Catches: a list of `--server` URLs of which only the first is dialled, and a daemon
/// that checks the server's certificate against the address it dialled rather than
/// the URL's host (the certificate names `localhost`; no `--tls-server-name` is
/// given).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_list_of_servers_is_tried_in_turn() {
    let pki = pki("reconnect-list");
    let (listener, addr) = listener().await;
    let (mut sessions, server) = serve_worker(pki.server_tls(), listener);
    let urls = [
        format!("https://127.0.0.1:{}", dead_port()),
        format!("https://localhost:{}", addr.port()),
    ];
    let long = Duration::from_secs(60);
    let mut d = Running::start(&pki, &urls, long, long, None, false);
    welcomed(&mut sessions, PROMPT).await;
    let retried = d
        .event(Duration::from_millis(100), |e| {
            matches!(e, Event::Retrying { .. }).then_some(())
        })
        .await;
    assert!(retried.is_none(), "one round reached the second server");
    server.abort();
}

/// Catches: a wait that is not reset by a session. After many failed rounds the wait
/// is a second or more; once a session was welcomed and ends, the daemon is back within
/// the base wait, not the grown one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_resets_the_wait() {
    let pki = pki("reconnect-reset");
    let port = dead_port();
    let url = format!("https://127.0.0.1:{port}");
    let ms = Duration::from_millis;
    let mut d = Running::start(&pki, &[url], ms(10), ms(3000), None, true);
    let waits = d.retried(9, Duration::from_secs(10)).await;
    assert!(waits[8] >= ms(1280), "{waits:?}");

    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("rebind");
    let (mut sessions, server) = serve_worker(pki.server_tls(), listener);
    let first = welcomed(&mut sessions, Duration::from_secs(10)).await;
    first.close();
    d.event(PROMPT, |e| {
        matches!(e, Event::Disconnected(_)).then_some(())
    })
    .await
    .expect("the session ends");
    let ended = Instant::now();
    welcomed(&mut sessions, PROMPT).await;
    assert!(
        ended.elapsed() < ms(700),
        "back after {:?}, not the base wait",
        ended.elapsed()
    );
    server.abort();
}
