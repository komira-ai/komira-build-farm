//! A throwaway certificate authority for one integration cell: a CA, a server
//! certificate for `localhost` and one daemon client certificate, written as PEM files.
//!
//! The keys are generated fresh on every call and never leave the cell's directory.
//! They are test material: the worker listener's mutual TLS needs them, and the
//! farm's real CA is not involved.

use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};

/// The name in the server certificate. A daemon verifies it with `--tls-server-name`.
pub const SERVER_NAME: &str = "localhost";

/// Where [`write`] put each file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkiFiles {
    /// The CA certificate: the server's client CA and the daemon's trust root.
    pub ca_cert: PathBuf,
    /// The server certificate and its key.
    pub server_cert: PathBuf,
    pub server_key: PathBuf,
    /// The daemon's client certificate and its key.
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
}

/// Why the certificates could not be made or written.
#[derive(Debug, thiserror::Error)]
pub enum PkiError {
    #[error("generate certificates: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Generates a CA, a server certificate for [`SERVER_NAME`] and a client certificate
/// that names `node_id` (as its one DNS subjectAltName, which the server binds to the
/// daemon's `Hello`, and as its common name), and writes them under `dir` (created if
/// missing).
///
/// # Errors
/// Generation fails, or a file cannot be written.
pub fn write(dir: &Path, node_id: &str) -> Result<PkiFiles, PkiError> {
    let mut ca = CertificateParams::new(Vec::<String>::new())?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "kbf integration cell CA");
    ca.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate()?)?;

    let leaf = |names: Vec<String>, cn: &str, eku| -> Result<(String, String), PkiError> {
        let mut params = CertificateParams::new(names)?;
        params.distinguished_name.push(DnType::CommonName, cn);
        params.extended_key_usages = vec![eku];
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &ca)?;
        Ok((cert.pem(), key.serialize_pem()))
    };
    let (server_cert, server_key) = leaf(
        vec![SERVER_NAME.to_owned()],
        "kbf integration server",
        ExtendedKeyUsagePurpose::ServerAuth,
    )?;
    let (client_cert, client_key) = leaf(
        vec![node_id.to_owned()],
        node_id,
        ExtendedKeyUsagePurpose::ClientAuth,
    )?;

    let create = std::fs::create_dir_all(dir);
    create.map_err(|source| PkiError::Write {
        path: dir.to_owned(),
        source,
    })?;
    let put = |file: &str, text: &str| {
        let path = dir.join(file);
        match std::fs::write(&path, text) {
            Ok(()) => Ok(path),
            Err(source) => Err(PkiError::Write { path, source }),
        }
    };
    Ok(PkiFiles {
        ca_cert: put("ca.pem", &ca.pem())?,
        server_cert: put("server.pem", &server_cert)?,
        server_key: put("server.key", &server_key)?,
        client_cert: put("client.pem", &client_cert)?,
        client_key: put("client.key", &client_key)?,
    })
}
