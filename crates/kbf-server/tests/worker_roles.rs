//! The worker listener's per-method role table (`kbf_server::worker_roles`): the table
//! and `admit` without a network, the layer over a stand-in service, then a frontend
//! with a state store (so `kbf.map.v1.Membership` is served there) under mutual TLS. A
//! storage certificate registers and heartbeats and a daemon certificate opens its
//! session, side by side; each is refused every method of the other; a frontend
//! certificate is refused every method; and a method not in the table is refused for
//! every certificate and in plain text. Past the role, a storage certificate registers
//! and beats only for the names it carries, and the worker listener's deny list applies
//! to it as it does to a daemon's.

#[path = "support/hooked.rs"]
mod hooked;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::channel::mpsc::unbounded;
use hooked::Hooked;
use kbf_front::Cache;
use kbf_map::{MemberId, MemberState};
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_proto::reapi::FindMissingBlobsRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::worker::{
    Capability, DaemonMessage, Hello, ServerMessage, daemon_message, server_message,
    worker_client::WorkerClient,
};
use kbf_server::members::{Members, MembershipConfig, PROTOCOL};
use kbf_server::worker_roles::{WORKER_LISTENER_ROLES, WorkerRolesLayer, admit, roles_for};
use kbf_server::{DenyList, EpochPolicy, Listeners, StateStore, WorkerTls, bind_server_with_state};
use kbf_state::Store;
use kbf_storage::service::{Admission, PeerRole, ROLE_URI_PREFIX};
use kbf_tls::TlsSource;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use sha2::{Digest, Sha256};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::codegen::{Service, http};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
use tonic::{Code, Status};
use tower_layer::Layer as _;

const PROMPT: Duration = Duration::from_secs(5);

const SESSION: &str = "/kbf.worker.v1.Worker/Session";
const REGISTER: &str = "/kbf.map.v1.Membership/Register";
const HEARTBEAT: &str = "/kbf.map.v1.Membership/Heartbeat";

/// Every method the worker listener serves and the one role that may call it, spelled
/// out here and not read from the table.
const EXPECTED: [(&str, PeerRole); 19] = {
    use PeerRole::{Daemon, Storage};
    [
        (SESSION, Daemon),
        ("/google.bytestream.ByteStream/Read", Daemon),
        ("/google.bytestream.ByteStream/Write", Daemon),
        ("/google.bytestream.ByteStream/QueryWriteStatus", Daemon),
        (
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs",
            Daemon,
        ),
        (
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs",
            Daemon,
        ),
        (
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchReadBlobs",
            Daemon,
        ),
        (
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/GetTree",
            Daemon,
        ),
        (
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/SplitBlob",
            Daemon,
        ),
        (
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/SpliceBlob",
            Daemon,
        ),
        (
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/GetChunkMapping",
            Daemon,
        ),
        (
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/RegisterChunkMapping",
            Daemon,
        ),
        ("/kbf.worker.v1.Segments/Begin", Daemon),
        ("/kbf.worker.v1.Segments/PutRange", Daemon),
        ("/kbf.worker.v1.Segments/Commit", Daemon),
        ("/kbf.worker.v1.Segments/Locate", Daemon),
        ("/kbf.worker.v1.Segments/ReadRange", Daemon),
        (REGISTER, Storage),
        (HEARTBEAT, Storage),
    ]
};

/// Methods the worker listener does not serve: REAPI's others, health, a method of a
/// served service that does not exist, a served one in another case or with a slash
/// after it, and no path.
const UNLISTED: [&str; 7] = [
    "/build.bazel.remote.execution.v2.Execution/Execute",
    "/build.bazel.remote.execution.v2.ActionCache/GetActionResult",
    "/build.bazel.remote.execution.v2.Capabilities/GetCapabilities",
    "/grpc.health.v1.Health/Check",
    "/kbf.worker.v1.Worker/Nothing",
    "/kbf.worker.v1.worker/session",
    "/kbf.map.v1.Membership/Register/",
];

fn role(name: &str) -> SanType {
    SanType::URI(
        format!("{ROLE_URI_PREFIX}{name}")
            .try_into()
            .expect("an IA5 name"),
    )
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

fn ip(text: &str) -> SanType {
    SanType::IpAddress(text.parse().expect("an IP address"))
}

/// A self-signed certificate with these subjectAltNames, as DER.
fn der(sans: Vec<SanType>) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = sans;
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("sign").der().to_vec()
}

/// What the table refuses a certificate naming `who` on `path` with.
fn refusal(who: PeerRole, path: &str, takes: PeerRole) -> String {
    format!("role {who} may not call {path} on the worker listener; it takes {takes}")
}

