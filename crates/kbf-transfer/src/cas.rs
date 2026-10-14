//! The blobs a lease reads and writes: the [`Cas`] trait, and the helpers every store
//! shares ([`digest_of`], [`fetch`], [`label`]).
//!
//! A runtime fetches an action, its command and its input tree through a `Cas`, and
//! stores outputs, stdout and stderr through it. Every blob is hashed on arrival: a
//! `Cas` that hands back other bytes than the digest names is caught by [`fetch`] (or
//! by the store itself, as [`crate::Transfers`] does), not trusted. The front verifies
//! every upload, so a put is not checked twice here.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt as _};
use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::config::DEFAULT_IN_FLIGHT;
use crate::spool::Spool;
use crate::stats::UploadStats;

/// The most bytes one ByteStream `WriteRequest` carries by default
/// (`--transfer-chunk-bytes`): the front's read chunk size, well under its message
/// limit. Callers that read a file for [`Cas::put_chunks`] read it this much at a time.
pub const WRITE_CHUNK_BYTES: usize = 1 << 20;

/// Why a blob could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CasError {
    /// The CAS does not hold the blob.
    #[error("blob {0} is not in the CAS")]
    Missing(String),
    /// The bytes the CAS returned do not hash to the digest asked for.
    #[error("blob {0} failed verification: its bytes hash to {1}")]
    Corrupt(String, String),
    /// The CAS could not be reached or failed the call.
    #[error("CAS call for blob {0} failed: {1}")]
    Unavailable(String, String),
    /// The bytes of a blob being stored could not be read from where they are (a file
    /// that shrank, an I/O error).
    #[error("blob {0} could not be read for storing: {1}")]
    Read(String, String),
    /// The bytes of a blob being fetched could not be written where they go
    /// ([`Cas::read_to`]'s file).
    #[error("blob {0} could not be written: {1}")]
    Write(String, String),
}

/// A content-addressed blob store, SHA-256 only.
pub trait Cas: Send + Sync + 'static {
    /// What one lease reads and writes through: [`crate::Scope`] for the transfer
    /// engine, which counts the lease's transfers and logs them when it is dropped;
    /// the store itself (an `Arc` of it) for a store with nothing per lease.
    type Lease: Cas;

    /// The store as the lease `lease` sees it. A runtime takes one per lease and drops
    /// it when the lease ends.
    fn lease(self: &Arc<Self>, lease: &LeaseId) -> Self::Lease;

    /// The blob's bytes. Callers verify them ([`fetch`] does).
    fn get(&self, digest: &Digest) -> impl Future<Output = Result<Vec<u8>, CasError>> + Send;

    /// Stores `bytes` and returns their digest once they are durable.
    fn put(&self, bytes: Vec<u8>) -> impl Future<Output = Result<Digest, CasError>> + Send;

    /// Stores the blob `digest` names, whose bytes arrive in `chunks`, and returns the
    /// digest once they are durable: for files too large to hold in memory. A chunk
    /// that fails to be read ends the upload with [`CasError::Read`]. The transfer
    /// engine streams the chunks, at most `--transfer-chunk-bytes` per message, and
    /// the front checks the bytes against `digest`; this default gathers them, calls
    /// [`Cas::put`] and checks the digest itself.
    fn put_chunks(
        &self,
        digest: Digest,
        chunks: Chunks,
    ) -> impl Future<Output = Result<Digest, CasError>> + Send {
        async move {
            let blob = label(&digest);
            let mut bytes = Vec::new();
            let mut chunks = chunks;
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(|e| CasError::Read(blob.clone(), e.to_string()))?;
                bytes.extend_from_slice(&chunk);
            }
            let stored = self.put(bytes).await?;
            if stored != digest {
                return Err(CasError::Corrupt(blob, label(&stored)));
            }
            Ok(stored)
        }
    }

    /// Writes the blob `digest` names into `file`, from its current offset, checked
    /// against `digest`, and flushes it. The transfer engine streams a large blob into
    /// the file chunk by chunk (hashed as it is written, a broken read resumed where it
    /// stopped); this default fetches it whole with [`fetch`] and writes it. On an
    /// error the file may hold part of the blob.
    fn read_to(
        &self,
        digest: &Digest,
        file: &mut tokio::fs::File,
    ) -> impl Future<Output = Result<(), CasError>> + Send {
        async move {
            let bytes = fetch(self, digest).await?;
            let written = async {
                file.write_all(&bytes).await?;
                file.flush().await
            };
            written
                .await
                .map_err(|e| CasError::Write(label(digest), e.to_string()))
        }
    }

    /// The digests among `digests` the store does not hold, in the order asked. A
    /// caller with many blobs to store asks about all of them once, before it stores
    /// any, and stores only these, rather than paying a question per blob.
    ///
    /// The transfer engine asks FindMissing in calls of at most
    /// `--transfer-find-missing-chunk` digests, all of the calls at once within
    /// `--transfer-in-flight`; a call that fails answers "missing" for its digests, and
    /// a [`Cas::put`] or [`Cas::put_chunks`] on the same lease of a digest answered
    /// missing does not ask again. This default asks nothing and answers every digest
    /// missing: a store whose puts need no question.
    fn missing(&self, digests: &[Digest]) -> impl Future<Output = Vec<Digest>> + Send {
        std::future::ready(digests.to_vec())
    }

    /// Stores every blob `spool` holds and returns once all are durable: an action's
    /// outputs, gathered by a walk that stored them into the spool. The transfer engine
    /// sends the ones the server lacks as whole segments (`crate::segments`), and blob
    /// by blob when the server cannot take segments; this default stores each one with
    /// [`Cas::put`], [`Cas::in_flight`] at a time.
    fn put_spool(&self, spool: Arc<Spool>) -> impl Future<Output = Result<(), CasError>> + Send {
        async move { crate::segments::put_each(self, &spool).await }
    }

    /// Where this lease's uploads spent their time so far. The transfer engine's
    /// [`crate::Scope`] counts it; this default is a store with nothing to count.
    fn upload_stats(&self) -> UploadStats {
        UploadStats::default()
    }

    /// How many requests this store runs at once, for the whole process. A caller that
    /// queues work ahead of the store (a tree walk holding files open for upload) keeps
    /// no more than this many items queued, so it holds that many, not the whole tree.
    /// The transfer engine answers `--transfer-in-flight`.
    fn in_flight(&self) -> usize {
        DEFAULT_IN_FLIGHT
    }
}

