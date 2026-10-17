//! The keys and certificate requests a node makes (design section 5.5).
//!
//! One ECDSA P-256 key per role, made on the node. The frontend takes nothing from a CSR
//! but its public key and the proof that the sender holds the private one, so a request
//! carries no names, no usages and no extensions. A `Renew` request also carries the
//! time it was made as a PKCS #10 attribute, inside the signed part, so the signature
//! binds the time the frontend checks against its own clock.

use rcgen::{Attribute, CertificateParams, DistinguishedName, KeyPair, PublicKeyData};

/// The attribute type of the time a renewal request was made (`issued_at`). The value
/// is a SET holding an OCTET STRING of the eight big-endian bytes of the unix seconds.
/// There is no registered arc for kbf; this one is in the UUID arc `2.25`, which needs
/// no registration, and is a constant of this protocol.
pub const ISSUED_AT_OID: &[u64] = &[2, 25, 0x6b62_6672_6e65_776c];

/// A new ECDSA P-256 key.
///
/// # Errors
/// The key generator failed.
pub fn new_key() -> Result<KeyPair, rcgen::Error> {
    KeyPair::generate()
}

/// The DER SubjectPublicKeyInfo of `key`: what a certificate for it must carry.
#[must_use]
pub fn spki_of(key: &KeyPair) -> Vec<u8> {
    key.subject_public_key_info()
}

/// A PKCS #10 request for `key`, with `issued_at` among the signed attributes if given.
///
/// # Errors
/// The signature failed.
pub fn request(key: &KeyPair, issued_at: Option<i64>) -> Result<Vec<u8>, rcgen::Error> {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    let attributes = issued_at
        .map(|t| {
            let mut value = vec![0x31, 0x0a, 0x04, 0x08];
            value.extend_from_slice(&t.to_be_bytes());
            Attribute {
                oid: ISSUED_AT_OID,
                values: value,
            }
        })
        .into_iter()
        .collect();
    let csr = params.serialize_request_with_attributes(key, attributes)?;
    Ok(csr.der().as_ref().to_vec())
}