/// Catches a row of the table changed, added or dropped (the Membership calls taken
/// from a daemon, a blob call or the session from a storage node, any call from a
/// frontend), and `admit` reading it other than it says: each row admits its role's
/// certificate and refuses the other two PERMISSION_DENIED, saying which role it takes.
///
/// Mutant seen red: the Membership rows written `&[Daemon, Storage]`.
#[test]
fn each_method_takes_its_one_role_and_refuses_the_others() {
    let table: Vec<(&str, Vec<PeerRole>)> = WORKER_LISTENER_ROLES
        .iter()
        .map(|(path, roles)| (*path, roles.to_vec()))
        .collect();
    let expected: Vec<(&str, Vec<PeerRole>)> = EXPECTED
        .iter()
        .map(|(path, role)| (*path, vec![*role]))
        .collect();
    assert_eq!(table, expected);
    for (path, takes) in EXPECTED {
        assert_eq!(roles_for(path), Some(&[takes][..]), "{path}");
        for who in PeerRole::ALL {
            let cert = der(vec![role(who.name())]);
            let got = admit(Admission::Certified, path, Some(&cert));
            if who == takes {
                assert!(got.is_ok(), "{who} on {path}: {got:?}");
                continue;
            }
            let refused = got.expect_err("another role");
            assert_eq!(refused.code(), Code::PermissionDenied, "{who} on {path}");
            assert_eq!(refused.message(), refusal(who, path, takes));
        }
    }
}

/// Catches a method missing from the table that is served anyway (default-allow), in
/// plain text or for any role: each is refused PERMISSION_DENIED, saying it is not
/// served. Case and a trailing slash count: the path must be a row byte for byte.
///
/// Mutant seen red: `roles_for` answering `None` with every role.
#[test]
fn a_method_missing_from_the_table_is_refused_whatever_the_caller() {
    for path in UNLISTED {
        assert_eq!(roles_for(path), None, "{path}");
        let mut answers = vec![admit(Admission::Open, path, None)];
        for who in PeerRole::ALL {
            let cert = der(vec![role(who.name())]);
            answers.push(admit(Admission::Certified, path, Some(&cert)));
        }
        for answer in answers {
            let refused = answer.expect_err("not served");
            assert_eq!(refused.code(), Code::PermissionDenied, "{path}");
            assert_eq!(
                refused.message(),
                format!("{path} is not served on the worker listener")
            );
        }
    }
    assert!(admit(Admission::Open, "", None).is_err(), "no path");
}

/// Catches a certified call let through without a certificate or a readable one, or
/// whose certificate names no role, two (the same twice too), a name that is no role,
/// or the role URI in another case or as a DNS name; and plain text asked for a
/// certificate (every trial cell would be refused). Plain text admits a listed method
/// whatever the certificate, as it has no identity to read.
#[test]
fn a_certified_call_needs_exactly_one_role_in_its_certificate() {
    let none = admit(Admission::Certified, SESSION, None).expect_err("none");
    assert_eq!(
        (none.code(), none.message()),
        (Code::Unauthenticated, "no client certificate")
    );
    let junk = admit(Admission::Certified, SESSION, Some(b"junk")).expect_err("junk");
    assert_eq!(junk.code(), Code::Unauthenticated);
    let uri = |text: &str| SanType::URI(text.try_into().expect("IA5"));
    let dns = |text: &str| SanType::DnsName(text.try_into().expect("IA5"));
    for (sans, why) in [
        (vec![dns("node-a")], "it names 0"),
        (vec![role("daemon"), role("storage")], "it names 2"),
        (vec![role("daemon"), role("daemon")], "it names 2"),
        (vec![role("admin")], "\"admin\" is not a role"),
        (vec![uri("URN:KBF:ROLE:daemon")], "it names 0"),
        (vec![dns("urn:kbf:role:daemon")], "it names 0"),
    ] {
        let cert = der(sans.clone());
        let refused = admit(Admission::Certified, SESSION, Some(&cert)).expect_err("refused");
        assert_eq!(refused.code(), Code::PermissionDenied, "{sans:?}");
        assert!(
            refused.message().contains(why),
            "{sans:?}: {}",
            refused.message()
        );
        assert!(admit(Admission::Open, SESSION, Some(&cert)).is_ok());
    }
    assert!(admit(Admission::Open, REGISTER, None).is_ok());
}

/// Counts the calls it serves, and answers each 200 with a body of `served`.
#[derive(Clone, Default)]
struct Inner(Arc<AtomicUsize>);

