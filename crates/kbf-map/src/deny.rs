//! What the deny list can name, and the one parser of it.
//!
//! The same entries are written in a deny list file (one per line; see
//! `docs/design/worker-protocol.md`), checked by the server for every call, and carried in
//! the storage map, so one parser reads them all. An entry is a kind and a value:
//!
//! ```text
//! serial 0A:1B:2C          # a certificate serial, hex; colons, case and leading zeros ignored
//! spki-sha256 <64 hex>     # SHA-256 of the certificate's DER SubjectPublicKeyInfo
//! node mac-07              # a node id, whatever certificate it presents
//! ```
//!
//! [`DenyEntry`] holds the canonical spelling (a serial in lowercase hex without colons or
//! leading zeros, `0` for zero; a key hash in lowercase), so two spellings of one entry
//! are equal, and [`std::fmt::Display`] writes the form the map carries on the wire.

use std::fmt;
use std::str::FromStr;

/// One thing the deny list refuses.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DenyEntry {
    /// A certificate serial, canonical: lowercase hex, no leading zeros.
    Serial(String),
    /// The SHA-256 of a certificate's SubjectPublicKeyInfo: 64 lowercase hex digits.
    SpkiSha256(String),
    /// A node id.
    Node(String),
}

/// Why a string is not a deny entry; the text names what is wrong.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DenyEntryError {
    /// More or fewer than a kind and a value.
    #[error("an entry is one kind and one value")]
    Shape,
    /// A kind with no value.
    #[error("{0} needs a value")]
    NoValue(String),
    /// A serial that is empty or not hex (colons are allowed).
    #[error("a serial is hex digits, colons allowed: {0:?}")]
    Serial(String),
    /// A key hash that is not 64 hex digits.
    #[error("an spki-sha256 is 64 hex digits: {0:?}")]
    Spki(String),
    /// A kind this build does not know.
    #[error("unknown entry kind {0:?}")]
    Kind(String),
}

/// A serial in canonical form: lowercase, without leading zeros (`0` for zero). `hex` must
/// be hex digits; [`DenyEntry`] checks that, certificate code passes bytes it formatted.
#[must_use]
pub fn normalize_serial(hex: &str) -> String {
    let trimmed = hex.trim_start_matches('0').to_ascii_lowercase();
    if trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed
    }
}

impl DenyEntry {
    /// Parses one line of a deny list file: `None` for a blank line or a comment, else
    /// the entry (anything after a `#` is a comment).
    ///
    /// # Errors
    /// [`DenyEntryError`] for a line that is not an entry.
    pub fn parse_line(line: &str) -> Result<Option<Self>, DenyEntryError> {
        let line = line.split('#').next().unwrap_or_default();
        if line.trim().is_empty() {
            return Ok(None);
        }
        line.parse().map(Some)
    }
}

impl FromStr for DenyEntry {
    type Err = DenyEntryError;

    /// Parses `kind value` (any whitespace between), the form [`fmt::Display`] writes.
    fn from_str(s: &str) -> Result<Self, DenyEntryError> {
        let mut words = s.split_whitespace();
        let (Some(kind), value, None) = (words.next(), words.next(), words.next()) else {
            return Err(DenyEntryError::Shape);
        };
        let value = value.ok_or_else(|| DenyEntryError::NoValue(kind.to_owned()))?;
        match kind {
            "serial" => {
                let hex: String = value.chars().filter(|c| *c != ':').collect();
                if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(DenyEntryError::Serial(value.to_owned()));
                }
                Ok(Self::Serial(normalize_serial(&hex)))
            }
            "spki-sha256" => {
                if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(DenyEntryError::Spki(value.to_owned()));
                }
                Ok(Self::SpkiSha256(value.to_ascii_lowercase()))
            }
            "node" => Ok(Self::Node(value.to_owned())),
            _ => Err(DenyEntryError::Kind(kind.to_owned())),
        }
    }
}

impl fmt::Display for DenyEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serial(v) => write!(f, "serial {v}"),
            Self::SpkiSha256(v) => write!(f, "spki-sha256 {v}"),
            Self::Node(v) => write!(f, "node {v}"),
        }
    }
}
