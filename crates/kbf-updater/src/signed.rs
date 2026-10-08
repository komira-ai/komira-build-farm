//! Signed documents (S2 of `docs/design/fleet-updates-security.md`): the envelope every
//! signed document travels in, the root-signed key statement, and which role a signing
//! key holds.
//!
//! An envelope carries a payload, the signer's ed25519 public key and a signature over a
//! context string followed by the payload. The context names the kind of document, so a
//! signature made over a key statement never verifies as a software set, even under a key
//! that could sign both.
//!
//! The node pins one key: the offline root key. The root key signs a key statement that
//! names the current component and platform keys, with a serial and an expiry. The
//! updater verifies under the newest valid statement it has seen and never goes back to
//! an older one ([`newest_statement`]).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use crate::Refusal;

/// The context a key statement's signature covers before its payload.
pub const STATEMENT_CONTEXT: &[u8] = b"kbf key statement v1\n";
/// The context a software set's signature covers before its payload.
pub const SET_CONTEXT: &[u8] = b"kbf software set v1\n";

/// An ed25519 public key.
pub type PublicKey = [u8; 32];

/// A signed document as it travels: all three fields are text so the envelope is JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// The document, base64 (standard alphabet, padded).
    pub payload: String,
    /// The signer's ed25519 public key, hex.
    pub key: String,
    /// The ed25519 signature over the context and the payload bytes, base64.
    pub signature: String,
}

/// An envelope whose signature verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    /// The key that signed it.
    pub signer: PublicKey,
    /// The payload bytes the signature covers.
    pub payload: Vec<u8>,
}

/// Parses a hex ed25519 public key (either case).
///
/// # Errors
/// Not hex, or not 32 bytes.
pub fn parse_key(hex_key: &str) -> Result<PublicKey, Refusal> {
    let bytes =
        hex::decode(hex_key).map_err(|e| Refusal::Malformed(format!("key {hex_key:?}: {e}")))?;
    PublicKey::try_from(bytes.as_slice())
        .map_err(|_| Refusal::Malformed(format!("key {hex_key:?} is not 32 bytes")))
}

impl Envelope {
    /// Signs `payload` under `context` with the key derived from `seed`. CI signs through
    /// its KMS; this is for tests and for an operator's offline tooling.
    #[must_use]
    pub fn seal(context: &[u8], payload: &[u8], seed: &[u8; 32]) -> Envelope {
        let pair = Ed25519KeyPair::from_seed_unchecked(seed).expect("any 32-byte seed is a key");
        let signature = pair.sign(&[context, payload].concat());
        Envelope {
            payload: BASE64.encode(payload),
            key: hex::encode(pair.public_key().as_ref()),
            signature: BASE64.encode(signature.as_ref()),
        }
    }

    /// Verifies the signature over `context` and the payload under the envelope's own
    /// key. Who that key is, and whether it may sign this document, is the caller's
    /// question.
    ///
    /// # Errors
    /// A field does not decode ([`Refusal::Malformed`]), or the signature does not verify
    /// ([`Refusal::BadSignature`]).
    pub fn open(&self, context: &[u8]) -> Result<Opened, Refusal> {
        let signer = parse_key(&self.key)?;
        let payload = BASE64
            .decode(&self.payload)
            .map_err(|e| Refusal::Malformed(format!("payload: {e}")))?;
        let signature = BASE64
            .decode(&self.signature)
            .map_err(|e| Refusal::Malformed(format!("signature: {e}")))?;
        UnparsedPublicKey::new(&ED25519, signer)
            .verify(&[context, payload.as_slice()].concat(), &signature)
            .map_err(|_| Refusal::BadSignature)?;
        Ok(Opened { signer, payload })
    }
}

/// The public key of an ed25519 seed (for tests and tooling).
#[must_use]
pub fn public_key(seed: &[u8; 32]) -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(seed).expect("any 32-byte seed is a key");
    PublicKey::try_from(pair.public_key().as_ref()).expect("ed25519 public keys are 32 bytes")
}

/// What a signing key may sign (S2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Sets that change only unprivileged kbf parts ([`crate::set::COMPONENT_ARTIFACTS`]).
    Component,
    /// Sets that change anything, including what runs as root.
    Platform,
}

/// The root-signed list of current signing keys (S2.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyStatement {
    /// Higher is newer. The updater never verifies under a lower serial than it has seen.
    pub serial: u64,
    /// Unix seconds after which the statement no longer verifies anything.
    pub expires: u64,
    /// Component keys, hex.
    pub component_keys: Vec<String>,
    /// Platform keys, hex.
    pub platform_keys: Vec<String>,
}

