//! Issue #79 end to end: the worker listener under mutual TLS binds each stream to the
//! node its client certificate names, and refuses what the deny list lists, reading the
//! list again at every check.

use std::future::pending;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_front::Cache;
use kbf_proto::worker::{
    Capability, DaemonMessage, Heartbeat, Hello, ServerMessage, daemon_message, server_message,
    worker_client::WorkerClient,
};
use kbf_server::{Args, ConfigError, DenyListError, bind_server};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SerialNumber,
};
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
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-server-node-binding")
            .join(name);
        std::fs::create_dir_all(&dir).expect("create the TLS directory");
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
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
        let listeners = self.args(extra).listeners().expect("listeners");
        let bound = bind_server(Arc::new(Cache::memory()), listeners, pending()).expect("bind");
        let addr = bound.worker;
        tokio::spawn(async move { bound.serving.await.expect("serve") });
        addr
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
