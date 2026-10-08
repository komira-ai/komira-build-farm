//! Test PKI and an HTTPS client for the gate's listener.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::client::ResolvesClientCert;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::sign::CertifiedKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// PEM certificate and key.
#[derive(Clone)]
pub struct Identity {
    pub cert: String,
    pub key: String,
}

impl Identity {
    pub fn chain(&self) -> Vec<CertificateDer<'static>> {
        CertificateDer::pem_slice_iter(self.cert.as_bytes())
            .map(Result::unwrap)
            .collect()
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::from_pem_slice(self.key.as_bytes()).unwrap()
    }

    /// The SPKI pin of this identity's certificate.
    pub fn pin(&self) -> [u8; 32] {
        crate::tls::spki_sha256(&self.chain()[0]).unwrap()
    }

    /// Writes `<name>.pem` and `<name>.key` under `dir`.
    pub fn write(&self, dir: &Path, name: &str) {
        std::fs::write(dir.join(format!("{name}.pem")), &self.cert).unwrap();
        std::fs::write(dir.join(format!("{name}.key")), &self.key).unwrap();
    }
}

/// A CA, the gate's certificate for `localhost` under it, and two self-signed client
/// identities: the pinned server and a stranger.
pub struct Pki {
    pub ca: String,
    pub gate: Identity,
    pub server: Identity,
    pub stranger: Identity,
}

fn self_signed(name: &str) -> Identity {
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec![name.to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    Identity {
        cert: cert.pem(),
        key: key.serialize_pem(),
    }
}

pub fn pki() -> Pki {
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
    let key = KeyPair::generate().unwrap();
    let gate = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    Pki {
        ca: ca.pem(),
        gate: Identity {
            cert: gate.pem(),
            key: key.serialize_pem(),
        },
        server: self_signed("kbf-server"),
        stranger: self_signed("stranger"),
    }
}

/// Serves `router` on a loopback port under the gate identity of `pki`, pinned to
/// its server identity, with the given handshake timeout.
pub async fn listen(router: axum::Router, pki: &Pki, handshake: std::time::Duration) -> SocketAddr {
    let config =
        crate::tls::server_config(pki.gate.chain(), pki.gate.private_key(), pki.server.pin())
            .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = Arc::new(listener);
    tokio::spawn(crate::tls::serve(
        move || {
            let listener = Arc::clone(&listener);
            async move { listener.accept().await.map(|(s, _)| s) }
        },
        config,
        router,
        handshake,
    ));
    addr
}

/// Connects to `addr` with `client` (or no client certificate) and sends one HTTP
/// request; the response's status and body, or the connection's error.
pub async fn request(
    addr: SocketAddr,
    ca: &str,
    client: Option<&Identity>,
    method: &str,
    path: &str,
    body: &[u8],
) -> std::io::Result<(u16, String)> {
    request_with(
        rustls::DEFAULT_VERSIONS,
        addr,
        ca,
        client,
        method,
        path,
        body,
    )
    .await
}

/// [`request`] limited to the given TLS versions.
pub async fn request_with(
    versions: &[&'static rustls::SupportedProtocolVersion],
    addr: SocketAddr,
    ca: &str,
    client: Option<&Identity>,
    method: &str,
    path: &str,
    body: &[u8],
) -> std::io::Result<(u16, String)> {
    let client = client.map(|id| (id, id));
    request_as(versions, addr, ca, client, method, path, body).await
}

/// Presents one certificate and key, whether or not they belong together: an
/// impostor can copy the pinned server's certificate (it is public) but not its key.
#[derive(Debug)]
pub struct Presents(pub Arc<CertifiedKey>);

impl ResolvesClientCert for Presents {
    fn resolve(&self, _: &[&[u8]], _: &[rustls::SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// [`request_with`], presenting the certificate of `client.0` and signing the handshake
/// with the private key of `client.1`.
pub async fn request_as(
    versions: &[&'static rustls::SupportedProtocolVersion],
    addr: SocketAddr,
    ca: &str,
    client: Option<(&Identity, &Identity)>,
    method: &str,
    path: &str,
    body: &[u8],
) -> std::io::Result<(u16, String)> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(ca.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let provider_key = |key: &PrivateKeyDer<'static>| {
        provider
            .key_provider
            .load_private_key(key.clone_key())
            .unwrap()
    };
    let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(versions)
        .unwrap()
        .with_root_certificates(roots);
    let config = match client {
        Some((cert, key)) => {
            let key = provider_key(&key.private_key());
            let presents = Presents(Arc::new(CertifiedKey::new(cert.chain(), key)));
            assert!(presents.has_certs());
            builder.with_client_cert_resolver(Arc::new(presents))
        }
        None => builder.with_no_client_auth(),
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let name = ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(name, tcp).await?;
    let head = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    tls.write_all(head.as_bytes()).await?;
    tls.write_all(body).await?;
    let mut response = Vec::new();
    tls.read_to_end(&mut response).await?;
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, b)| b.to_owned());
    Ok((status, body))
}
