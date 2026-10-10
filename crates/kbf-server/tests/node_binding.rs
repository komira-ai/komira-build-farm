//! Issue #79 end to end: the worker listener under mutual TLS binds each stream to the
//! node its client certificate names, and refuses what the deny list lists, reading the
//! list again at every check.

mod support;

use std::future::pending;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_front::Cache;
use kbf_proto::worker::{
    Capability, DaemonMessage, Heartbeat, Hello, NodeStatus, ServerMessage, Start, daemon_message,
    server_message, worker_client::WorkerClient,
};
use kbf_server::{Api, Args, ConfigError, DenyListError, bind_server, bind_server_with_api};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SerialNumber,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};
use tonic::{Code, Status, Streaming};

const PROMPT: Duration = Duration::from_secs(5);

/// A cell CA on disk, a server certificate, and client certificates on demand.
struct Pki {
    dir: PathBuf,
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    fn new(name: &str) -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Self::with_ca(name, ca)
    }

    /// A PKI whose CA is made from `ca`.
    fn with_ca(name: &str, ca: CertificateParams) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-server-node-binding")
            .join(name);
        std::fs::create_dir_all(&dir).expect("create the TLS directory");
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
        let pki = Self { dir, ca };
        let (cert, key) = pki.leaf(
            &["localhost"],
            "server",
            1,
            ExtendedKeyUsagePurpose::ServerAuth,
        );
        pki.write("ca.pem", &pki.ca.pem());
        pki.write("server.pem", &cert);
        pki.write("server.key", &key);
        pki
    }

    fn leaf(
        &self,
        names: &[&str],
        cn: &str,
        serial: u64,
        eku: ExtendedKeyUsagePurpose,
    ) -> (String, String) {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let mut params = CertificateParams::new(names).expect("leaf params");
        params.distinguished_name.push(DnType::CommonName, cn);
        params.extended_key_usages = vec![eku];
        params.serial_number = Some(SerialNumber::from(serial));
        let key = KeyPair::generate().expect("leaf key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        (cert.pem(), key.serialize_pem())
    }

    /// A daemon identity whose certificate has `names` as DNS names and `serial`.
    fn client(&self, names: &[&str], serial: u64) -> Identity {
        let (cert, key) = self.leaf(
            names,
            "cn-is-not-read",
            serial,
            ExtendedKeyUsagePurpose::ClientAuth,
        );
        Identity::from_pem(cert, key)
    }

    fn write(&self, file: &str, text: &str) -> String {
        let path = self.dir.join(file);
        std::fs::write(&path, text).expect("write");
        path.to_str().expect("UTF-8 path").to_owned()
    }

    fn path(&self, file: &str) -> String {
        self.dir.join(file).to_str().expect("UTF-8 path").to_owned()
    }

    fn args(&self, extra: &[&str]) -> Args {
        let base = [
            "kbf-server",
            "--listen",
            "127.0.0.1:0",
            "--worker-listen",
            "127.0.0.1:0",
            "--worker-tls-cert",
            &self.path("server.pem"),
            "--worker-tls-key",
            &self.path("server.key"),
            "--worker-client-ca",
            &self.path("ca.pem"),
        ]
        .map(str::to_owned);
        Args::parse_from(base.iter().map(String::as_str).chain(extra.iter().copied()))
    }

    /// Starts a server with these flags added; returns its worker address.
    fn serve(&self, extra: &[&str]) -> SocketAddr {
        self.serve_both(extra).1
    }

    /// Starts a server with these flags added; returns its REAPI and worker addresses.
    fn serve_both(&self, extra: &[&str]) -> (SocketAddr, SocketAddr) {
        let listeners = self.args(extra).listeners().expect("listeners");
        let bound = bind_server(Arc::new(Cache::memory()), listeners, pending()).expect("bind");
        let addrs = (bound.reapi, bound.worker);
        tokio::spawn(async move { bound.serving.await.expect("serve") });
        addrs
    }
}

