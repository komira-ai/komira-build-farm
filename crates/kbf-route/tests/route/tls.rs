//! Mutual TLS to real storage nodes: each is verified against its own name.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kbf_map::{Member, MemberId, MemberState};
use kbf_route::{Liveness, Router, TlsNodes};
use kbf_storage::service::{Admission, Config, StorageService};
use kbf_storage::{Storage, StorageError};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

use super::support::{Cell, Node, blob_read_from, blob_where, blobs_read_from, collect, map, view};

/// The farm's one CA.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca =
            CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
        Self { ca }
    }

    /// A certificate for the name `san`, signed by the CA, with this EKU.
    fn leaf(&self, san: SanType, eku: ExtendedKeyUsagePurpose) -> Identity {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = vec![san];
        params.extended_key_usages = vec![eku];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        Identity::from_pem(cert.pem(), key.serialize_pem())
    }

    fn server(&self, san: SanType) -> Identity {
        self.leaf(san, ExtendedKeyUsagePurpose::ServerAuth)
    }

    fn client(&self) -> Identity {
        let name = SanType::DnsName("a-daemon".try_into().expect("an IA5 name"));
        self.leaf(name, ExtendedKeyUsagePurpose::ClientAuth)
    }

    fn ca(&self) -> Certificate {
        Certificate::from_pem(self.ca.pem())
    }

    /// Nodes that trust the CA and present a client certificate.
    fn nodes(&self) -> Arc<TlsNodes> {
        Arc::new(TlsNodes::new(self.ca(), self.client()))
    }

    /// Nodes as [`Pki::nodes`], giving up on a silent one as `liveness` says.
    fn nodes_with(&self, liveness: Liveness) -> Arc<TlsNodes> {
        Arc::new(TlsNodes::new(self.ca(), self.client()).with_liveness(liveness))
    }

    /// `node` served over mutual TLS with `identity` as its certificate.
    async fn serve(&self, node: &Arc<Node>, identity: Identity) -> u16 {
        let storage: Arc<dyn Storage> = node.clone();
        let service = StorageService::new(
            storage,
            Admission::Open,
            Config {
                write_bytes_in_flight: 1 << 20,
            },
        );
        let tls = ServerTlsConfig::new()
            .identity(identity)
            .client_ca_root(self.ca());
        let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let port = incoming.local_addr().expect("address").port();
        let mut server = Server::builder().tls_config(tls).expect("TLS config");
        tokio::spawn(
            server
                .add_routes(service.routes())
                .serve_with_incoming(incoming),
        );
        port
    }
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

fn ip() -> SanType {
    SanType::IpAddress([127, 0, 0, 1].into())
}

fn at(id: u64, addr: String) -> Member {
    Member {
        id: MemberId(id),
        addr,
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    }
}

/// A blob that the map of replication 1 over members 1 and 2 reads from `owner`.
fn blob_of(map: &kbf_map::Map, owner: u64) -> (Vec<u8>, kbf_types::Digest) {
    blob_where(|d| map.read_order(d) == [MemberId(owner)])
}

/// Catches a router that cannot reach nodes over mutual TLS, and the control for the
/// refusals below: node 1 is dialled as `localhost` and presents a certificate for it, node
/// 2 is dialled as `127.0.0.1` and presents a certificate for that, and a blob of each is
/// read.
#[tokio::test]
async fn each_node_is_reached_by_its_own_name() {
    let pki = Pki::new();
    let (one, two) = (Node::new(), Node::new());
    let port_1 = pki.serve(&one, pki.server(dns("localhost"))).await;
    let port_2 = pki.serve(&two, pki.server(ip())).await;
    let members = vec![
        at(1, format!("localhost:{port_1}")),
        at(2, format!("127.0.0.1:{port_2}")),
    ];
    let map = map(1, 1, members);
    let router = Router::new(Cell::with(view(&map)), pki.nodes());
    let (blob_1, digest_1) = blob_of(&map, 1);
    let (blob_2, digest_2) = blob_of(&map, 2);
    one.hold(&blob_1);
    two.hold(&blob_2);

    // Node 1 twice: the second call finds the connection made for the first.
    let reads = [
        (blob_1.clone(), digest_1),
        (blob_2, digest_2),
        (blob_1, digest_1),
    ];
    for (blob, digest) in reads {
        let (bytes, _) = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
            .await
            .expect("bytes");
        assert_eq!(bytes, blob);
    }
}

