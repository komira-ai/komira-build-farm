//! `linux verify-cert`: check, read only, that one certificate is what its row of the
//! farm's certificate table says (docs/deploy/kbf-modes.md, "A node's certificate").
//! kbf-deploy issues no certificate: the farm's one issuer does, from the one CA, and
//! this verb checks what it made before a unit is pointed at it.
//!
//! The table, a row per certificate (the issuer's, with the role each names and the
//! extended key usage, EKU, it allows):
//!
//! | row | role | EKU |
//! |---|---|---|
//! | `frontend-worker` | `frontend` | serverAuth, clientAuth (the worker listener, and the frontend calling storage) |
//! | `storage` | `storage` | serverAuth, clientAuth (the storage listener, and replication calls) |
//! | `daemon` | `daemon` | clientAuth (a daemon's client certificate) |
//!
//! The frontend's REAPI listener, the name clients dial, presents a certificate the
//! farm CA does not issue, so it is no row: `--row frontend-reapi` is refused.
//!
//! The checks, each a `FAIL <check>:` line:
//!
//! - `role`: the subjectAltName holds exactly one URI `urn:kbf:role:<role>` (what a
//!   storage node's admission reads: kbf-storage's `role_of`), and it is the row's.
//! - `eku`: the certificate's EKU set is the row's, no more and no less (none at all
//!   is not the row's either, though TLS would take it for any use).
//! - `name` (`daemon` only): its DNS and IP names are exactly `--node-id`, the one name
//!   kbf binds a daemon's certificate to.
//! - `chain`: it chains to `--ca-cert` now (signatures and validity period, webpki's
//!   checks), for each use in the row: as a TLS server under its first DNS name or
//!   address, and as a TLS client.
//!
//! Exit codes: 0 the certificate is the row's, 1 a check failed, 2 refused (a file that
//! cannot be read, no certificate in `--cert`, no usable CA, `--node-id` for a row that
//! is not `daemon`), with a `REFUSED:` line.

use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, ValueEnum};
use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use x509_parser::extensions::GeneralName;

use super::check::Failure;

/// How a certificate names its role: `urn:kbf:role:<name>` in its subjectAltName.
pub const ROLE_URI_PREFIX: &str = "urn:kbf:role:";

/// The name of the EKU that lets a TLS server present the certificate.
pub const SERVER_AUTH: &str = "serverAuth";
/// The name of the EKU that lets a TLS client present the certificate.
pub const CLIENT_AUTH: &str = "clientAuth";

/// A row of the certificate table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Row {
    /// The frontend's worker listener, and the frontend when it calls storage.
    FrontendWorker,
    /// A storage node's listener, and its calls to other storage nodes.
    Storage,
    /// A daemon's client certificate.
    Daemon,
}

impl Row {
    /// Every row, in the table's order.
    pub const ALL: [Row; 3] = [Row::FrontendWorker, Row::Storage, Row::Daemon];

    /// The role the row's certificate names after [`ROLE_URI_PREFIX`].
    #[must_use]
    pub fn role(self) -> &'static str {
        match self {
            Row::FrontendWorker => "frontend",
            Row::Storage => "storage",
            Row::Daemon => "daemon",
        }
    }

    /// The EKU set the row's certificate allows, serverAuth first.
    #[must_use]
    pub fn ekus(self) -> &'static [&'static str] {
        match self {
            Row::FrontendWorker | Row::Storage => &[SERVER_AUTH, CLIENT_AUTH],
            Row::Daemon => &[CLIENT_AUTH],
        }
    }
}

#[derive(Args)]
pub struct VerifyArgs {
    /// The row of the certificate table the certificate is for. The frontend's REAPI
    /// certificate is not the farm CA's and is no row.
    #[arg(long, value_enum)]
    row: Row,
    /// The certificate (PEM; any certificates after the first are its intermediates).
    #[arg(long)]
    cert: PathBuf,
    /// The farm CA's certificate (PEM).
    #[arg(long)]
    ca_cert: PathBuf,
    /// The node id a daemon's certificate names, its one DNS name (`--row daemon`
    /// only, and required there).
    #[arg(long, required_if_eq("row", "daemon"))]
    node_id: Option<String>,
}