fn hello(node: &str) -> daemon_message::Message {
    let entry = |key: &str, value: &str| Capability {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    daemon_message::Message::Hello(Hello {
        protocol_version: 1,
        node_id: node.to_owned(),
        capabilities: vec![
            entry("arch", "x86_64"),
            entry("cpus", "1"),
            entry("mem_gib", "1"),
        ],
        ..Hello::default()
    })
}

/// A daemon's open session: what it sends, and what the server sends back.
struct Session {
    tx: UnboundedSender<DaemonMessage>,
    inbound: Streaming<ServerMessage>,
}

impl Session {
    /// Opens a session as `identity` with a first `Hello` for `node`, and returns it
    /// once welcomed, or the status the server refused it with.
    async fn open(
        addr: SocketAddr,
        pki: &Pki,
        identity: Identity,
        node: &str,
    ) -> Result<Self, Status> {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(pki.ca.pem()))
            .domain_name("localhost")
            .identity(identity);
        let channel = Endpoint::from_shared(format!("https://{addr}"))
            .expect("endpoint")
            .tls_config(tls)
            .expect("tls")
            .connect()
            .await
            .expect("connect");
        let (tx, rx) = unbounded();
        Self::send_on(&tx, hello(node));
        let mut inbound = WorkerClient::new(channel).session(rx).await?.into_inner();
        match timeout(PROMPT, inbound.message())
            .await
            .expect("an answer in time")?
        {
            Some(ServerMessage {
                message: Some(server_message::Message::Welcome(_)),
            }) => Ok(Self { tx, inbound }),
            other => panic!("expected Welcome or an error, got {other:?}"),
        }
    }

    fn send_on(tx: &UnboundedSender<DaemonMessage>, message: daemon_message::Message) {
        tx.unbounded_send(DaemonMessage {
            message: Some(message),
        })
        .expect("the stream is open");
    }

    fn send(&self, message: daemon_message::Message) {
        Self::send_on(&self.tx, message);
    }

    /// Sends `message` if the client still has the stream; a stream the server has
    /// ended may already be closed on this side.
    fn send_if_open(&self, message: daemon_message::Message) {
        let _ = self.tx.unbounded_send(DaemonMessage {
            message: Some(message),
        });
    }

    /// The next `Start`, skipping the lease offer before it.
    async fn start(&mut self) -> Start {
        loop {
            match self.next().await {
                Ok(Some(ServerMessage {
                    message: Some(server_message::Message::Start(start)),
                })) => return start,
                Ok(Some(_)) => {}
                other => panic!("expected a Start, got {other:?}"),
            }
        }
    }

    /// The next message, or the status the stream ends with.
    async fn next(&mut self) -> Result<Option<ServerMessage>, Status> {
        timeout(PROMPT, self.inbound.message())
            .await
            .expect("a message or an end in time")
    }

    async fn heartbeat(&mut self, seq: u64) -> Result<Option<ServerMessage>, Status> {
        self.send(daemon_message::Message::Heartbeat(Heartbeat {
            seq,
            ..Heartbeat::default()
        }));
        self.next().await
    }
}

fn refused(opened: Result<Session, Status>) -> Status {
    match opened {
        Ok(_) => panic!("the session was welcomed"),
        Err(status) => status,
    }
}

