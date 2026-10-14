//! The REAPI backend: the engine's calls to kbf-server over its one connection.
//! ByteStream `Read` from an offset, ByteStream `Write` in messages of at most
//! `--transfer-chunk-bytes`, and FindMissing. Each function is one attempt; the engine
//! (`crate::engine`) holds the permits and retries.

use std::sync::atomic::{AtomicU64, Ordering};

use futures::channel::mpsc;
use futures::{SinkExt as _, StreamExt as _, stream};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, ReadResponse, WriteRequest};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{Digest, FindMissingBlobsRequest};
use kbf_proto::worker::segments_client::SegmentsClient;
use tonic::transport::Channel;
use tonic::{Code, Status, Streaming};

use crate::cas::{CasError, Chunks, label};

/// The largest gRPC message the engine decodes from the cache services: kbf-front's
/// `MAX_MESSAGE_BYTES`, a full 1 GiB batch plus room for the digests and statuses
/// around it. tonic's default of 4 MiB would refuse a full batch once framing is added.
pub const MAX_MESSAGE_BYTES: usize = (1 << 30) + (1 << 20);

/// The largest `Segments` message: kbf-front's `MAX_CONTROL_MESSAGE_BYTES`.
pub const MAX_CONTROL_MESSAGE_BYTES: usize = (4 << 20) + (1 << 20);

/// The clients over the engine's one connection. The server serves one cache per cell,
/// so resource names carry no instance name.
#[derive(Debug)]
pub(crate) struct Wire {
    bytestream: ByteStreamClient<Channel>,
    cas: ContentAddressableStorageClient<Channel>,
    segments: SegmentsClient<Channel>,
    uploads: AtomicU64,
    chunk_bytes: usize,
}

