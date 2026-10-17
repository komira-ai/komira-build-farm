//! `Enroll` over `kbf.enroll.v1`, from the node (design sections 5.3 to 5.6).
//!
//! [`Frontend`] is a channel to the enroll listener whose TLS is checked by an
//! [`Anchor`](crate::pin::Anchor): the token's pin, or (the declared fallback) the system
//! roots. [`enroll`] makes one key per role, sends the token secret and a request per
//! role over that channel, checks the answer, and installs it atomically. The key never
//! leaves the node. A refusal is a typed [`RefusalReason`], so a caller can tell an
//! expired token from a used-up one.
//!
//! One attempt per call. The keys and requests of an attempt are persisted before it is
//! sent, so calling again after a lost answer sends the same bytes and the frontend, which
//! counts one use per request, answers with what it already issued; the caller decides how
//! long to wait between attempts ([`crate::renew::Backoff`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use kbf_proto::kbf::enroll::v1::enroll_client::EnrollClient;
use kbf_proto::kbf::enroll::v1::{
    EnrollRequest, EnrollResponse, NodeFacts, RenewRequest, RenewResponse, RoleCsr,
};
use kbf_types::{RefusalReason, RoleSet, Token};
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tonic::Code;
use tonic::codegen::http::Uri;
use tonic::transport::{Channel, Endpoint};
use tower_service::Service;

use crate::answer::{self, AnswerError, Expect};
use crate::config::{Config, FrontendUrl, HostName};
use crate::pin::{Anchor, AnchorError, client_config};
use crate::store::{Node, Store, StoreError, StoredConfig};
use crate::token;

/// How long a connection (TCP and the TLS handshake) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Why the frontend could not be reached.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The TLS configuration could not be built.
    #[error(transparent)]
    Anchor(#[from] AnchorError),
    /// The server's certificate was refused: not the pinned CA's, not for the name,
    /// expired, or not the frontend's. The node must not retry as if the network were
    /// down; the operator must look.
    #[error("the frontend's certificate was refused: {0}")]
    Untrusted(String),
    /// The frontend did not answer.
    #[error("cannot reach the frontend: {0}")]
    Unreachable(String),
}

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The frontend refused for a reason in the closed set.
    #[error("refused: {}", .0.message())]
    Refused(RefusalReason),
    /// Any other status (a transport failure, an unknown refusal).
    #[error("the frontend answered {0}")]
    Status(tonic::Status),
}

impl CallError {
    /// Whether the same request can succeed later with no change.
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Refused(reason) => reason.code().retryable(),
            Self::Status(status) => matches!(
                status.code(),
                Code::Unavailable | Code::ResourceExhausted | Code::DeadlineExceeded
            ),
        }
    }
}

impl From<tonic::Status> for CallError {
    fn from(status: tonic::Status) -> Self {
        RefusalReason::ALL
            .into_iter()
            .find(|r| r.code().number() == status.code() as i32 && r.message() == status.message())
            .map_or(Self::Status(status), Self::Refused)
    }
}

/// Dials the frontend's `host:port` and runs the TLS handshake with the node's config.
#[derive(Clone)]
struct Dialer {
    tls: TlsConnector,
    name: ServerName<'static>,
    addr: String,
    /// The last certificate refusal, for [`connect`] to tell from a network failure.
    refusal: Arc<Mutex<Option<String>>>,
}

impl Service<Uri> for Dialer {
    type Response = TokioIo<TlsStream<TcpStream>>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let tcp = TcpStream::connect(&this.addr).await?;
            let tls = this.tls.connect(this.name.clone(), tcp).await;
            tls.map(TokioIo::new).inspect_err(|e| {
                let cert_error = e
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                    .filter(|e| matches!(e, rustls::Error::InvalidCertificate(_)));
                if let Some(error) = cert_error {
                    *this
                        .refusal
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(error.to_string());
                }
            })
        })
    }
}

/// A connection to the frontend's enroll listener.
#[derive(Clone, Debug)]
pub struct Frontend {
    channel: Channel,
}

/// The name TLS checks the server's certificate for.
fn server_name(host: &HostName) -> Option<ServerName<'static>> {
    match host {
        HostName::Ip(ip) => Some(ServerName::IpAddress((*ip).into())),
        HostName::Dns(name) => ServerName::try_from(name.clone()).ok(),
    }
}

