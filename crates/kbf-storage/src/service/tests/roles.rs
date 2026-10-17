//! Who may call what: the role table, the certificate reading, and every endpoint over
//! real mutual TLS.

use std::sync::Arc;

use futures::stream;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{QueryWriteStatusRequest, ReadRequest};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{ActionResult, FindMissingBlobsRequest};
use kbf_proto::storage::action_store_client::ActionStoreClient;
use kbf_proto::storage::{CommitRequest, GetRequest, PrepareRequest, Writer};
use kbf_proto::worker::segments_client::SegmentsClient;
use kbf_proto::worker::{
    BeginRequest, CommitRequest as SegmentsCommitRequest, LocateRequest, PutRangeRequest,
    ReadRangeRequest,
};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity, ServerTlsConfig};
use tonic::{Code, Status};

use super::fake::{Mem, serve_routes, sha};
use crate::Storage;
use crate::service::{
    Admission, Config, Endpoint, PeerRole, ROLE_URI_PREFIX, StorageService, role_of,
};

fn uri(role: &str) -> SanType {
    SanType::URI(
        format!("{ROLE_URI_PREFIX}{role}")
            .try_into()
            .expect("an IA5 name"),
    )
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

/// A self-signed certificate with these subjectAltNames.
fn der(sans: Vec<SanType>) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = sans;
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("sign").der().to_vec()
}

/// The roles section 9 names, as the tests spell them (not read from the code under
/// test), for each endpoint.
fn expected(endpoint: Endpoint) -> &'static [&'static str] {
    match endpoint {
        Endpoint::FindMissing | Endpoint::Read | Endpoint::Write | Endpoint::QueryWriteStatus => {
            &["frontend", "storage", "daemon"]
        }
        Endpoint::WriteLeg => &["storage"],
        Endpoint::ActionGet => &["frontend"],
        Endpoint::ActionPrepare | Endpoint::ActionCommit => &["frontend", "daemon"],
        Endpoint::Segments => &["daemon"],
    }
}

/// Catches an endpoint that takes a role it should not, or refuses one it should take:
/// every role against every endpoint, the answer compared with the table the design
/// gives, written out here.
#[test]
fn each_endpoint_takes_the_roles_the_design_gives_it() {
    let certified = Admission::Certified;
    for role in PeerRole::ALL {
        let leaf = der(vec![uri(role.name())]);
        assert_eq!(role_of(&leaf).expect("a role"), role);
        for endpoint in Endpoint::ALL {
            let answer = certified.check_leaf(Some(&leaf), endpoint);
            if expected(endpoint).contains(&role.name()) {
                assert!(answer.is_ok(), "{role:?} on {endpoint:?}");
            } else {
                let status = answer.expect_err("refused");
                assert_eq!(
                    status.code(),
                    Code::PermissionDenied,
                    "{role:?} on {endpoint:?}"
                );
                assert!(
                    status.message().contains(role.name()),
                    "{}",
                    status.message()
                );
            }
        }
    }
    for endpoint in Endpoint::ALL {
        let names: Vec<&str> = endpoint.allowed().iter().map(|r| r.name()).collect();
        assert_eq!(names, expected(endpoint), "{endpoint:?}");
    }
}

/// Catches a certificate read wrongly: no certificate must be UNAUTHENTICATED, bytes that
/// are not one UNAUTHENTICATED, and a certificate with no role, with two, or with a name
/// that is not a role PERMISSION_DENIED (a DNS name that looks like a role is not one);
/// and plain text, which has no identity, admitting every call.
#[test]
fn a_certificate_must_name_exactly_one_known_role() {
    let certified = Admission::Certified;
    let refused = |leaf: Option<&[u8]>| {
        certified
            .check_leaf(leaf, Endpoint::Read)
            .expect_err("refused")
    };
    assert_eq!(refused(None).code(), Code::Unauthenticated);
    assert_eq!(refused(Some(b"not DER")).code(), Code::Unauthenticated);
    for sans in [
        vec![dns("daemon")],
        vec![dns("urn:kbf:role:daemon")],
        vec![uri("daemon"), uri("storage")],
        vec![uri("admin")],
        vec![],
    ] {
        let leaf = der(sans.clone());
        assert_eq!(
            refused(Some(&leaf)).code(),
            Code::PermissionDenied,
            "{sans:?}"
        );
    }
    for endpoint in Endpoint::ALL {
        let answer = Admission::Open.check_leaf(None, endpoint);
        if endpoint == Endpoint::WriteLeg {
            // No identity shows the caller is a storage node: a copy is never admitted.
            assert_eq!(answer.expect_err("refused").code(), Code::Unauthenticated);
        } else {
            assert!(answer.is_ok(), "{endpoint:?}");
        }
    }
}