impl KeyStatement {
    /// The role `key` holds under this statement; a key named in both lists is a
    /// platform key, the role the root key gave it at most.
    #[must_use]
    pub fn role_of(&self, key: &PublicKey) -> Option<Role> {
        let named = |list: &[String]| list.iter().any(|k| parse_key(k).ok() == Some(*key));
        if named(&self.platform_keys) {
            Some(Role::Platform)
        } else if named(&self.component_keys) {
            Some(Role::Component)
        } else {
            None
        }
    }
}

/// A key statement whose root signature verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedStatement {
    /// The envelope it came in, as the updater stores it.
    pub envelope: Envelope,
    /// Its payload bytes.
    pub payload: Vec<u8>,
    /// The statement.
    pub statement: KeyStatement,
}

/// Opens a key statement: signed by `root` under [`STATEMENT_CONTEXT`], every key hex.
///
/// # Errors
/// A bad signature, a signer other than the root key ([`Refusal::NotRootSigned`]), or a
/// payload that is not a key statement.
pub fn open_statement(root: &PublicKey, envelope: &Envelope) -> Result<TrustedStatement, Refusal> {
    let opened = envelope.open(STATEMENT_CONTEXT)?;
    if opened.signer != *root {
        return Err(Refusal::NotRootSigned);
    }
    let statement: KeyStatement = serde_json::from_slice(&opened.payload)
        .map_err(|e| Refusal::Malformed(format!("key statement: {e}")))?;
    for key in statement
        .component_keys
        .iter()
        .chain(&statement.platform_keys)
    {
        parse_key(key)?;
    }
    Ok(TrustedStatement {
        envelope: envelope.clone(),
        payload: opened.payload,
        statement,
    })
}

