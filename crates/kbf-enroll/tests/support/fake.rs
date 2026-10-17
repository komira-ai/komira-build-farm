//! An in-process frontend built from `kbf.enroll.v1`: the `Enroll` service on a listener
//! with server authentication only, and the `ColdStore` service on one that demands a
//! client certificate. It keeps what design section 5.6 keeps (token uses, one use per
//! request, the registry of keys) and has knobs to make it misbehave in the ways the
//! node must survive.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use kbf_peer::{PeerRole, role_of};
use kbf_proto::kbf::enroll::v1::cold_store_server::{ColdStore, ColdStoreServer};
use kbf_proto::kbf::enroll::v1::enroll_server::{Enroll, EnrollServer};
use kbf_proto::kbf::enroll::v1::{
    EnrollRequest, EnrollResponse, GetKeysRequest, GetKeysResponse, IssuedCert, NodeConfig,
    RenewRequest, RenewResponse,
};
use kbf_types::{RefusalReason, Role, Token, TokenId, TokenSecret};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};
use x509_parser::prelude::{FromDer, X509CertificationRequest};

use super::pki::{FrontendCert, Issue, Pki, now};

/// A token the fake accepts.
struct TokenRow {
    secret: [u8; 32],
    max_uses: u32,
    uses: u32,
    expired: bool,
}

/// A node the fake registered: the key it holds for each role.
struct NodeRow {
    keys: HashMap<String, Vec<u8>>,
    revoked: bool,
}

/// How the fake spoils an answer, so the node's checks can be seen to hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spoil {
    /// `server_time` this many seconds off.
    Skew(i64),
    /// A certificate dropped.
    DropCert,
    /// The first certificate twice.
    DuplicateCert,
    /// A certificate for a role nobody asked for.
    ExtraCert,
    /// The role name of the first certificate changed.
    RenameRole,
    /// The CA bundle replaced by text that is not PEM.
    BundleNotPem,
    /// The CA bundle replaced by a PEM block whose text is not base64.
    BundleBadBase64,
    /// The CA bundle replaced by a PEM block that is not a certificate.
    BundleNotCert,
    /// The CA bundle holds a CA other than the pinned one.
    BundleWithoutPin,
    /// Certificates for a key the node did not make.
    OtherKey,
    /// A node name other than the node's.
    OtherName,
    /// The role URI of another role in the certificate.
    OtherRoleUri,
    /// The reported serial changed.
    BadSerial,
    /// The reported start of validity changed.
    BadNotBefore,
    /// The reported expiry changed.
    BadNotAfter,
    /// Signed by a CA the bundle does not hold.
    UntrustedSigner,
    /// Valid only in the past.
    Expired,
    /// Not valid yet.
    NotYetValid,
    /// `renew_after` at the expiry.
    LateRenewAfter,
    /// Leaf bytes that are not a certificate.
    GarbageLeaf,
    /// Trailing bytes after the certificate.
    TrailingBytes,
}

struct State {
    tokens: HashMap<String, TokenRow>,
    nodes: HashMap<String, NodeRow>,
    answers: HashMap<Vec<u8>, EnrollResponse>,
    enrolls: Vec<EnrollRequest>,
    renews: Vec<RenewRequest>,
    refuse: Option<RefusalReason>,
    lose_next: bool,
    spoil: Option<Spoil>,
    generation: u64,
    keys_calls: u32,
    keys_revoked: bool,
}

/// The fake frontend.
pub struct Fake {
    pub pki: Arc<Pki>,
    other: Pki,
    next: Pki,
    clock: AtomicI64,
    life_s: i64,
    state: Mutex<State>,
}

/// A running fake: the listeners' addresses and the shutdown that ends them.
pub struct Running {
    pub fake: Arc<Fake>,
    pub enroll_addr: SocketAddr,
    pub worker_addr: SocketAddr,
    shutdown: Vec<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        for tx in self.shutdown.drain(..) {
            let _ = tx.send(());
        }
    }
}

