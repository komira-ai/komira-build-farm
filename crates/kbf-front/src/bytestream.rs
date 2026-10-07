//! `ByteStream`: Read with offset and limit, Write, and QueryWriteStatus for CAS blobs.
//!
//! Uploads follow the RFC's one-state-machine-per-stream rule (section 9.4): bytes
//! arrive from offset 0 in order, are checked against the digest, become durable, and
//! only then is the write acknowledged. There is no partial resume: a stream that
//! breaks starts again from 0, so `QueryWriteStatus` reports a blob's full size once it
//! is durable and 0 before, never the bytes a live stream has buffered.

use std::sync::Arc;

use bytes::Bytes;
use futures::stream;
use kbf_objstore::ObjectStore;
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use tonic::{Request, Response, Status, Streaming};

use crate::cache::{Cache, VerifiedBlob};
use crate::meta_log::MetaLog;
use crate::{READ_CHUNK_BYTES, wire};

/// The `ByteStream` service over a [`Cache`].
#[derive(Debug)]
pub struct ByteStreamService<M, O> {
    cache: Arc<Cache<M, O>>,
}

impl<M, O> ByteStreamService<M, O> {
    /// The service over `cache`.
    pub const fn new(cache: Arc<Cache<M, O>>) -> Self {
        Self { cache }
    }
}

#[tonic::async_trait]
impl<M: MetaLog, O: ObjectStore + 'static> ByteStream for ByteStreamService<M, O> {
    type ReadStream = stream::Iter<std::vec::IntoIter<Result<ReadResponse, Status>>>;

    /// Streams `[read_offset, read_offset + read_limit)` of the blob (to the end when
    /// the limit is 0), in messages of at most [`READ_CHUNK_BYTES`]. The whole blob is
    /// read and verified first, so no unverified byte is sent.
    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let request = request.into_inner();
        let digest = wire::read_resource(&request.resource_name)?;
        let offset = u64::try_from(request.read_offset).map_err(|_| {
            Status::out_of_range(format!("read_offset {} is negative", request.read_offset))
        })?;
        let limit = u64::try_from(request.read_limit).map_err(|_| {
            Status::invalid_argument(format!("read_limit {} is negative", request.read_limit))
        })?;
        if offset > digest.size_bytes {
            return Err(Status::out_of_range(format!(
                "read_offset {offset} is past the end of {digest}"
            )));
        }
        let blob = self.cache.read_blob(&digest).await?;
        let end = match limit {
            0 => digest.size_bytes,
            n => digest.size_bytes.min(offset.saturating_add(n)),
        };
        // Both bounds are at most the blob's length, which fits in memory.
        let wanted = blob.slice(offset as usize..end as usize);
        let messages: Vec<Result<ReadResponse, Status>> = wanted
            .chunks(READ_CHUNK_BYTES)
            .map(|c| Ok(ReadResponse { data: c.to_vec() }))
            .collect();
        Ok(Response::new(stream::iter(messages)))
    }

    /// Receives a blob from offset 0, verifies it, stores it, and only then answers
    /// with its size. A blob already durable is answered at once.
    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("write stream sent no message"))?;
        let name = first.resource_name.clone();
        let digest = wire::write_resource(&name)?;
        let size = digest.size_bytes;
        let committed = |size: u64| {
            Response::new(WriteResponse {
                committed_size: i64::try_from(size).unwrap_or(i64::MAX),
            })
        };
        // The cheapest upload is the one we skip (RFC 9.4).
        if self.cache.is_durable(&digest).await? {
            return Ok(committed(size));
        }
        let mut received: Vec<u8> = Vec::new();
        let mut message = first;
        loop {
            if !message.resource_name.is_empty() && message.resource_name != name {
                return Err(Status::invalid_argument(format!(
                    "write stream changed resource from {name:?} to {:?}",
                    message.resource_name
                )));
            }
            if u64::try_from(message.write_offset).ok() != Some(received.len() as u64) {
                return Err(Status::invalid_argument(format!(
                    "write_offset {} but {} bytes were received; uploads start at 0 and \
                     do not resume",
                    message.write_offset,
                    received.len()
                )));
            }
            if (received.len() + message.data.len()) as u64 > size {
                return Err(Status::invalid_argument(format!(
                    "more than the {size} bytes {digest} names"
                )));
            }
            received.extend_from_slice(&message.data);
            if message.finish_write {
                break;
            }
            message = stream.message().await?.ok_or_else(|| {
                Status::invalid_argument("write stream ended without finish_write")
            })?;
        }
        let blob = VerifiedBlob::new(digest, Bytes::from(received))?;
        self.cache.store_blobs(vec![blob]).await?;
        Ok(committed(size))
    }

    /// The blob's full size and `complete` once it is durable; 0 and incomplete before.
    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        let digest = wire::write_resource(&request.into_inner().resource_name)?;
        let complete = self.cache.is_durable(&digest).await?;
        let committed_size = if complete {
            i64::try_from(digest.size_bytes).unwrap_or(i64::MAX)
        } else {
            0
        };
        Ok(Response::new(QueryWriteStatusResponse {
            committed_size,
            complete,
        }))
    }
}