/// Why the certificate could not be checked (exit 2).
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("cannot read {path}: {why}")]
    Read { path: PathBuf, why: String },
    #[error("the certificate cannot be read: {0}")]
    NotCert(String),
    #[error("the CA certificate holds no usable certificate: {0}")]
    Ca(String),
    #[error("--node-id is for --row daemon only")]
    NodeId,
}

impl VerifyError {
    fn not_cert(e: impl ToString) -> VerifyError {
        VerifyError::NotCert(e.to_string())
    }

    fn ca(e: impl ToString) -> VerifyError {
        VerifyError::Ca(e.to_string())
    }
}

/// How the PEM certificate `cert_pem` differs from `row` (none: it is the row's),
/// checked against the CA certificates in `ca_pem` at `now`; `node_id` for a daemon.
///
/// # Errors
/// [`VerifyError`]: `cert_pem` holds no certificate, `ca_pem` no usable one, or a
/// `node_id` is given for a row that is not `daemon`.
pub fn verify(
    ca_pem: &[u8],
    cert_pem: &[u8],
    row: Row,
    node_id: Option<&str>,
    now: SystemTime,
) -> Result<Vec<Failure>, VerifyError> {
    let now = UnixTime::since_unix_epoch(now.duration_since(UNIX_EPOCH).unwrap_or_default());
    if node_id.is_some() && row != Row::Daemon {
        return Err(VerifyError::NodeId);
    }
    let mut chain = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(VerifyError::not_cert)?;
    if chain.is_empty() {
        return Err(VerifyError::NotCert("no CERTIFICATE block".to_owned()));
    }
    let end_entity = chain.remove(0);
    let (_, cert) =
        x509_parser::parse_x509_certificate(&end_entity).map_err(VerifyError::not_cert)?;
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(ca_pem) {
        roots
            .add(ca.map_err(VerifyError::ca)?)
            .map_err(VerifyError::ca)?;
    }
    let roots = Arc::new(roots);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = WebPkiServerVerifier::builder_with_provider(roots.clone(), provider.clone())
        .build()
        .map_err(VerifyError::ca)?;
    let client = WebPkiClientVerifier::builder_with_provider(roots, provider)
        .build()
        .map_err(VerifyError::ca)?;

    let mut failures = Vec::new();
    let mut fail = |rule: &'static str, message: String| failures.push(Failure { rule, message });
    let san = cert.subject_alternative_name().ok().flatten();
    let mut roles = Vec::new();
    let mut names = Vec::new();
    for general in san.iter().flat_map(|s| &s.value.general_names) {
        match general {
            GeneralName::URI(uri) => roles.extend(uri.strip_prefix(ROLE_URI_PREFIX)),
            GeneralName::DNSName(dns) => names.push((*dns).to_owned()),
            GeneralName::IPAddress(bytes) => names.extend(ip(bytes)),
            _ => {}
        }
    }
    match roles.as_slice() {
        [role] if *role == row.role() => {}
        [role] => fail(
            "role",
            format!(
                "it names the role {ROLE_URI_PREFIX}{role}, not {ROLE_URI_PREFIX}{}",
                row.role()
            ),
        ),
        _ => fail(
            "role",
            format!(
                "it names {} roles ({ROLE_URI_PREFIX}<role>), not exactly one",
                roles.len()
            ),
        ),
    }

    let got = ekus(&cert);
    if got != row.ekus() {
        let shown = if got.is_empty() {
            "none".to_owned()
        } else {
            got.join(",")
        };
        fail(
            "eku",
            format!(
                "its extended key usage is {shown}, not {}",
                row.ekus().join(",")
            ),
        );
    }

    if let Some(id) = node_id
        && names != [id]
    {
        fail(
            "name",
            format!(
                "it names {}, not exactly the node id {id} (a daemon's one DNS name)",
                if names.is_empty() {
                    "no DNS name or address".to_owned()
                } else {
                    names.join(" ")
                }
            ),
        );
    }

    for &eku in row.ekus() {
        let checked = if eku == SERVER_AUTH {
            match names
                .first()
                .and_then(|n| ServerName::try_from(n.clone()).ok())
            {
                Some(name) => server
                    .verify_server_cert(&end_entity, &chain, &name, &[], now)
                    .map(drop),
                None => {
                    fail(
                        "chain",
                        "it names no DNS name or address, so no TLS client can check it \
                         as a server"
                            .to_owned(),
                    );
                    continue;
                }
            }
        } else {
            client
                .verify_client_cert(&end_entity, &chain, now)
                .map(drop)
        };
        if let Err(e) = checked {
            fail(
                "chain",
                format!("it does not chain to the CA for {eku}: {e}"),
            );
        }
    }
    Ok(failures)
}

