//! The role a peer's certificate names (`docs/design/architecture-v2.md`, section 9: mTLS
//! on every hop with the role, frontend, storage or daemon, in the certificate).
//!
//! A certificate names its role as exactly one URI subjectAltName
//! `urn:kbf:role:<name>` ([`ROLE_URI_PREFIX`]). [`role_of`] is the one place that reads
//! it; every listener that takes a client certificate asks it, then decides whether the
//! role may call the endpoint. A DNS name that looks like a role is not one, and the
//! subject's common name is not read.
//!
//! The certificate is not verified here: TLS has already verified it against the
//! listener's client CA. Which gRPC status a refusal is belongs to the caller (see
//! [`RoleError`]).

use std::fmt;

use x509_parser::extensions::{GeneralName, ParsedExtension};

/// How a certificate names a role: this, then the role's [`PeerRole::name`].
pub const ROLE_URI_PREFIX: &str = "urn:kbf:role:";

/// The role a peer's certificate names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerRole {
    /// A frontend, on behalf of a client or of a daemon's result.
    Frontend,
    /// A storage node (replication, repair).
    Storage,
    /// A build node.
    Daemon,
}

impl PeerRole {
    /// Every role, in the order section 9 lists them.
    pub const ALL: [Self; 3] = [Self::Frontend, Self::Storage, Self::Daemon];

    /// The name a certificate spells after [`ROLE_URI_PREFIX`].
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Frontend => "frontend",
            Self::Storage => "storage",
            Self::Daemon => "daemon",
        }
    }

    /// The role spelled `name` (exactly, case included), if it is one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.name() == name)
    }
}

impl fmt::Display for PeerRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a certificate names no role.
///
/// A listener answers [`RoleError::NotACertificate`] UNAUTHENTICATED (there is no
/// identity to judge) and the others PERMISSION_DENIED (the peer is known, its
/// certificate does not give it a role).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoleError {
    /// The bytes are not an X.509 certificate.
    #[error("the client certificate does not parse: {0}")]
    NotACertificate(String),
    /// The certificate names no role, or more than one.
    #[error(
        "the client certificate must name exactly one role ({ROLE_URI_PREFIX}<name>); \
         it names {0}"
    )]
    Count(usize),
    /// The one role URI names something that is not a role.
    #[error("{0:?} is not a role")]
    Unknown(String),
}

/// The role a DER certificate names: its one `urn:kbf:role:<name>` URI subjectAltName.
///
/// # Errors
/// See [`RoleError`].
pub fn role_of(der: &[u8]) -> Result<PeerRole, RoleError> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| RoleError::NotACertificate(e.to_string()))?;
    let names: Vec<&str> = cert
        .extensions()
        .iter()
        .filter_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => Some(&san.general_names),
            _ => None,
        })
        .flatten()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => uri.strip_prefix(ROLE_URI_PREFIX),
            _ => None,
        })
        .collect();
    let [name] = names.as_slice() else {
        return Err(RoleError::Count(names.len()));
    };
    PeerRole::from_name(name).ok_or_else(|| RoleError::Unknown((*name).to_owned()))
}
