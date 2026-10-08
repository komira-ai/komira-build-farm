//! Who is on a worker stream: the node a daemon's client certificate names, and the
//! deny list (issue #79).
//!
//! **The rule.** Under mutual TLS a daemon's client certificate must carry exactly one
//! DNS name in its subjectAltName extension, and that name must equal, byte for byte,
//! the `node_id` of every `Hello` the stream sends. The subject's common name is not
//! read. A certificate for one node therefore cannot open, or take over, another
//! node's session.
//!
//! **The deny list** is a file the server reads again at every check, so an edit takes
//! effect without a restart. Each line is blank, a `#` comment, or one entry:
//!
//! ```text
//! serial 0a:1b:2c            # the certificate serial, hex (colons and case ignored)
//! spki-sha256 <64 hex>       # SHA-256 of the certificate's DER SubjectPublicKeyInfo
//! node mac-07                # a node id, whatever certificate it comes with
//! ```
//!
//! A stream whose certificate or node is listed is refused PERMISSION_DENIED at its
//! first `Hello`, and ended at the next `Hello` or `Heartbeat` it sends after the entry
//! is added. A deny list that cannot be read or parsed refuses every check (fail
//! closed) until it is fixed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tonic::Status;
use x509_parser::extensions::{GeneralName, ParsedExtension};

/// What the server takes from a daemon's client certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerCert {
    /// Every DNS name in the certificate's subjectAltName.
    names: Vec<String>,
    /// The serial, lower-case hex without leading zeros.
    serial: String,
    /// SHA-256 of the DER SubjectPublicKeyInfo, lower-case hex.
    spki_sha256: String,
}

impl PeerCert {
    /// Reads a DER certificate (TLS has already verified it against the client CA).
    ///
    /// # Errors
    /// The bytes are not an X.509 certificate.
    pub fn from_der(der: &[u8]) -> Result<Self, String> {
        let (_, cert) = x509_parser::parse_x509_certificate(der)
            .map_err(|e| format!("the client certificate does not parse: {e}"))?;
        let names = cert
            .extensions()
            .iter()
            .filter_map(|e| match e.parsed_extension() {
                ParsedExtension::SubjectAlternativeName(san) => Some(&san.general_names),
                _ => None,
            })
            .flatten()
            .filter_map(|name| match name {
                GeneralName::DNSName(dns) => Some((*dns).to_owned()),
                _ => None,
            })
            .collect();
        Ok(Self {
            names,
            serial: normal_hex(&hex(cert.raw_serial())),
            spki_sha256: hex(&Sha256::digest(cert.public_key().raw)),
        })
    }

    /// The certificate's serial, as the deny list spells it.
    #[must_use]
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// The SHA-256 of its SubjectPublicKeyInfo, as the deny list spells it.
    #[must_use]
    pub fn spki_sha256(&self) -> &str {
        &self.spki_sha256
    }

    /// Whether this certificate names `node_id` under the rule in the module docs.
    ///
    /// # Errors
    /// PERMISSION_DENIED: the certificate has no DNS name, more than one, or another.
    pub fn names(&self, node_id: &str) -> Result<(), Status> {
        match self.names.as_slice() {
            [only] if only == node_id => Ok(()),
            [only] => Err(Status::permission_denied(format!(
                "the client certificate is for node {only:?}, not {node_id:?}"
            ))),
            names => Err(Status::permission_denied(format!(
                "the client certificate must name exactly one node as a DNS subjectAltName; \
                 it names {}",
                names.len()
            ))),
        }
    }
}

/// How the worker listener knows who is on a stream.
#[derive(Clone, Debug)]
pub enum Peers {
    /// Plain text: no certificate, so no node is bound to one. For tests and trials on
    /// one machine only.
    Unauthenticated,
    /// Mutual TLS: every stream's certificate must name its node, and is checked
    /// against the deny list if there is one.
    Certified {
        /// Read again at every check.
        deny_list: Option<DenyList>,
    },
}

impl Peers {
    /// The certificate of a new stream, from the chain TLS verified (`leaf` is its
    /// first certificate; `None` in plain text).
    ///
    /// # Errors
    /// UNAUTHENTICATED: under mutual TLS, there is no certificate or it does not parse.
    pub fn peer(&self, leaf: Option<&[u8]>) -> Result<Option<PeerCert>, Status> {
        match (self, leaf) {
            (Self::Unauthenticated, _) => Ok(None),
            (Self::Certified { .. }, None) => Err(Status::unauthenticated("no client certificate")),
            (Self::Certified { .. }, Some(der)) => PeerCert::from_der(der)
                .map(Some)
                .map_err(Status::unauthenticated),
        }
    }

