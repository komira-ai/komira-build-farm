//! An in-process fake kbf-server for daemon tests: a throwaway CA, a mutual-TLS
//! `kbf.worker.v1` server on a loopback port, and a scripted peer per session. Also a
//! CAS in memory ([`memory`]) and fresh scratch directories.

#![allow(dead_code)] // Each test binary uses its own part of this module.

pub mod memory;

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{Stream, StreamExt};
use kbf_daemon::{
    Clock, Daemon, DaemonConfig, DriverReport, Event, FakeRuntime, Moment, NodeReport, Runtime,
    TlsFiles,
};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{
    Cancel, DaemonMessage, Heartbeat, HeartbeatAck, Hello, LeaseId, LeaseOffer,
    Result as WorkerResult, ResultAck, ServerMessage, Start, Welcome, daemon_message,
    server_message,
    worker_server::{Worker, WorkerServer},
};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};

/// The heartbeat interval every fake Welcome names.
pub const INTERVAL: Duration = Duration::from_millis(100);

/// How long a test waits for something that should happen promptly.
pub const PROMPT: Duration = Duration::from_secs(5);

/// PEM text of a CA, a server certificate for `localhost`, and a client certificate,
/// with the client's files written where the daemon reads them.
pub struct Pki {
    pub ca: String,
    pub server_cert: String,
    pub server_key: String,
    pub client: TlsFiles,
}

impl Pki {
    /// The server half: mutual TLS that requires a client certificate from this CA.
    pub fn server_tls(&self) -> ServerTlsConfig {
        ServerTlsConfig::new()
            .identity(Identity::from_pem(&self.server_cert, &self.server_key))
            .client_ca_root(Certificate::from_pem(&self.ca))
    }
}

/// An empty directory for this run of a test, under Cargo's per-target temporary
/// directory. Each run gets its own, so what a run leaves behind never meets the next.
pub fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-daemon-scratch")
        .join(format!("{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

pub fn pki(name: &str) -> Pki {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-daemon-tests")
        .join(name);
    std::fs::create_dir_all(&dir).expect("create the test's TLS directory");

    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "kbf test CA");
    ca.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("CA key")).expect("CA");

    let leaf = |names: Vec<String>, cn: &str, eku: ExtendedKeyUsagePurpose| {
        let mut params = CertificateParams::new(names).expect("leaf params");
        params.distinguished_name.push(DnType::CommonName, cn);
        params.extended_key_usages = vec![eku];
        let key = KeyPair::generate().expect("leaf key");
        let cert = params.signed_by(&key, &ca).expect("sign leaf");
        (cert.pem(), key.serialize_pem())
    };
    let (server_cert, server_key) = leaf(
        vec!["localhost".to_owned()],
        "kbf test server",
        ExtendedKeyUsagePurpose::ServerAuth,
    );
    let (client_cert, client_key) = leaf(
        vec!["node-1".to_owned()],
        "node-1",
        ExtendedKeyUsagePurpose::ClientAuth,
    );

    let write = |file: &str, text: &str| {
        let path = dir.join(file);
        std::fs::write(&path, text).expect("write a TLS file");
        path
    };
    let client = TlsFiles {
        ca_cert: write("ca.pem", &ca.pem()),
        cert: write("client.pem", &client_cert),
        key: write("client.key", &client_key),
        server_name: Some("localhost".to_owned()),
    };
    Pki {
        ca: ca.pem(),
        server_cert,
        server_key,
        client,
    }
}

type Outbound = Arc<Mutex<Option<mpsc::UnboundedSender<Result<ServerMessage, Status>>>>>;

/// The server's side of one daemon session.
pub struct Peer {
    from_daemon: mpsc::UnboundedReceiver<(Instant, DaemonMessage)>,
    outbound: Outbound,
    /// Whether heartbeats are acknowledged; tests clear it to fake a silent server.
    pub acking: Arc<AtomicBool>,
}

