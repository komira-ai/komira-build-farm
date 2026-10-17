//! A small HTTP/1.1 client for a registry: one connection per request over TCP or
//! rustls, redirects followed without credentials, every wait bounded.
//!
//! Written over hyper's connection API rather than a client crate: the daemon links
//! this, and a daemon must not link a standalone HTTP client (the layering test in
//! `kbf-it`); hyper is already in its closure through tonic.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Incoming;
use hyper::header::{AUTHORIZATION, CONNECTION, HOST, HeaderName, LOCATION, USER_AGENT};
use hyper::http::uri::Authority;
use hyper::{Request, Response, Uri};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::Error;

/// How long each kind of wait may last.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Connecting, including the TLS handshake.
    pub connect: Duration,
    /// From the request to the response's headers.
    pub head: Duration,
    /// Between two chunks of a body: a stall, not a total.
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(30),
            head: Duration::from_secs(60),
            idle: Duration::from_secs(60),
        }
    }
}

/// Redirects followed for one request.
const MAX_REDIRECTS: usize = 5;

/// The roots a client trusts.
#[derive(Clone, Debug)]
pub(crate) enum Roots {
    /// The host's system store.
    System,
    /// Exactly these.
    Given(Vec<CertificateDer<'static>>),
}

pub(crate) struct Http {
    roots: Roots,
    /// The `host:port` authorities that may be spoken to over plain HTTP.
    plain: Vec<String>,
    pub(crate) timeouts: Timeouts,
    tls: OnceLock<Result<TlsConnector, String>>,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("plain", &self.plain)
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

impl Http {
    pub(crate) fn new(roots: Roots) -> Self {
        Self {
            roots,
            plain: Vec::new(),
            timeouts: Timeouts::default(),
            tls: OnceLock::new(),
        }
    }

    pub(crate) fn allow_plain(&mut self, authority: String) {
        self.plain.push(authority);
    }

    /// Whether `authority` (`host:port`) is spoken to over plain HTTP.
    pub(crate) fn is_plain(&self, authority: &str) -> bool {
        self.plain.iter().any(|a| a == authority)
    }

    /// The TLS client, made on first use: loading the system store is not free, and a
    /// node that fetches nothing never does.
    fn connector(&self) -> Result<&TlsConnector, Error> {
        let made = self.tls.get_or_init(|| {
            let certs = match &self.roots {
                Roots::System => rustls_native_certs::load_native_certs().certs,
                Roots::Given(certs) => certs.clone(),
            };
            let mut store = rustls::RootCertStore::empty();
            let (added, _ignored) = store.add_parsable_certificates(certs);
            if added == 0 {
                return Err("no trusted root certificates".to_owned());
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("the ring provider supports the default TLS versions")
                .with_root_certificates(store)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Ok(TlsConnector::from(Arc::new(config)))
        });
        made.as_ref().map_err(|why| Error::Transport {
            what: "TLS".to_owned(),
            why: why.clone(),
        })
    }

    /// GETs `url` with `headers`, following redirects. The headers go to `url` only: a
    /// redirect target (a blob's CDN) gets the request without `Authorization`.
    pub(crate) async fn get(
        &self,
        url: &str,
        headers: &[(HeaderName, String)],
    ) -> Result<Response<Incoming>, Error> {
        let mut url = url.to_owned();
        let mut headers = headers.to_vec();
        for _ in 0..=MAX_REDIRECTS {
            let response = self.get_once(&url, &headers).await?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            let to = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::Transport {
                    what: url.clone(),
                    why: "a redirect with no Location".to_owned(),
                })?;
            url = resolve(&url, to)?;
            headers.retain(|(name, _)| name != AUTHORIZATION);
        }
        Err(Error::Transport {
            what: url,
            why: format!("more than {MAX_REDIRECTS} redirects"),
        })
    }

