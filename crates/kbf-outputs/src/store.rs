//! Where outputs go: the [`Store`] trait a driver implements over its CAS client.

use std::future::Future;

use kbf_proto::reapi::Digest;
use sha2::{Digest as _, Sha256};

/// A file of at most this many bytes is read whole and stored with [`Store::put`];
/// a larger one is hashed in chunks of this size and stored with
/// [`Store::put_file`].
pub const CHUNK_BYTES: usize = 1 << 20;

/// Why a store refused or failed a blob.
pub type StoreError = Box<dyn std::error::Error + Send + Sync>;

/// A content-addressed blob store, SHA-256 only, as [`crate::collect`] writes to it.
pub trait Store: Sync {
    /// Stores `bytes` and returns their digest.
    fn put(&self, bytes: Vec<u8>) -> impl Future<Output = Result<Digest, StoreError>> + Send;

    /// Stores the bytes of `file`, read from its current offset to its end, which
    /// hash to `digest`. A store streams them rather than holding them whole, and
    /// fails if they do not hash to `digest`.
    fn put_file(
        &self,
        file: std::fs::File,
        digest: Digest,
    ) -> impl Future<Output = Result<Digest, StoreError>> + Send;
}

/// The SHA-256 digest of `bytes`.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        // A slice is never longer than `isize::MAX`.
        size_bytes: bytes.len() as i64,
    }
}