/// Catches (issue #79): a first `Hello` whose `node_id` is never compared with the
/// client certificate, so a certificate the cell CA signed for node A opens (and, since
/// only the newest stream of a node counts, takes over) node B's session. Also a
/// certificate that names no node, or several, accepted; and the common name read
/// instead of the DNS name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_for_one_node_cannot_open_another_nodes_session() {
    let pki = Pki::new("bind");
    let addr = pki.serve(&[]);

    let mut b = Session::open(addr, &pki, pki.client(&["node-b"], 2), "node-b")
        .await
        .expect("node-b as itself");
    assert!(matches!(b.heartbeat(1).await, Ok(Some(_))));

    let as_b = refused(Session::open(addr, &pki, pki.client(&["node-a"], 3), "node-b").await);
    assert_eq!(as_b.code(), Code::PermissionDenied, "{as_b:?}");
    assert!(as_b.message().contains("\"node-a\""), "{}", as_b.message());
    // node-b's own session is still the node's: its heartbeats are acknowledged.
    assert!(matches!(
        b.heartbeat(2).await,
        Ok(Some(ServerMessage {
            message: Some(server_message::Message::HeartbeatAck(_))
        }))
    ));

    for names in [&[][..], &["node-a", "node-b"][..]] {
        let status = refused(Session::open(addr, &pki, pki.client(names, 4), "node-b").await);
        assert_eq!(status.code(), Code::PermissionDenied, "{names:?}");
    }
    // The common name says node-b; only the DNS name counts.
    let (cert, key) = pki.leaf(&[], "node-b", 5, ExtendedKeyUsagePurpose::ClientAuth);
    let cn = refused(Session::open(addr, &pki, Identity::from_pem(cert, key), "node-b").await);
    assert_eq!(cn.code(), Code::PermissionDenied);
}

/// Catches (issue #79): the deny list not consulted at `Hello`, read only when the
/// server starts (a serial denied afterwards must be refused on the node's next
/// connection), and an open stream of a denied certificate kept: its next `Heartbeat`
/// must end it. Also the list's serial spelling (colons, upper case) not matched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_certificate_is_refused_and_its_open_stream_ended() {
    let pki = Pki::new("deny");
    let list = pki.write("deny.list", "# empty at start\n");
    let addr = pki.serve(&["--worker-deny-list", &list]);

    let mut open = Session::open(addr, &pki, pki.client(&["node-a"], 0x0a1b), "node-a")
        .await
        .expect("not denied yet");
    assert!(matches!(open.heartbeat(1).await, Ok(Some(_))));

    pki.write(
        "deny.list",
        "serial 0A:1B   # node-a's leaked certificate\n",
    );
    let again = refused(Session::open(addr, &pki, pki.client(&["node-a"], 0x0a1b), "node-a").await);
    assert_eq!(again.code(), Code::PermissionDenied, "{again:?}");
    assert!(
        again.message().contains("serial a1b"),
        "{}",
        again.message()
    );
    let ended = open
        .heartbeat(2)
        .await
        .expect_err("the denied stream ended");
    assert_eq!(ended.code(), Code::PermissionDenied);

    // A new certificate for the same node (another serial) is not denied.
    Session::open(addr, &pki, pki.client(&["node-a"], 0x0a1c), "node-a")
        .await
        .expect("a reissued certificate");
}

/// Catches: a resent `Hello` on an open stream not checked against the deny list, so
/// a node denied by id keeps resizing itself on the stream it already has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resent_hello_from_a_denied_node_ends_the_stream() {
    let pki = Pki::new("deny-node");
    let list = pki.write("deny.list", "");
    let addr = pki.serve(&["--worker-deny-list", &list]);
    let mut open = Session::open(addr, &pki, pki.client(&["mac-07"], 9), "mac-07")
        .await
        .expect("welcomed");
    pki.write("deny.list", "node mac-07\n");
    open.send(hello("mac-07"));
    let ended = open.next().await.expect_err("ended");
    assert_eq!(ended.code(), Code::PermissionDenied);
    assert!(
        ended.message().contains("node mac-07"),
        "{}",
        ended.message()
    );
}

