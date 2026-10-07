//! The blobs a lease reads and writes: the [`Cas`] trait, and [`CasClient`], the
//! daemon's client of a server front's CAS over ByteStream.
//!
//! A runtime fetches an action, its command and its input tree through a `Cas`, and
//! stores outputs, stdout and stderr through it. Blob bytes never travel on the
//! `kbf.worker.v1` stream; they go over this separate connection.
//!
//! Every blob is hashed on arrival: a `Cas` that hands back other bytes than the digest
//! names is caught by [`fetch`], not trusted. The front verifies every upload, so a
//! put is not checked twice here.

use std::future::Future;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::stream;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::reapi::Digest;
use sha2::{Digest as _, Sha256};
use tonic::Code;
use tonic::transport::Channel;

/// The most bytes one ByteStream `WriteRequest` carries: the front's read chunk size,
/// well under its message limit.
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
}

/// A content-addressed blob store, SHA-256 only.
pub trait Cas: Send + Sync + 'static {
    /// The blob's bytes. Callers verify them ([`fetch`] does).
    fn get(&self, digest: &Digest) -> impl Future<Output = Result<Vec<u8>, CasError>> + Send;

    /// Stores `bytes` and returns their digest once they are durable.
    fn put(&self, bytes: Vec<u8>) -> impl Future<Output = Result<Digest, CasError>> + Send;

    /// Stores the bytes of `file`, from its current offset to its end, which hash to
    /// `digest`; returns the digest once they are durable. For outputs too large to
    /// hold in memory: [`CasClient`] streams the file in [`WRITE_CHUNK_BYTES`]
    /// messages, and the front checks the bytes against `digest`. This default reads
    /// the file whole, calls [`Cas::put`] and checks the digest itself.
    fn put_file(
        &self,
        file: std::fs::File,
        digest: Digest,
    ) -> impl Future<Output = Result<Digest, CasError>> + Send {
        async move {
            let blob = label(&digest);
            let bytes = read_whole(file)
                .await
                .map_err(|e| CasError::Unavailable(blob.clone(), format!("read: {e}")))?;
            let stored = self.put(bytes).await?;
            if stored != digest {
                return Err(CasError::Corrupt(blob, label(&stored)));
            }
            Ok(stored)
        }
    }
}

