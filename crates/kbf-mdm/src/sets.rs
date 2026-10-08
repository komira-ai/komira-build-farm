//! Signed software sets, as far as `enforce` needs them (S2, S3.1).
//!
//! A set names a pool's software; CI signs it with the component or the platform key,
//! and an offline root key signs a key statement naming those two keys. The gate
//! enforces only a macOS build that a set names, so it checks, before anything else:
//! the statement verifies under the root key the gate's host pins and has not
//! expired; the set verifies under a **platform** key the statement names (a macOS
//! build runs below everything, so the component key cannot cover it, S2.2); the set
//! has not expired; and it names a macOS build. The gate's own rules on top (pool,
//! platform, its per-pool floor, the newest statement) are in [`crate::gate`].
//!
//! Encoding (the gate's; `kbf-updater` is to read the same): each signed document is a
//! JSON envelope `{"payload", "key", "signature"}`, all base64 (standard alphabet,
//! padded): the payload is the document's JSON bytes, the key the signer's 32-byte
//! ed25519 public key, the signature ed25519 over the payload bytes exactly.
//! Documents may carry fields the gate does not read; the envelope may not.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::clock::parse_rfc3339;

/// The `kind` of a key statement and of a set.
pub const STATEMENT_KIND: &str = "kbf-key-statement-v1";
pub const SET_KIND: &str = "kbf-set-v1";

/// A signed document as it travels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub payload: String,
    pub key: String,
    pub signature: String,
}

#[derive(Deserialize)]
struct Statement {
    kind: String,
    serial: u64,
    expires: String,
    #[serde(default)]
    platform_keys: Vec<String>,
    #[serde(default)]
    component_keys: Vec<String>,
}

#[derive(Deserialize)]
struct Platform {
    os: String,
    arch: String,
}

#[derive(Deserialize)]
struct Macos {
    version: String,
    build: String,
}

#[derive(Deserialize)]
struct SetDoc {
    kind: String,
    pool: String,
    platform: Platform,
    serial: u64,
    min_serial: u64,
    expires: String,
    macos: Option<Macos>,
    #[serde(default)]
    profiles: Vec<String>,
}

/// What the gate takes from a verified set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedSet {
    /// The key statement's serial.
    pub statement_serial: u64,
    pub pool: String,
    pub os: String,
    pub arch: String,
    pub serial: u64,
    pub min_serial: u64,
    /// Seconds since the epoch.
    pub expires: i64,
    pub macos_version: String,
    pub macos_build: String,
    /// SHA-256 digests (lowercase hex) of the profiles the set names.
    pub profiles: Vec<String>,
}