/// The statement to verify a set under: the newer of the one the updater stored and the
/// one offered with the set. An offered statement older than the stored one is ignored,
/// never used; two different statements with one serial are refused (the root key
/// signed both, which only a compromise or a mistake explains). The pick must not have
/// expired at `now` (unix seconds).
///
/// # Errors
/// Either statement fails [`open_statement`], there is none, two differ under one
/// serial ([`Refusal::StatementConflict`]), or the pick has expired.
pub fn newest_statement(
    root: &PublicKey,
    stored: Option<&Envelope>,
    offered: Option<&Envelope>,
    now: u64,
) -> Result<TrustedStatement, Refusal> {
    let stored = stored.map(|e| open_statement(root, e)).transpose()?;
    let offered = offered.map(|e| open_statement(root, e)).transpose()?;
    let pick = match (stored, offered) {
        (None, None) => return Err(Refusal::NoStatement),
        (Some(one), None) | (None, Some(one)) => one,
        (Some(stored), Some(offered)) => {
            if offered.statement.serial > stored.statement.serial {
                offered
            } else if offered.statement.serial == stored.statement.serial
                && offered.payload != stored.payload
            {
                return Err(Refusal::StatementConflict(stored.statement.serial));
            } else {
                stored
            }
        }
    };
    if now >= pick.statement.expires {
        return Err(Refusal::StatementExpired(pick.statement.serial));
    }
    Ok(pick)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: [u8; 32] = [1; 32];
    const OTHER: [u8; 32] = [2; 32];

    fn statement(serial: u64, expires: u64, component: &[[u8; 32]]) -> KeyStatement {
        KeyStatement {
            serial,
            expires,
            component_keys: component
                .iter()
                .map(|s| hex::encode(public_key(s)))
                .collect(),
            platform_keys: vec![hex::encode(public_key(&[9; 32]))],
        }
    }

    fn seal_statement(s: &KeyStatement, seed: &[u8; 32]) -> Envelope {
        Envelope::seal(STATEMENT_CONTEXT, &serde_json::to_vec(s).unwrap(), seed)
    }

    /// Catches: a verifier that ignores the signature, the context, or the payload.
    #[test]
    fn a_signature_verifies_only_over_its_own_context_and_payload() {
        let env = Envelope::seal(SET_CONTEXT, b"hello", &OTHER);
        let opened = env.open(SET_CONTEXT).unwrap();
        assert_eq!(opened.payload, b"hello");
        assert_eq!(opened.signer, public_key(&OTHER));
        assert_eq!(env.open(STATEMENT_CONTEXT), Err(Refusal::BadSignature));
        let mut tampered = env.clone();
        tampered.payload = BASE64.encode(b"hellp");
        assert_eq!(tampered.open(SET_CONTEXT), Err(Refusal::BadSignature));
        let mut other_key = env;
        other_key.key = hex::encode(public_key(&ROOT));
        assert_eq!(other_key.open(SET_CONTEXT), Err(Refusal::BadSignature));
    }

    /// Catches: an envelope whose fields do not decode reaching the verifier, or a
    /// short key accepted.
    #[test]
    fn undecodable_fields_are_malformed() {
        let good = Envelope::seal(SET_CONTEXT, b"x", &OTHER);
        for bad in [
            Envelope {
                key: "zz".into(),
                ..good.clone()
            },
            Envelope {
                key: "abcd".into(),
                ..good.clone()
            },
            Envelope {
                payload: "!".into(),
                ..good.clone()
            },
            Envelope {
                signature: "!".into(),
                ..good.clone()
            },
        ] {
            assert!(
                matches!(bad.open(SET_CONTEXT), Err(Refusal::Malformed(_))),
                "{bad:?}"
            );
        }
    }

    /// Catches: roles mixed up, a platform key read as a component key, or an unnamed
    /// key given a role.
    #[test]
    fn a_key_holds_the_role_the_statement_gives_it() {
        let mut s = statement(1, 10, &[OTHER]);
        assert_eq!(s.role_of(&public_key(&OTHER)), Some(Role::Component));
        assert_eq!(s.role_of(&public_key(&[9; 32])), Some(Role::Platform));
        assert_eq!(s.role_of(&public_key(&ROOT)), None);
        s.platform_keys.push(hex::encode(public_key(&OTHER)));
        assert_eq!(s.role_of(&public_key(&OTHER)), Some(Role::Platform));
    }

    /// Catches: a key statement accepted when a key other than the pinned root signed
    /// it, or under the set context, or with a key that does not parse.
    #[test]
    fn only_the_root_key_signs_statements() {
        let root = public_key(&ROOT);
        let s = statement(1, 10, &[OTHER]);
        assert_eq!(
            open_statement(&root, &seal_statement(&s, &ROOT))
                .unwrap()
                .statement,
            s
        );
        assert_eq!(
            open_statement(&root, &seal_statement(&s, &OTHER)),
            Err(Refusal::NotRootSigned)
        );
        let as_set = Envelope::seal(SET_CONTEXT, &serde_json::to_vec(&s).unwrap(), &ROOT);
        assert_eq!(open_statement(&root, &as_set), Err(Refusal::BadSignature));
        let not_json = Envelope::seal(STATEMENT_CONTEXT, b"{", &ROOT);
        assert!(matches!(
            open_statement(&root, &not_json),
            Err(Refusal::Malformed(_))
        ));
        let mut bad_key = s;
        bad_key.platform_keys.push("00".into());
        assert!(matches!(
            open_statement(&root, &seal_statement(&bad_key, &ROOT)),
            Err(Refusal::Malformed(_))
        ));
    }

    /// Catches: verifying under an older offered statement (a revoked key comes back),
    /// keeping the stored one when a newer is offered, accepting two statements with one
    /// serial, or an expired statement.
    #[test]
    fn the_newest_statement_wins_and_never_goes_back() {
        let root = public_key(&ROOT);
        let old = seal_statement(&statement(4, 100, &[OTHER]), &ROOT);
        let new = seal_statement(&statement(5, 100, &[]), &ROOT);
        let pick = |stored: Option<&Envelope>, offered: Option<&Envelope>, now| {
            newest_statement(&root, stored, offered, now).map(|t| t.statement.serial)
        };
        assert_eq!(pick(None, None, 0), Err(Refusal::NoStatement));
        assert_eq!(pick(Some(&old), None, 0), Ok(4));
        assert_eq!(pick(None, Some(&old), 0), Ok(4));
        assert_eq!(pick(Some(&old), Some(&new), 0), Ok(5));
        assert_eq!(pick(Some(&new), Some(&old), 0), Ok(5));
        assert_eq!(pick(Some(&new), Some(&new), 0), Ok(5));
        let twin = seal_statement(&statement(5, 100, &[OTHER]), &ROOT);
        assert_eq!(
            pick(Some(&new), Some(&twin), 0),
            Err(Refusal::StatementConflict(5))
        );
        assert_eq!(pick(Some(&new), None, 99), Ok(5));
        assert_eq!(
            pick(Some(&new), None, 100),
            Err(Refusal::StatementExpired(5))
        );
        let forged = seal_statement(&statement(6, 100, &[OTHER]), &OTHER);
        assert_eq!(
            pick(Some(&new), Some(&forged), 0),
            Err(Refusal::NotRootSigned)
        );
        assert_eq!(pick(Some(&forged), None, 0), Err(Refusal::NotRootSigned));
    }
}
