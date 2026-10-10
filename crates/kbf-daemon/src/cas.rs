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
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::channel::mpsc;
use futures::{SinkExt as _, Stream, StreamExt as _, stream};
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
    /// The bytes of a blob being stored could not be read from where they are (a file
    /// that shrank, an I/O error).
    #[error("blob {0} could not be read for storing: {1}")]
    Read(String, String),
}

/// A content-addressed blob store, SHA-256 only.
pub trait Cas: Send + Sync + 'static {
    /// The blob's bytes. Callers verify them ([`fetch`] does).
    fn get(&self, digest: &Digest) -> impl Future<Output = Result<Vec<u8>, CasError>> + Send;

    /// Stores `bytes` and returns their digest once they are durable.
    fn put(&self, bytes: Vec<u8>) -> impl Future<Output = Result<Digest, CasError>> + Send;

    /// Stores the blob `digest` names, whose bytes arrive in `chunks`, and returns the
    /// digest once they are durable: for files too large to hold in memory. A chunk
    /// that fails to be read ends the upload with [`CasError::Read`]. [`CasClient`]
    /// streams the chunks, at most [`WRITE_CHUNK_BYTES`] per message, and the front
    /// checks the bytes against `digest`; this default gathers them, calls
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

/// A client of the server's CAS: reads with ByteStream `Read`, writes with
/// ByteStream `Write`, one blob per call. The server serves one cache per cell, so
/// resource names carry no instance name. The daemon binaries dial the server's
/// worker listener with it, over mutual TLS.
#[derive(Debug)]
pub struct CasClient {
    bytestream: ByteStreamClient<Channel>,
    uploads: AtomicU64,
}

impl CasClient {
    /// A client over `channel`, a connection to a `ByteStream` service.
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

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        let name = self.upload_name(&digest);
        // A task reads the chunks and hands the requests over a channel that holds
        // one, so a chunk or two is in memory at a time.
        let (requests, upload) = mpsc::channel(1);
        let feeder = tokio::spawn(feed(name, digest.size_bytes, chunks, requests));
        let mut client = self.bytestream.clone();
        let response = client.write(upload).await;
        // A chunk that could not be read ends the upload short; that is the error to
        // report, over the front's complaint about the short write.
        // A feeder that did not finish (a panic, a runtime shutting down) failed to read.
        let fed = feeder
            .await
            .map_err(std::io::Error::other)
            .map_err(Stop::Read)
            .and_then(std::convert::identity);
        if let Err(Stop::Read(e)) = fed {
            return Err(CasError::Read(label(&digest), e.to_string()));
        }
        let response = response
            .map_err(|status| call_error(&digest, &status))?
            .into_inner();
        committed(&digest, response.committed_size)
    }
}

/// Sends the WriteRequests that upload `chunks` as the blob `name`, `size` bytes long,
/// none carrying more than [`WRITE_CHUNK_BYTES`]. The first names the resource; the
/// one that reaches `size` bytes, or an empty one sent when the chunks end short of
/// it, finishes the write.
async fn feed(
    name: String,
    size: i64,
    chunks: Chunks,
    mut requests: mpsc::Sender<WriteRequest>,
) -> Result<(), Stop> {
    let mut pieces = chunks.flat_map(split);
    let mut offset = 0_i64;
    loop {
        let data = if offset >= size {
            Vec::new()
        } else {
            let next = pieces.next().await.transpose().map_err(Stop::Read)?;
            next.unwrap_or_default()
        };
        let next = offset + data.len() as i64;
        let finish = next >= size || data.is_empty();
        let request = WriteRequest {
            resource_name: if offset == 0 {
                name.clone()
            } else {
                String::new()
            },
            write_offset: offset,
            finish_write: finish,
            data,
        };
        requests.send(request).await.map_err(|_| Stop::Upload)?;
        if finish {
            return Ok(());
        }
        offset = next;
    }
}

/// Why [`feed`] stopped before the last message.
enum Stop {
    /// A chunk could not be read.
    Read(std::io::Error),
    /// The upload stopped taking messages: the front refused it, and its status says
    /// why.
    Upload,
}

/// `chunk` in pieces of at most [`WRITE_CHUNK_BYTES`]; an error stays one item.
fn split(
    chunk: std::io::Result<Vec<u8>>,
) -> stream::Iter<std::vec::IntoIter<std::io::Result<Vec<u8>>>> {
    let pieces: Vec<std::io::Result<Vec<u8>>> = match chunk {
        Ok(bytes) => bytes
            .chunks(WRITE_CHUNK_BYTES)
            .map(|piece| Ok(piece.to_vec()))
            .collect(),
        Err(e) => vec![Err(e)],
    };
    stream::iter(pieces)
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