impl Service<http::Request<()>> for Inner {
    type Response = http::Response<String>;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: http::Request<()>) -> Self::Future {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(http::Response::new("served".to_owned())))
    }
}

async fn through(layer: WorkerRolesLayer, inner: &Inner, path: &str) -> http::Response<String> {
    let mut service = layer.layer(inner.clone());
    std::future::poll_fn(|cx| service.poll_ready(cx))
        .await
        .expect("ready");
    let request = http::Request::builder()
        .uri(path)
        .body(())
        .expect("request");
    service.call(request).await.expect("infallible")
}

/// Catches the layer letting a refused call reach the service, answering it with
/// another status than the table's, or not passing an admitted one on. Under mutual
/// TLS a call that carries no TLS connection (so no certificate) is UNAUTHENTICATED.
#[tokio::test]
async fn the_layer_answers_a_refusal_itself_and_passes_an_admitted_call_on() {
    let inner = Inner::default();
    let open = WorkerRolesLayer::new(Admission::Open);
    let refused = through(open, &inner, UNLISTED[0]).await;
    let status = Status::from_header_map(refused.headers()).expect("a gRPC status");
    assert_eq!(status.code(), Code::PermissionDenied);
    assert!(refused.body().is_empty());
    assert_eq!(inner.0.load(Ordering::SeqCst), 0);

    let certified = WorkerRolesLayer::new(Admission::Certified);
    let refused = through(certified, &inner, SESSION).await;
    let status = Status::from_header_map(refused.headers()).expect("a gRPC status");
    assert_eq!(status.code(), Code::Unauthenticated);
    assert_eq!(inner.0.load(Ordering::SeqCst), 0);

    let served = through(open, &inner, REGISTER).await;
    assert_eq!(served.body(), "served");
    assert_eq!(inner.0.load(Ordering::SeqCst), 1);
    assert_eq!(format!("{open:?}"), "WorkerRolesLayer { admission: Open }");
}

/// A client certificate and what a deny list line spells about it.
struct Client {
    identity: Identity,
    /// SHA-256 of its SubjectPublicKeyInfo, lower-case hex.
    spki_sha256: String,
}

/// A cell CA, the worker listener's certificate, and role certificates on demand.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Self {
            ca: CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA"),
        }
    }

    /// The worker listener's mutual TLS: a `localhost` server certificate (no role).
    fn worker_tls(&self) -> WorkerTls {
        self.worker_tls_denying(None)
    }

    /// [`Pki::worker_tls`] with a deny list, read again at every check.
    fn worker_tls_denying(&self, deny_list: Option<DenyList>) -> WorkerTls {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        WorkerTls {
            server: TlsSource::fixed(
                "worker",
                cert.pem().as_bytes(),
                key.serialize_pem().as_bytes(),
                Some(self.ca.pem().as_bytes()),
            )
            .expect("server TLS"),
            deny_list,
        }
    }

    /// A client certificate for node `node-a` (its one DNS name, as a daemon's must be)
    /// naming `who`.
    fn client(&self, who: PeerRole) -> Identity {
        self.client_named(who, vec![dns("node-a")], None).identity
    }

    /// A client certificate carrying exactly `names` (DNS and IP subjectAltNames) and the
    /// role `who`, with `serial` if one is given.
    fn client_named(&self, who: PeerRole, names: Vec<SanType>, serial: Option<u64>) -> Client {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = names;
        params.subject_alt_names.push(role(who.name()));
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.serial_number = serial.map(SerialNumber::from);
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        let (_, parsed) = x509_parser::parse_x509_certificate(cert.der()).expect("parse");
        let spki_sha256 = Sha256::digest(parsed.public_key().raw)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Client {
            identity: Identity::from_pem(cert.pem(), key.serialize_pem()),
            spki_sha256,
        }
    }

    /// A connection to the worker listener at `addr` presenting `who`'s certificate.
    async fn channel(&self, addr: SocketAddr, who: PeerRole) -> Channel {
        self.channel_as(addr, self.client(who)).await
    }

    /// A connection to the worker listener at `addr` presenting `identity`.
    async fn channel_as(&self, addr: SocketAddr, identity: Identity) -> Channel {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost")
            .identity(identity);
        Endpoint::from_shared(format!("https://{addr}"))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
            .connect()
            .await
            .expect("connect")
    }
}

