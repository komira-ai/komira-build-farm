//! What the node checks in the frontend's answer before it writes a file
//! (design sections 5.4 step 3, 5.8 and 5.10).
//!
//! An `EnrollResponse` and a `RenewResponse` carry the same material: one certificate per
//! role, the CA bundle, the frontend's clock and the time to renew. The node installs
//! nothing it could not validate:
//!
//! - the clock: the frontend's `server_time` is within two minutes of the node's;
//! - the bundle holds the certificate whose hash is the pin (so a forged bundle, which
//!   the node has nothing else to compare with, is caught);
//! - exactly the requested roles come back, each certificate for the key the node made,
//!   naming the role and the node, matching the fields the frontend reported;
//! - each certificate is signed by the pinned root, or by a root the bundle shows is
//!   signed by it (a rotation's cross-signature, section 6.4);
//! - it is valid now, and the time to renew is before it expires.
//!
//! Everything takes the time as a parameter.

use base64::Engine as _;
use kbf_peer::role_of;
use kbf_proto::kbf::enroll::v1::IssuedCert;
use kbf_types::{CaPin, NodeId, Role};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use x509_parser::prelude::{FromDer, GeneralName, ParsedExtension, X509Certificate};

use crate::pin::pin_of;

/// The most the frontend's clock may differ from the node's, in seconds.
pub const MAX_SKEW_S: i64 = 120;

/// Why an answer is refused. Nothing is installed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AnswerError {
    /// The clocks differ by more than [`MAX_SKEW_S`].
    #[error(
        "the frontend's clock ({theirs}) differs from this node's ({ours}) by more than {MAX_SKEW_S} s"
    )]
    ClockSkew {
        /// This node's unix time.
        ours: i64,
        /// The frontend's unix time.
        theirs: i64,
    },
    /// The CA bundle is not PEM certificates.
    #[error("the CA bundle does not parse")]
    Bundle,
    /// The bundle lacks the certificate the token pinned.
    #[error("the CA bundle does not contain the pinned root")]
    PinNotInBundle,
    /// Not exactly the requested roles.
    #[error("the certificates are not exactly the requested roles")]
    RoleSet,
    /// A certificate for one role is wrong in the way named.
    #[error("the {role} certificate {fault}")]
    Cert {
        /// The role.
        role: Role,
        /// What is wrong.
        fault: CertFault,
    },
    /// The time to renew is not before the earliest expiry.
    #[error("the time to renew is not before the certificates expire")]
    RenewAfter,
}

/// What is wrong with one certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CertFault {
    /// Not an X.509 certificate.
    #[error("does not parse")]
    Parse,
    /// Not for the key this node made.
    #[error("is for another key")]
    Key,
    /// It does not name the role (exactly once).
    #[error("does not name the role")]
    Role,
    /// It does not name the node.
    #[error("does not name this node")]
    Name,
    /// Its serial or validity differs from what the frontend reported.
    #[error("differs from the fields the frontend reported")]
    Fields,
    /// Not signed by the pinned root or a root it signed.
    #[error("is not signed by the pinned root")]
    Untrusted,
    /// Not valid now.
    #[error("is not valid now")]
    Validity,
}

/// What the node expects of an answer.
#[derive(Clone, Copy, Debug)]
pub struct Expect<'a> {
    /// The token's pin.
    pub pin: &'a CaPin,
    /// This node's id.
    pub node_id: &'a NodeId,
    /// The roles requested and the SubjectPublicKeyInfo DER of the key made for each.
    pub keys: &'a [(Role, Vec<u8>)],
    /// This node's unix time.
    pub now: i64,
}

/// An accepted certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cert {
    /// The role.
    pub role: Role,
    /// The leaf, PEM.
    pub pem: String,
    /// The serial, lower-case hex.
    pub serial: String,
    /// Unix seconds.
    pub not_before: i64,
    /// Unix seconds.
    pub not_after: i64,
}

/// PEM text for `der` under `label`, 64 columns.
#[must_use]
pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