impl Wire {
    pub fn new(channel: Channel, chunk_bytes: usize) -> Self {
        Self {
            bytestream: ByteStreamClient::new(channel.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES),
            segments: crate::segments::client(channel.clone()),
            cas: ContentAddressableStorageClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES),
            uploads: AtomicU64::new(0),
            chunk_bytes,
        }
    }

    /// The `Segments` client over the engine's connection.
    pub fn segments(&self) -> SegmentsClient<Channel> {
        self.segments.clone()
    }

    /// The blob's bytes from `offset` on, as the server streams them.
    pub async fn read(
        &self,
        digest: &Digest,
        offset: i64,
    ) -> Result<Streaming<ReadResponse>, Status> {
        let request = ReadRequest {
            resource_name: format!("blobs/{}", label(digest)),
            read_offset: offset,
            read_limit: 0,
        };
        let mut client = self.bytestream.clone();
        Ok(client.read(request).await?.into_inner())
    }

    /// Writes `bytes`, the blob `digest` names, in messages of at most the chunk size.
    pub async fn write(&self, digest: &Digest, bytes: &[u8]) -> Result<i64, Status> {
        let name = self.upload_name(digest);
        let chunk = self.chunk_bytes;
        // One message per chunk; an empty blob is one message with no data.
        let count = bytes.len().div_ceil(chunk).max(1);
        let requests: Vec<WriteRequest> = (0..count)
            .map(|i| {
                let start = i * chunk;
                let end = bytes.len().min(start + chunk);
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
        let response = client.write(stream::iter(requests)).await?;
        Ok(response.into_inner().committed_size)
    }

    /// Writes the blob `digest` names from `chunks`, re-cut to the chunk size. A chunk
    /// that cannot be read is [`WriteFailed::Read`], reported over the server's
    /// complaint about the short write.
    pub async fn write_chunks(&self, digest: &Digest, chunks: Chunks) -> Result<i64, WriteFailed> {
        let name = self.upload_name(digest);
        // A task reads the chunks and hands the requests over a channel that holds
        // one, so a chunk or two is in memory at a time.
        let (requests, upload) = mpsc::channel(1);
        let feeder = tokio::spawn(feed(
            name,
            digest.size_bytes,
            chunks,
            self.chunk_bytes,
            requests,
        ));
        let mut client = self.bytestream.clone();
        let response = client.write(upload).await;
        // A feeder that did not finish (a panic, a runtime shutting down) failed to read.
        let fed = feeder
            .await
            .map_err(std::io::Error::other)
            .map_err(Stop::Read)
            .and_then(std::convert::identity);
        if let Err(Stop::Read(e)) = fed {
            return Err(WriteFailed::Read(e));
        }
        Ok(response
            .map_err(WriteFailed::Call)?
            .into_inner()
            .committed_size)
    }

    /// The digests among `digests` the server does not hold.
    pub async fn find_missing(&self, digests: Vec<Digest>) -> Result<Vec<Digest>, Status> {
        let request = FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: digests,
            digest_function: 0,
        };
        let mut client = self.cas.clone();
        let response = client.find_missing_blobs(request).await?;
        Ok(response.into_inner().missing_blob_digests)
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

/// Why a streamed write failed.
#[derive(Debug)]
pub(crate) enum WriteFailed {
    /// The call failed.
    Call(Status),
    /// A chunk could not be read.
    Read(std::io::Error),
}

/// Why [`feed`] stopped before the last message.
enum Stop {
    /// A chunk could not be read.
    Read(std::io::Error),
    /// The upload stopped taking messages: the server refused it, and its status says
    /// why.
    Upload,
}

/// Sends the WriteRequests that upload `chunks` as the blob `name`, `size` bytes long,
/// none carrying more than `chunk` bytes. The first names the resource; the one that
/// reaches `size` bytes, or an empty one sent when the chunks end short of it,
/// finishes the write.
async fn feed(
    name: String,
    size: i64,
    chunks: Chunks,
    chunk: usize,
    mut requests: mpsc::Sender<WriteRequest>,
) -> Result<(), Stop> {
    let mut pieces = chunks.flat_map(|c| split(c, chunk));
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

/// `chunk` in pieces of at most `most` bytes; an error stays one item.
fn split(
    chunk: std::io::Result<Vec<u8>>,
    most: usize,
) -> stream::Iter<std::vec::IntoIter<std::io::Result<Vec<u8>>>> {
    let pieces: Vec<std::io::Result<Vec<u8>>> = match chunk {
        Ok(bytes) => bytes.chunks(most).map(|piece| Ok(piece.to_vec())).collect(),
        Err(e) => vec![Err(e)],
    };
    stream::iter(pieces)
}

/// A failed call as a [`CasError`]: NOT_FOUND is a missing blob, anything else the
/// CAS failing.
pub(crate) fn call_error(digest: &Digest, status: &Status) -> CasError {
    if status.code() == Code::NotFound {
        CasError::Missing(label(digest))
    } else {
        CasError::Unavailable(
            label(digest),
            format!("{:?}: {}", status.code(), status.message()),
        )
    }
}

/// Whether a failed call is worth another attempt: UNAVAILABLE, and the codes tonic
/// gives a stream whose connection broke under it (UNKNOWN, INTERNAL and CANCELLED for
/// an HTTP/2 or socket error), and ABORTED. NOT_FOUND, a refusal and a bad request are
/// not retried.
pub(crate) fn retryable(status: &Status) -> bool {
    matches!(
        status.code(),
        Code::Unavailable | Code::Unknown | Code::Internal | Code::Cancelled | Code::Aborted
    )
}

/// `digest` if the CAS committed all of its bytes.
pub(crate) fn committed(digest: &Digest, size: i64) -> Result<Digest, CasError> {
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
    use crate::cas::digest_of;

    /// Catches a NOT_FOUND reported as an outage (the action would be retried as an
    /// infrastructure failure instead of failing on its missing input), and an outage
    /// reported as a missing blob.
    #[test]
    fn not_found_is_missing_and_the_rest_is_unavailable() {
        let digest = digest_of(b"x");
        assert_eq!(
            call_error(&digest, &Status::not_found("gone")),
            CasError::Missing(label(&digest))
        );
        assert_eq!(
            call_error(&digest, &Status::unavailable("down")),
            CasError::Unavailable(label(&digest), "Unavailable: down".to_owned())
        );
    }

    /// Catches a refusal retried (a missing blob or a denied call would cost every
    /// retry's backoff before failing as it must), and a broken connection not retried.
    #[test]
    fn only_outages_and_broken_streams_are_retried() {
        for code in [
            Code::Unavailable,
            Code::Unknown,
            Code::Internal,
            Code::Cancelled,
            Code::Aborted,
        ] {
            assert!(retryable(&Status::new(code, "")), "{code:?}");
        }
        for code in [
            Code::NotFound,
            Code::InvalidArgument,
            Code::PermissionDenied,
            Code::Unauthenticated,
            Code::FailedPrecondition,
            Code::Unimplemented,
            Code::DataLoss,
            Code::ResourceExhausted,
            Code::OutOfRange,
        ] {
            assert!(!retryable(&Status::new(code, "")), "{code:?}");
        }
    }
}