pub const LIFE_S: i64 = 7 * 86_400;

impl Fake {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pki: Arc::new(Pki::new()),
            other: Pki::named("another CA"),
            next: Pki::named("kbf test CA next"),
            clock: AtomicI64::new(now()),
            life_s: LIFE_S,
            state: Mutex::new(State {
                tokens: HashMap::new(),
                nodes: HashMap::new(),
                answers: HashMap::new(),
                enrolls: Vec::new(),
                renews: Vec::new(),
                refuse: None,
                lose_next: false,
                spoil: None,
                generation: 1,
                keys_calls: 0,
                keys_revoked: false,
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("fake state")
    }

    /// The frontend's (and, in these tests, the node's) unix time.
    pub fn now(&self) -> i64 {
        self.clock.load(Ordering::Relaxed)
    }

    pub fn set_now(&self, secs: i64) {
        self.clock.store(secs, Ordering::Relaxed);
    }

    /// A token with `uses` uses; returns it as the node reads it.
    pub fn mint(&self, id_bits: u64, uses: u32) -> Token {
        let secret = [id_bits as u8; 32];
        let id = TokenId::from_bits(id_bits);
        self.state().tokens.insert(
            id.to_string(),
            TokenRow {
                secret,
                max_uses: uses,
                uses: 0,
                expired: false,
            },
        );
        Token::new(id, TokenSecret::from_bytes(secret), self.pki.pin())
    }

    pub fn expire(&self, token: &Token) {
        self.state()
            .tokens
            .get_mut(&token.id().to_string())
            .expect("token")
            .expired = true;
    }

    pub fn uses(&self, token: &Token) -> u32 {
        self.state().tokens[&token.id().to_string()].uses
    }

    pub fn refuse_with(&self, reason: Option<RefusalReason>) {
        self.state().refuse = reason;
    }

    pub fn lose_next_answer(&self) {
        self.state().lose_next = true;
    }

    pub fn spoil(&self, how: Option<Spoil>) {
        self.state().spoil = how;
    }

    pub fn set_generation(&self, generation: u64) {
        self.state().generation = generation;
    }

    pub fn revoke_keys(&self) {
        self.state().keys_revoked = true;
    }

    pub fn enrolls(&self) -> Vec<EnrollRequest> {
        self.state().enrolls.clone()
    }

    pub fn renews(&self) -> Vec<RenewRequest> {
        self.state().renews.clone()
    }

    pub fn keys_calls(&self) -> u32 {
        self.state().keys_calls
    }

    /// The root of the CA the next rotation would move to, and its cross-signature by the current one.
    pub fn rotation(&self) -> (Vec<u8>, Vec<u8>) {
        (self.next.root_der(), self.pki.cross_sign(&self.next))
    }

    pub fn next_pki(&self) -> &Pki {
        &self.next
    }

    fn config(&self, roles: &[String], generation: u64) -> NodeConfig {
        NodeConfig {
            generation,
            roles: roles.to_vec(),
            worker_endpoint: "https://localhost:1".to_owned(),
            storage_port: 8983,
            credential_mode: 1,
            tls_reload_interval_s: 30,
            ..NodeConfig::default()
        }
    }

    fn issue_all(
        &self,
        node_id: &str,
        advertise: &str,
        csrs: &[(String, Vec<u8>)],
        spoil: Option<&Spoil>,
    ) -> Result<Vec<IssuedCert>, Status> {
        let advertised = advertise.rsplit_once(':').map(|(host, _)| host);
        let now = self.now();
        let mut out = Vec::new();
        for (role, csr) in csrs {
            let role: Role = role.parse().map_err(|_| Status::invalid_argument("role"))?;
            let (mut not_before, mut not_after) = (now - 300, now - 300 + self.life_s);
            match spoil {
                Some(Spoil::Expired) => (not_before, not_after) = (now - 2 * 86_400, now - 86_400),
                Some(Spoil::NotYetValid) => {
                    (not_before, not_after) = (now + 86_400, now + 3 * 86_400)
                }
                _ => {}
            }
            let other_key;
            let csr_der = if spoil == Some(&Spoil::OtherKey) {
                let key = kbf_enroll_test_key();
                other_key = key;
                &other_key
            } else {
                csr
            };
            let how = Issue {
                csr_der,
                role,
                node_name: if spoil == Some(&Spoil::OtherName) {
                    "someone-else"
                } else {
                    node_id
                },
                extra_dns: advertised.filter(|_| role == Role::Storage),
                role_uri: (spoil == Some(&Spoil::OtherRoleUri))
                    .then(|| "urn:kbf:role:frontend".to_owned()),
                not_before,
                not_after,
            };
            let pki = if spoil == Some(&Spoil::UntrustedSigner) {
                &self.other
            } else {
                &self.pki
            };
            out.push(pki.issue(&how).map_err(Status::invalid_argument)?);
        }
        Ok(out)
    }

    fn answer_parts(
        &self,
        mut certs: Vec<IssuedCert>,
        spoil: Option<&Spoil>,
    ) -> (Vec<IssuedCert>, Vec<u8>, i64, i64) {
        let now = self.now();
        let mut bundle = self.pki.root_pem().into_bytes();
        let mut server_time = now;
        let mut renew_after = now + self.life_s / 2;
        match spoil {
            Some(Spoil::Skew(s)) => server_time += s,
            Some(Spoil::DropCert) => {
                certs.pop();
            }
            Some(Spoil::DuplicateCert) => certs.push(certs[0].clone()),
            Some(Spoil::ExtraCert) => {
                let mut extra = certs[0].clone();
                extra.role = if extra.role == "daemon" {
                    "storage"
                } else {
                    "daemon"
                }
                .to_owned();
                certs.push(extra);
            }
            Some(Spoil::RenameRole) => {
                certs[0].role = if certs[0].role == "daemon" {
                    "storage"
                } else {
                    "daemon"
                }
                .to_owned();
            }
            Some(Spoil::BundleNotPem) => bundle = b"this is not pem".to_vec(),
            Some(Spoil::BundleBadBase64) => {
                bundle = b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n".to_vec();
            }
            Some(Spoil::BundleNotCert) => {
                bundle = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".to_vec();
            }
            Some(Spoil::BundleWithoutPin) => bundle = self.other.root_pem().into_bytes(),
            Some(Spoil::BadSerial) => certs[0].serial = "ffff".to_owned(),
            Some(Spoil::BadNotBefore) => certs[0].not_before -= 1,
            Some(Spoil::BadNotAfter) => certs[0].not_after += 1,
            Some(Spoil::LateRenewAfter) => renew_after = certs[0].not_after,
            Some(Spoil::GarbageLeaf) => certs[0].leaf_der = vec![1, 2, 3],
            Some(Spoil::TrailingBytes) => certs[0].leaf_der.push(0),
            _ => {}
        }
        (certs, bundle, server_time, renew_after)
    }
}

/// The service handle tonic is given: the fake behind a local type, since the generated
/// traits are foreign.
pub struct Svc(pub Arc<Fake>);

impl std::ops::Deref for Svc {
    type Target = Fake;
    fn deref(&self) -> &Fake {
        &self.0
    }
}

/// A request for a key the node does not hold.
fn kbf_enroll_test_key() -> Vec<u8> {
    let key = rcgen::KeyPair::generate().expect("key");
    rcgen::CertificateParams::default()
        .serialize_request(&key)
        .expect("csr")
        .der()
        .as_ref()
        .to_vec()
}

fn status_of(reason: RefusalReason) -> Status {
    Status::new(
        tonic::Code::from_i32(reason.code().number()),
        reason.message(),
    )
}

fn spki_of_csr(csr: &[u8]) -> Result<Vec<u8>, Status> {
    let (_, parsed) =
        X509CertificationRequest::from_der(csr).map_err(|_| status_of(RefusalReason::BadCsr))?;
    parsed
        .verify_signature()
        .map_err(|_| status_of(RefusalReason::BadCsr))?;
    Ok(parsed.certification_request_info.subject_pki.raw.to_vec())
}

#[tonic::async_trait]
impl Enroll for Svc {
    async fn enroll(
        &self,
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let request = request.into_inner();
        let mut state = self.state();
        state.enrolls.push(request.clone());
        if let Some(reason) = state.refuse {
            return Err(status_of(reason));
        }
        let key: Vec<u8> = request
            .csrs
            .iter()
            .flat_map(|c| c.csr_der.clone())
            .collect();
        let spoil = state.spoil.clone();
        let csrs: Vec<(String, Vec<u8>)> = request
            .csrs
            .iter()
            .map(|c| (c.role.clone(), c.csr_der.clone()))
            .collect();
        let row = state
            .tokens
            .get_mut(&request.token_id)
            .filter(|t| t.secret.as_slice() == request.token_secret.as_slice())
            .ok_or_else(|| status_of(RefusalReason::TokenRefused))?;
        if row.expired {
            return Err(status_of(RefusalReason::TokenExpired));
        }
        let replay = state.answers.get(&key).cloned();
        let response = match replay {
            Some(response) => response,
            None => {
                let row = state.tokens.get_mut(&request.token_id).expect("token");
                if row.uses >= row.max_uses {
                    return Err(status_of(RefusalReason::TokenUsedUp));
                }
                row.uses += 1;
                let certs = self.issue_all(&request.node_id, &request.advertise, &csrs, None)?;
                let mut keys = HashMap::new();
                for (role, csr) in &csrs {
                    keys.insert(role.clone(), spki_of_csr(csr)?);
                }
                state.nodes.insert(
                    request.node_id.clone(),
                    NodeRow {
                        keys,
                        revoked: false,
                    },
                );
                let roles: Vec<String> = csrs.iter().map(|(r, _)| r.clone()).collect();
                let response = EnrollResponse {
                    certs,
                    ca_bundle_pem: self.pki.root_pem().into_bytes(),
                    config: Some(self.config(&roles, state.generation)),
                    server_time: self.now(),
                    renew_after: self.now() + self.life_s / 2,
                };
                state.answers.insert(key, response.clone());
                response
            }
        };
        let mut response = response;
        if let Some(spoil) = &spoil {
            let certs = self.issue_all(&request.node_id, &request.advertise, &csrs, Some(spoil))?;
            let (certs, bundle, server_time, renew_after) = self.answer_parts(certs, Some(spoil));
            response.certs = certs;
            response.ca_bundle_pem = bundle;
            response.server_time = server_time;
            response.renew_after = renew_after;
        }
        if std::mem::take(&mut state.lose_next) {
            return Err(Status::unavailable("answer lost"));
        }
        Ok(Response::new(response))
    }

