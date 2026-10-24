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
//! A certificate is also judged against a deny list (`kbf_map::DenyEntry`: a serial, a
//! public-key hash or a node id). [`identity_of`] reads what such an entry is compared
//! with, as a [`PeerIdentity`] (the names above, the serial and the SHA-256 of the
//! SubjectPublicKeyInfo, each in the entry's canonical spelling), and
//! [`PeerIdentity::denied_by`] says which entry of a list, if any, names it. A node id names
//! a certificate by one of its DNS names, up to ASCII case ([`PeerNames::has_dns_name`]).
//! It is the one reader: the frontend's worker listener and membership service and every
//! storage node use it, so they cannot disagree about which certificate an entry names.
//!
//! The certificate is not verified here: TLS has already verified it against the
//! listener's client CA. Which gRPC status a refusal is belongs to the caller (see
//! [`RoleError`]).

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

use kbf_map::{DenyEntry, normalize_serial};
use sha2::{Digest as _, Sha256};
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
    /// A host's updater: it dials the update coordinator, and calls the frontend's worker
    /// listener for its own node only. Its certificate also names the host's role
    /// (`urn:kbf:host-role:<name>`, read with [`uri_values_of`]).
    Updater,
}

impl PeerRole {
    /// Every role, in the order section 9 lists them, then the updater.
    pub const ALL: [Self; 4] = [Self::Frontend, Self::Storage, Self::Daemon, Self::Updater];

    /// The name a certificate spells after [`ROLE_URI_PREFIX`].
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Frontend => "frontend",
            Self::Storage => "storage",
            Self::Daemon => "daemon",
            Self::Updater => "updater",
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
    let names = uri_values_of(der, ROLE_URI_PREFIX)?;
    let [name] = names.as_slice() else {
        return Err(RoleError::Count(names.len()));
    };
    PeerRole::from_name(name).ok_or_else(|| RoleError::Unknown(name.clone()))
}

/// What follows `prefix` in each of the certificate's URI subjectAltNames that starts with
/// it, in the order the certificate lists them (byte for byte: the prefix is not matched
/// ignoring case). [`role_of`] reads the role this way; a listener for a certificate that
/// names more than a role (the update coordinator reads an updater's host role) reads the
/// other prefix the same way.
///
/// # Errors
/// [`RoleError::NotACertificate`]: the bytes are not an X.509 certificate.
pub fn uri_values_of(der: &[u8], prefix: &str) -> Result<Vec<String>, RoleError> {
    let cert = parse(der)?;
    Ok(alt_names(&cert)
        .filter_map(|name| match name {
            GeneralName::URI(uri) => uri.strip_prefix(prefix).map(str::to_owned),
            _ => None,
        })
        .collect())
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
    /// Every name as written: the DNS names, then the IP addresses.
    #[must_use]
    pub fn all(&self) -> Vec<String> {
        self.dns
            .iter()
            .cloned()
            .chain(self.ips.iter().map(ToString::to_string))
            .collect()
    }

    /// The DNS names as written, in the order the certificate lists them.
    #[must_use]
    pub fn dns(&self) -> &[String] {
        &self.dns
    }

    /// The IP addresses, in the order the certificate lists them.
    #[must_use]
    pub fn ips(&self) -> &[IpAddr] {
        &self.ips
    }

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

    /// Whether a `node` deny entry with the value `node` names these names: a DNS name equal
    /// to it up to ASCII case, which is how DNS compares names and how [`PeerNames::covers`]
    /// does. IP names are never matched, so an entry holding an IP address names nothing (a
    /// deny writer refuses one), and a wildcard name is matched by the entry that spells it,
    /// since a deny errs towards denying. A name with a trailing dot is another string and is
    /// not matched, nor is a `host:port`.
    #[must_use]
    pub fn has_dns_name(&self, node: &str) -> bool {
        self.dns.iter().any(|name| name.eq_ignore_ascii_case(node))
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
    Ok(names_in(&parse(der)?))
}

fn names_in(cert: &X509Certificate<'_>) -> PeerNames {
    let mut names = PeerNames::default();
    for name in alt_names(cert) {
        match name {
            GeneralName::DNSName(dns) => names.dns.push((*dns).to_owned()),
            GeneralName::IPAddress(raw) => names.ips.extend(ip_of(raw)),
            _ => {}
        }
    }
    names
}

/// What a deny list entry is compared with: the certificate's names, its serial and the
/// hash of its public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    names: PeerNames,
    /// Lower-case hex without leading zeros, as `DenyEntry::Serial` spells it.
    serial: String,
    /// SHA-256 of the DER SubjectPublicKeyInfo, lower-case hex.
    spki_sha256: String,
}

