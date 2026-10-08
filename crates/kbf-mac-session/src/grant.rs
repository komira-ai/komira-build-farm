//! The MDM gate's admin grant: the only way a lease user becomes an administrator
//! (docs/design/fleet-updates-security.md S5.2 "The admin grant", S8).
//!
//! The gate defines the format (`kbf-mdm`'s `grant` module); this module accepts
//! exactly that and nothing else. The grant text is exactly five lines, each ending in
//! one LF, each field separated from its value by one space:
//!
//! ```text
//! kbf-grant-v1
//! serial <serial>
//! lease <lease id>
//! issued <time>
//! not-after <time>
//! ```
//!
//! `<time>` is UTC as `YYYY-MM-DDTHH:MM:SSZ`, and `not-after` is exactly `issued` plus
//! 3600 seconds. The signature is Ed25519 over the text's bytes by the gate's grant
//! key. The helper receives the gate's `token`, which the server passes unchanged:
//! `<payload>.<signature>`, each base64url without padding, the payload being the
//! text. The Mac's grant keys are the gate's public keys (the `key` of a
//! `grant-admin` answer), standard base64, one per line.
//!
//! A grant is accepted only if the token has exactly one `.` and both parts decode;
//! the signature verifies strictly (no non-canonical encodings, no small-order keys
//! or `R`) under one of the keys this Mac holds; the text parses as exactly the five
//! lines above; `serial` is this Mac's; `lease` is the lease being created;
//! `not-after` is exactly `issued` plus [`LIFETIME`]; this Mac's clock is not past
//! `not-after`; and `not-after` is at most [`MAX_AHEAD`] ahead of that clock. Single
//! use comes from the ledger: a lease id is created once.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};

/// The first line of every grant.
pub const MAGIC: &str = "kbf-grant-v1";

/// How long the gate makes every grant valid: `not-after` is exactly `issued` plus
/// this.
pub const LIFETIME: Duration = Duration::from_secs(60 * 60);

/// How far ahead of this Mac's clock `not-after` may lie: the hour, plus five minutes
/// for clocks that disagree.
pub const MAX_AHEAD: Duration = Duration::from_secs(65 * 60);

/// The one form of a grant's times.
const TIME: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

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

/// Checks the grant token `token` against `keys` and `expect`.
///
/// # Errors
/// Why the grant is refused.
pub fn verify(token: &str, keys: &GrantKeys, expect: Expect<'_>) -> Result<(), String> {
    let (payload, signature) = token
        .split_once('.')
        .ok_or("the grant is not <payload>.<signature>")?;
    let text = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "the grant's payload is not base64url")?;
    let signature: [u8; 64] = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("the grant's signature is not 64 bytes of base64url")?;
    if !keys.verifies(&text, &Signature::from_bytes(&signature)) {
        return Err("the grant's signature does not verify under the gate's keys".to_owned());
    }
    let fields = Fields::parse(&text)?;
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
    if fields.not_after - fields.issued != secs(LIFETIME) {
        return Err(format!(
            "the grant's not-after is not {} minutes after its issued",
            LIFETIME.as_secs() / 60
        ));
    }
    // A `SystemTime` holds at most `i64::MAX` seconds, so the cast keeps the value.
    let now = expect
        .now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970")?
        .as_secs()
        .cast_signed();
    if now > fields.not_after {
        return Err("the grant has expired".to_owned());
    }
    if fields.not_after - now > secs(MAX_AHEAD) {
        return Err(format!(
            "the grant's not-after is more than {} minutes ahead of this Mac's clock",
            MAX_AHEAD.as_secs() / 60
        ));
    }
    Ok(())
}

fn secs(duration: Duration) -> i64 {
    // Both durations are constants of about an hour.
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
    fn parse(text: &'a [u8]) -> Result<Self, String> {
        let malformed = || "the grant is malformed".to_owned();
        let text = std::str::from_utf8(text).map_err(|_| malformed())?;
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
        let issued = time(field("issued")?).ok_or_else(malformed)?;
        let not_after = time(field("not-after")?).ok_or_else(malformed)?;
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

/// Seconds since the epoch of a time in the one form, `YYYY-MM-DDTHH:MM:SSZ`; `None`
/// for any other spelling (an offset, fractions, a sign, another width).
fn time(text: &str) -> Option<i64> {
    let at = PrimitiveDateTime::parse(text, TIME).ok()?.assume_utc();
    (at.format(TIME).ok()? == text).then(|| OffsetDateTime::unix_timestamp(at))
}

/// Test grants, signed with keys made from fixed seeds, in the gate's format.
#[cfg(test)]
pub(crate) mod testing {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use ed25519_dalek::{Signer as _, SigningKey};
    use time::OffsetDateTime;

    /// The signing key of seed `seed`.
    pub(crate) fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// The key file line of seed `seed`'s public key.
    pub(crate) fn key_line(seed: u8) -> String {
        STANDARD.encode(key(seed).verifying_key().to_bytes())
    }

    /// `text` signed by seed `seed`'s key, as a grant token.
    pub(crate) fn sign(seed: u8, text: &str) -> String {
        token(text.as_bytes(), &key(seed).sign(text.as_bytes()).to_bytes())
    }

    /// A token of any payload and signature bytes.
    pub(crate) fn token(payload: &[u8], signature: &[u8]) -> String {
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(signature)
        )
    }

    /// A time as the gate writes one.
    pub(crate) fn utc(secs: i64) -> String {
        OffsetDateTime::from_unix_timestamp(secs)
            .unwrap()
            .format(super::TIME)
            .unwrap()
    }

    /// The text of a grant the gate issues at `issued`.
    pub(crate) fn text(serial: &str, lease: &str, issued: i64) -> String {
        text_until(serial, lease, issued, issued + 3600)
    }

    /// A grant text with any `not-after`.
    pub(crate) fn text_until(serial: &str, lease: &str, issued: i64, not_after: i64) -> String {
        format!(
            "kbf-grant-v1\nserial {serial}\nlease {lease}\nissued {}\nnot-after {}\n",
            utc(issued),
            utc(not_after)
        )
    }
}

#[cfg(test)]
mod tests;