/// Catches one name checked for every node (a `--tls-server-name` for the farm): node-03,
/// dialled as `127.0.0.1`, presents the certificate that node-02, `localhost`, holds. The
/// farm's CA signed it, but it is not node-03's: the call is refused, and the node never
/// saw a request. Node 02 is still reached by its own name.
#[tokio::test]
async fn one_nodes_certificate_is_refused_when_dialling_another() {
    let pki = Pki::new();
    let (node_02, node_03) = (Node::new(), Node::new());
    let port_02 = pki.serve(&node_02, pki.server(dns("localhost"))).await;
    // Node 03 answers with node 02's name.
    let port_03 = pki.serve(&node_03, pki.server(dns("localhost"))).await;
    let members = vec![
        at(1, format!("localhost:{port_02}")),
        at(2, format!("127.0.0.1:{port_03}")),
    ];
    let map = map(1, 1, members);
    let router = Router::new(Cell::with(view(&map)), pki.nodes());
    let (blob_02, digest_02) = blob_of(&map, 1);
    let (blob_03, digest_03) = blob_of(&map, 2);
    node_02.hold(&blob_02);
    node_03.hold(&blob_03);

    let error = router
        .read_range(&digest_03, 0, 0)
        .await
        .err()
        .expect("refused");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert_eq!(
        node_03.read_count(),
        0,
        "the handshake failed before any call"
    );

    let (bytes, _) = collect(router.read_range(&digest_02, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, blob_02);
}

/// Catches a connection kept for a member that moved: member 1 is at node A, then at node
/// B under the same id, and a blob only B holds is read.
#[tokio::test]
async fn a_member_that_moves_is_dialled_at_its_new_address() {
    let pki = Pki::new();
    let (a, b) = (Node::new(), Node::new());
    let port_a = pki.serve(&a, pki.server(dns("localhost"))).await;
    let port_b = pki.serve(&b, pki.server(ip())).await;
    let first = map(1, 1, vec![at(1, format!("localhost:{port_a}"))]);
    let moved = map(2, 1, vec![at(1, format!("127.0.0.1:{port_b}"))]);
    let cell = Cell::with(view(&first));
    let router = Router::new(cell.clone(), pki.nodes());
    let digest = b.hold(b"only the new address has this");
    a.hold(b"only the old address has that");

    let error = router
        .read_range(&digest, 0, 0)
        .await
        .err()
        .expect("not at A");
    assert!(matches!(error, StorageError::NotFound(_)), "{error:?}");
    cell.set(view(&moved));
    let (bytes, _) = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, b"only the new address has this");
}

/// Catches an address that is not one dialled anyway, and a TLS configuration that is not
/// usable reported as an outage of the node: both are internal errors naming the member.
#[tokio::test]
async fn a_member_that_cannot_be_dialled_is_an_internal_error() {
    let pki = Pki::new();
    let bad = at(7, "not a host:1".to_owned());
    let error = pki.nodes().channel(&bad).expect_err("not an address");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
    assert!(error.to_string().contains("m7"), "{error}");

    let junk = TlsNodes::new(
        Certificate::from_pem("not a certificate"),
        Identity::from_pem("not a certificate", "not a key"),
    );
    let error = junk
        .channel(&at(8, "localhost:1".to_owned()))
        .expect_err("not usable");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
    assert!(error.to_string().contains("TLS"), "{error}");
}

/// How long a test waits for a call that should have failed over before it calls the call
/// stuck. Not a speed: the calls under test come back in a fraction of it.
const STUCK: Duration = Duration::from_secs(10);

/// Liveness short enough that a test need not wait for the defaults, and long enough that a
/// healthy node on a busy machine is not given up on.
fn quick() -> Liveness {
    Liveness {
        connect: Duration::from_secs(1),
        keepalive_interval: Duration::from_millis(500),
        keepalive_timeout: Duration::from_millis(500),
    }
}

/// Catches `liveness()` answering something other than what `with_liveness` set (the
/// default always, or one field for another): the daemon's tests read its flags back
/// through it.
#[test]
fn the_nodes_report_the_liveness_they_were_given() {
    let pki = Pki::new();
    assert_eq!(pki.nodes().liveness(), Liveness::default());
    assert_eq!(pki.nodes_with(quick()).liveness(), quick());
}

/// Reads `digest` whole through `router`, or fails the test if the read is stuck.
async fn read_whole(router: &Router, digest: &kbf_types::Digest) -> Vec<u8> {
    let read = tokio::time::timeout(STUCK, router.read_blob(digest)).await;
    read.expect("the read was stuck on a node that does not answer")
        .expect("a blob")
        .to_vec()
}