/// Catches the unowned-copy marker honoured from a role other than the frontend: under
/// mutual TLS only a certificate naming `frontend` may send it (every other role, no
/// certificate and a certificate with no role may not); plain text has no identity and
/// admits it as it admits every call but a copy.
#[test]
fn only_the_frontend_may_ask_for_an_unowned_copy() {
    for role in PeerRole::ALL {
        let leaf = der(vec![uri(role.name())]);
        assert_eq!(
            Admission::Certified.permits_unowned_leaf(Some(&leaf)),
            role == PeerRole::Frontend,
            "{role:?}"
        );
    }
    assert!(!Admission::Certified.permits_unowned_leaf(None));
    assert!(!Admission::Certified.permits_unowned_leaf(Some(b"not DER")));
    assert!(!Admission::Certified.permits_unowned_leaf(Some(&der(vec![]))));
    assert!(Admission::Open.permits_unowned_leaf(None));
}

/// Catches an unowned marker that changes what a chain copy is: a leg that carries the marker
/// is still a leg (judged by a copy's rules: stored where the map says the node owns the blob,
/// refused where it does not), never an unowned local write. Only a call with no leg, that asked,
/// from a caller that may ask, is taken as an unowned copy. (Admission already refuses a leg
/// from any caller but a storage node, and a storage node may not ask for an unowned copy, so
/// no call reaches the guard with both; it is tested where it is decided.)
#[test]
fn a_chain_copy_with_the_unowned_marker_is_still_a_chain_copy() {
    use super::super::generation::takes_unowned;
    use kbf_map::MemberId;
    let leg = Some(MemberId(2));
    for marked in [false, true] {
        for permitted in [false, true] {
            assert!(
                !takes_unowned(leg, marked, permitted),
                "{marked} {permitted}"
            );
        }
    }
    assert!(takes_unowned(None, true, true));
    assert!(!takes_unowned(None, true, false));
    assert!(!takes_unowned(None, false, true));
}

/// A CA, the server's certificate, and client certificates by role.
pub(crate) struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    pub(crate) fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
        Self { ca }
    }

    fn server(&self) -> ServerTlsConfig {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        ServerTlsConfig::new()
            .identity(Identity::from_pem(cert.pem(), key.serialize_pem()))
            .client_ca_root(Certificate::from_pem(self.ca.pem()))
    }

    /// What a storage node holds under certificate `role`: one certificate that names the
    /// role and carries serverAuth and clientAuth, to serve with and to call its peers with
    /// (the node certificate of the deployment: one role, both usages).
    pub(crate) fn node(&self, role: &str) -> (ServerTlsConfig, ClientTlsConfig) {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.subject_alt_names.push(uri(role));
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        let identity = || Identity::from_pem(cert.pem(), key.serialize_pem());
        let server = ServerTlsConfig::new()
            .identity(identity())
            .client_ca_root(Certificate::from_pem(self.ca.pem()));
        let client = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost")
            .identity(identity());
        (server, client)
    }

    /// A client of the server at `addr`, with a certificate for `role`.
    pub(crate) async fn client(&self, addr: std::net::SocketAddr, role: &str) -> Channel {
        self.client_naming(addr, vec![uri(role)]).await
    }

    /// [`Pki::client`] that connects when first used.
    pub(crate) fn lazy_client(&self, addr: std::net::SocketAddr, role: &str) -> Channel {
        self.endpoint(addr, vec![vec![uri(role)]]).connect_lazy()
    }

    /// A client of the server at `addr`, with a certificate whose subjectAltNames are
    /// `sans`.
    async fn client_naming(&self, addr: std::net::SocketAddr, sans: Vec<SanType>) -> Channel {
        self.client_presenting(addr, vec![sans]).await
    }

    /// A client of the server at `addr` presenting a chain of certificates from the CA,
    /// one per entry of `chain` with those subjectAltNames; the first is the leaf, whose
    /// key the client holds.
    async fn client_presenting(
        &self,
        addr: std::net::SocketAddr,
        chain: Vec<Vec<SanType>>,
    ) -> Channel {
        self.endpoint(addr, chain).connect().await.expect("connect")
    }

    fn endpoint(
        &self,
        addr: std::net::SocketAddr,
        chain: Vec<Vec<SanType>>,
    ) -> tonic::transport::Endpoint {
        let mut pems = Vec::new();
        let mut leaf_key = None;
        for sans in chain {
            let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
            params.subject_alt_names = sans;
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
            let key = KeyPair::generate().expect("key");
            pems.push(params.signed_by(&key, &self.ca).expect("sign").pem());
            leaf_key.get_or_insert(key);
        }
        let key = leaf_key.expect("a chain of at least one certificate");
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost")
            .identity(Identity::from_pem(pems.join("\n"), key.serialize_pem()));
        Channel::from_shared(format!("https://{addr}"))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
    }
}