/// Why a set was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SetError {
    #[error("{0}: not a signed document")]
    Envelope(&'static str),
    #[error("the key statement is not signed by the root key")]
    NotRoot,
    #[error("{0}: the signature does not verify")]
    Signature(&'static str),
    #[error("{0}: {1}")]
    Document(&'static str, String),
    #[error("the key statement has expired")]
    StatementExpired,
    #[error("the set is signed by a component key, which cannot cover a macOS build")]
    ComponentKey,
    #[error("the set's key is not named by the key statement")]
    UnknownKey,
    #[error("the set has expired")]
    SetExpired,
    #[error("the set names no macOS build")]
    NoMacosBuild,
}

fn decode_key(text: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = STANDARD.decode(text).ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Checks `envelope`'s signature under its own key and returns the key and payload.
fn open(envelope: &Envelope, what: &'static str) -> Result<(VerifyingKey, Vec<u8>), SetError> {
    let key = decode_key(&envelope.key).ok_or(SetError::Envelope(what))?;
    let payload = STANDARD
        .decode(&envelope.payload)
        .map_err(|_| SetError::Envelope(what))?;
    let signature: [u8; 64] = STANDARD
        .decode(&envelope.signature)
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or(SetError::Envelope(what))?;
    key.verify_strict(&payload, &Signature::from_bytes(&signature))
        .map_err(|_| SetError::Signature(what))?;
    Ok((key, payload))
}

fn document<T: for<'de> Deserialize<'de>>(
    payload: &[u8],
    what: &'static str,
) -> Result<T, SetError> {
    serde_json::from_slice(payload).map_err(|e| SetError::Document(what, e.to_string()))
}

fn expiry(text: &str, what: &'static str) -> Result<i64, SetError> {
    parse_rfc3339(text)
        .ok_or_else(|| SetError::Document(what, "expires is not an RFC 3339 time".into()))
}

fn names(keys: &[String], key: &VerifyingKey) -> bool {
    keys.iter().filter_map(|k| decode_key(k)).any(|k| k == *key)
}

/// Verifies a set under a key statement and the pinned root key, at time `now`.
///
/// # Errors
/// Any check of the module documentation fails.
pub fn verify(
    root: &VerifyingKey,
    statement: &Envelope,
    set: &Envelope,
    now: i64,
) -> Result<VerifiedSet, SetError> {
    let (signer, payload) = open(statement, "key statement")?;
    if signer != *root {
        return Err(SetError::NotRoot);
    }
    let statement: Statement = document(&payload, "key statement")?;
    if statement.kind != STATEMENT_KIND {
        return Err(SetError::Document(
            "key statement",
            format!("kind is not {STATEMENT_KIND}"),
        ));
    }
    if expiry(&statement.expires, "key statement")? <= now {
        return Err(SetError::StatementExpired);
    }
    let (signer, payload) = open(set, "set")?;
    if !names(&statement.platform_keys, &signer) {
        return Err(if names(&statement.component_keys, &signer) {
            SetError::ComponentKey
        } else {
            SetError::UnknownKey
        });
    }
    let doc: SetDoc = document(&payload, "set")?;
    if doc.kind != SET_KIND {
        return Err(SetError::Document("set", format!("kind is not {SET_KIND}")));
    }
    let expires = expiry(&doc.expires, "set")?;
    if expires <= now {
        return Err(SetError::SetExpired);
    }
    let macos = doc.macos.ok_or(SetError::NoMacosBuild)?;
    Ok(VerifiedSet {
        statement_serial: statement.serial,
        pool: doc.pool,
        os: doc.platform.os,
        arch: doc.platform.arch,
        serial: doc.serial,
        min_serial: doc.min_serial,
        expires,
        macos_version: macos.version,
        macos_build: macos.build,
        profiles: doc.profiles,
    })
}

/// Reads a root public key: 32 bytes, base64.
pub fn parse_root_key(text: &str) -> Option<VerifyingKey> {
    decode_key(text.trim())
}

/// Test material: signing keys and envelopes.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub fn public(key: &SigningKey) -> String {
        STANDARD.encode(key.verifying_key().to_bytes())
    }

    pub fn seal(key: &SigningKey, payload: &serde_json::Value) -> Envelope {
        let bytes = serde_json::to_vec(payload).unwrap();
        Envelope {
            payload: STANDARD.encode(&bytes),
            key: public(key),
            signature: STANDARD.encode(key.sign(&bytes).to_bytes()),
        }
    }

    pub const ROOT: u8 = 1;
    pub const PLATFORM: u8 = 2;
    pub const COMPONENT: u8 = 3;

    pub fn statement(serial: u64, expires: &str) -> Envelope {
        seal(
            &key(ROOT),
            &serde_json::json!({
                "kind": STATEMENT_KIND, "serial": serial, "expires": expires,
                "platform_keys": [public(&key(PLATFORM))],
                "component_keys": [public(&key(COMPONENT))],
            }),
        )
    }

    pub fn set_doc(pool: &str, serial: u64, min_serial: u64, build: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": SET_KIND, "pool": pool,
            "platform": {"os": "macos", "arch": "arm64"},
            "serial": serial, "min_serial": min_serial,
            "expires": "2030-01-01T00:00:00Z",
            "macos": {"version": "27.1", "build": build},
            "profiles": [],
            "xcode": [{"build": "17A100", "sha256": "00"}],
        })
    }

    pub fn set(pool: &str, serial: u64, min_serial: u64, build: &str) -> Envelope {
        seal(&key(PLATFORM), &set_doc(pool, serial, min_serial, build))
    }
}

#[cfg(test)]
#[path = "sets_tests.rs"]
mod tests;
