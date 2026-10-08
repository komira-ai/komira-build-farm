//! The MDM gate's admin grant: the only way a lease user becomes an administrator
//! (docs/design/fleet-updates-security.md S5.2, S8).
//!
//! The format is the gate's own (`kbf-mdm`'s `grant` module): `grant-admin` answers
//! `{"grant", "signature", "key", "erase_at"}`, where `grant` is this text, every line
//! ending in a newline, in this order and nothing else:
//!
//! ```text
//! kbf-grant-v1
//! serial <the Mac's serial number>
//! lease <lease id>
//! issued <RFC 3339 time>
//! not-after <issued + 1 hour>
//! ```
//!
//! and `signature` is Ed25519 over those exact bytes by the gate's grant key, in
//! standard base64. The daemon forwards that answer as it is; the helper reads
//! `grant` and `signature` and ignores the rest: `key` names the key that signed, and
//! the helper trusts only the public keys installed on the Mac (one, or two while a
//! rotation overlaps), in the same base64 form.
//!
//! The helper accepts a grant only if the signature verifies strictly (no small-order
//! key or `R`, no non-canonical encoding) under a key it holds, the serial is this
//! Mac's, the lease is the one being created, `not-after` lies after now and at most
//! [`LIFETIME`] after `issued`, and `issued` is no later than [`CLOCK_SKEW`] ahead of
//! now. Single use comes from the ledger: a lease id is created once.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The first line of every grant.
pub const MAGIC: &str = "kbf-grant-v1";

/// How long the gate makes a grant valid (its `GRANT_LIFETIME`): a grant whose
/// `not-after` lies further after its `issued` is refused, so a gate that issues
/// long-lived grants is noticed rather than trusted.
pub const LIFETIME: Duration = Duration::from_secs(60 * 60);

/// How far ahead of this Mac's clock a grant's `issued` may lie: the gate's and the
/// Mac's clocks may disagree by this much.
pub const CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);

/// A grant as the helper receives it: the gate's `grant-admin` answer, of which only
/// these two fields are read (unknown fields, `key` and `erase_at` among them, are
/// ignored).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminGrant {
    /// The grant text.
    pub grant: String,
    /// The Ed25519 signature over `grant`, standard base64.
    pub signature: String,
}

/// The gate's public grant keys this Mac accepts.
#[derive(Clone, Debug)]
pub struct GrantKeys(Vec<VerifyingKey>);

impl GrantKeys {
    /// Parses a key file: one 32-byte Ed25519 public key per line in standard base64
    /// (the `key` of a `grant-admin` answer); blank lines and lines starting with `#`
    /// are skipped. A small-order ("weak") key is refused: anyone can sign for it.
    ///
    /// # Errors
    /// A line is not a valid key, or the file names none.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut keys = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bytes: [u8; 32] = STANDARD
                .decode(line)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| format!("line {}: not a 32-byte base64 key", number + 1))?;
            let key = VerifyingKey::from_bytes(&bytes)
                .ok()
                .filter(|key| !key.is_weak())
                .ok_or_else(|| format!("line {}: not a usable Ed25519 public key", number + 1))?;
            keys.push(key);
        }
        if keys.is_empty() {
            return Err("the grant key file names no key".to_owned());
        }
        Ok(Self(keys))
    }

    /// Whether `signature` over `text` verifies under one of the keys.
    fn verifies(&self, text: &[u8], signature: &Signature) -> bool {
        self.0
            .iter()
            .any(|key| key.verify_strict(text, signature).is_ok())
    }
}

/// What a grant must match: this Mac and this lease, at this time.
#[derive(Clone, Copy, Debug)]
pub struct Expect<'a> {
    pub serial: &'a str,
    pub lease: &'a str,
    pub now: SystemTime,
}