fn listeners(worker_tls: Option<WorkerTls>) -> Listeners {
    let loopback: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls,
        heartbeat_interval: Duration::from_secs(5),
        hello_wait: Duration::from_secs(10),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamTiming::default(),
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// A frontend with a state store, so its worker listener serves `Membership` as well
/// as the daemons' calls. Returns the worker listener's address.
fn frontend(worker_tls: Option<WorkerTls>) -> SocketAddr {
    frontend_over(worker_tls).0
}

/// [`frontend`], and the state store behind it, so a test can read the map it keeps.
fn frontend_over(worker_tls: Option<WorkerTls>) -> (SocketAddr, Arc<Hooked>) {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: MembershipConfig {
            ttl: Duration::from_secs(60),
            dwell: Duration::from_secs(60),
            replication: 2,
        },
        history: None,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::memory()),
        listeners(worker_tls),
        None,
        kbf_auth::Policy::allow_all(),
        Some(state),
        std::future::pending::<()>(),
    )
    .expect("binds");
    tokio::spawn(bound.serving);
    (bound.worker, store)
}

/// The storage node `node-a`, as the certificate `Pki::client` mints for it names it.
fn registration() -> pb::RegisterRequest {
    register_as("node-a:8982", "node-a:8982")
}

/// A registration under `instance` that says it is dialled at `addr`.
fn register_as(instance: &str, addr: &str) -> pb::RegisterRequest {
    pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: instance.to_owned(),
        addr: addr.to_owned(),
        weight: 1,
        zone: "a".to_owned(),
        protocol: PROTOCOL,
        ..pb::RegisterRequest::default()
    }
}

/// The state member `id` is in, in the map the store holds.
fn state_of(store: &Arc<Hooked>, id: u64) -> MemberState {
    let members = Members::new(
        Arc::clone(&store.fake) as Arc<dyn Store>,
        MembershipConfig::default(),
    );
    let map = members.current_map().expect("read").expect("a map");
    map.member(MemberId(id)).expect("the member").state
}

/// Opens a worker session for node-a on `channel` and waits for its `Welcome`, or the
/// status the server refused it with.
async fn session(channel: &Channel) -> Result<(), Status> {
    let entry = |key: &str, value: &str| Capability {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    let hello = daemon_message::Message::Hello(Hello {
        protocol_version: 1,
        node_id: "node-a".to_owned(),
        capabilities: vec![
            entry("arch", "x86_64"),
            entry("cpus", "1"),
            entry("mem_gib", "1"),
            entry("drivers", "fake"),
        ],
        ..Hello::default()
    });
    let (tx, rx) = unbounded();
    tx.unbounded_send(DaemonMessage {
        message: Some(hello),
    })
    .expect("open");
    let mut client = WorkerClient::new(channel.clone());
    let mut inbound = tokio::time::timeout(PROMPT, client.session(rx))
        .await
        .expect("an answer in time")?
        .into_inner();
    let first = tokio::time::timeout(PROMPT, inbound.message())
        .await
        .expect("an answer in time")?;
    match first {
        Some(ServerMessage {
            message: Some(server_message::Message::Welcome(_)),
        }) => Ok(()),
        other => panic!("expected Welcome or an error, got {other:?}"),
    }
}

/// Calls `path` on `channel` with an empty message, as no generated client would, and
/// returns the answer's status (OK if it was answered).
async fn raw(channel: &Channel, path: &'static str) -> Status {
    let mut grpc = tonic::client::Grpc::new(channel.clone());
    grpc.ready().await.expect("ready");
    let codec = tonic_prost::ProstCodec::<(), ()>::default();
    let call = grpc.unary(
        tonic::Request::new(()),
        PathAndQuery::from_static(path),
        codec,
    );
    match tokio::time::timeout(PROMPT, call)
        .await
        .expect("an answer in time")
    {
        Ok(_) => Status::ok(""),
        Err(status) => status,
    }
}

/// Asserts that `who` is refused every row of the table that does not take it, by the
/// table (its message, not a service's own check).
async fn refused_by_the_table(channel: &Channel, who: PeerRole) {
    for (path, takes) in EXPECTED.into_iter().filter(|(_, takes)| *takes != who) {
        let status = raw(channel, path).await;
        assert_eq!(status.code(), Code::PermissionDenied, "{who} on {path}");
        assert_eq!(status.message(), refusal(who, path, takes));
    }
}

/// Catches a storage node and a daemon that cannot both be served: one node's two
/// certificates, one per connection, on one frontend at the same time. The storage
/// certificate registers and heartbeats; the daemon certificate opens its session and
/// finds blobs; the storage connection heartbeats again after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_half_of_a_node_is_served_on_its_own_certificate() {
    let pki = Pki::new();
    let worker = frontend(Some(pki.worker_tls()));
    let mut membership = MembershipClient::new(pki.channel(worker, PeerRole::Storage).await);
    let registered = membership
        .register(registration())
        .await
        .expect("a storage certificate registers")
        .into_inner();
    let beat = pb::HeartbeatRequest {
        member_id: registered.member_id,
        boot: registered.boot,
        ..pb::HeartbeatRequest::default()
    };
    membership
        .heartbeat(beat)
        .await
        .expect("a storage certificate heartbeats");

    let daemon = pki.channel(worker, PeerRole::Daemon).await;
    session(&daemon).await.expect("a daemon opens its session");
    ContentAddressableStorageClient::new(daemon)
        .find_missing_blobs(FindMissingBlobsRequest::default())
        .await
        .expect("a daemon finds blobs");
    membership
        .heartbeat(beat)
        .await
        .expect("the storage connection is still served");
}

