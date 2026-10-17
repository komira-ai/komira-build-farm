//! The role a peer's certificate names (`docs/design/architecture-v2.md`, section 9: mTLS
//! on every hop with the role, frontend, storage or daemon, in the certificate).
//!
//! A certificate names its role as exactly one URI subjectAltName
//! `urn:kbf:role:<name>` ([`ROLE_URI_PREFIX`]). [`role_of`] is the one place that reads
//! it; every listener that takes a client certificate asks it, then decides whether the
//! role may call the endpoint. A DNS name that looks like a role is not one, and the
//! subject's common name is not read.
//!
//! A certificate also names the host it speaks for, as DNS and IP subjectAltNames.
//! [`names_of`] reads them as [`PeerNames`], and [`PeerNames::covers`] says whether one
//! of them is a given host (the host of a `host:port` address, [`host_of`]): DNS names
//! match exactly up to ASCII case, IP names as parsed addresses, and a wildcard name
//! covers nothing. The role URI, other URIs and the common name are never names.
//!
//! The certificate is not verified here: TLS has already verified it against the
//! listener's client CA. Which gRPC status a refusal is belongs to the caller (see
//! [`RoleError`]).

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

use x509_parser::certificate::X509Certificate;
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

/// Why a certificate names no role (and, for [`names_of`], that the bytes are no
/// certificate: the only variant it answers).
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
    let cert = parse(der)?;
    let names: Vec<&str> = alt_names(&cert)
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

fn parse(der: &[u8]) -> Result<X509Certificate<'_>, RoleError> {
    x509_parser::parse_x509_certificate(der)
        .map(|(_, cert)| cert)
        .map_err(|e| RoleError::NotACertificate(e.to_string()))
}

/// Every entry of the certificate's subjectAltName extensions.
fn alt_names<'c, 'd>(cert: &'c X509Certificate<'d>) -> impl Iterator<Item = &'c GeneralName<'d>> {
    cert.extensions()
        .iter()
        .filter_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => Some(&san.general_names),
            _ => None,
        })
        .flatten()
}

/// The hosts a certificate speaks for: its DNS and IP subjectAltNames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerNames {
    dns: Vec<String>,
    ips: Vec<IpAddr>,
}

impl PeerNames {
    /// Whether one of the names is `host`: a bare host, without port or brackets (see
    /// [`host_of`]). A host that parses as an IP address is covered by an IP name of
    /// the same address and by no DNS name; any other host is covered by a DNS name equal
    /// to it up to ASCII case and by no IP name. A DNS name with a `*` in it covers
    /// nothing, not even itself, and the empty host is covered by nothing.
    #[must_use]
    pub fn covers(&self, host: &str) -> bool {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return self.ips.contains(&ip);
        }
        !host.is_empty()
            && self
                .dns
                .iter()
                .any(|name| !name.contains('*') && name.eq_ignore_ascii_case(host))
    }
}

/// The DNS names as written, then the IPs, comma separated; `no DNS or IP name` for none.
impl fmt::Display for PeerNames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.dns.is_empty() && self.ips.is_empty() {
            return f.write_str("no DNS or IP name");
        }
        let all: Vec<String> = self
            .dns
            .iter()
            .cloned()
            .chain(self.ips.iter().map(ToString::to_string))
            .collect();
        f.write_str(&all.join(", "))
    }
}

/// The DNS and IP names a DER certificate carries in its subjectAltName extensions.
///
/// # Errors
/// [`RoleError::NotACertificate`]: the bytes are not an X.509 certificate.
pub fn names_of(der: &[u8]) -> Result<PeerNames, RoleError> {
    let cert = parse(der)?;
    let mut names = PeerNames::default();
    for name in alt_names(&cert) {
        match name {
            GeneralName::DNSName(dns) => names.dns.push((*dns).to_owned()),
            GeneralName::IPAddress(raw) => names.ips.extend(ip_of(raw)),
            _ => {}
        }
    }
    Ok(names)
}

/// The address an IP subjectAltName holds: four bytes (IPv4) or sixteen (IPv6).
fn ip_of(raw: &[u8]) -> Option<IpAddr> {
    match <[u8; 4]>::try_from(raw) {
        Ok(v4) => Some(IpAddr::from(v4)),
        Err(_) => <[u8; 16]>::try_from(raw).ok().map(IpAddr::from),
    }
}

/// The host of an address `host`, `host:port`, `[v6]` or `[v6]:port`: the port is cut
/// (digits that make a port, after the last colon, and only when no other colon is in the
/// host) and the brackets of an IPv6 literal come off. Anything else is returned whole, so
/// it can only match a name that is spelled exactly like it: an unbracketed IPv6 address
/// (its colons are not a port), a bracketed name that is no IPv6 literal, a path or
/// userinfo in the address.
#[must_use]
pub fn host_of(addr: &str) -> &str {
    if let Some(inner) = addr.strip_prefix('[') {
        return match inner.split_once(']') {
            Some((literal, rest))
                if literal.parse::<Ipv6Addr>().is_ok()
                    && (rest.is_empty() || rest.strip_prefix(':').is_some_and(is_port)) =>
            {
                literal
            }
            _ => addr,
        };
    }
    match addr.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && is_port(port) => host,
        _ => addr,
    }
}

fn is_port(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) && text.parse::<u16>().is_ok()
}