    async fn get_once(
        &self,
        url: &str,
        headers: &[(HeaderName, String)],
    ) -> Result<Response<Incoming>, Error> {
        let transport = |why: String| Error::Transport {
            what: url.to_owned(),
            why,
        };
        let uri: Uri = url.parse().map_err(|e| transport(format!("{e}")))?;
        // A URL built here or named by a redirect has a scheme and a host; one that has
        // none connects to nothing and fails there.
        let authority = uri.authority().map_or("", Authority::as_str);
        let host = uri.host().unwrap_or_default();
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") if self.is_plain(authority) => false,
            _ => return Err(Error::Insecure(url.to_owned())),
        };
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
        // Before connecting: a name that cannot be a server's is no reason to dial.
        let name = ServerName::try_from(host.to_owned()).map_err(|e| transport(e.to_string()))?;
        let connect = async {
            let tcp = TcpStream::connect((host, port))
                .await
                .map_err(|e| format!("connect: {e}"))?;
            if tls {
                let stream = self
                    .connector()
                    .map_err(|e| e.to_string())?
                    .connect(name, tcp)
                    .await
                    .map_err(|e| format!("TLS: {e}"))?;
                Ok(send(TokioIo::new(stream)).await)
            } else {
                Ok(send(TokioIo::new(tcp)).await)
            }
        };
        let mut sender = tokio::time::timeout(self.timeouts.connect, connect)
            .await
            .map_err(|_| transport("connecting timed out".to_owned()))?
            .map_err(transport)?;
        let mut request = Request::get(uri.path_and_query().map_or("/", |p| p.as_str()))
            .header(HOST, authority)
            .header(USER_AGENT, "kbf-image")
            .header(CONNECTION, "close");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let request = request
            .body(Empty::<Bytes>::new())
            .map_err(|e| transport(e.to_string()))?;
        tokio::time::timeout(self.timeouts.head, sender.send_request(request))
            .await
            .map_err(|_| transport("no response headers in time".to_owned()))?
            .map_err(|e| transport(e.to_string()))
    }

    /// The next chunk of `body`, or `None` at its end.
    pub(crate) async fn chunk(
        &self,
        what: &str,
        body: &mut Incoming,
    ) -> Result<Option<Bytes>, Error> {
        let frame = tokio::time::timeout(self.timeouts.idle, body.frame())
            .await
            .map_err(|_| Error::Transport {
                what: what.to_owned(),
                why: "the body stalled".to_owned(),
            })?;
        match frame {
            None => Ok(None),
            Some(Err(e)) => Err(Error::Transport {
                what: what.to_owned(),
                why: e.to_string(),
            }),
            // Trailers come after all the data: a trailers frame is the body's end.
            Some(Ok(frame)) => Ok(frame.into_data().ok()),
        }
    }

    /// The whole body of `response`, at most `limit` bytes.
    pub(crate) async fn read_limited(
        &self,
        what: &str,
        response: Response<Incoming>,
        limit: u64,
    ) -> Result<Vec<u8>, Error> {
        let mut body = response.into_body();
        let mut out = Vec::new();
        while let Some(data) = self.chunk(what, &mut body).await? {
            if out.len() as u64 + data.len() as u64 > limit {
                return Err(Error::TooLarge {
                    what: what.to_owned(),
                    limit,
                });
            }
            out.extend_from_slice(&data);
        }
        Ok(out)
    }
}

/// Runs the HTTP/1 handshake over `io`, the connection's driver on a task of its own.
async fn send<I>(io: TokioIo<I>) -> hyper::client::conn::http1::SendRequest<Empty<Bytes>>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // The handshake does no I/O and has no failure to report; what a broken connection
    // says comes out of the request, and of the body.
    let (sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("hyper's HTTP/1 handshake does no I/O");
    tokio::spawn(async move {
        // It ends with the response; an error then is the server closing early, which
        // the body read reports.
        let _ = connection.await;
    });
    sender
}

/// The URL `location` names when it is a redirect from `from`: absolute, or a path on
/// `from`'s own scheme and authority.
fn resolve(from: &str, location: &str) -> Result<String, Error> {
    if location.contains("://") {
        return Ok(location.to_owned());
    }
    let uri: Uri = from.parse().map_err(|e| Error::Transport {
        what: from.to_owned(),
        why: format!("{e}"),
    })?;
    let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
        return Err(Error::Transport {
            what: from.to_owned(),
            why: "not an absolute URL".to_owned(),
        });
    };
    let path = if location.starts_with('/') {
        location.to_owned()
    } else {
        // Relative to the directory of the request's path.
        let base = uri.path();
        let dir = &base[..=base.rfind('/').unwrap_or(0)];
        format!("{dir}{location}")
    };
    Ok(format!("{scheme}://{authority}{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a redirect target built wrongly: absolute, rooted and relative forms.
    #[test]
    fn locations_resolve_against_the_request() {
        let from = "https://r.example:5000/v2/a/blobs/sha256:x";
        assert_eq!(
            resolve(from, "https://cdn.example/blob?sig=1").unwrap(),
            "https://cdn.example/blob?sig=1"
        );
        assert_eq!(
            resolve(from, "/other?x=1").unwrap(),
            "https://r.example:5000/other?x=1"
        );
        assert_eq!(
            resolve(from, "sibling").unwrap(),
            "https://r.example:5000/v2/a/blobs/sibling"
        );
        assert!(resolve("not a url", "/x").is_err());
        assert!(resolve("/relative", "/x").is_err());
    }
}