/// Catches the Membership calls taken from a daemon's certificate on the worker
/// listener: Register and Heartbeat, called as a daemon would, are refused
/// PERMISSION_DENIED by the table (its message: the handler's own storage check is
/// behind it and says otherwise), and so is every other row that is not a daemon's.
///
/// Mutant seen red: the Membership rows written `&[Daemon, Storage]` (the handler then
/// refuses with its own message).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_certificate_may_not_register_or_heartbeat() {
    let pki = Pki::new();
    let worker = frontend(Some(pki.worker_tls()));
    let channel = pki.channel(worker, PeerRole::Daemon).await;
    let mut membership = MembershipClient::new(channel.clone());
    let register = membership
        .register(registration())
        .await
        .expect_err("refused");
    let heartbeat = membership
        .heartbeat(pb::HeartbeatRequest {
            member_id: 1,
            boot: 1,
            ..pb::HeartbeatRequest::default()
        })
        .await
        .expect_err("refused");
    for (path, status) in [(REGISTER, register), (HEARTBEAT, heartbeat)] {
        assert_eq!(status.code(), Code::PermissionDenied, "{path}");
        let want = refusal(PeerRole::Daemon, path, PeerRole::Storage);
        assert_eq!(status.message(), want);
    }
    refused_by_the_table(&channel, PeerRole::Daemon).await;
}

/// Catches the worker session or a blob call taken from a storage node's certificate
/// (naming node-a, as a daemon's would, so nothing but the role tells them apart): its
/// session is refused PERMISSION_DENIED, and every daemon row is refused by the table.
///
/// Mutant seen red: the layer left off the worker listener (`worker_peers` without
/// `.layer`): the session is welcomed and FindMissingBlobs answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_certificate_opens_no_session_and_makes_no_blob_call() {
    let pki = Pki::new();
    let worker = frontend(Some(pki.worker_tls()));
    let channel = pki.channel(worker, PeerRole::Storage).await;
    let status = session(&channel).await.expect_err("refused");
    assert_eq!(status.code(), Code::PermissionDenied);
    assert_eq!(
        status.message(),
        refusal(PeerRole::Storage, SESSION, PeerRole::Daemon)
    );
    refused_by_the_table(&channel, PeerRole::Storage).await;
}

/// Catches a frontend's certificate (same CA) taken for anything on the worker
/// listener: every row of the table refuses it PERMISSION_DENIED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_certificate_may_call_nothing() {
    let pki = Pki::new();
    let worker = frontend(Some(pki.worker_tls()));
    let channel = pki.channel(worker, PeerRole::Frontend).await;
    refused_by_the_table(&channel, PeerRole::Frontend).await;
}

