//! First-contact trust (design section 5.4).
//!
//! At first contact a node knows the URL and the token, and the token carries the
//! SHA-256 of the farm CA's root certificate. [`PinVerifier`] checks the enroll
//! listener's chain against that one hash, with no public CA involved:
//!
//! 1. some certificate in the chain the server presents has the pinned hash;
//! 2. the leaf chains to it, validly, for the host the node dialed, with the
//!    `serverAuth` usage (the checks `webpki` makes, run with the pinned certificate as
//!    the only trust anchor);
//! 3. the leaf carries the `frontend` role, so a storage or daemon certificate, which
//!    any enrolled node holds, cannot impersonate the enrollment server.
//!
//! [`Anchor::Roots`] is the declared fallback (`tls.trust = "public"`): the system
//! roots verify the name. The node's answer checks (the bundle holds the pinned root and
//! every certificate chains to it) are the same in both modes and live in `answer.rs`.

use std::sync::Arc;

use kbf_peer::{PeerRole, role_of};
use kbf_types::CaPin;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{
    WebPkiServerVerifier, verify_server_cert_signed_by_trust_anchor, verify_server_name,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, OtherError, RootCertStore,
    SignatureScheme,
};
use sha2::{Digest, Sha256};

/// What the node trusts for the enroll listener.
#[derive(Clone, Debug)]
pub enum Anchor {
    /// `tls.trust = "pin"`: the CA certificate whose SHA-256 this is.
    Pin(CaPin),
    /// `tls.trust = "public"`: these roots (the system's, see [`system_roots`]).
    Roots(Arc<RootCertStore>),
}

/// Why a TLS configuration could not be built.
#[derive(Debug, thiserror::Error)]
pub enum AnchorError {
    /// `tls.trust = "public"` with no roots to trust.
    #[error("no trust roots: the system has none")]
    NoRoots(#[from] rustls::client::VerifierBuilderError),
    /// The TLS library refused its protocol versions.
    #[error("tls configuration: {0}")]
    Tls(#[from] Error),
}

/// Why the pin verifier refused a chain, carried inside the TLS error.
#[derive(Debug, thiserror::Error)]
enum PinFault {
    #[error("the server's chain holds no certificate with the pinned hash")]
    NotInChain,
    #[error("the server's certificate does not carry the frontend role")]
    NotFrontend,
}

fn refuse(fault: PinFault) -> Error {
    Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(fault))))
}

/// The SHA-256 of a certificate's DER: what the pin is.
#[must_use]
pub fn pin_of(der: &[u8]) -> [u8; 32] {
    Sha256::digest(der).into()
}

/// The system's trusted roots. Roots that fail to parse are skipped.
#[must_use]
pub fn system_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    roots
}

/// Verifies the enroll listener against the token's pin.
#[derive(Debug)]
pub struct PinVerifier {
    pin: CaPin,
    provider: Arc<CryptoProvider>,
}

impl PinVerifier {
    /// A verifier for `pin`.
    #[must_use]
    pub fn new(pin: CaPin, provider: Arc<CryptoProvider>) -> Self {
        Self { pin, provider }
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let pinned = std::iter::once(end_entity)
            .chain(intermediates)
            .find(|cert| &pin_of(cert) == self.pin.as_bytes())
            .ok_or_else(|| refuse(PinFault::NotInChain))?;
        let mut roots = RootCertStore::empty();
        roots.add(pinned.clone())?;
        let leaf = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &leaf,
            &roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        verify_server_name(&leaf, server_name)?;
        match role_of(end_entity) {
            Ok(PeerRole::Frontend) => Ok(ServerCertVerified::assertion()),
            _ => Err(refuse(PinFault::NotFrontend)),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The TLS client configuration for dialing the frontend: HTTP/2, the
/// server verified by `anchor`, and `identity` (certificate chain and key) when the
/// server demands a client certificate.
///
/// # Errors
/// [`AnchorError`].
pub fn client_config(anchor: Anchor) -> Result<ClientConfig, AnchorError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier: Arc<dyn ServerCertVerifier> = match anchor {
        Anchor::Pin(pin) => Arc::new(PinVerifier::new(pin, provider.clone())),
        Anchor::Roots(roots) => {
            WebPkiServerVerifier::builder_with_provider(roots, provider.clone()).build()?
        }
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(config)
}
