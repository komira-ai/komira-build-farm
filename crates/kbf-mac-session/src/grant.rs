//! The MDM gate's admin grant: the only way a lease user becomes an administrator
//! (docs/design/fleet-updates-security.md S5.2, S8).
//!
//! A grant is `<payload>.<signature>`, both base64url without padding. The signature
//! is Ed25519 over the payload's exact bytes, by the gate's grant key; Macs hold the
//! public half (one key, or two while a rotation overlaps). The payload is four lines,
//! each ending in a newline, in this order and nothing else:
//!
//! ```text
//! kbf-mac-admin-grant v1
//! serial <the Mac's serial number>
//! lease <term>.<seq>
//! not-after <unix seconds>
//! ```
//!
//! The helper accepts a grant only if the signature verifies (strictly: no
//! non-canonical encodings) under a key it holds, the serial is this Mac's, the lease
//! is the one being created, and `not-after` is in the future but no further ahead than
//! [`MAX_LIFETIME`]. Single use comes from the ledger: a lease id is created once.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use kbf_types::LeaseId;

/// The first line of every grant payload.
pub const MAGIC: &str = "kbf-mac-admin-grant v1";

/// How far ahead a grant's `not-after` may lie: the gate issues grants for one hour
/// (S5.2), plus five minutes for clocks that disagree. A later one is refused, so a
/// gate that issues long-lived grants is noticed rather than trusted.
pub const MAX_LIFETIME: Duration = Duration::from_secs(65 * 60);

/// The gate's public grant keys this Mac accepts.
#[derive(Clone, Debug)]
pub struct GrantKeys(Vec<VerifyingKey>);

impl GrantKeys {
    /// Parses a key file: one 32-byte Ed25519 public key per line in hex; blank lines
    /// and lines starting with `#` are skipped.
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
            let bytes: [u8; 32] = hex::decode(line)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| format!("line {}: not a 32-byte hex key", number + 1))?;
            let key = VerifyingKey::from_bytes(&bytes)
                .map_err(|_| format!("line {}: not an Ed25519 public key", number + 1))?;
            keys.push(key);
        }
        if keys.is_empty() {
            return Err("the grant key file names no key".to_owned());
        }
        Ok(Self(keys))
    }

    /// Whether `signature` over `payload` verifies under one of the keys.
    fn verifies(&self, payload: &[u8], signature: &Signature) -> bool {
        self.0
            .iter()
            .any(|key| key.verify_strict(payload, signature).is_ok())
    }
}

/// What a grant must match: this Mac and this lease, at this time.
#[derive(Clone, Copy, Debug)]
pub struct Expect<'a> {
    pub serial: &'a str,
    pub lease: LeaseId,
    pub now: SystemTime,
}

/// Checks `grant` against `keys` and `expect`.
///
/// # Errors
/// Why the grant is refused.
pub fn verify(grant: &str, keys: &GrantKeys, expect: Expect<'_>) -> Result<(), String> {
    let (payload, signature) = grant
        .split_once('.')
        .ok_or("the grant is not <payload>.<signature>")?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "the grant's payload is not base64url")?;
    let signature: [u8; 64] = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("the grant's signature is not 64 bytes of base64url")?;
    if !keys.verifies(&payload, &Signature::from_bytes(&signature)) {
        return Err("the grant's signature does not verify under the gate's keys".to_owned());
    }
    let fields = Fields::parse(&payload)?;
    if fields.serial != expect.serial {
        return Err(format!(
            "the grant names serial {:?}, not this Mac's",
            fields.serial
        ));
    }
    if fields.lease != expect.lease.to_string() {
        return Err(format!(
            "the grant names lease {}, not {}",
            fields.lease, expect.lease
        ));
    }
    let now = expect
        .now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970")?;
    let not_after = Duration::from_secs(fields.not_after);
    if not_after <= now {
        return Err("the grant has expired".to_owned());
    }
    if not_after - now > MAX_LIFETIME {
        return Err(format!(
            "the grant is valid for longer than {} minutes",
            MAX_LIFETIME.as_secs() / 60
        ));
    }
    Ok(())
}

/// A payload's fields, read after its signature verified.
struct Fields<'a> {
    serial: &'a str,
    lease: &'a str,
    not_after: u64,
}