impl Peer {
    /// Sends a server message.
    pub fn send(&self, message: server_message::Message) {
        let guard = self.outbound.lock().expect("outbound lock");
        let tx = guard.as_ref().expect("session is open");
        tx.send(Ok(ServerMessage {
            message: Some(message),
        }))
        .expect("the stream is open");
    }

    pub fn welcome(&self) {
        self.welcome_every(INTERVAL);
    }

    /// A Welcome that asks for a heartbeat every `interval` and names no lease epoch,
    /// as a server that predates the field.
    pub fn welcome_every(&self, interval: Duration) {
        self.welcome_with(interval, 0);
    }

    /// A Welcome that names lease epoch `epoch` (0: none).
    pub fn welcome_epoch(&self, epoch: u64) {
        self.welcome_with(INTERVAL, epoch);
    }

    fn welcome_with(&self, interval: Duration, epoch: u64) {
        self.send(server_message::Message::Welcome(Welcome {
            protocol_version: 1,
            heartbeat_interval_ms: interval.as_millis() as u64,
            epoch,
        }));
    }

    pub fn offer(&self, term: u64, seq: u64) {
        self.send(server_message::Message::LeaseOffer(LeaseOffer {
            lease_id: Some(LeaseId { term, seq }),
            kind: "action".to_owned(),
            action_digest: Some(digest()),
        }));
    }

    pub fn start(&self, term: u64, seq: u64, kind: &str) {
        self.start_action(term, seq, kind, digest());
    }

    /// A Start of lease `term.seq` running the action `action`.
    pub fn start_action(&self, term: u64, seq: u64, kind: &str, action: Digest) {
        self.send(server_message::Message::Start(Start {
            lease_id: Some(LeaseId { term, seq }),
            kind: kind.to_owned(),
            action_digest: Some(action),
            ..Start::default()
        }));
    }

    /// A Start of lease `term.seq`, or of no lease, that names heartbeat
    /// `heartbeat_seq` (0: the Hello) and the window after it in which it may run.
    pub fn start_within(&self, lease: Option<(u64, u64)>, heartbeat_seq: u64, valid_for: Duration) {
        self.start_action_within(lease, digest(), heartbeat_seq, valid_for);
    }

    /// [`Self::start_within`], running the action `action`.
    pub fn start_action_within(
        &self,
        lease: Option<(u64, u64)>,
        action: Digest,
        heartbeat_seq: u64,
        valid_for: Duration,
    ) {
        self.send(server_message::Message::Start(Start {
            lease_id: lease.map(|(term, seq)| LeaseId { term, seq }),
            kind: "action".to_owned(),
            action_digest: Some(action),
            heartbeat_seq,
            valid_for_ms: valid_for.as_millis() as u64,
            ..Start::default()
        }));
    }

    /// Cancels lease `term.seq`, or no lease at all.
    pub fn cancel(&self, lease: Option<(u64, u64)>) {
        self.send(server_message::Message::Cancel(Cancel {
            lease_id: lease.map(|(term, seq)| LeaseId { term, seq }),
        }));
    }

    /// Acknowledges the Result of lease `term.seq`, or of no lease at all.
    pub fn ack_result(&self, lease: Option<(u64, u64)>, accepted: bool) {
        self.send(server_message::Message::ResultAck(ResultAck {
            lease_id: lease.map(|(term, seq)| LeaseId { term, seq }),
            accepted,
        }));
    }

    /// Ends the session from the server side.
    pub fn close(&self) {
        self.outbound.lock().expect("outbound lock").take();
    }

    /// The next daemon message `pick` accepts, skipping others, with its arrival time.
    pub async fn expect<T>(
        &mut self,
        within: Duration,
        mut pick: impl FnMut(&daemon_message::Message) -> Option<T>,
    ) -> Option<(Instant, T)> {
        let deadline = Instant::now() + within;
        loop {
            let (at, msg) = timeout(deadline - Instant::now(), self.from_daemon.recv())
                .await
                .ok()??;
            if let Some(t) = msg.message.as_ref().and_then(&mut pick) {
                return Some((at, t));
            }
        }
    }

