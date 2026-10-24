//! A client of the frontend's loopback operator API (`docs/api.md`): one `GET` or `POST` with
//! the operator bearer token from a file, held to the rule of [`crate::secret`]. Safe-to-stop
//! ([`crate::safe_http`]) and node health ([`crate::node_health`]) are reads of it; draining a
//! node ([`crate::node_drain`]) is a read and two writes.
//!
//! A host's updater has no token and does not reach the loopback API: [`OperatorApi::as_updater`]
//! calls the frontend's worker listener over mutual TLS, presenting the updater's certificate,
//! which the frontend serves for the node in that certificate and no other, and for four calls
//! only (`docs/design/updates.md`, "Trust"). It sends no `Authorization` header and no secret:
//! the host never holds a token. The files are read on every call, so a renewed certificate is
//! used at once.
//!
//! The request has the headers of `kbf cluster`, including a JSON content type: the gated
//! routes want one on every call, a `GET` too. A `POST` carries a JSON body or none.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::agent::coordinator::{CoordinatorError, Endpoint, TlsFiles, dial, down, read_all};
use crate::identity::Identity;
use crate::ports::PortError;
use crate::reply::Reply;
use crate::secret::read_token;

/// How long a connect, a write or a read may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The most bytes of an answer that are read.
pub(crate) const REPLY_MAX: u64 = 1024 * 1024;

/// What error texts call the far side.
pub const WHAT: &str = "the operator API";

/// What an operator checks when the API refuses a call: the flag of the token file (named in
/// [`crate::secret`]) and the API's own gates.
pub const CHECK: &str = "--token-file and the API's gates";

/// The role the frontend's certificate names.
const FRONTEND_ROLE: &str = "frontend";

/// Where a call goes.
#[derive(Clone)]
enum Wire {
    /// A loopback address, in the clear.
    Loopback(SocketAddr),
    /// The frontend's worker listener, over mutual TLS with a host updater's certificate.
    Frontend(Arc<Frontend>),
}

/// The frontend and the files a host's updater presents to it.
struct Frontend {
    endpoint: Endpoint,
    files: TlsFiles,
    identity: Identity,
}

/// The operator API at one address. It has no `Debug`: nothing about it should be printed.
#[derive(Clone)]
pub struct OperatorApi {
    wire: Wire,
    /// What error texts call the far side.
    what: &'static str,
    /// The file of the operator token; none for a call that sends no credential.
    token_file: Option<PathBuf>,
    timeout: Duration,
}

impl OperatorApi {
    /// A client for the API at `addr`. The token file is read once now, so that a start-up
    /// with a bad file fails at once and not at the first roll; each call reads it again.
    pub fn new(addr: SocketAddr, token_file: PathBuf) -> Result<OperatorApi, String> {
        read_token(&token_file)?;
        Ok(OperatorApi {
            wire: Wire::Loopback(addr),
            what: WHAT,
            token_file: Some(token_file),
            timeout: TIMEOUT,
        })
    }

    /// A client for the node calls of a host's updater at the frontend at `endpoint`, for the
    /// host whose updater certificate (in `files`) names `identity`: mutual TLS on the worker
    /// listener, no token. The frontend serves the four calls about that node and refuses every
    /// other; a certificate that now names another node than `identity` is not presented.
    pub fn as_updater(endpoint: Endpoint, files: TlsFiles, identity: Identity) -> OperatorApi {
        OperatorApi {
            wire: Wire::Frontend(Arc::new(Frontend {
                endpoint,
                files,
                identity,
            })),
            what: WHAT,
            token_file: None,
            timeout: TIMEOUT,
        }
    }

    /// What this client's error texts call the API it talks to.
    pub fn what(&self) -> &'static str {
        self.what
    }

    pub fn with_timeout(mut self, timeout: Duration) -> OperatorApi {
        self.timeout = timeout;
        self
    }

    /// `GET path`. A refused or timed-out connection and a cut answer are
    /// [`PortError::unreachable`]; the status is the caller's to judge.
    pub fn get(&self, path: &str) -> Result<Reply, PortError> {
        self.call("GET", path, "")
    }

    /// `POST path` with `body` (JSON text, or empty for none). Failures as for
    /// [`OperatorApi::get`]; the status is the caller's to judge.
    pub fn post(&self, path: &str, body: &str) -> Result<Reply, PortError> {
        self.call("POST", path, body)
    }

    fn call(&self, method: &str, path: &str, body: &str) -> Result<Reply, PortError> {
        let token = match &self.token_file {
            Some(file) => Some(read_token(file).map_err(PortError::other)?),
            None => None,
        };
        let what = self.what;
        let raw = self.exchange(method, path, body, token.as_deref())?;
        Reply::parse(&raw, what)
    }

    /// The request, with the headers of `kbf cluster`.
    fn request(host: &str, method: &str, path: &str, body: &str, token: Option<&str>) -> String {
        let bearer = token
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
             Accept: application/json\r\n{bearer}\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// One request, one answer: the raw bytes, up to [`REPLY_MAX`].
    fn exchange(
        &self,
        method: &str,
        path: &str,
        body: &str,
        token: Option<&str>,
    ) -> Result<Vec<u8>, PortError> {
        let what = self.what;
        match &self.wire {
            Wire::Loopback(addr) => {
                let request = Self::request(&addr.to_string(), method, path, body, token);
                self.loopback(*addr, request.as_bytes())
                    .map_err(|e| PortError::unreachable(format!("{what}: {}", e.kind())))
            }
            Wire::Frontend(frontend) => {
                let host = format!("{}:{}", frontend.endpoint.host, frontend.endpoint.port);
                let request = Self::request(&host, method, path, body, None);
                self.through(frontend, request.as_bytes())
                    .map_err(|e| match e {
                        CoordinatorError::Files(_) => PortError::other(format!(
                            "{what}: this host's TLS files cannot be used"
                        )),
                        CoordinatorError::Untrusted(_) => PortError::other(format!(
                            "{what}: what answered is not the frontend (its certificate is not \
                         the farm's, not for its name, or not a frontend's)"
                        )),
                        _ => PortError::unreachable(format!("{what}: unreachable")),
                    })
            }
        }
    }

    fn loopback(&self, addr: SocketAddr, request: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.write_all(request)?;
        let mut raw = Vec::new();
        stream.take(REPLY_MAX).read_to_end(&mut raw)?;
        Ok(raw)
    }

    fn through(&self, frontend: &Frontend, request: &[u8]) -> Result<Vec<u8>, CoordinatorError> {
        let mut tls = dial(
            &frontend.endpoint,
            &frontend.files,
            Some(&frontend.identity),
            self.timeout,
            FRONTEND_ROLE,
        )?;
        tls.write_all(request).map_err(down)?;
        tls.flush().map_err(down)?;
        read_all(
            &mut tls,
            self.timeout,
            usize::try_from(REPLY_MAX).unwrap_or(usize::MAX),
        )
    }
}