impl<'a> Fields<'a> {
    fn parse(payload: &'a [u8]) -> Result<Self, String> {
        let malformed = || "the grant's payload is malformed".to_owned();
        let text = std::str::from_utf8(payload).map_err(|_| malformed())?;
        let body = text.strip_suffix('\n').ok_or_else(malformed)?;
        let mut lines = body.split('\n');
        let mut field = |key: &str| {
            lines
                .next()
                .and_then(|line| line.strip_prefix(key))
                .and_then(|line| line.strip_prefix(' '))
                .filter(|value| !value.is_empty())
                .ok_or_else(malformed)
        };
        if field("kbf-mac-admin-grant")? != "v1" {
            return Err(malformed());
        }
        let serial = field("serial")?;
        let lease = field("lease")?;
        let not_after = field("not-after")?.parse().map_err(|_| malformed())?;
        if lines.next().is_some() {
            return Err(malformed());
        }
        Ok(Self {
            serial,
            lease,
            not_after,
        })
    }
}

/// Test grants, signed with keys made from fixed seeds.
#[cfg(test)]
pub(crate) mod testing {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::{Signer as _, SigningKey};

    /// The signing key of seed `seed`.
    pub(crate) fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// The key file line of seed `seed`'s public key.
    pub(crate) fn key_line(seed: u8) -> String {
        hex::encode(key(seed).verifying_key().to_bytes())
    }

    /// `payload` signed by seed `seed`'s key, as a grant.
    pub(crate) fn sign(seed: u8, payload: &str) -> String {
        sign_bytes(seed, payload.as_bytes())
    }

    /// `payload` signed by seed `seed`'s key, as a grant.
    pub(crate) fn sign_bytes(seed: u8, payload: &[u8]) -> String {
        let signature = key(seed).sign(payload);
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }

    /// A well-formed payload.
    pub(crate) fn payload(serial: &str, lease: &str, not_after: u64) -> String {
        format!("kbf-mac-admin-grant v1\nserial {serial}\nlease {lease}\nnot-after {not_after}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{key_line, payload, sign};
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn keys() -> GrantKeys {
        GrantKeys::parse(&format!("# the gate\n{}\n\n", key_line(1))).unwrap()
    }

    fn expect() -> Expect<'static> {
        Expect {
            serial: "C02XYZ",
            lease: LeaseId::new(4, 2),
            now: UNIX_EPOCH + Duration::from_secs(NOW),
        }
    }

    fn check(grant: &str) -> Result<(), String> {
        verify(grant, &keys(), expect())
    }

    #[test]
    fn a_grant_for_this_mac_and_lease_is_accepted() {
        assert_eq!(
            check(&sign(1, &payload("C02XYZ", "4.2", NOW + 3600))),
            Ok(())
        );
    }

    /// Catches: trusting the payload without checking the signature, or accepting a
    /// key the Mac does not hold.
    #[test]
    fn a_grant_signed_by_another_key_is_refused() {
        let error = check(&sign(2, &payload("C02XYZ", "4.2", NOW + 3600))).unwrap_err();
        assert!(error.contains("does not verify"), "{error}");
    }

    /// Catches: verifying the signature over something other than the payload's bytes
    /// (a payload edited after signing still passing).
    #[test]
    fn an_edited_payload_is_refused() {
        let good = sign(1, &payload("C02XYZ", "4.2", NOW + 3600));
        let (_, signature) = good.split_once('.').unwrap();
        let edited = URL_SAFE_NO_PAD.encode(payload("C02XYZ", "4.3", NOW + 3600));
        let error = check(&format!("{edited}.{signature}")).unwrap_err();
        assert!(error.contains("does not verify"), "{error}");
    }

    /// Catches: dropping the serial check, so one grant serves every Mac.
    #[test]
    fn a_grant_for_another_mac_is_refused() {
        let error = check(&sign(1, &payload("OTHER", "4.2", NOW + 3600))).unwrap_err();
        assert!(error.contains("serial"), "{error}");
    }

    /// Catches: dropping the lease check, so one grant serves every lease.
    #[test]
    fn a_grant_for_another_lease_is_refused() {
        let error = check(&sign(1, &payload("C02XYZ", "4.3", NOW + 3600))).unwrap_err();
        assert!(error.contains("lease 4.3"), "{error}");
    }

    /// Catches: dropping the expiry check, or `<` for `<=` at the instant of expiry.
    #[test]
    fn an_expired_grant_is_refused() {
        for not_after in [NOW - 1, NOW] {
            let error = check(&sign(1, &payload("C02XYZ", "4.2", not_after))).unwrap_err();
            assert!(error.contains("expired"), "{error}");
        }
        assert_eq!(check(&sign(1, &payload("C02XYZ", "4.2", NOW + 1))), Ok(()));
    }

    /// Catches: accepting a grant valid for days, which a gate bug or a stolen grant
    /// key would issue.
    #[test]
    fn a_grant_valid_too_long_is_refused() {
        let limit = NOW + MAX_LIFETIME.as_secs();
        assert_eq!(check(&sign(1, &payload("C02XYZ", "4.2", limit))), Ok(()));
        let error = check(&sign(1, &payload("C02XYZ", "4.2", limit + 1))).unwrap_err();
        assert!(error.contains("longer than 65 minutes"), "{error}");
    }

    #[test]
    fn a_clock_before_1970_refuses_every_grant() {
        let before = Expect {
            now: UNIX_EPOCH - Duration::from_secs(1),
            ..expect()
        };
        let grant = sign(1, &payload("C02XYZ", "4.2", NOW));
        assert!(
            verify(&grant, &keys(), before)
                .unwrap_err()
                .contains("1970")
        );
    }

    /// Catches: a lax parser, under which a signed payload with an extra or missing
    /// line or a field out of order could mean something the gate did not sign.
    #[test]
    fn a_signed_but_malformed_payload_is_refused() {
        for bad in [
            "kbf-mac-admin-grant v2\nserial C02XYZ\nlease 4.2\nnot-after 1800003600\n",
            "kbf-mac-admin-grant v1\nlease 4.2\nserial C02XYZ\nnot-after 1800003600\n",
            "kbf-mac-admin-grant v1\nserial C02XYZ\nlease 4.2\nnot-after 1800003600",
            "kbf-mac-admin-grant v1\nserial C02XYZ\nlease 4.2\nnot-after 1800003600\nadmin yes\n",
            "kbf-mac-admin-grant v1\nserial C02XYZ\nlease 4.2\n",
            "kbf-mac-admin-grant v1\nserial \nlease 4.2\nnot-after 1800003600\n",
            "kbf-mac-admin-grant v1\nserialC02XYZ\nlease 4.2\nnot-after 1800003600\n",
            "kbf-mac-admin-grant v1\nserial C02XYZ\nlease 4.2\nnot-after soon\n",
        ] {
            let error = check(&sign(1, bad)).unwrap_err();
            assert!(error.contains("malformed"), "{bad:?}: {error}");
        }
        let error = check(&super::testing::sign_bytes(1, b"\xff\n")).unwrap_err();
        assert!(error.contains("malformed"), "{error}");
    }

    #[test]
    fn a_grant_that_is_not_two_base64url_parts_is_refused() {
        let good = sign(1, &payload("C02XYZ", "4.2", NOW + 3600));
        let (body, signature) = good.split_once('.').unwrap();
        assert!(
            check("no-dot")
                .unwrap_err()
                .contains("<payload>.<signature>")
        );
        assert!(
            check(&format!("{body}!.{signature}"))
                .unwrap_err()
                .contains("payload")
        );
        assert!(
            check(&format!("{body}.{}", &signature[..10]))
                .unwrap_err()
                .contains("64 bytes")
        );
        assert!(
            check(&format!("{body}.{signature}="))
                .unwrap_err()
                .contains("64 bytes")
        );
    }

    /// Catches: a key file whose bad line is skipped (a typo then leaving the Mac with
    /// fewer keys than the operator thinks), or an empty file accepted.
    #[test]
    fn key_files_are_strict() {
        let two = GrantKeys::parse(&format!("{}\n{}\n", key_line(1), key_line(2))).unwrap();
        assert_eq!(two.0.len(), 2);
        let grant = sign(2, &payload("C02XYZ", "4.2", NOW + 60));
        assert_eq!(verify(&grant, &two, expect()), Ok(()));
        assert!(GrantKeys::parse("# none\n").unwrap_err().contains("no key"));
        assert!(GrantKeys::parse("abcd\n").unwrap_err().contains("line 1"));
        let not_a_point = format!("{}\n{}", key_line(1), "02".repeat(32));
        assert!(
            GrantKeys::parse(&not_a_point)
                .unwrap_err()
                .contains("line 2: not an Ed25519")
        );
    }
}