    pub async fn hello(&mut self) -> Hello {
        self.expect(PROMPT, |m| match m {
            daemon_message::Message::Hello(h) => Some(h.clone()),
            _ => None,
        })
        .await
        .expect("a Hello")
        .1
    }

    /// The first Heartbeat that arrives at or after `after`, skipping older ones.
    pub async fn heartbeat_after(&mut self, after: Instant) -> Heartbeat {
        loop {
            let (at, heartbeat) = self
                .expect(PROMPT, |m| match m {
                    daemon_message::Message::Heartbeat(h) => Some(h.clone()),
                    _ => None,
                })
                .await
                .expect("a Heartbeat");
            if at >= after {
                return heartbeat;
            }
        }
    }

    pub async fn heartbeat(&mut self) -> Heartbeat {
        self.expect(PROMPT, |m| match m {
            daemon_message::Message::Heartbeat(h) => Some(h.clone()),
            _ => None,
        })
        .await
        .expect("a Heartbeat")
        .1
    }

    pub async fn result(&mut self, within: Duration) -> Option<(Instant, WorkerResult)> {
        self.expect(within, |m| match m {
            daemon_message::Message::Result(r) => Some(r.clone()),
            _ => None,
        })
        .await
    }
}

/// The action every Start of these helpers names, unless the test passes its own.
pub fn digest() -> Digest {
    Digest {
        hash: "ab".repeat(32),
        size_bytes: 142,
    }
}

struct FakeServer {
    sessions: mpsc::UnboundedSender<Peer>,
}

#[tonic::async_trait]
impl Worker for FakeServer {
    type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;

    async fn session(
        &self,
        request: Request<Streaming<DaemonMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let (tx, rx) = mpsc::unbounded_channel();
        let outbound: Outbound = Arc::new(Mutex::new(Some(tx)));
        let acking = Arc::new(AtomicBool::new(true));
        let (seen_tx, from_daemon) = mpsc::unbounded_channel();
        let mut inbound = request.into_inner();
        let (out, ack) = (Arc::clone(&outbound), Arc::clone(&acking));
        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                if let Some(daemon_message::Message::Heartbeat(h)) = &msg.message
                    && ack.load(Ordering::SeqCst)
                    && let Some(tx) = out.lock().expect("outbound lock").as_ref()
                {
                    let _ = tx.send(Ok(ServerMessage {
                        message: Some(server_message::Message::HeartbeatAck(HeartbeatAck {
                            seq: h.seq,
                        })),
                    }));
                }
                if seen_tx.send((Instant::now(), msg)).is_err() {
                    break;
                }
            }
        });
        let _ = self.sessions.send(Peer {
            from_daemon,
            outbound,
            acking,
        });
        let stream =
            futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|m| (m, rx)) });
        Ok(Response::new(Box::pin(stream)))
    }
}

/// Serves the fake worker service with `tls` on `listener`: every session the daemon
/// opens arrives on the returned receiver. The task stops when the handle is aborted.
pub fn serve_worker(
    tls: ServerTlsConfig,
    listener: tokio::net::TcpListener,
) -> (mpsc::UnboundedReceiver<Peer>, JoinHandle<()>) {
    let (sessions_tx, sessions) = mpsc::unbounded_channel();
    let router = Server::builder()
        .tls_config(tls)
        .expect("server TLS config")
        .add_service(WorkerServer::new(FakeServer {
            sessions: sessions_tx,
        }));
    let server = tokio::spawn(async move {
        router
            .serve_with_incoming(TcpIncoming::from(listener))
            .await
            .expect("fake server");
    });
    (sessions, server)
}