/// The rest of `file`, read on the blocking pool.
async fn read_whole(mut file: std::fs::File) -> std::io::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map(|_| bytes)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Up to `max` more bytes of `file`, read on the blocking pool; the file comes back
/// with them.
async fn read_chunk(
    mut file: std::fs::File,
    max: u64,
) -> std::io::Result<(std::fs::File, Vec<u8>)> {
    tokio::task::spawn_blocking(move || {
        let mut chunk = Vec::new();
        (&mut file).take(max).read_to_end(&mut chunk)?;
        Ok((file, chunk))
    })
    .await
    .map_err(std::io::Error::other)?
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

/// Reads a blob and checks its bytes against `digest`.
///
/// # Errors
/// The blob is missing or unreachable, or its bytes hash to another digest.
pub async fn fetch(cas: &impl Cas, digest: &Digest) -> Result<Vec<u8>, CasError> {
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

/// A client of a server front's CAS: reads with ByteStream `Read`, writes with
/// ByteStream `Write`, one blob per call. The front serves one cache per cell, so
/// resource names carry no instance name.
#[derive(Debug)]
pub struct CasClient {
    bytestream: ByteStreamClient<Channel>,
    uploads: AtomicU64,
}

impl CasClient {
    /// A client over `channel`, a connection to the front's REAPI listener.
    #[must_use]
    pub fn new(channel: Channel) -> Self {
        Self {
            bytestream: ByteStreamClient::new(channel),
            uploads: AtomicU64::new(0),
        }
    }

    /// A REAPI resource name needs a fresh id per upload; the front reads none of it.
    fn upload_name(&self, digest: &Digest) -> String {
        let n = self.uploads.fetch_add(1, Ordering::Relaxed);
        format!(
            "uploads/kbf-daemon-{}-{n}/blobs/{}",
            std::process::id(),
            label(digest)
        )
    }
}

/// A failed call as a [`CasError`]: NOT_FOUND is a missing blob, anything else the
/// CAS failing.
fn call_error(digest: &Digest, status: &tonic::Status) -> CasError {
    if status.code() == Code::NotFound {
        CasError::Missing(label(digest))
    } else {
        CasError::Unavailable(
            label(digest),
            format!("{:?}: {}", status.code(), status.message()),
        )
    }
}

impl Cas for CasClient {
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let request = ReadRequest {
            resource_name: format!("blobs/{}", label(digest)),
            read_offset: 0,
            read_limit: 0,
        };
        let mut client = self.bytestream.clone();
        let fail = |status: tonic::Status| call_error(digest, &status);
        let mut chunks = client.read(request).await.map_err(fail)?.into_inner();
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.message().await.map_err(fail)? {
            bytes.extend_from_slice(&chunk.data);
        }
        Ok(bytes)
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let digest = digest_of(&bytes);
        let name = self.upload_name(&digest);
        // One message per chunk; an empty blob is one message with no data.
        let count = bytes.len().div_ceil(WRITE_CHUNK_BYTES).max(1);
        let requests: Vec<WriteRequest> = (0..count)
            .map(|i| {
                let start = i * WRITE_CHUNK_BYTES;
                let end = bytes.len().min(start + WRITE_CHUNK_BYTES);
                WriteRequest {
                    // REAPI: the first message names the resource; later ones may.
                    resource_name: if i == 0 { name.clone() } else { String::new() },
                    write_offset: start as i64,
                    finish_write: i + 1 == count,
                    data: bytes[start..end].to_vec(),
                }
            })
            .collect();
        let mut client = self.bytestream.clone();
        let response = client
            .write(stream::iter(requests))
            .await
            .map_err(|status| call_error(&digest, &status))?
            .into_inner();
        committed(&digest, response.committed_size)
    }

    async fn put_file(&self, file: std::fs::File, digest: Digest) -> Result<Digest, CasError> {
        let name = self.upload_name(&digest);
        let size = digest.size_bytes;
        // A read that fails ends the stream early; its error is reported over the
        // front's complaint about the short write.
        let read_error = Arc::new(Mutex::new(None));
        let failed = Arc::clone(&read_error);
        // Each message's chunk is read as the stream is polled, so at most one chunk
        // is in memory. The first message names the resource; the last finishes the
        // write once `size` bytes are sent, or at the end of the file.
        let requests = stream::unfold(Some((file, 0_i64)), move |state| {
            let name = name.clone();
            let failed = Arc::clone(&failed);
            async move {
                let (file, offset) = state?;
                let want = (size - offset).clamp(0, WRITE_CHUNK_BYTES as i64);
                let (file, data) = match read_chunk(file, want.unsigned_abs()).await {
                    Ok(read) => read,
                    Err(e) => {
                        *failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
                        return None;
                    }
                };
                let next = offset + data.len() as i64;
                let finish = next >= size || (data.len() as i64) < want;
                let request = WriteRequest {
                    resource_name: if offset == 0 { name } else { String::new() },
                    write_offset: offset,
                    finish_write: finish,
                    data,
                };
                Some((request, (!finish).then_some((file, next))))
            }
        });
        let mut client = self.bytestream.clone();
        let response = client.write(requests).await;
        if let Some(e) = read_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Err(CasError::Unavailable(label(&digest), format!("read: {e}")));
        }
        let response = response
            .map_err(|status| call_error(&digest, &status))?
            .into_inner();
        committed(&digest, response.committed_size)
    }
}

/// `digest` if the CAS committed all of its bytes.
fn committed(digest: &Digest, size: i64) -> Result<Digest, CasError> {
    if size != digest.size_bytes {
        return Err(CasError::Unavailable(
            label(digest),
            format!("the CAS committed {size} bytes of {}", digest.size_bytes),
        ));
    }
    Ok(digest.clone())
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

    /// Catches a NOT_FOUND reported as an outage (the action would be retried as an
    /// infrastructure failure instead of failing on its missing input), and an outage
    /// reported as a missing blob.
    #[test]
    fn not_found_is_missing_and_the_rest_is_unavailable() {
        let digest = digest_of(b"x");
        assert_eq!(
            call_error(&digest, &tonic::Status::not_found("gone")),
            CasError::Missing(label(&digest))
        );
        let error = call_error(&digest, &tonic::Status::unavailable("down"));
        assert!(
            matches!(&error, CasError::Unavailable(blob, why) if *blob == label(&digest) && why.contains("down")),
            "{error:?}"
        );
    }
}
