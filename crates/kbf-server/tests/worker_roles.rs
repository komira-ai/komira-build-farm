//! The worker listener's per-method role table (`kbf_server::worker_roles`): the table
//! and `admit` without a network, the layer over a stand-in service, then a frontend
//! with a state store (so `kbf.map.v1.Membership` is served there) under mutual TLS. A
//! storage certificate registers and heartbeats and a daemon certificate opens its
//! session, side by side; each is refused every method of the other; a frontend
//! certificate is refused every method; and a method not in the table is refused for
//! every certificate and in plain text.

#[path = "support/hooked.rs"]
mod hooked;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::channel::mpsc::unbounded;
use hooked::Hooked;
use kbf_front::Cache;
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_proto::reapi::FindMissingBlobsRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::worker::{
    Capability, DaemonMessage, Hello, ServerMessage, daemon_message, server_message,
    worker_client::WorkerClient,
};
use kbf_server::members::{MembershipConfig, PROTOCOL};
use kbf_server::worker_roles::{WORKER_LISTENER_ROLES, WorkerRolesLayer, admit, roles_for};
use kbf_server::{EpochPolicy, Listeners, StateStore, WorkerTls, bind_server_with_state};
use kbf_state::Store;
use kbf_storage::service::{Admission, PeerRole, ROLE_URI_PREFIX};
use kbf_tls::TlsSource;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
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
const EXPECTED: [(&str, PeerRole); 20] = {
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
        ("/kbf.update.v1.ReleaseService/Fetch", PeerRole::Daemon),
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
            deny_list: None,
        }
    }

    /// A client certificate for node `node-a` (its one DNS name, as a daemon's must be)
    /// naming `who`.
    fn client(&self, who: PeerRole) -> Identity {
        let mut params = CertificateParams::new(vec!["node-a".to_owned()]).expect("params");
        params.subject_alt_names.push(role(who.name()));
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        Identity::from_pem(cert.pem(), key.serialize_pem())
    }

    /// A connection to the worker listener at `addr` presenting `who`'s certificate.
    async fn channel(&self, addr: SocketAddr, who: PeerRole) -> Channel {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost")
            .identity(self.client(who));
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
    bound.worker
}

fn registration() -> pb::RegisterRequest {
    pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: "storage-a".to_owned(),
        addr: "192.0.2.1:8982".to_owned(),
        weight: 1,
        zone: "a".to_owned(),
        protocol: PROTOCOL,
        ..pb::RegisterRequest::default()
    }
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