    /// Whether the stream whose certificate is `peer` may speak as `node_id` now: the
    /// certificate names it, and neither is on the deny list.
    ///
    /// # Errors
    /// PERMISSION_DENIED: the certificate names another node, or is (or the node is)
    /// denied. UNAVAILABLE: the deny list cannot be read or parsed.
    pub async fn admit(&self, peer: Option<&PeerCert>, node_id: &str) -> Result<(), Status> {
        if let Some(peer) = peer {
            peer.names(node_id)?;
        }
        let Self::Certified {
            deny_list: Some(deny_list),
        } = self
        else {
            return Ok(());
        };
        let denied = deny_list.current().await.map_err(|e| {
            tracing::error!(error = %e, "the deny list refuses every stream until it is fixed");
            Status::unavailable(format!("the server's deny list is unusable: {e}"))
        })?;
        match denied.refuses(peer, node_id) {
            Some(why) => Err(Status::permission_denied(format!("denied: {why}"))),
            None => Ok(()),
        }
    }
}

/// The deny list file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenyList {
    path: PathBuf,
}

/// Why the deny list cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum DenyListError {
    /// The file cannot be read.
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A line is not an entry.
    #[error("{path}:{line}: {why}")]
    Parse {
        path: PathBuf,
        line: usize,
        why: String,
    },
}

impl DenyList {
    /// The deny list at `path`, read once now so that a bad file stops the server at
    /// start rather than at the first daemon.
    ///
    /// # Errors
    /// The file cannot be read or parsed.
    pub fn open(path: &Path) -> Result<Self, DenyListError> {
        let list = Self {
            path: path.to_owned(),
        };
        let text = std::fs::read_to_string(path).map_err(|source| list.unreadable(source))?;
        list.parse(&text)?;
        Ok(list)
    }

    /// What the file denies now.
    ///
    /// # Errors
    /// The file cannot be read or parsed.
    pub async fn current(&self) -> Result<Denied, DenyListError> {
        let text = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|source| self.unreadable(source))?;
        self.parse(&text)
    }

    fn unreadable(&self, source: std::io::Error) -> DenyListError {
        DenyListError::Read {
            path: self.path.clone(),
            source,
        }
    }

    fn parse(&self, text: &str) -> Result<Denied, DenyListError> {
        Denied::parse(text).map_err(|(line, why)| DenyListError::Parse {
            path: self.path.clone(),
            line,
            why,
        })
    }
}

/// The entries of a deny list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Denied {
    serials: BTreeSet<String>,
    spkis: BTreeSet<String>,
    nodes: BTreeSet<String>,
}

impl Denied {
    /// Parses a deny list (format in the module docs).
    ///
    /// # Errors
    /// The 1-based number of the first bad line, and what is wrong with it.
    pub fn parse(text: &str) -> Result<Self, (usize, String)> {
        let mut denied = Self::default();
        for (at, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or_default();
            let mut words = line.split_whitespace();
            let (Some(kind), value, None) = (words.next(), words.next(), words.next()) else {
                if line.trim().is_empty() {
                    continue;
                }
                return Err((at + 1, "an entry is one kind and one value".to_owned()));
            };
            let value = value.ok_or_else(|| (at + 1, format!("{kind} needs a value")))?;
            let bad = |what: &str| (at + 1, format!("{what}: {value:?}"));
            match kind {
                "serial" => {
                    let hex: String = value.chars().filter(|c| *c != ':').collect();
                    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(bad("a serial is hex digits, colons allowed"));
                    }
                    denied.serials.insert(normal_hex(&hex));
                }
                "spki-sha256" => {
                    if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(bad("an spki-sha256 is 64 hex digits"));
                    }
                    denied.spkis.insert(value.to_ascii_lowercase());
                }
                "node" => {
                    denied.nodes.insert(value.to_owned());
                }
                _ => return Err((at + 1, format!("unknown entry kind {kind:?}"))),
            }
        }
        Ok(denied)
    }

    /// Why a stream with certificate `peer` speaking as `node_id` is denied, if it is.
    #[must_use]
    pub fn refuses(&self, peer: Option<&PeerCert>, node_id: &str) -> Option<String> {
        if self.nodes.contains(node_id) {
            return Some(format!("node {node_id}"));
        }
        let peer = peer?;
        if self.serials.contains(&peer.serial) {
            return Some(format!("certificate serial {}", peer.serial));
        }
        self.spkis
            .contains(&peer.spki_sha256)
            .then(|| format!("public key {}", peer.spki_sha256))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lower case, without leading zeros (a serial of zero is `0`).
fn normal_hex(hex: &str) -> String {
    let trimmed = hex.trim_start_matches('0').to_ascii_lowercase();
    if trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed
    }
}