    async fn renew(
        &self,
        request: Request<RenewRequest>,
    ) -> Result<Response<RenewResponse>, Status> {
        let request = request.into_inner();
        let mut state = self.state();
        state.renews.push(request.clone());
        if let Some(reason) = state.refuse {
            return Err(status_of(reason));
        }
        let spoil = state.spoil.clone();
        let generation = state.generation;
        let row = state
            .nodes
            .get(&request.node_id)
            .ok_or_else(|| status_of(RefusalReason::UnknownNode))?;
        if row.revoked {
            return Err(status_of(RefusalReason::NodeRevoked));
        }
        if (request.issued_at - self.now()).abs() > 300 {
            return Err(status_of(RefusalReason::StaleIssuedAt));
        }
        let mut csrs = Vec::new();
        for csr in &request.csrs {
            // Key continuity: the request must be signed by the key the registry holds,
            // and must carry the time it was made inside the signed part.
            let spki = spki_of_csr(&csr.csr_der)?;
            if row.keys.get(&csr.role) != Some(&spki) {
                return Err(status_of(RefusalReason::WrongKey));
            }
            let mut stamp = vec![0x31, 0x0a, 0x04, 0x08];
            stamp.extend_from_slice(&request.issued_at.to_be_bytes());
            if !csr
                .csr_der
                .windows(stamp.len())
                .any(|w| w == stamp.as_slice())
            {
                return Err(status_of(RefusalReason::MalformedRequest));
            }
            csrs.push((csr.role.clone(), csr.csr_der.clone()));
        }
        let certs = self.issue_all(&request.node_id, "", &csrs, spoil.as_ref())?;
        let (certs, bundle, server_time, renew_after) = self.answer_parts(certs, spoil.as_ref());
        let roles: Vec<String> = csrs.iter().map(|(r, _)| r.clone()).collect();
        Ok(Response::new(RenewResponse {
            certs,
            ca_bundle_pem: bundle,
            config: (request.config_generation != generation)
                .then(|| self.config(&roles, generation)),
            server_time,
            renew_after,
        }))
    }
}

#[tonic::async_trait]
impl ColdStore for Svc {
    async fn get_keys(
        &self,
        request: Request<GetKeysRequest>,
    ) -> Result<Response<GetKeysResponse>, Status> {
        let certs = request
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("no client certificate"))?;
        let role =
            role_of(certs[0].as_ref()).map_err(|e| Status::permission_denied(e.to_string()))?;
        let mut state = self.state();
        state.keys_calls += 1;
        if role != PeerRole::Storage || state.keys_revoked {
            return Err(Status::permission_denied("not a storage node, or revoked"));
        }
        Ok(Response::new(GetKeysResponse {
            access_key_id: "AKIATESTKEY0000".to_owned(),
            secret_access_key: "secret-that-must-stay-in-memory".to_owned(),
        }))
    }
}

