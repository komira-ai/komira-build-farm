//! The admin grant (S5.2, S8): what `grant-admin` returns, signed with the gate's own
//! key, for `kbf-mac-session` on the Mac to verify before it creates an administrator.
//! This module defines the format; fleet-updates-security.md S5.2 ("The admin grant")
//! states it for the verifier.
//!
//! The grant text is exactly these five lines, each ending in one LF (no CR, no blank
//! line, no trailing whitespace), fields separated by one space:
//!
//! ```text
//! kbf-grant-v1
//! serial <serial>
//! lease <lease id>
//! issued <time>
//! not-after <time>
//! ```
//!
//! - `<serial>`: 1 to 32 ASCII letters and digits, the Mac's hardware serial.
//! - `<lease id>`: 1 to 128 of `[A-Za-z0-9._-]`.
//! - `<time>`: UTC, `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339, whole seconds, a `Z`); `not-after`
//!   is exactly `issued` plus 3600 seconds.
//!
//! The signature is Ed25519 (RFC 8032, pure: no prehash, no context) over the text's
//! bytes, final LF included, by the gate's grant key. The public key is the 32-byte
//! Ed25519 key. `grant-admin` answers with the text, the signature and the key in
//! standard base64, and with [`Grant::token`]: `<payload>.<signature>`, each base64url
//! without padding, the single string the server hands to `kbf-mac-session`
//! unchanged. The Mac holds the public key (installed by the MDM) and records used
//! lease ids, which makes a grant single-use; the gate issues at most one grant per
//! held request.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;

use crate::clock::{HOUR, format_rfc3339};

/// How long a grant may be used after it is issued.
pub const GRANT_LIFETIME: i64 = HOUR;

/// The gate's grant key.
pub struct GrantKey(SigningKey);

impl std::fmt::Debug for GrantKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("GrantKey").field(&self.public()).finish()
    }
}

/// A signed grant as `grant-admin` returns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Grant {
    /// The grant text above.
    pub grant: String,
    /// base64 ed25519 signature over `grant`.
    pub signature: String,
    /// base64 public key that verifies it.
    pub key: String,
    /// `<base64url(grant)>.<base64url(signature)>`, unpadded: what `kbf-mac-session`
    /// takes.
    pub token: String,
}

impl GrantKey {
    /// Reads a key file: the 32-byte ed25519 seed, base64.
    pub fn parse(text: &str) -> Option<Self> {
        let seed: [u8; 32] = STANDARD.decode(text.trim()).ok()?.try_into().ok()?;
        Some(Self(SigningKey::from_bytes(&seed)))
    }

    /// The public key, base64.
    pub fn public(&self) -> String {
        STANDARD.encode(self.0.verifying_key().to_bytes())
    }

    /// Signs a grant for `serial` and `lease`, issued at `now`.
    pub fn sign(&self, serial: &str, lease: &str, now: i64) -> Grant {
        let grant = format!(
            "kbf-grant-v1\nserial {serial}\nlease {lease}\nissued {}\nnot-after {}\n",
            format_rfc3339(now),
            format_rfc3339(now + GRANT_LIFETIME),
        );
        let signature = self.0.sign(grant.as_bytes()).to_bytes();
        Grant {
            token: format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(&grant),
                URL_SAFE_NO_PAD.encode(signature)
            ),
            grant,
            signature: STANDARD.encode(signature),
            key: self.public(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, VerifyingKey};

    #[test]
    fn a_grant_names_the_mac_and_lease_expires_in_an_hour_and_verifies() {
        let key = GrantKey::parse(&STANDARD.encode([5u8; 32])).unwrap();
        let grant = key.sign("C02X", "lease-1", 1_800_000_000);
        assert_eq!(
            grant.grant,
            "kbf-grant-v1\nserial C02X\nlease lease-1\nissued 2027-01-15T08:00:00Z\nnot-after 2027-01-15T09:00:00Z\n"
        );
        let public: [u8; 32] = STANDARD.decode(&grant.key).unwrap().try_into().unwrap();
        let sig: [u8; 64] = STANDARD
            .decode(&grant.signature)
            .unwrap()
            .try_into()
            .unwrap();
        VerifyingKey::from_bytes(&public)
            .unwrap()
            .verify_strict(grant.grant.as_bytes(), &Signature::from_bytes(&sig))
            .unwrap();
        assert!(format!("{key:?}").starts_with("GrantKey(\""));
        // The token carries the same bytes, base64url without padding.
        let (payload, token_sig) = grant.token.split_once('.').unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.decode(payload).unwrap(),
            grant.grant.as_bytes()
        );
        assert_eq!(URL_SAFE_NO_PAD.decode(token_sig).unwrap(), sig);
        assert!(!grant.token.contains(['=', '+', '/']));
        assert!(GrantKey::parse("short").is_none());
    }
}