impl PeerIdentity {
    /// The identity of a caller whose certificate a TLS-terminating proxy verified and
    /// which names `node_id` and the IP addresses `ips` that certificate's leaf holds (the
    /// proxy's `x-kbf-node-ips`): that one DNS name and exactly those addresses, and no
    /// serial or key hash (the proxy does not forward them), so a `serial` or `spki-sha256`
    /// deny entry cannot name it. An IP address outside `ips` is covered by no name.
    #[must_use]
    pub fn proxied(node_id: &str, ips: &[IpAddr]) -> Self {
        Self {
            names: PeerNames {
                dns: vec![node_id.to_owned()],
                ips: ips.to_vec(),
            },
            serial: String::new(),
            spki_sha256: String::new(),
        }
    }

    /// The DNS and IP names.
    #[must_use]
    pub fn names(&self) -> &PeerNames {
        &self.names
    }

    /// The serial, as a deny entry spells it.
    #[must_use]
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// The SHA-256 of the SubjectPublicKeyInfo, as a deny entry spells it.
    #[must_use]
    pub fn spki_sha256(&self) -> &str {
        &self.spki_sha256
    }

    /// The first entry of `denied` that names this certificate: its serial, its key hash,
    /// or a node id that is one of its DNS names ([`PeerNames::has_dns_name`], up to ASCII
    /// case). IP names, the role URI and the common name never match a `node` entry.
    #[must_use]
    pub fn denied_by<'a>(&self, denied: &'a [DenyEntry]) -> Option<&'a DenyEntry> {
        denied.iter().find(|entry| match entry {
            DenyEntry::Serial(serial) => *serial == self.serial,
            DenyEntry::SpkiSha256(hash) => *hash == self.spki_sha256,
            DenyEntry::Node(node) => self.names.has_dns_name(node),
        })
    }
}

/// The most IP addresses one `x-kbf-node-ips` header holds.
pub const NODE_IPS_MAX: usize = 16;

/// The `x-kbf-node-ips` value for `ips`: canonical text (IPv6 compressed), sorted, each
/// address once, comma separated with no space; `None` for no address.
///
/// # Errors
/// [`IpsError::TooMany`]: more than [`NODE_IPS_MAX`] distinct addresses.
pub fn node_ips_value(ips: &[IpAddr]) -> Result<Option<String>, IpsError> {
    let mut ips = ips.to_vec();
    ips.sort_unstable();
    ips.dedup();
    if ips.len() > NODE_IPS_MAX {
        return Err(IpsError::TooMany(ips.len()));
    }
    if ips.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        ips.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
    ))
}

/// The addresses of an `x-kbf-node-ips` value, all or none: at most [`NODE_IPS_MAX`]
/// comma-separated addresses, each in the canonical text [`node_ips_value`] writes
/// (so no space, no empty element, no zone or port, no uncompressed IPv6), none twice.
/// An IPv4-mapped IPv6 address stays IPv6 and equals no IPv4 name.
///
/// # Errors
/// [`IpsError::Malformed`] for anything else.
pub fn parse_node_ips(value: &str) -> Result<Vec<IpAddr>, IpsError> {
    let mut ips = Vec::new();
    for part in value.split(',') {
        let ip: IpAddr = part.parse().map_err(|_| IpsError::Malformed)?;
        if ip.to_string() != part || ips.contains(&ip) || ips.len() == NODE_IPS_MAX {
            return Err(IpsError::Malformed);
        }
        ips.push(ip);
    }
    Ok(ips)
}

/// Why an IP list cannot be forwarded or read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IpsError {
    /// More distinct addresses than [`NODE_IPS_MAX`].
    #[error("the certificate names {0} IP addresses; at most {NODE_IPS_MAX} are forwarded")]
    TooMany(usize),
    /// Not a list of canonical addresses.
    #[error("not a list of distinct canonical IP addresses")]
    Malformed,
}

/// The identity a DER certificate gives a deny list.
///
/// # Errors
/// [`RoleError::NotACertificate`]: the bytes are not an X.509 certificate.
pub fn identity_of(der: &[u8]) -> Result<PeerIdentity, RoleError> {
    let cert = parse(der)?;
    Ok(PeerIdentity {
        names: names_in(&cert),
        serial: normalize_serial(&hex(cert.raw_serial())),
        spki_sha256: hex(&Sha256::digest(cert.public_key().raw)),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