/// A worker service that answers every session `UNAVAILABLE` with `message`, as a
/// follower that knows no leader does.
struct Unavailable(&'static str);

#[tonic::async_trait]
impl Worker for Unavailable {
    type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;

    async fn session(
        &self,
        _: Request<Streaming<DaemonMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        Err(Status::unavailable(self.0))
    }
}

/// Serves, with `tls` on `listener`, a worker service that answers every session
/// `UNAVAILABLE` with `message`.
pub fn serve_unavailable(
    tls: ServerTlsConfig,
    listener: tokio::net::TcpListener,
    message: &'static str,
) -> JoinHandle<()> {
    let router = Server::builder()
        .tls_config(tls)
        .expect("server TLS config")
        .add_service(WorkerServer::new(Unavailable(message)));
    tokio::spawn(async move {
        router
            .serve_with_incoming(TcpIncoming::from(listener))
            .await
            .expect("unavailable server");
    })
}

/// A clock that runs with the test's own (uptime) clock and jumps forward on
/// [`SuspendClock::suspend`], as a suspend-counting clock does across a suspend while
/// the uptime clock, and every tokio timer, stands still.
#[derive(Debug)]
pub struct SuspendClock {
    origin: std::time::Instant,
    slept: Mutex<Duration>,
}

impl SuspendClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            origin: std::time::Instant::now(),
            slept: Mutex::new(Duration::ZERO),
        })
    }

    /// The machine was suspended for `d`: the clock jumps forward by `d` at once.
    pub fn suspend(&self, d: Duration) {
        *self.slept.lock().expect("clock lock") += d;
    }
}

impl Clock for SuspendClock {
    fn now(&self) -> Moment {
        let slept = *self.slept.lock().expect("clock lock");
        Moment::from_origin(self.origin.elapsed() + slept)
    }
}

/// A daemon with a runtime (by default the fake one), connected to a fake server.
pub struct Harness<R: Runtime = FakeRuntime> {
    pub runtime: Arc<R>,
    pub sessions: mpsc::UnboundedReceiver<Peer>,
    pub events: mpsc::UnboundedReceiver<Event>,
    pub report: NodeReport,
    daemon: JoinHandle<()>,
    server: JoinHandle<()>,
    /// Whether the server takes up new connections; see [`Harness::hold_connections`].
    open: tokio::sync::watch::Sender<bool>,
}

impl<R: Runtime> Drop for Harness<R> {
    fn drop(&mut self) {
        self.daemon.abort();
        self.server.abort();
    }
}

impl Harness<FakeRuntime> {
    /// Starts a server and a daemon whose fake runtime takes `run_for` per lease and
    /// whose fence time is `fence_after`. `name` keeps each test's TLS files apart.
    pub async fn start(name: &str, run_for: Duration, fence_after: Duration) -> Self {
        Self::with_runtime(name, Arc::new(FakeRuntime::new(run_for)), fence_after).await
    }

    /// Starts a server and a daemon with the fake runtime whose driver reports what
    /// `driver` sends ([`Daemon::with_driver_report`]). The daemon checks its fence
    /// every 20 ms, so the driver's wakes fall among the fence's rechecks.
    pub async fn with_driver(
        name: &str,
        driver: tokio::sync::watch::Receiver<DriverReport>,
    ) -> Self {
        let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
        Self::build(
            name,
            runtime,
            Duration::from_secs(40),
            |config| config.recheck_every = Duration::from_millis(20),
            None,
            Some(driver),
        )
        .await
    }