/// Catches: a deny list flag accepted without mutual TLS (it could never be applied),
/// and a server that starts with a deny list it cannot read or parse.
#[test]
fn the_deny_list_flag_needs_tls_and_a_good_file() {
    let plain = Args::try_parse_from(["kbf-server", "--worker-deny-list", "deny.list"]);
    assert!(plain.is_err(), "a deny list without TLS");

    let pki = Pki::new("deny-flag");
    let bad = pki.write("bad.list", "serial\n");
    let error = pki
        .args(&["--worker-deny-list", &bad])
        .listeners()
        .expect_err("bad");
    assert!(matches!(
        error,
        ConfigError::DenyList(DenyListError::Parse { line: 1, .. })
    ));
    assert!(
        error.to_string().starts_with("--worker-deny-list: "),
        "{error}"
    );
    let missing = pki.args(&["--worker-deny-list", &pki.path("missing.list")]);
    assert!(matches!(
        missing.listeners(),
        Err(ConfigError::DenyList(DenyListError::Read { .. }))
    ));
}

/// Catches: a check stricter than the rule, refusing a certificate made the way a
/// plain `openssl` CA script makes one: an EC P-256 key, a CA limited to path length
/// 0 that may also sign revocation lists, a 20-byte serial, the node id as both the
/// common name and the one DNS name, critical key usage, and key identifiers. Also
/// such a long serial not matched by a deny entry spelled as `openssl x509 -serial`
/// prints it (upper case, no colons).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_from_an_openssl_ca_script_passes() {
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.distinguished_name.push(DnType::CommonName, "farm CA");
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let pki = Pki::with_ca("openssl-profile", ca);
    let list = pki.write("deny.list", "");
    let addr = pki.serve(&["--worker-deny-list", &list]);

    let node = "mac-mini-03";
    let serial: Vec<u8> = (0x31..=0x44).collect();
    let mut params = CertificateParams::new(vec![node.to_owned()]).expect("leaf params");
    params.distinguished_name.push(DnType::CommonName, node);
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.use_authority_key_identifier_extension = true;
    params.serial_number = Some(SerialNumber::from(serial.clone()));
    let key = KeyPair::generate().expect("P-256 key");
    let cert = params.signed_by(&key, &pki.ca).expect("sign");
    let identity = || Identity::from_pem(cert.pem(), key.serialize_pem());

    let mut session = Session::open(addr, &pki, identity(), node)
        .await
        .expect("the openssl-made certificate is accepted");
    assert!(matches!(
        session.heartbeat(1).await,
        Ok(Some(ServerMessage {
            message: Some(server_message::Message::HeartbeatAck(_))
        }))
    ));

    let printed: String = serial.iter().map(|b| format!("{b:02X}")).collect();
    pki.write("deny.list", &format!("serial {printed}\n"));
    let denied = refused(Session::open(addr, &pki, identity(), node).await);
    assert_eq!(denied.code(), Code::PermissionDenied, "{denied:?}");
}

/// A cell with a deny list, a REAPI client of it, and node-a's session holding the
/// lease of one action: a daemon about to be revoked while it runs work.
async fn node_a_holding_a_lease(
    name: &str,
) -> (Pki, support::Client, support::Job, Session, Start) {
    let pki = Pki::new(name);
    let list = pki.write("deny.list", "");
    let (reapi, worker) = pki.serve_both(&["--worker-deny-list", &list]);
    let cell = support::Client::connect(reapi, worker).await;
    let job = support::Job::new("build", &[]);
    cell.upload(&job.blobs()).await;
    let mut daemon = Session::open(worker, &pki, pki.client(&["node-a"], 0x51), "node-a")
        .await
        .expect("welcomed");
    // The operation stream is not read: the tests check the action cache instead.
    let _operations = cell.execute(&job.action).await;
    let start = daemon.start().await;
    (pki, cell, job, daemon, start)
}

/// Catches: a `Result` not checked against the deny list, so a daemon revoked while
/// it holds a lease writes its result into the action cache on its open stream before
/// its next heartbeat would have ended that stream (the harm issue #79 names).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_daemon_cannot_deliver_a_result() {
    let (pki, cell, job, mut daemon, start) = node_a_holding_a_lease("deny-result").await;
    let result = support::output(&cell, "built by a revoked node", 0).await;
    pki.write("deny.list", "serial 51\n");
    daemon.send(daemon_message::Message::Result(support::ran(
        start.lease_id,
        &result,
    )));
    let ended = daemon
        .next()
        .await
        .expect_err("the Result ended the stream");
    assert_eq!(ended.code(), Code::PermissionDenied, "{ended:?}");
    assert!(ended.message().contains("serial 51"), "{}", ended.message());
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
}