/// A store shared behind an `Arc` is the same store.
impl<C: Cas> Cas for Arc<C> {
    type Lease = C::Lease;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> C::Lease {
        C::lease(self.as_ref(), lease)
    }

    fn get(&self, digest: &Digest) -> impl Future<Output = Result<Vec<u8>, CasError>> + Send {
        C::get(self, digest)
    }

    fn put(&self, bytes: Vec<u8>) -> impl Future<Output = Result<Digest, CasError>> + Send {
        C::put(self, bytes)
    }

    fn put_chunks(
        &self,
        digest: Digest,
        chunks: Chunks,
    ) -> impl Future<Output = Result<Digest, CasError>> + Send {
        C::put_chunks(self, digest, chunks)
    }

    fn read_to(
        &self,
        digest: &Digest,
        file: &mut tokio::fs::File,
    ) -> impl Future<Output = Result<(), CasError>> + Send {
        C::read_to(self, digest, file)
    }

    fn missing(&self, digests: &[Digest]) -> impl Future<Output = Vec<Digest>> + Send {
        C::missing(self, digests)
    }

    fn put_spool(&self, spool: Arc<Spool>) -> impl Future<Output = Result<(), CasError>> + Send {
        C::put_spool(self, spool)
    }

    fn upload_stats(&self) -> UploadStats {
        C::upload_stats(self)
    }

    fn in_flight(&self) -> usize {
        C::in_flight(self)
    }
}

/// A blob's bytes on their way to [`Cas::put_chunks`], one chunk at a time.
pub type Chunks = Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>>;

/// The SHA-256 digest of `bytes`.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        // A slice is never longer than `isize::MAX`.
        size_bytes: bytes.len() as i64,
    }
}

/// Reads a blob and checks its bytes against `digest`.
///
/// # Errors
/// The blob is missing or unreachable, or its bytes hash to another digest.
pub async fn fetch<C: Cas + ?Sized>(cas: &C, digest: &Digest) -> Result<Vec<u8>, CasError> {
    let bytes = cas.get(digest).await?;
    let actual = digest_of(&bytes);
    if actual != *digest {
        return Err(CasError::Corrupt(label(digest), label(&actual)));
    }
    Ok(bytes)
}

/// `hash/size`, the way REAPI resource names spell a digest.
#[must_use]
pub fn label(digest: &Digest) -> String {
    format!("{}/{}", digest.hash, digest.size_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a digest that is not REAPI's SHA-256 spelling (lowercase hex, byte size).
    #[test]
    fn digest_of_is_sha256_hex_and_size() {
        let digest = digest_of(b"abc");
        assert_eq!(
            digest.hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest.size_bytes, 3);
        assert_eq!(label(&digest), format!("{}/3", digest.hash));
    }
}
