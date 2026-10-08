//! The gate's listener: TLS that requires a client certificate and accepts exactly one
//! client key, `kbf-server`'s, pinned by the SHA-256 of its SubjectPublicKeyInfo
//! (S5.2). No CA is involved: a certificate any CA issued is refused unless it carries
//! the pinned key, and the handshake proves the client holds that key's private half.
//! The certificate's names and dates are not read; rotating the server's key is a
//! change of the pin on the gate's host.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite};

/// The SHA-256 of a certificate's SubjectPublicKeyInfo (DER), the pin's form; `None`
/// if the bytes are not a certificate.
pub fn spki_sha256(certificate: &[u8]) -> Option<[u8; 32]> {
    let (_, cert) = x509_parser::parse_x509_certificate(certificate).ok()?;
    Some(Sha256::digest(cert.tbs_certificate.subject_pki.raw).into())
}

/// Accepts exactly the client key whose SPKI hash is `pin`.
#[derive(Debug)]
struct PinnedClient {
    pin: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for PinnedClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let spki = spki_sha256(end_entity).ok_or(rustls::Error::InvalidCertificate(
            CertificateError::BadEncoding,
        ))?;
        if bool::from(spki.ct_eq(&self.pin)) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Reads the gate's certificate chain and key from PEM files.
///
/// # Errors
/// A file cannot be read, or holds no certificate or key.
pub fn load_identity(
    cert: &Path,
    key: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| format!("{}: {e}", cert.display()))?;
    if chain.is_empty() {
        return Err(format!("{}: no certificate", cert.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {e}", key.display()))?;
    Ok((chain, key))
}

/// The listener's TLS configuration: the gate's identity, and the client pin.
///
/// # Errors
/// The key does not match the certificate or is of an unsupported type.
pub fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    pin: [u8; 32],
) -> Result<Arc<ServerConfig>, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinnedClient {
        pin,
        algorithms: provider.signature_verification_algorithms,
    };
    let config = ServerConfig::builder_with_provider(Arc::clone(&provider) as Arc<CryptoProvider>)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(chain, key)?;
    Ok(Arc::new(config))
}

/// Serves `router` on every connection `accept` yields whose TLS handshake (under
/// `config`, within `handshake`) succeeds. An accept error is logged and the loop goes
/// on. Never returns.
pub async fn serve<S, F, A>(
    mut accept: A,
    config: Arc<ServerConfig>,
    router: axum::Router,
    handshake: Duration,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Future<Output = std::io::Result<S>>,
    A: FnMut() -> F,
{
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    loop {
        match accept().await {
            Ok(stream) => {
                tokio::spawn(connection(
                    stream,
                    acceptor.clone(),
                    router.clone(),
                    handshake,
                ));
            }
            Err(e) => {
                tracing::warn!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn connection<S>(
    stream: S,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    handshake: Duration,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let tls = match tokio::time::timeout(handshake, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            tracing::info!("refused a TLS client: {e}");
            return;
        }
        Err(_) => {
            tracing::info!("a TLS client did not finish its handshake in time");
            return;
        }
    };
    let service = hyper_util::service::TowerToHyperService::new(router);
    let io = hyper_util::rt::TokioIo::new(tls);
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, service)
        .await
    {
        tracing::debug!("connection: {e}");
    }
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;