/// Catches: a session that goes on reading its stream after it refused a message
/// (`continue` where it must stop), so what a revoked daemon sends after the refusal
/// is acted on once the list stops naming it: here a `Heartbeat`, and a `Result` for
/// the lease the node still holds, which would land in the action cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_sent_after_a_refusal_is_acted_on() {
    let (pki, cell, job, mut daemon, start) = node_a_holding_a_lease("deny-after").await;
    let result = support::output(&cell, "sent after the refusal", 0).await;
    pki.write("deny.list", "node node-a\n");
    let ended = daemon
        .heartbeat(1)
        .await
        .expect_err("the Heartbeat ended the stream");
    assert_eq!(ended.code(), Code::PermissionDenied, "{ended:?}");

    pki.write("deny.list", "");
    daemon.send_if_open(daemon_message::Message::Heartbeat(Heartbeat {
        seq: 2,
        ..Heartbeat::default()
    }));
    daemon.send_if_open(daemon_message::Message::Result(support::ran(
        start.lease_id,
        &result,
    )));
    assert!(
        !matches!(daemon.next().await, Ok(Some(_))),
        "a message answered after the refusal"
    );
    // No new session for node-a: its first heartbeat would give the lease up, and a
    // late Result would then be refused for that reason instead. The server has had
    // ample time to handle what the old stream sent.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
}

/// `GET /v1/nodes` on the operator API at `api`, as JSON.
async fn nodes(api: SocketAddr) -> serde_json::Value {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let request = "GET /v1/nodes HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    serde_json::from_str(body).expect("JSON")
}

fn macos(version: &str) -> daemon_message::Message {
    daemon_message::Message::NodeStatus(NodeStatus {
        os_name: "macOS".to_owned(),
        os_version: version.to_owned(),
        daemon_version: "0.1.0".to_owned(),
        ..NodeStatus::default()
    })
}

/// Catches: a `NodeStatus` not checked against the deny list, so a node revoked while
/// its stream is open goes on writing the software the operator API reports for it.
/// Also a status recorded before the check. The control: the same stream's status
/// before the revocation is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_daemon_cannot_report_its_status() {
    let pki = Pki::new("deny-status");
    let list = pki.write("deny.list", "");
    let listeners = pki
        .args(&["--worker-deny-list", &list])
        .listeners()
        .expect("listeners");
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: None,
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
    };
    let bound = bind_server_with_api(Arc::new(Cache::memory()), listeners, Some(api), pending())
        .expect("bind");
    let (worker, api) = (bound.worker, bound.api.expect("an API address"));
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    let mut open = Session::open(worker, &pki, pki.client(&["mac-07"], 9), "mac-07")
        .await
        .expect("welcomed");

    open.send(macos("15.1"));
    let deadline = tokio::time::Instant::now() + PROMPT;
    while nodes(api).await["nodes"][0]["software"]["os_version"] != "15.1" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the status never arrived"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    pki.write("deny.list", "node mac-07\n");
    open.send(macos("15.2"));
    let ended = open
        .next()
        .await
        .expect_err("the NodeStatus ended the stream");
    assert_eq!(ended.code(), Code::PermissionDenied, "{ended:?}");
    assert!(
        ended.message().contains("node mac-07"),
        "{}",
        ended.message()
    );
    let got = nodes(api).await;
    assert_eq!(got["nodes"][0]["node_id"], "mac-07", "{got}");
    assert_eq!(got["nodes"][0]["software"]["os_version"], "15.1", "{got}");
}