/// Checks an answer and returns the certificates to install.
///
/// # Errors
/// [`AnswerError`], on the first thing wrong.
pub fn check(
    expect: &Expect<'_>,
    certs: &[IssuedCert],
    ca_bundle_pem: &[u8],
    server_time: i64,
    renew_after: i64,
) -> Result<Vec<Cert>, AnswerError> {
    if (server_time - expect.now).abs() > MAX_SKEW_S {
        return Err(AnswerError::ClockSkew {
            ours: expect.now,
            theirs: server_time,
        });
    }
    let bundle = CertificateDer::pem_slice_iter(ca_bundle_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AnswerError::Bundle)?;
    let bundle = bundle
        .iter()
        .map(|der| parse(der).ok_or(AnswerError::Bundle))
        .collect::<Result<Vec<_>, _>>()?;
    let pinned = bundle
        .iter()
        .find(|c| &pin_of(c.as_ref()) == expect.pin.as_bytes())
        .ok_or(AnswerError::PinNotInBundle)?;
    // The keys that may sign a leaf: the pinned root's, and the keys of roots it signed.
    let mut trusted = vec![pinned.public_key()];
    trusted.extend(
        bundle
            .iter()
            .filter(|c| c.verify_signature(Some(pinned.public_key())).is_ok())
            .map(|c| c.public_key()),
    );

    let by_role = |role: Role| {
        let mut found = certs.iter().filter(|c| c.role == role.name());
        match (found.next(), found.next()) {
            (Some(cert), None) => Some(cert),
            _ => None,
        }
    };
    if certs.len() != expect.keys.len() {
        return Err(AnswerError::RoleSet);
    }
    let mut accepted = Vec::new();
    for (role, spki) in expect.keys {
        let issued = by_role(*role).ok_or(AnswerError::RoleSet)?;
        let fault = |fault| AnswerError::Cert { role: *role, fault };
        let cert = parse(&issued.leaf_der).ok_or(fault(CertFault::Parse))?;
        if cert.public_key().raw != spki.as_slice() {
            return Err(fault(CertFault::Key));
        }
        if !matches!(role_of(&issued.leaf_der), Ok(r) if r.name() == role.name()) {
            return Err(fault(CertFault::Role));
        }
        if !names_node(&cert, expect.node_id, *role) {
            return Err(fault(CertFault::Name));
        }
        let validity = cert.validity();
        let serial = cert.serial.to_str_radix(16);
        if serial != issued.serial
            || validity.not_before.timestamp() != issued.not_before
            || validity.not_after.timestamp() != issued.not_after
        {
            return Err(fault(CertFault::Fields));
        }
        if !trusted
            .iter()
            .any(|key| cert.verify_signature(Some(key)).is_ok())
        {
            return Err(fault(CertFault::Untrusted));
        }
        if expect.now < issued.not_before - MAX_SKEW_S || expect.now >= issued.not_after {
            return Err(fault(CertFault::Validity));
        }
        accepted.push(Cert {
            role: *role,
            pem: pem_encode("CERTIFICATE", &issued.leaf_der),
            serial,
            not_before: issued.not_before,
            not_after: issued.not_after,
        });
    }
    if accepted.iter().any(|c| renew_after >= c.not_after) {
        return Err(AnswerError::RenewAfter);
    }
    Ok(accepted)
}

fn parse<'a>(der: &'a [u8]) -> Option<X509Certificate<'a>> {
    match X509Certificate::from_der(der) {
        Ok((&[], cert)) => Some(cert),
        _ => None,
    }
}

/// Whether the certificate's DNS names include the node id; a daemon's are exactly it.
fn names_node(cert: &X509Certificate<'_>, node_id: &NodeId, role: Role) -> bool {
    let names: Vec<&str> = cert
        .extensions()
        .iter()
        .filter_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => Some(&san.general_names),
            _ => None,
        })
        .flatten()
        .filter_map(|name| match name {
            GeneralName::DNSName(dns) => Some(*dns),
            _ => None,
        })
        .collect();
    match role {
        Role::Daemon => names == [node_id.as_str()],
        Role::Storage => names.contains(&node_id.as_str()),
    }
}