/// Connects to `url` and verifies the server by `anchor`.
///
/// # Errors
/// [`ConnectError`]; a certificate that does not match the pin is
/// [`ConnectError::Untrusted`], not a network failure.
pub async fn connect(url: &FrontendUrl, anchor: Anchor) -> Result<Frontend, ConnectError> {
    let tls = TlsConnector::from(Arc::new(client_config(anchor)?));
    let refusal = Arc::new(Mutex::new(None));
    let Some(name) = server_name(&url.host) else {
        return Err(ConnectError::Unreachable(format!(
            "{} is not a valid server name",
            url.host
        )));
    };
    let dialer = Dialer {
        tls,
        name,
        addr: format!("{}:{}", bare(&url.host), url.port),
        refusal: refusal.clone(),
    };
    // The channel's own scheme is `http` because tonic refuses `https` without its own TLS
    // configuration; the dialer above wraps every connection in TLS before tonic sees it.
    let endpoint = Endpoint::from_shared(format!("http://{}:{}", bare(&url.host), url.port))
        .map_err(|e| ConnectError::Unreachable(e.to_string()))?
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(CALL_TIMEOUT);
    match endpoint.connect_with_connector(dialer).await {
        Ok(channel) => Ok(Frontend { channel }),
        Err(error) => {
            let refused = refusal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            Err(refused.map_or_else(
                || ConnectError::Unreachable(format!("{error:?}")),
                ConnectError::Untrusted,
            ))
        }
    }
}

/// The host without the brackets of an IPv6 literal.
fn bare(host: &HostName) -> String {
    match host {
        HostName::Ip(ip) => ip.to_string(),
        HostName::Dns(name) => name.clone(),
    }
}

impl Frontend {
    /// `Enroll`.
    ///
    /// # Errors
    /// [`CallError`].
    pub async fn enroll(&self, request: EnrollRequest) -> Result<EnrollResponse, CallError> {
        Ok(EnrollClient::new(self.channel.clone())
            .enroll(request)
            .await?
            .into_inner())
    }

    /// `Renew`.
    ///
    /// # Errors
    /// [`CallError`].
    pub async fn renew(&self, request: RenewRequest) -> Result<RenewResponse, CallError> {
        Ok(EnrollClient::new(self.channel.clone())
            .renew(request)
            .await?
            .into_inner())
    }
}

/// Why an enrollment attempt failed.
#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    /// The frontend refused or could not be called.
    #[error(transparent)]
    Call(#[from] CallError),
    /// The frontend's answer was refused; nothing was installed and the token is not
    /// marked used locally.
    #[error("the answer was refused: {0}")]
    Answer(#[from] AnswerError),
    /// A file could not be written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What an enrollment attempt needs besides the connection and the store.
#[derive(Debug)]
pub struct Enrollment<'a> {
    /// The validated `kbf.toml`.
    pub config: &'a Config,
    /// The token read from its file.
    pub token: &'a Token,
    /// The roles to request, one key each. The frontend refuses a role the token lacks.
    pub roles: RoleSet,
    /// What to tell the frontend about this node.
    pub facts: NodeFacts,
    /// This node's unix time.
    pub now: i64,
}

/// Makes the keys, sends the token and the requests, checks the answer and installs it;
/// then removes the token file. Returns at once with the node as it is when it is
/// already enrolled (including an install a crash cut short, which it finishes).
///
/// # Errors
/// [`EnrollError`]. On [`EnrollError::Call`] and [`EnrollError::Answer`] nothing was
/// installed and the token file is untouched.
pub async fn enroll(
    frontend: &Frontend,
    store: &Store,
    attempt: &Enrollment<'_>,
) -> Result<Node, EnrollError> {
    if let Some(node) = store.recover()? {
        return Ok(node);
    }
    let pending = store.pending(attempt.roles)?;
    let config = attempt.config;
    let request = EnrollRequest {
        token_id: attempt.token.id().to_string(),
        token_secret: attempt.token.secret().as_bytes().to_vec(),
        node_id: config.node_id.to_string(),
        advertise: config
            .advertise
            .as_ref()
            .map(|a| format!("{}:{}", a.host, a.port))
            .unwrap_or_default(),
        zone: config.zone.clone(),
        csrs: pending
            .iter()
            .map(|p| RoleCsr {
                role: p.role.name().to_owned(),
                csr_der: p.csr_der.clone(),
            })
            .collect(),
        facts: Some(attempt.facts.clone()),
    };
    let response = frontend.enroll(request).await?;
    let keys: Vec<_> = pending.iter().map(|p| (p.role, p.spki.clone())).collect();
    let certs = answer::check(
        &Expect {
            pin: attempt.token.pin(),
            node_id: &config.node_id,
            keys: &keys,
            now: attempt.now,
        },
        &response.certs,
        &response.ca_bundle_pem,
        response.server_time,
        response.renew_after,
    )?;
    let node = Node::new(
        &config.node_id,
        attempt.token.pin(),
        &certs,
        response.renew_after,
        response
            .config
            .as_ref()
            .map(StoredConfig::from_proto)
            .unwrap_or_default(),
    );
    store.stage(&certs, &response.ca_bundle_pem, &node)?;
    store.commit()?;
    if let Err(error) = token::remove(&config.token_file) {
        tracing::warn!(%error, "enrolled, but the token file could not be removed");
    }
    Ok(node)
}