/// Catches a method the listener does not list but answers anyway: under mutual TLS
/// for each role, and in plain text, each is refused PERMISSION_DENIED (not
/// UNIMPLEMENTED, which is what the router would answer without the table). The control
/// in plain text: a listed method (Register) is served.
///
/// Mutant seen red: `roles_for` answering `None` with every role.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_method_missing_from_the_table_is_refused_on_the_wire() {
    let pki = Pki::new();
    let worker = frontend(Some(pki.worker_tls()));
    let plain = frontend(None);
    let plain = Endpoint::from_shared(format!("http://{plain}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut channels = vec![plain.clone()];
    for who in PeerRole::ALL {
        channels.push(pki.channel(worker, who).await);
    }
    for channel in &channels {
        for path in UNLISTED {
            let status = raw(channel, path).await;
            assert_eq!(status.code(), Code::PermissionDenied, "{path}: {status:?}");
            assert_eq!(
                status.message(),
                format!("{path} is not served on the worker listener")
            );
        }
    }
    MembershipClient::new(plain)
        .register(registration())
        .await
        .expect("plain text serves a listed method");
}

/// A client of the membership service presenting a storage certificate that carries
/// `names`.
async fn storage_node(
    pki: &Pki,
    worker: SocketAddr,
    names: Vec<SanType>,
) -> MembershipClient<Channel> {
    let client = pki.client_named(PeerRole::Storage, names, None);
    MembershipClient::new(pki.channel_as(worker, client.identity).await)
}

/// Catches a storage certificate that may register any instance id and address. Before
/// the check, the certificate for `node-a` registered under another host's instance id
/// (taking over that member's id and raising its boot, which left the real node's
/// heartbeats superseded) or with another host's address (so peers and frontends dialled
/// a node the attacker named). Here the certificate names `node-a` and every
/// registration that puts another host in the instance id or the address is
/// PERMISSION_DENIED, naming the value and the certificate's names, and writes nothing
/// (the first registration admitted is member 1). Shapes that read like `node-a` but
/// are another host (a longer name, a prefixed one, userinfo in front of another host)
/// are refused; the host of `host:port`, a bare host, and the name in another case are
/// admitted, each its own member.
///
/// Mutants seen red: the instance id left unchecked; the address left unchecked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_certificate_registers_only_the_instance_and_address_it_names() {
    let pki = Pki::new();
    let worker = frontend(Some(pki.worker_tls()));
    let mut node = storage_node(&pki, worker, vec![dns("node-a")]).await;
    for (instance, addr, offending) in [
        ("node-b:8982", "node-a:8982", "node-b:8982"),
        ("node-a:8982", "node-b:8982", "node-b:8982"),
        ("node-b:1", "node-c:1", "node-b:1"),
        ("node-a.example:1", "node-a:1", "node-a.example:1"),
        ("anode-a:1", "node-a:1", "anode-a:1"),
        ("node-a:1", "node-a@node-b:8982", "node-a@node-b:8982"),
        ("node-a:1", "node-a:1/x", "node-a:1/x"),
    ] {
        let status = node
            .register(register_as(instance, addr))
            .await
            .expect_err(offending);
        assert_eq!(status.code(), Code::PermissionDenied, "{instance} {addr}");
        let message = status.message();
        assert!(message.contains(offending), "{message}");
        assert!(
            message.contains("node-a"),
            "the certificate's names: {message}"
        );
    }
    for (want, (instance, addr)) in [
        ("node-a:9999", "node-a:8982"),
        ("node-a", "node-a"),
        ("NODE-A:1", "Node-A:2"),
    ]
    .into_iter()
    .enumerate()
    {
        let joined = node
            .register(register_as(instance, addr))
            .await
            .expect("admitted")
            .into_inner();
        let id = u64::try_from(want).expect("small") + 1;
        assert_eq!(
            joined.member_id, id,
            "{instance} {addr}: no refusal wrote a member"
        );
    }
}

/// Catches a certificate with no name for the host taken for one that has. A storage
/// certificate with only its role, with a wildcard name (no wildcard is matched, not even
/// against its own spelling), with an IP written as a DNS name, or with another host's
/// IP registers nothing: PERMISSION_DENIED, saying to reissue the certificate with the
/// node's names. A certificate that does name the host is member 1 afterwards, so none
/// of the refusals wrote a member.
///
/// Mutants seen red: a name starting `*.` matched against the host's parent domain;
/// a DNS name compared to an IP host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_with_no_covering_name_registers_nothing() {
    let pki = Pki::new();
    let worker = frontend(Some(pki.worker_tls()));
    for (names, host) in [
        (vec![], "node-a:1"),
        (vec![dns("*.farm.example")], "node-a.farm.example:1"),
        (vec![dns("*.farm.example")], "*.farm.example:1"),
        (vec![dns("192.0.2.1")], "192.0.2.1:1"),
        (vec![ip("192.0.2.2")], "192.0.2.1:1"),
        (vec![dns("node-b")], "node-a:1"),
    ] {
        let mut node = storage_node(&pki, worker, names.clone()).await;
        let status = node
            .register(register_as(host, host))
            .await
            .expect_err(host);
        assert_eq!(status.code(), Code::PermissionDenied, "{names:?} {host}");
        assert!(
            status.message().contains("reissue"),
            "{names:?} {host}: {}",
            status.message()
        );
    }
    let mut named = storage_node(&pki, worker, vec![dns("node-a")]).await;
    let joined = named
        .register(registration())
        .await
        .expect("a certificate that names the host registers")
        .into_inner();
    assert_eq!(joined.member_id, 1, "no refusal wrote a member");
}