/// Catches a TLS handshake that waits for ever: node 1 takes the TCP connection (the
/// operating system does, nobody accepts it) and never speaks, node 2 is a real node. A
/// blob both own is read from node 2 because node 1 is given up on, not waited for.
#[tokio::test]
async fn a_node_that_takes_the_connection_and_never_speaks_is_given_up_on() {
    let pki = Pki::new();
    let mute = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let mute_port = mute.local_addr().expect("address").port();
    let two = Node::new();
    let port_2 = pki.serve(&two, pki.server(ip())).await;
    let map = map(
        1,
        2,
        vec![
            at(1, format!("127.0.0.1:{mute_port}")),
            at(2, format!("127.0.0.1:{port_2}")),
        ],
    );
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    two.hold(&blob);
    let router = Router::new(Cell::with(view(&map)), pki.nodes_with(quick()));

    assert_eq!(read_whole(&router, &digest).await, blob);
    assert_eq!(two.read_count(), 1);
    drop(mute);
}

/// Catches a TCP connect that waits for the operating system: node 1 is a listener whose
/// queue of connections nobody has accepted is full, so a new connection gets no answer at
/// all (the packets are dropped, as a firewall would). Node 2 is a real node.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_node_whose_packets_are_dropped_is_given_up_on() {
    let pki = Pki::new();
    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .expect("bind");
    let mute_port = socket.local_addr().expect("address").port();
    let listener = socket.listen(1).expect("listen");
    // Connections up to the queue's length are taken by the system; keep them open until
    // one is not.
    let mut filling = Vec::new();
    for _ in 0..16 {
        let dial = tokio::net::TcpStream::connect(("127.0.0.1", mute_port));
        match tokio::time::timeout(Duration::from_millis(500), dial).await {
            Ok(connected) => filling.push(connected.expect("connected")),
            Err(_) => break,
        }
    }
    assert!(filling.len() < 16, "the queue never filled");

    let two = Node::new();
    let port_2 = pki.serve(&two, pki.server(ip())).await;
    let map = map(
        1,
        2,
        vec![
            at(1, format!("127.0.0.1:{mute_port}")),
            at(2, format!("127.0.0.1:{port_2}")),
        ],
    );
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    two.hold(&blob);
    let router = Router::new(Cell::with(view(&map)), pki.nodes_with(quick()));

    assert_eq!(read_whole(&router, &digest).await, blob);
    drop((listener, filling));
}

/// A TCP relay to `upstream` that can be told to stop passing bytes while keeping both
/// connections open: from then on the node behind it is a host that has dropped off the
/// network.
struct Relay {
    port: u16,
    stalled: Arc<AtomicBool>,
}

impl Relay {
    async fn to(upstream: u16) -> Self {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        let stalled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stalled);
        tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    return;
                };
                let Ok(server) = tokio::net::TcpStream::connect(("127.0.0.1", upstream)).await
                else {
                    return;
                };
                let (from_client, to_client) = client.into_split();
                let (from_server, to_server) = server.into_split();
                tokio::spawn(pass(from_client, to_server, Arc::clone(&flag)));
                tokio::spawn(pass(from_server, to_client, Arc::clone(&flag)));
            }
        });
        Self { port, stalled }
    }

    fn stall(&self) {
        self.stalled.store(true, Ordering::SeqCst);
    }
}

/// Copies `from` to `to` until `from` ends; once `stalled` is set, reads and drops.
async fn pass(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    stalled: Arc<AtomicBool>,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        match from.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if !stalled.load(Ordering::SeqCst) && to.write_all(&buffer[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Catches a connection that is never checked once it is up: node 1 is reached through a
/// relay, answers one read, and then the relay stops passing anything (neither way, and no
/// connection is closed). A read of a blob node 1 holds is sent into the silence; it is
/// failed by the unanswered pings, and the blob comes from node 2.
#[tokio::test]
async fn a_node_that_goes_silent_after_connecting_is_given_up_on() {
    let pki = Pki::new();
    let (one, two) = (Node::new(), Node::new());
    let port_1 = pki.serve(&one, pki.server(ip())).await;
    let port_2 = pki.serve(&two, pki.server(ip())).await;
    let relay = Relay::to(port_1).await;
    let map = map(
        1,
        2,
        vec![
            at(1, format!("127.0.0.1:{}", relay.port)),
            at(2, format!("127.0.0.1:{port_2}")),
        ],
    );
    let blobs = blobs_read_from(&map, &[1, 2], 2);
    for (blob, _) in &blobs {
        one.hold(blob);
    }
    two.hold(&blobs[1].0);
    let router = Router::new(Cell::with(view(&map)), pki.nodes_with(quick()));

    assert_eq!(read_whole(&router, &blobs[0].1).await, blobs[0].0);
    assert_eq!(one.read_count(), 1);

    relay.stall();
    assert_eq!(read_whole(&router, &blobs[1].1).await, blobs[1].0);
    assert_eq!(two.read_count(), 1);
}