/// Every gRPC method the service admits, with the endpoint whose roles it takes: one
/// row per method, so a check left out of one `Segments` handler is called too.
const RPCS: [(&str, Endpoint); 12] = [
    ("FindMissingBlobs", Endpoint::FindMissing),
    ("Read", Endpoint::Read),
    ("Write", Endpoint::Write),
    ("QueryWriteStatus", Endpoint::QueryWriteStatus),
    ("ActionStore.Get", Endpoint::ActionGet),
    ("ActionStore.Prepare", Endpoint::ActionPrepare),
    ("ActionStore.Commit", Endpoint::ActionCommit),
    ("Segments.Begin", Endpoint::Segments),
    ("Segments.PutRange", Endpoint::Segments),
    ("Segments.Commit", Endpoint::Segments),
    ("Segments.Locate", Endpoint::Segments),
    ("Segments.ReadRange", Endpoint::Segments),
];

/// One call of the method `rpc` names (a row of [`RPCS`]), as a client of the wire would
/// make it.
async fn call_rpc(rpc: &str, channel: &Channel) -> Result<(), Status> {
    let mut segments = SegmentsClient::new(channel.clone());
    match rpc {
        "Segments.PutRange" => segments
            .put_range(PutRangeRequest::default())
            .await
            .map(drop),
        "Segments.Commit" => segments
            .commit(SegmentsCommitRequest::default())
            .await
            .map(drop),
        "Segments.Locate" => segments.locate(LocateRequest::default()).await.map(drop),
        "Segments.ReadRange" => segments
            .read_range(ReadRangeRequest::default())
            .await
            .map(drop),
        _ => {
            let (_, endpoint) = RPCS
                .into_iter()
                .find(|(name, _)| *name == rpc)
                .expect("a row of RPCS");
            call(endpoint, channel).await
        }
    }
}

/// One call to `endpoint`, as a client of the wire would make it (`Segments.Begin` for
/// the `Segments` endpoint).
async fn call(endpoint: Endpoint, channel: &Channel) -> Result<(), Status> {
    let digest = super::wire::to_proto(&sha(b"x"));
    match endpoint {
        Endpoint::FindMissing => ContentAddressableStorageClient::new(channel.clone())
            .find_missing_blobs(FindMissingBlobsRequest {
                instance_name: String::new(),
                blob_digests: vec![digest],
                digest_function: 0,
            })
            .await
            .map(drop),
        Endpoint::Read => ByteStreamClient::new(channel.clone())
            .read(ReadRequest {
                resource_name: format!("blobs/{}/1", "ab".repeat(32)),
                read_offset: 0,
                read_limit: 0,
            })
            .await
            .map(drop),
        Endpoint::Write => {
            let requests = super::write_of(&super::upload_name(&sha(b"x")), &[(b"x", true)]);
            ByteStreamClient::new(channel.clone())
                .write(stream::iter(requests))
                .await
                .map(drop)
        }
        Endpoint::WriteLeg => {
            let requests = super::write_of(&super::upload_name(&sha(b"x")), &[(b"x", true)]);
            let mut request = tonic::Request::new(stream::iter(requests));
            request
                .metadata_mut()
                .insert(crate::replicate::LEG_HEADER, "2".parse().expect("ASCII"));
            ByteStreamClient::new(channel.clone())
                .write(request)
                .await
                .map(drop)
        }
        Endpoint::QueryWriteStatus => ByteStreamClient::new(channel.clone())
            .query_write_status(QueryWriteStatusRequest {
                resource_name: super::upload_name(&sha(b"x")),
            })
            .await
            .map(drop),
        Endpoint::ActionGet => ActionStoreClient::new(channel.clone())
            .get(GetRequest {
                map_generation: 0,
                action: Some(digest),
            })
            .await
            .map(drop),
        Endpoint::ActionPrepare => ActionStoreClient::new(channel.clone())
            .prepare(PrepareRequest {
                map_generation: 0,
                result: Some(ActionResult::default()),
            })
            .await
            .map(drop),
        Endpoint::ActionCommit => ActionStoreClient::new(channel.clone())
            .commit(CommitRequest {
                map_generation: 0,
                action: Some(digest),
                record: None,
                writer: Writer::Daemon as i32,
            })
            .await
            .map(drop),
        Endpoint::Segments => SegmentsClient::new(channel.clone())
            .begin(BeginRequest::default())
            .await
            .map(drop),
    }
}

