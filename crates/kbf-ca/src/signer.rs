//! The seam the CA key sits behind.

use std::fmt;
use std::path::Path;

use rcgen::{KeyPair, PublicKeyData, SigningKey};

use crate::error::CaError;
use crate::key::PublicKey;
use crate::keyfile;

/// A signature algorithm. There is one: the CA signs ECDSA P-256 with SHA-256, DER-encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    /// `ecdsa-with-SHA256` on the P-256 curve.
    EcdsaP256Sha256,
}

/// A signer failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the signer failed: {0}")]
pub struct SignError(pub String);

/// Where the private key lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Custody {
    /// In this process's memory, loaded from a file: development and tests.
    InProcess,
    /// Outside this process: a signer process, a hardware module or a cloud KMS.
    External,
}

/// What signs for the CA. Nothing in the issuance path may assume the key bytes are in
/// the process.
pub trait Signer: Send + Sync {
    /// The DER SubjectPublicKeyInfo of the key (ECDSA P-256).
    fn public_key(&self) -> &[u8];

    /// Where the private key lives.
    fn custody(&self) -> Custody;

    /// Signs `tbs` and returns the DER ECDSA signature.
    ///
    /// # Errors
    /// [`SignError`] when the signer cannot.
    fn sign(&self, tbs: &[u8], algorithm: Algorithm) -> Result<Vec<u8>, SignError>;
}

/// The CA key in a file in this process. For development and tests: production refuses
/// it. `Debug` prints the type's name and nothing else.
pub struct FileSigner {
    key: KeyPair,
    spki: Vec<u8>,
}

impl FileSigner {
    /// The key file's name in a CA directory.
    pub const KEY_FILE: &'static str = "ca.key";

    fn from_key(key: KeyPair) -> Result<Self, CaError> {
        let spki = key.subject_public_key_info();
        PublicKey::from_spki(&spki).map_err(|_| CaError::KeyType)?;
        Ok(Self { key, spki })
    }

    /// A new key from the operating system's random source, not written anywhere.
    ///
    /// # Errors
    /// [`CaError::Build`] when the key cannot be made.
    pub fn generate() -> Result<Self, CaError> {
        Self::from_key(KeyPair::generate()?)
    }

    /// The key of a PKCS #8 PEM.
    ///
    /// # Errors
    /// [`CaError::Build`] for text that is not a key; [`CaError::KeyType`] for a key that
    /// is not ECDSA P-256.
    pub fn from_pem(pem: &str) -> Result<Self, CaError> {
        Self::from_key(KeyPair::from_pem(pem)?)
    }

    /// The key as PKCS #8 PEM: the secret. Callers write it to a file closed to others.
    #[must_use]
    pub fn to_pem(&self) -> String {
        self.key.serialize_pem()
    }

    /// Writes the key as `dir/ca.key` (mode 0600, created in that mode, never overwriting)
    /// in `dir`, which is made with mode 0700 or accepted if it exists, is owned by the
    /// running account and is closed to others.
    ///
    /// # Errors
    /// [`CaError::KeyFile`] when the directory is unfit or the file exists.
    pub fn store_in(&self, dir: &Path) -> Result<(), CaError> {
        keyfile::ensure_dir(dir, keyfile::effective_uid())?;
        keyfile::write_new(&dir.join(Self::KEY_FILE), self.to_pem().as_bytes(), 0o600)?;
        Ok(())
    }

    /// A new key, stored by [`Self::store_in`].
    ///
    /// # Errors
    /// As [`Self::generate`] and [`Self::store_in`].
    pub fn create_in(dir: &Path) -> Result<Self, CaError> {
        let signer = Self::generate()?;
        signer.store_in(dir)?;
        Ok(signer)
    }

    /// Loads `dir/ca.key` for the running account: [`Self::load_owned_by`] with the
    /// effective user id.
    ///
    /// # Errors
    /// As [`Self::load_owned_by`].
    pub fn load(dir: &Path) -> Result<Self, CaError> {
        Self::load_owned_by(dir, keyfile::effective_uid())
    }

    /// Loads `dir/ca.key`, refusing a directory or file that is a symlink, is not owned by
    /// `uid`, or lets group or others in, and a file that is not a regular file.
    ///
    /// # Errors
    /// [`CaError::KeyFile`] naming the fault; [`CaError::Build`] or [`CaError::KeyType`]
    /// for contents that are not a P-256 key.
    pub fn load_owned_by(dir: &Path, uid: u32) -> Result<Self, CaError> {
        keyfile::check_dir(dir, uid)?;
        Self::load_key(dir, uid)
    }

    /// Loads `dir/ca.key` for the running account from a directory that is shared with a
    /// group: [`Self::load_owned_by`] except that the directory may be readable and
    /// searchable by its group (mode up to 0750). Nothing else is relaxed: the directory
    /// must not be writable by the group or reachable by others, and the key file must
    /// still be closed to group and others.
    ///
    /// # Errors
    /// As [`Self::load_owned_by`].
    pub fn load_in_shared_dir(dir: &Path) -> Result<Self, CaError> {
        let uid = keyfile::effective_uid();
        keyfile::check_shared_dir(dir, uid)?;
        Self::load_key(dir, uid)
    }

    fn load_key(dir: &Path, uid: u32) -> Result<Self, CaError> {
        let pem = keyfile::read_secret(&dir.join(Self::KEY_FILE), uid)?;
        Self::from_pem(&pem)
    }
}

impl Signer for FileSigner {
    fn public_key(&self) -> &[u8] {
        &self.spki
    }

    fn custody(&self) -> Custody {
        Custody::InProcess
    }

    fn sign(&self, tbs: &[u8], algorithm: Algorithm) -> Result<Vec<u8>, SignError> {
        let Algorithm::EcdsaP256Sha256 = algorithm;
        // ring signs with a valid key unless the operating system's random source fails,
        // which stops the process, as it does for `SystemEntropy`.
        Ok(SigningKey::sign(&self.key, tbs).expect("the operating system's random source failed"))
    }
}

impl fmt::Debug for FileSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FileSigner")
    }
}