    /// Waits until the fake runtime has started `n` leases.
    pub async fn started(&self, n: usize) {
        let deadline = Instant::now() + PROMPT;
        while self.runtime.started().len() < n {
            assert!(Instant::now() < deadline, "no lease started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl<R: Runtime> Harness<R> {
    /// Starts a server and a daemon running leases through `runtime`, whose fence time
    /// is `fence_after`. `name` keeps each test's TLS files apart.
    pub async fn with_runtime(name: &str, runtime: Arc<R>, fence_after: Duration) -> Self {
        Self::build(name, runtime, fence_after, |_| {}, None, None).await
    }

    /// Starts a server and a daemon running leases through `runtime`, whose fence time
    /// is `fence_after`, which checks the fence at least every `recheck_every`, and
    /// which reads the returned clock instead of the system's.
    pub async fn suspendable(
        name: &str,
        runtime: Arc<R>,
        fence_after: Duration,
        recheck_every: Duration,
    ) -> (Self, Arc<SuspendClock>) {
        let clock = SuspendClock::new();
        let h = Self::build(
            name,
            runtime,
            fence_after,
            |config| config.recheck_every = recheck_every,
            Some(Arc::clone(&clock) as Arc<dyn Clock>),
            None,
        )
        .await;
        (h, clock)
    }

    async fn build(
        name: &str,
        runtime: Arc<R>,
        fence_after: Duration,
        configure: impl FnOnce(&mut DaemonConfig),
        clock: Option<Arc<dyn Clock>>,
        driver: Option<tokio::sync::watch::Receiver<DriverReport>>,
    ) -> Self {
        let pki = pki(name);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        let (sessions_tx, sessions) = mpsc::unbounded_channel();
        let router = Server::builder()
            .tls_config(pki.server_tls())
            .expect("server TLS config")
            .add_service(WorkerServer::new(FakeServer {
                sessions: sessions_tx,
            }));
        let (open, open_rx) = tokio::sync::watch::channel(true);
        // A connection waits here, before its TLS handshake, while the test holds them.
        let incoming = TcpIncoming::from(listener).then(move |conn| {
            let mut open = open_rx.clone();
            async move {
                let _ = open.wait_for(|open| *open).await;
                conn
            }
        });
        let server = tokio::spawn(async move {
            router
                .serve_with_incoming(incoming)
                .await
                .expect("fake server");
        });

        let report = NodeReport::detect(&[runtime.driver()]).expect("detect this node");
        let mut config = DaemonConfig::new(
            format!("https://127.0.0.1:{port}"),
            pki.client,
            "node-1".to_owned(),
        );
        config.fence_after = fence_after;
        config.reconnect_after = Duration::from_millis(100);
        configure(&mut config);
        let (events_tx, events) = mpsc::unbounded_channel();
        let mut daemon = Daemon::new(config, Arc::clone(&runtime), report.clone())
            .expect("daemon config")
            .with_events(events_tx);
        if let Some(clock) = clock {
            daemon = daemon.with_clock(clock);
        }
        if let Some(driver) = driver {
            daemon = daemon.with_driver_report(driver);
        }
        let daemon = tokio::spawn(daemon.run(std::future::pending()));
        Self {
            runtime,
            sessions,
            events,
            report,
            daemon,
            server,
            open,
        }
    }

    /// While `held`, a daemon's new connection is accepted but not served: its TLS
    /// handshake, and so its `connect`, waits until the hold is lifted.
    pub fn hold_connections(&self, held: bool) {
        self.open.send_replace(!held);
    }

    /// The next session the daemon opens.
    pub async fn session(&mut self) -> Peer {
        timeout(PROMPT, self.sessions.recv())
            .await
            .expect("the daemon connects")
            .expect("server running")
    }

    /// A session past Hello and Welcome.
    pub async fn welcomed(&mut self) -> Peer {
        let mut peer = self.session().await;
        peer.hello().await;
        peer.welcome();
        peer
    }

    /// The next event `pick` accepts, skipping others.
    pub async fn event<T>(
        &mut self,
        within: Duration,
        mut pick: impl FnMut(&Event) -> Option<T>,
    ) -> Option<(Instant, T)> {
        let deadline = Instant::now() + within;
        loop {
            let e = timeout(deadline - Instant::now(), self.events.recv())
                .await
                .ok()??;
            if let Some(t) = pick(&e) {
                return Some((Instant::now(), t));
            }
        }
    }
}