/// Catches an unexpected role admitted on any method, over real mutual TLS (the
/// certificate really is the transport's, not a test's stand-in): each of the three
/// roles, one that is not a role, a certificate with no role (a node's DNS name only)
/// and one with two calls every method of [`RPCS`]; a call is refused
/// PERMISSION_DENIED exactly when the certificate does not name one role the endpoint
/// takes, and
/// otherwise gets as far as the store (any other answer, including the store's own
/// errors). A refused `Segments` call never reaches the store's handler.
///
/// Mutants seen red: the check removed from `Segments.Commit` alone (frontend, storage,
/// admin, no role and two roles all reach it); `contains` negated in
/// `Admission::check_leaf`.
#[tokio::test]
async fn every_method_refuses_an_unexpected_role_over_mutual_tls() {
    let pki = Pki::new();
    let mem = Arc::new(Mem::default());
    let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
    let config = Config {
        write_bytes_in_flight: 1 << 20,
    };
    let service = StorageService::new(storage, Admission::Certified, config);
    let addr = serve_routes(service.routes(), Some(pki.server())).await;
    let callers = [
        ("frontend", vec![uri("frontend")]),
        ("storage", vec![uri("storage")]),
        ("daemon", vec![dns("node-1"), uri("daemon")]),
        ("admin", vec![uri("admin")]),
        ("no role", vec![dns("node-1")]),
        ("two roles", vec![uri("frontend"), uri("daemon")]),
    ];
    for (who, sans) in callers {
        let channel = pki.client_naming(addr, sans).await;
        for (rpc, endpoint) in RPCS {
            let forwarded = mem.segments.calls.lock().expect("lock").len();
            let answer = call_rpc(rpc, &channel).await;
            let refused = matches!(&answer, Err(s) if s.code() == Code::PermissionDenied);
            let allowed = expected(endpoint).contains(&who);
            assert_eq!(refused, !allowed, "{who} on {rpc}: {answer:?}");
            if refused {
                let now = mem.segments.calls.lock().expect("lock").len();
                assert_eq!(now, forwarded, "{who} on {rpc} reached the store");
            }
        }
    }
    // A client of the allowed role really reached the store.
    let channel = pki.client(addr, "daemon").await;
    call(Endpoint::Write, &channel)
        .await
        .expect("a daemon writes");
    assert!(mem.blobs.lock().expect("lock").contains_key(&sha(b"x")));
}

/// Catches the role read from a certificate of the presented chain other than the leaf
/// (the last, or any of them): a peer holding a frontend key could append a daemon's
/// public certificate after its own and write segments as a daemon. Over real mutual
/// TLS, chains of two certificates from the same CA: frontend first, daemon second, is
/// refused PERMISSION_DENIED on `Segments` (daemon only); daemon first, frontend
/// second, is refused on `ActionStore.Get` (frontend only) and gets past the check on
/// `Segments`.
///
/// Mutant seen red: `chain.first()` read as `chain.last()` in `Admission::check`.
#[tokio::test]
async fn the_role_is_read_from_the_leaf_certificate_alone() {
    let pki = Pki::new();
    let storage: Arc<dyn Storage> = Arc::new(Mem::default());
    let config = Config {
        write_bytes_in_flight: 1 << 20,
    };
    let service = StorageService::new(storage, Admission::Certified, config);
    let addr = serve_routes(service.routes(), Some(pki.server())).await;
    let refused = |answer: &Result<(), Status>| matches!(answer, Err(s) if s.code() == Code::PermissionDenied);

    let frontend_leaf = pki
        .client_presenting(addr, vec![vec![uri("frontend")], vec![uri("daemon")]])
        .await;
    let answer = call(Endpoint::Segments, &frontend_leaf).await;
    assert!(refused(&answer), "frontend leaf on Segments: {answer:?}");

    let daemon_leaf = pki
        .client_presenting(addr, vec![vec![uri("daemon")], vec![uri("frontend")]])
        .await;
    let answer = call(Endpoint::ActionGet, &daemon_leaf).await;
    assert!(
        refused(&answer),
        "daemon leaf on ActionStore.Get: {answer:?}"
    );
    let answer = call(Endpoint::Segments, &daemon_leaf).await;
    assert!(!refused(&answer), "daemon leaf on Segments: {answer:?}");
}