/// Catches an IP name matched as text. A certificate with the IP names `::1` and
/// `192.0.2.1` covers an address whose host is that IP in any spelling (the bracketed
/// literal with or without a port, the expanded form), and nothing else: another IP,
/// and an unbracketed `::1:8982` (an IPv6 address of its own, not `::1` and a port).
///
/// Mutant seen red: the port cut at the last `:` whatever comes before it (`::1:8982`
/// read as `::1` and a port).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ip_name_covers_the_host_of_the_address_in_any_spelling() {
    let pki = Pki::new();
    let worker = frontend(Some(pki.worker_tls()));
    let mut node = storage_node(&pki, worker, vec![ip("::1"), ip("192.0.2.1")]).await;
    for host in [
        "[::1]:8982",
        "[0:0:0:0:0:0:0:1]:9",
        "[::1]",
        "192.0.2.1:8982",
        "192.0.2.1",
    ] {
        node.register(register_as(host, host))
            .await
            .unwrap_or_else(|e| panic!("{host} is covered: {e}"));
    }
    for host in ["[::2]:1", "192.0.2.2:1", "::1:8982", "[::1", "[::1]x"] {
        let status = node
            .register(register_as(host, host))
            .await
            .expect_err(host);
        assert_eq!(status.code(), Code::PermissionDenied, "{host}");
    }
}

/// Catches a heartbeat that any storage certificate may send for any member, and a
/// registration that takes another node's instance over. Node A (`node-a`) registers
/// rebuilding (`CatchingUp`) and node B (`node-b`) registers beside it. B's certificate
/// then beats A's member id with A's boot, saying the rebuild is done: PERMISSION_DENIED,
/// and A stays `CatchingUp` (before the check this promoted A to `Active`). With another
/// boot it is the same PERMISSION_DENIED and not the FAILED_PRECONDITION that would tell
/// B the boot is wrong (the check comes before the boot is compared, so a foreign
/// certificate learns nothing about a boot). B registering A's instance id is refused
/// and A's boot is still the live one. A's own certificate still gets
/// FAILED_PRECONDITION for a wrong boot, and its right one promotes it.
///
/// Mutants seen red: the holder check removed from the heartbeat; the check placed after
/// the boot comparison.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_certificate_beats_only_for_the_member_whose_address_it_names() {
    let pki = Pki::new();
    let (worker, store) = frontend_over(Some(pki.worker_tls()));
    let mut a = storage_node(&pki, worker, vec![dns("node-a")]).await;
    let mut b = storage_node(&pki, worker, vec![dns("node-b")]).await;
    let first = a
        .register(pb::RegisterRequest {
            catching_up: true,
            ..registration()
        })
        .await
        .expect("A registers")
        .into_inner();
    let second = b
        .register(register_as("node-b:8982", "node-b:8982"))
        .await
        .expect("B registers")
        .into_inner();
    assert_eq!(state_of(&store, first.member_id), MemberState::CatchingUp);

    let beat = |joined: &pb::RegisterResponse, boot: u64| pb::HeartbeatRequest {
        member_id: joined.member_id,
        boot,
        catching_up: false,
        ..pb::HeartbeatRequest::default()
    };
    for boot in [first.boot, first.boot + 7] {
        let status = b
            .heartbeat(beat(&first, boot))
            .await
            .expect_err("B beats A's member");
        assert_eq!(status.code(), Code::PermissionDenied, "boot {boot}");
        assert!(
            status.message().contains(&first.member_id.to_string()),
            "{}",
            status.message()
        );
    }
    assert_eq!(
        state_of(&store, first.member_id),
        MemberState::CatchingUp,
        "B's certificate did not say A's rebuild is done"
    );

    let takeover = b
        .register(register_as("node-a:8982", "node-b:8982"))
        .await
        .expect_err("B registers A's instance");
    assert_eq!(takeover.code(), Code::PermissionDenied);

    let stale = a
        .heartbeat(beat(&first, first.boot + 7))
        .await
        .expect_err("a wrong boot");
    assert_eq!(stale.code(), Code::FailedPrecondition);
    a.heartbeat(beat(&first, first.boot))
        .await
        .expect("A's boot is still the live one");
    assert_eq!(state_of(&store, first.member_id), MemberState::Active);
    b.heartbeat(beat(&second, second.boot))
        .await
        .expect("B beats its own member");
}

/// Where a test's deny list lives.
fn list_path(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("worker-roles");
    std::fs::create_dir_all(&dir).expect("directory");
    dir.join(name)
}

/// Replaces the list at `path` by renaming a new file over it, as the list's module docs
/// ask, so the server sees the change at its next check.
fn write_list(path: &Path, text: &str) {
    let staged = path.with_extension("new");
    std::fs::write(&staged, text).expect("write");
    std::fs::rename(&staged, path).expect("replace");
}

