//! Reading the files and making a `rustls` server configuration of them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::RootCertStore;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use sha2::{Digest, Sha256};

/// The files a listener's TLS is made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsFiles {
    /// The PEM certificate chain, leaf first.
    pub cert: PathBuf,
    /// The PEM private key of the leaf.
    pub key: PathBuf,
    /// The PEM bundle of CAs a client certificate must chain to; `None` asks for none.
    pub client_ca: Option<PathBuf>,
}

/// Why a set of files does not make a TLS configuration.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A file could not be read.
    #[error("read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The reason.
        #[source]
        source: std::io::Error,
    },
    /// A file is not PEM of the kind it must be, or holds none of it.
    #[error("{path}: {reason}")]
    Pem {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The files do not agree or are not usable together (the key is not the leaf's, the
    /// key is of a type that is not supported, the CA certificates are unusable).
    #[error("the files do not make a TLS configuration: {0}")]
    Config(String),
    /// The leaf's validity ended before now.
    #[error("the certificate expired at {not_after} (seconds since the epoch)")]
    Expired {
        /// The end of the leaf's validity.
        not_after: i64,
    },
}

/// The bytes of the files, as read at one moment.
#[derive(Debug)]
pub(crate) struct Material {
    pub(crate) cert: Vec<u8>,
    pub(crate) key: Vec<u8>,
    pub(crate) client_ca: Option<Vec<u8>>,
}

/// A configuration built from [`Material`], and when its leaf stops being valid.
pub(crate) struct Built {
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) not_after: i64,
}

fn read_file(path: &Path) -> Result<Vec<u8>, TlsError> {
    std::fs::read(path).map_err(|source| TlsError::Read {
        path: path.to_owned(),
        source,
    })
}

/// Reads each file once.
pub(crate) fn read(files: &TlsFiles) -> Result<Material, TlsError> {
    Ok(Material {
        cert: read_file(&files.cert)?,
        key: read_file(&files.key)?,
        client_ca: files.client_ca.as_deref().map(read_file).transpose()?,
    })
}

/// A digest of what was read, so a look at unchanged files is told from one at changed
/// files without keeping the bytes. Each part is length-prefixed, so a byte moved from
/// the end of one file to the start of the next changes it.
pub(crate) fn digest(material: &Material) -> [u8; 32] {
    let mut hash = Sha256::new();
    let parts = [
        Some(&material.cert),
        Some(&material.key),
        material.client_ca.as_ref(),
    ];
    for part in parts {
        // A missing CA (None) is told from an empty one (Some(empty)).
        match part {
            Some(bytes) => {
                hash.update([1]);
                hash.update((bytes.len() as u64).to_be_bytes());
                hash.update(bytes);
            }
            None => hash.update([0]),
        }
    }
    hash.finalize().into()
}

fn pem_error(path: &Path, reason: impl std::fmt::Display) -> TlsError {
    TlsError::Pem {
        path: path.to_owned(),
        reason: reason.to_string(),
    }
}

fn certificates(path: &Path, bytes: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let found = CertificateDer::pem_slice_iter(bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| pem_error(path, e))?;
    if found.is_empty() {
        return Err(pem_error(path, "no certificate"));
    }
    Ok(found)
}

/// When the leaf's validity ends, in seconds since the epoch, and whether it has.
fn validity(leaf: &CertificateDer<'_>, path: &Path) -> Result<(i64, bool), TlsError> {
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|e| pem_error(path, format!("the leaf is not an X.509 certificate: {e}")))?;
    let validity = parsed.validity();
    Ok((
        validity.not_after.timestamp(),
        validity.time_to_expiration().is_none(),
    ))
}

/// The configuration `material` makes. With `reject_expired`, a leaf whose validity has
/// ended is refused (a reload does; the first load does not, as a server has always
/// started with the certificate it was given).
pub(crate) fn build(
    files: &TlsFiles,
    material: &Material,
    reject_expired: bool,
) -> Result<Built, TlsError> {
    let chain = certificates(&files.cert, &material.cert)?;
    let key = PrivateKeyDer::from_pem_slice(&material.key).map_err(|e| pem_error(&files.key, e))?;
    let (not_after, expired) = validity(&chain[0], &files.cert)?;
    if reject_expired && expired {
        return Err(TlsError::Expired { not_after });
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = match (&files.client_ca, &material.client_ca) {
        (Some(path), Some(bytes)) => {
            let mut roots = RootCertStore::empty();
            // Certificates that do not parse as trust anchors are left out; if none
            // does, the verifier cannot be made, which is the refusal.
            roots.add_parsable_certificates(certificates(path, bytes)?);
            let verifier =
                WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                    .build()
                    .map_err(|e| pem_error(path, format!("no usable CA certificate: {e}")))?;
            Some(verifier)
        }
        _ => None,
    };
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|builder| {
            match verifier {
                Some(verifier) => builder.with_client_cert_verifier(verifier),
                None => builder.with_no_client_auth(),
            }
            .with_single_cert(chain, key)
        })
        .map_err(|e| TlsError::Config(e.to_string()))?;
    // What tonic's own acceptor offers: gRPC runs over HTTP/2 only.
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(Built {
        config: Arc::new(config),
        not_after,
    })
}