fn incoming() -> (TcpIncoming, SocketAddr) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = tokio::net::TcpListener::from_std(std_listener).expect("listener");
    (TcpIncoming::from(listener), addr)
}

/// Starts both listeners. The enroll listener presents `frontend_cert`; the worker
/// listener presents the same kind of certificate and asks for a client certificate
/// signed by the CA.
pub fn start_with(fake: Arc<Fake>, frontend_cert: &FrontendCert) -> Running {
    let identity = fake.pki.frontend(frontend_cert);
    let (enroll_in, enroll_addr) = incoming();
    let (worker_in, worker_addr) = incoming();
    let mut shutdown = Vec::new();

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    shutdown.push(tx);
    let tls =
        ServerTlsConfig::new().identity(Identity::from_pem(&identity.chain_pem, &identity.key_pem));
    let svc = EnrollServer::new(Svc(fake.clone()));
    tokio::spawn(async move {
        let _ = Server::builder()
            .tls_config(tls)
            .expect("tls")
            .add_service(svc)
            .serve_with_incoming_shutdown(enroll_in, async {
                let _ = rx.await;
            })
            .await;
    });

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    shutdown.push(tx);
    let worker_identity = fake.pki.frontend(&FrontendCert::good());
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(
            &worker_identity.chain_pem,
            &worker_identity.key_pem,
        ))
        .client_ca_root(Certificate::from_pem(fake.pki.root_pem()));
    let svc = ColdStoreServer::new(Svc(fake.clone()));
    tokio::spawn(async move {
        let _ = Server::builder()
            .tls_config(tls)
            .expect("tls")
            .add_service(svc)
            .serve_with_incoming_shutdown(worker_in, async {
                let _ = rx.await;
            })
            .await;
    });

    Running {
        fake,
        enroll_addr,
        worker_addr,
        shutdown,
    }
}

pub fn start() -> Running {
    start_with(Fake::new(), &FrontendCert::good())
}
