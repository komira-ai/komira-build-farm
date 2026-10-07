//! An in-process fake kbf-server for daemon tests: a throwaway CA, a mutual-TLS
//! `kbf.worker.v1` server on a loopback port, and a scripted peer per session.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::Stream;
use kbf_daemon::{Daemon, DaemonConfig, Event, FakeRuntime, NodeReport, Runtime, TlsFiles};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{
    DaemonMessage, Heartbeat, HeartbeatAck, Hello, LeaseId, LeaseOffer, Result as WorkerResult,
    ServerMessage, Start, Welcome, daemon_message, server_message,
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
struct Pki {
    ca: String,
    server_cert: String,
    server_key: String,
    client: TlsFiles,
}

fn pki(name: &str) -> Pki {
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
    let (client_cert, client_key) = leaf(Vec::new(), "node-1", ExtendedKeyUsagePurpose::ClientAuth);

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
        self.send(server_message::Message::Welcome(Welcome {
            protocol_version: 1,
            heartbeat_interval_ms: INTERVAL.as_millis() as u64,
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
        self.send(server_message::Message::Start(Start {
            lease_id: Some(LeaseId { term, seq }),
            kind: kind.to_owned(),
            action_digest: Some(digest()),
            ..Start::default()
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

fn digest() -> Digest {
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

/// A daemon with a fake runtime, connected to a fake server.
pub struct Harness {
    pub runtime: Arc<FakeRuntime>,
    pub sessions: mpsc::UnboundedReceiver<Peer>,
    pub events: mpsc::UnboundedReceiver<Event>,
    pub report: NodeReport,
    daemon: JoinHandle<()>,
    server: JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.daemon.abort();
        self.server.abort();
    }
}

impl Harness {
    /// Starts a server and a daemon whose fake runtime takes `run_for` per lease and
    /// whose fence time is `fence_after`. `name` keeps each test's TLS files apart.
    pub async fn start(name: &str, run_for: Duration, fence_after: Duration) -> Self {
        let pki = pki(name);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        let (sessions_tx, sessions) = mpsc::unbounded_channel();
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(&pki.server_cert, &pki.server_key))
            .client_ca_root(Certificate::from_pem(&pki.ca));
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

        let runtime = Arc::new(FakeRuntime::new(run_for));
        let report = NodeReport::detect(&[runtime.driver()]).expect("detect this node");
        let mut config = DaemonConfig::new(
            format!("https://127.0.0.1:{port}"),
            pki.client,
            "node-1".to_owned(),
        );
        config.fence_after = fence_after;
        config.reconnect_after = Duration::from_millis(100);
        let (events_tx, events) = mpsc::unbounded_channel();
        let daemon = Daemon::new(config, Arc::clone(&runtime), report.clone())
            .expect("daemon config")
            .with_events(events_tx);
        let daemon = tokio::spawn(daemon.run(std::future::pending()));
        Self {
            runtime,
            sessions,
            events,
            report,
            daemon,
            server,
        }
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

    /// Waits until the fake runtime has started `n` leases.
    pub async fn started(&self, n: usize) {
        let deadline = Instant::now() + PROMPT;
        while self.runtime.started().len() < n {
            assert!(Instant::now() < deadline, "no lease started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