/// Checks `grant` against `keys` and `expect`.
///
/// # Errors
/// Why the grant is refused.
pub fn verify(grant: &AdminGrant, keys: &GrantKeys, expect: Expect<'_>) -> Result<(), String> {
    let signature: [u8; 64] = STANDARD
        .decode(&grant.signature)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("the grant's signature is not 64 bytes of base64")?;
    if !keys.verifies(grant.grant.as_bytes(), &Signature::from_bytes(&signature)) {
        return Err("the grant's signature does not verify under the gate's keys".to_owned());
    }
    let fields = Fields::parse(&grant.grant)?;
    if fields.serial != expect.serial {
        return Err(format!(
            "the grant names serial {:?}, not this Mac's",
            fields.serial
        ));
    }
    if fields.lease != expect.lease {
        return Err(format!(
            "the grant names lease {}, not {}",
            fields.lease, expect.lease
        ));
    }
    // A `SystemTime` holds at most `i64::MAX` seconds, so the cast keeps the value.
    let now = expect
        .now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970")?
        .as_secs()
        .cast_signed();
    if fields.not_after <= now {
        return Err("the grant has expired".to_owned());
    }
    if fields.not_after - fields.issued > secs(LIFETIME) {
        return Err(format!(
            "the grant is valid for longer than {} minutes",
            LIFETIME.as_secs() / 60
        ));
    }
    if fields.issued > now + secs(CLOCK_SKEW) {
        return Err("the grant was issued in the future".to_owned());
    }
    Ok(())
}

fn secs(duration: Duration) -> i64 {
    // Both durations are constants of an hour or less.
    duration.as_secs().cast_signed()
}

/// A grant's fields, read after its signature verified.
struct Fields<'a> {
    serial: &'a str,
    lease: &'a str,
    issued: i64,
    not_after: i64,
}

impl<'a> Fields<'a> {
    fn parse(text: &'a str) -> Result<Self, String> {
        let malformed = || "the grant is malformed".to_owned();
        let body = text.strip_suffix('\n').ok_or_else(malformed)?;
        let mut lines = body.split('\n');
        if lines.next() != Some(MAGIC) {
            return Err(malformed());
        }
        let mut field = |key: &str| {
            lines
                .next()
                .and_then(|line| line.strip_prefix(key))
                .and_then(|line| line.strip_prefix(' '))
                .filter(|value| !value.is_empty())
                .ok_or_else(malformed)
        };
        let serial = field("serial")?;
        let lease = field("lease")?;
        let time = |text: &str| {
            OffsetDateTime::parse(text, &Rfc3339)
                .map(OffsetDateTime::unix_timestamp)
                .map_err(|_| malformed())
        };
        let issued = time(field("issued")?)?;
        let not_after = time(field("not-after")?)?;
        if lines.next().is_some() {
            return Err(malformed());
        }
        Ok(Self {
            serial,
            lease,
            issued,
            not_after,
        })
    }
}

/// Test grants, signed with keys made from fixed seeds, in the gate's format.
#[cfg(test)]
pub(crate) mod testing {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer as _, SigningKey};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    use super::AdminGrant;

    /// The signing key of seed `seed`.
    pub(crate) fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// The key file line of seed `seed`'s public key.
    pub(crate) fn key_line(seed: u8) -> String {
        STANDARD.encode(key(seed).verifying_key().to_bytes())
    }

    /// `text` signed by seed `seed`'s key, as a grant.
    pub(crate) fn sign(seed: u8, text: &str) -> AdminGrant {
        AdminGrant {
            grant: text.to_owned(),
            signature: STANDARD.encode(key(seed).sign(text.as_bytes()).to_bytes()),
        }
    }

    /// An RFC 3339 UTC time, as the gate writes one.
    pub(crate) fn rfc3339(secs: i64) -> String {
        OffsetDateTime::from_unix_timestamp(secs)
            .unwrap()
            .format(&Rfc3339)
            .unwrap()
    }

    /// The text of a grant the gate issues at `issued` (valid for an hour).
    pub(crate) fn text(serial: &str, lease: &str, issued: i64) -> String {
        text_until(serial, lease, issued, issued + 3600)
    }

    /// A grant text with any `not-after`.
    pub(crate) fn text_until(serial: &str, lease: &str, issued: i64, not_after: i64) -> String {
        format!(
            "kbf-grant-v1\nserial {serial}\nlease {lease}\nissued {}\nnot-after {}\n",
            rfc3339(issued),
            rfc3339(not_after)
        )
    }
}

#[cfg(test)]
mod tests;