/// The EKU names of `cert`, serverAuth and clientAuth first, then any others as
/// RFC 5280 names them, then unknown ones by OID; none for a certificate without the
/// extension (or with two, which no TLS library takes).
fn ekus(cert: &x509_parser::certificate::X509Certificate<'_>) -> Vec<String> {
    let Some(ext) = cert.extended_key_usage().ok().flatten() else {
        return Vec::new();
    };
    let e = ext.value;
    [
        (e.server_auth, SERVER_AUTH),
        (e.client_auth, CLIENT_AUTH),
        (e.any, "anyExtendedKeyUsage"),
        (e.code_signing, "codeSigning"),
        (e.email_protection, "emailProtection"),
        (e.time_stamping, "timeStamping"),
        (e.ocsp_signing, "OCSPSigning"),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, name)| name.to_owned())
    .chain(e.other.iter().map(|oid| oid.to_id_string()))
    .collect()
}

/// An IP subjectAltName's address, written as a TLS client would dial it.
fn ip(bytes: &[u8]) -> Option<String> {
    let v4 = <[u8; 4]>::try_from(bytes).map(IpAddr::from);
    v4.or_else(|_| <[u8; 16]>::try_from(bytes).map(IpAddr::from))
        .ok()
        .map(|a| a.to_string())
}

/// `verify-cert`: the lines to print and whether every check passed.
///
/// # Errors
/// [`VerifyError`].
pub fn run(a: &VerifyArgs, now: SystemTime) -> Result<(Vec<String>, bool), VerifyError> {
    let read = |path: &Path| {
        std::fs::read(path).map_err(|e| VerifyError::Read {
            path: path.to_owned(),
            why: e.to_string(),
        })
    };
    let failures = verify(
        &read(&a.ca_cert)?,
        &read(&a.cert)?,
        a.row,
        a.node_id.as_deref(),
        now,
    )?;
    let mut lines: Vec<String> = failures.iter().map(ToString::to_string).collect();
    let ok = failures.is_empty();
    lines.push(if ok {
        format!(
            "CERT OK: {} is a {} certificate: role {ROLE_URI_PREFIX}{}, extended key usage \
             {}, chains to {}",
            a.cert.display(),
            a.row
                .to_possible_value()
                .expect("no row is skipped")
                .get_name(),
            a.row.role(),
            a.row.ekus().join(","),
            a.ca_cert.display()
        )
    } else {
        format!("CERT BROKEN: {} failed check(s)", failures.len())
    });
    Ok((lines, ok))
}

/// The verb: 0 the certificate is the row's, 1 a check failed, 2 refused.
pub fn cli(a: &VerifyArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match run(a, SystemTime::now()) {
        Ok((lines, ok)) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
#[path = "verify_cert_tests.rs"]
mod tests;