/// Catches `--worker-deny-list` that stops at the daemons: a storage certificate named
/// in the list kept registering and beating. Four storage nodes register and beat while
/// the list is empty. The list then names node A's certificate by serial (in the
/// list's spelling, not the DER's), node B's by public key and node C's node id, and not
/// node D: the three are refused Register and Heartbeat PERMISSION_DENIED, saying what
/// matched, and D goes on. Emptying the list lets A beat again (the list is read at
/// every check).
///
/// Mutants seen red: Register not checked; Heartbeat not checked; the node entry matched
/// against nothing (only serial and key looked at).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listed_storage_certificate_may_neither_register_nor_heartbeat() {
    let pki = Pki::new();
    let path = list_path("storage.list");
    write_list(&path, "# nobody yet\n");
    let list = DenyList::open(&path).expect("the list");
    let worker = frontend(Some(pki.worker_tls_denying(Some(list))));
    let mut nodes = Vec::new();
    for (name, serial) in [
        ("node-a", 0x0a1b),
        ("node-b", 2),
        ("node-c", 3),
        ("node-d", 4),
    ] {
        let cert = pki.client_named(PeerRole::Storage, vec![dns(name)], Some(serial));
        let mut client = MembershipClient::new(pki.channel_as(worker, cert.identity).await);
        let host = format!("{name}:8982");
        let joined = client
            .register(register_as(&host, &host))
            .await
            .expect("registers while nothing is listed")
            .into_inner();
        let beat = pb::HeartbeatRequest {
            member_id: joined.member_id,
            boot: joined.boot,
            ..pb::HeartbeatRequest::default()
        };
        client.heartbeat(beat).await.expect("beats");
        nodes.push((client, host, beat, cert.spki_sha256));
    }

    let key_of_b = nodes[1].3.clone();
    write_list(
        &path,
        &format!("serial 0A:1B\nspki-sha256 {key_of_b}\nnode node-c\n"),
    );
    for (at, matched) in [
        (0, "denied: certificate serial a1b".to_owned()),
        (1, format!("denied: public key {key_of_b}")),
        (2, "denied: node node-c".to_owned()),
    ] {
        let (client, host, beat, _) = &mut nodes[at];
        let registered = client
            .register(register_as(host.as_str(), host.as_str()))
            .await
            .expect_err("a listed certificate registers");
        assert_eq!(registered.code(), Code::PermissionDenied, "{host}");
        assert_eq!(registered.message(), matched);
        let beaten = client
            .heartbeat(*beat)
            .await
            .expect_err("a listed certificate beats");
        assert_eq!(beaten.code(), Code::PermissionDenied, "{host}");
        assert_eq!(beaten.message(), matched);
    }
    let (client, host, beat, _) = &mut nodes[3];
    client.heartbeat(*beat).await.expect("D is not listed");
    client
        .register(register_as(host.as_str(), host.as_str()))
        .await
        .expect("D registers again");

    write_list(&path, "");
    let (client, _, beat, _) = &mut nodes[0];
    client
        .heartbeat(*beat)
        .await
        .expect("A is no longer listed");
}

/// Catches an unreadable deny list that lets storage nodes through: it refuses every
/// Register and Heartbeat UNAVAILABLE (a node retries those) until the file is fixed,
/// as it refuses a daemon's stream, and says the list is unusable.
///
/// Mutant seen red: a list that fails to parse treated as an empty one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unusable_deny_list_refuses_membership_calls_unavailable() {
    let pki = Pki::new();
    let path = list_path("broken.list");
    write_list(&path, "serial 0a\n");
    let list = DenyList::open(&path).expect("the list");
    let worker = frontend(Some(pki.worker_tls_denying(Some(list))));
    let mut node = MembershipClient::new(pki.channel(worker, PeerRole::Storage).await);
    let joined = node
        .register(registration())
        .await
        .expect("registers")
        .into_inner();
    let beat = pb::HeartbeatRequest {
        member_id: joined.member_id,
        boot: joined.boot,
        ..pb::HeartbeatRequest::default()
    };

    write_list(&path, "this is not an entry\n");
    let registered = node.register(registration()).await.expect_err("refused");
    let beaten = node.heartbeat(beat).await.expect_err("refused");
    for status in [registered, beaten] {
        assert_eq!(status.code(), Code::Unavailable);
        assert!(
            status.message().contains("deny list is unusable"),
            "{}",
            status.message()
        );
    }
    write_list(&path, "serial 0a\n");
    node.heartbeat(beat)
        .await
        .expect("the fixed list admits it");
}
