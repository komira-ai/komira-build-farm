//! The blobs a lease reads and writes: the [`Cas`] trait and [`MemoryCas`].
//!
//! The driver fetches an action, its command and its input tree through a `Cas`, and
//! stores outputs, stdout and stderr through it. The daemon's CAS client (over
//! ByteStream, with the local cache in front) implements the same trait; `MemoryCas`
//! holds blobs in this process, for tests and simulation.
//!
//! Every blob is hashed on arrival: a `Cas` that hands back the wrong bytes is caught
//! here, not trusted ([`fetch`]).

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Mutex, PoisonError};

use kbf_proto::reapi::Digest;
use sha2::{Digest as _, Sha256};

/// Why a blob could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CasError {
    /// The CAS does not hold the blob.
    #[error("blob {0} is not in the CAS")]
    Missing(String),
    /// The bytes the CAS returned do not hash to the digest asked for.
    #[error("blob {0} failed verification: its bytes hash to {1}")]
    Corrupt(String, String),
}

/// A content-addressed blob store, SHA-256 only.
pub trait Cas: Send + Sync + 'static {
    /// The blob's bytes. Callers verify them ([`fetch`] does).
    fn get(&self, digest: &Digest) -> impl Future<Output = Result<Vec<u8>, CasError>> + Send;

    /// Stores `bytes` and returns their digest.
    fn put(&self, bytes: Vec<u8>) -> impl Future<Output = Result<Digest, CasError>> + Send;
}

/// The SHA-256 digest of `bytes`.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
    }
}

/// Reads a blob and checks its bytes against `digest`.
pub async fn fetch(cas: &impl Cas, digest: &Digest) -> Result<Vec<u8>, CasError> {
    let bytes = cas.get(digest).await?;
    let actual = digest_of(&bytes);
    if actual != *digest {
        return Err(CasError::Corrupt(
            label(digest),
            format!("{}/{}", actual.hash, actual.size_bytes),
        ));
    }
    Ok(bytes)
}

/// `hash/size`, the way REAPI resource names spell a digest.
pub(crate) fn label(digest: &Digest) -> String {
    format!("{}/{}", digest.hash, digest.size_bytes)
}

/// A CAS in this process's memory.
#[derive(Debug, Default)]
pub struct MemoryCas {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryCas {
    /// An empty CAS.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `bytes` and returns their digest.
    pub fn insert(&self, bytes: impl Into<Vec<u8>>) -> Digest {
        let bytes = bytes.into();
        let digest = digest_of(&bytes);
        self.lock().insert(label(&digest), bytes);
        digest
    }

    /// Replaces the bytes stored under `digest`, for tests of verification.
    pub fn corrupt(&self, digest: &Digest, bytes: impl Into<Vec<u8>>) {
        self.lock().insert(label(digest), bytes.into());
    }

    /// The blob under `digest`, unverified.
    #[must_use]
    pub fn blob(&self, digest: &Digest) -> Option<Vec<u8>> {
        self.lock().get(&label(digest)).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        // Each update is one map insert, so the map is consistent after a panic.
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Cas for MemoryCas {
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.blob(digest)
            .ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        Ok(self.insert(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    /// Catches a digest that is not REAPI's SHA-256 spelling (lowercase hex, byte size).
    #[test]
    fn digest_of_is_sha256_hex_and_size() {
        let digest = digest_of(b"abc");
        assert_eq!(
            digest.hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest.size_bytes, 3);
    }

    /// Catches a round trip that loses or changes bytes.
    #[test]
    fn put_then_fetch_returns_the_bytes() {
        let cas = MemoryCas::new();
        let digest = block_on(cas.put(b"hello".to_vec())).expect("put");
        assert_eq!(block_on(fetch(&cas, &digest)).expect("fetch"), b"hello");
    }

    /// Catches a missing blob read as empty bytes.
    #[test]
    fn a_missing_blob_is_an_error() {
        let cas = MemoryCas::new();
        let digest = digest_of(b"never stored");
        assert_eq!(
            block_on(fetch(&cas, &digest)),
            Err(CasError::Missing(label(&digest)))
        );
    }

    /// Catches a flipped byte being trusted: the CAS answered, but with other bytes.
    #[test]
    fn corrupt_bytes_are_refused() {
        let cas = MemoryCas::new();
        let digest = cas.insert(b"good".to_vec());
        cas.corrupt(&digest, b"evil".to_vec());
        let error = block_on(fetch(&cas, &digest)).expect_err("refused");
        assert!(matches!(error, CasError::Corrupt(..)), "{error}");
        assert!(error.to_string().contains("failed verification"));
    }
}
