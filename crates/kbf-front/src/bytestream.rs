//! `ByteStream`: Read with offset and limit, Write, and QueryWriteStatus for CAS blobs.
//!
//! Uploads follow one rule per stream (`docs/design/storage.md#writes`): bytes
//! arrive from offset 0 in order, are checked against the digest, become durable, and
//! only then is the write acknowledged. There is no partial resume: a stream that
//! breaks starts again from 0, so `QueryWriteStatus` reports a blob's full size once it
//! is durable and 0 before, never the bytes a live stream has buffered.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use kbf_auth::{Authorizers, authorize};
use kbf_objstore::ObjectStore;
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use tonic::{Request, Response, Status, Streaming};

use kbf_invocation::CallKind;

use crate::cache::{Cache, VerifiedBlob};
use crate::meta_log::MetaLog;
use crate::recorder::Recorder;
use crate::request_metadata::call_metadata;
use crate::wire::Resource;
use crate::{MAX_BLOB_BYTES, wire};

/// The `ByteStream` service over a [`Cache`]. Read is authorized by
/// [`Authorizers::cas_get`], Write and QueryWriteStatus by [`Authorizers::cas_put`],
/// each against the instance name before `blobs/` or `uploads/` in the resource name.
#[derive(Debug)]
pub struct ByteStreamService<M, O> {
    cache: Arc<Cache<M, O>>,
    authorizers: Arc<Authorizers>,
    recorder: Recorder,
}

impl<M, O> ByteStreamService<M, O> {
    /// The service over `cache`, every call allowed.
    #[must_use]
    pub fn new(cache: Arc<Cache<M, O>>) -> Self {
        Self::with_authorizers(cache, Arc::new(Authorizers::allow_all()))
    }

    /// The service over `cache`, each call authorized by `authorizers`.
    #[must_use]
    pub const fn with_authorizers(cache: Arc<Cache<M, O>>, authorizers: Arc<Authorizers>) -> Self {
        Self {
            cache,
            authorizers,
            recorder: Recorder::off(),
        }
    }

    /// This service, each Read and Write that carries an invocation id sent to
    /// `recorder`: the bytes read, and the bytes of each blob written.
    #[must_use]
    pub fn recording(mut self, recorder: Recorder) -> Self {
        self.recorder = recorder;
        self
    }
}

#[tonic::async_trait]
impl<M: MetaLog, O: ObjectStore + 'static> ByteStream for ByteStreamService<M, O> {
    type ReadStream = BoxStream<'static, Result<ReadResponse, Status>>;

    /// Streams `[read_offset, read_offset + read_limit)` of the blob (to the end when
    /// the limit is 0), in messages of at most [`crate::READ_CHUNK_BYTES`], from
    /// [`Cache::read_range`]: the blob is read in pieces and hashed as it goes, never
    /// held whole, and a stream that ends without an error carried only verified
    /// bytes. A blob found damaged after bytes were sent ends the stream with
    /// UNAVAILABLE (the bytes sent before it are not to be trusted), as one found
    /// before the first byte is the call's error. A range reads, and hashes, the whole
    /// blob (the cache keeps one digest per blob). Each message is one copy of its
    /// chunk, into the generated message's `Vec`.
    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let metadata = call_metadata(&request);
        let request = request.into_inner();
        let Resource { instance, digest } = wire::read_resource(&request.resource_name)?;
        let cas_get = &*self.authorizers.cas_get;
        authorize(
            cas_get,
            &caller,
            "/google.bytestream.ByteStream/Read",
            &instance,
        )
        .await?;
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
        let chunks = self.cache.read_range(&digest, offset, limit).await?;
        let end = match limit {
            0 => digest.size_bytes,
            n => digest.size_bytes.min(offset.saturating_add(n)),
        };
        self.recorder.call(&metadata, &caller, &instance, &|| {
            CallKind::Downloaded(end - offset)
        });
        let messages = chunks.map(|chunk| {
            let data = chunk?.to_vec();
            Ok(ReadResponse { data })
        });
        Ok(Response::new(messages.boxed()))
    }

    /// Receives a blob from offset 0, verifies it, stores it, and only then answers
    /// with its size. A blob already durable is answered at once. Past the first
    /// message, nothing is read until [`Cache::admit`] has admitted the blob's size.
    /// Each call is counted and timed in the cache's metrics (`kbf_bytestream_write_*`).
    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let mut timed = self.cache.metrics().write.start();
        let caller = kbf_auth::metadata(&request);
        let metadata = call_metadata(&request);
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("write stream sent no message"))?;
        let name = first.resource_name.clone();
        let Resource { instance, digest } = wire::write_resource(&name)?;
        let cas_put = &*self.authorizers.cas_put;
        authorize(
            cas_put,
            &caller,
            "/google.bytestream.ByteStream/Write",
            &instance,
        )
        .await?;
        let size = digest.size_bytes;
        let committed = |size: u64| {
            Response::new(WriteResponse {
                committed_size: i64::try_from(size).unwrap_or(i64::MAX),
            })
        };
        if size > MAX_BLOB_BYTES {
            return Err(Status::invalid_argument(format!(
                "{digest} is larger than the largest blob this cache accepts \
                 ({MAX_BLOB_BYTES} bytes)"
            )));
        }
        // The cheapest upload is the one we skip.
        if self.cache.is_durable(&digest).await? {
            self.cache.metrics().write_present.add(1);
            timed.ok();
            return Ok(committed(size));
        }
        // Admitted before anything past the first message is read, so every byte held
        // in `received` is counted under `--write-bytes-in-flight`; a Write over the
        // bound waits here. The first message is held like a batch, under the message
        // size limit.
        let admitted = self.cache.admit(size).await;
        let mut buffering = self.cache.buffering();
        // Allocated at the admitted size, so a growing buffer never holds more than
        // was admitted (`size` is at most `MAX_BLOB_BYTES`, checked above).
        let mut received: Vec<u8> = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
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
            buffering.add(message.data.len() as u64);
            timed.bytes(message.data.len() as u64);
            if message.finish_write {
                break;
            }
            message = stream.message().await?.ok_or_else(|| {
                Status::invalid_argument("write stream ended without finish_write")
            })?;
        }
        let received = Bytes::from(received);
        // The receive buffer is now the blob's bytes, held under `admitted` until the
        // cache has copied them into a segment.
        drop(buffering);
        let blob = VerifiedBlob::new(digest, received)?;
        self.cache.store_admitted(vec![blob], admitted).await?;
        self.recorder
            .call(&metadata, &caller, &instance, &|| CallKind::Uploaded(size));
        timed.ok();
        Ok(committed(size))
    }

    /// The blob's full size and `complete` once it is durable; 0 and incomplete before.
    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let Resource { instance, digest } =
            wire::write_resource(&request.into_inner().resource_name)?;
        let call = "/google.bytestream.ByteStream/QueryWriteStatus";
        authorize(&*self.authorizers.cas_put, &caller, call, &instance).await?;
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
