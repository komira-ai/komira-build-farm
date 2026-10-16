//! `ByteStream`: Read with offset and limit, Write, and QueryWriteStatus for CAS blobs.
//!
//! Uploads follow one rule per stream (`docs/design/storage.md#writes`): bytes
//! arrive from offset 0 in order, are checked against the digest, become durable, and
//! only then is the write acknowledged. There is no partial resume: a stream that
//! breaks starts again from 0, so `QueryWriteStatus` reports a blob's full size once it
//! is durable and 0 before, never the bytes a live stream has buffered.
//!
//! No blob is too large, and none is held whole: a blob of at most
//! [`MAX_UNSTAGED_BLOB_BYTES`] is received into memory and stored with others; a larger
//! one is written as its messages arrive onto the write tier when it has room for it
//! ([`Cache::stage_streamed`]), else to the store ([`Cache::store_streamed`]), and a
//! Read is served in windows of [`READ_WINDOW_BYTES`](crate::READ_WINDOW_BYTES)
//! ([`Cache::read_windows`]).

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use kbf_auth::{Authorizers, authorize};
use kbf_objstore::{BodyStream, ObjectStore, ObjectStoreError};
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use tonic::{Request, Response, Status, Streaming};

use kbf_invocation::CallKind;

use crate::recorder::Recorder;
use crate::request_metadata::call_metadata;
use crate::{MAX_UNSTAGED_BLOB_BYTES, READ_CHUNK_BYTES};
use kbf_storage::MetaLog;
use kbf_storage::wire;
use kbf_storage::wire::Resource;
use kbf_storage::{Cache, VerifiedBlob};

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
    type ReadStream = Pin<Box<dyn Stream<Item = Result<ReadResponse, Status>> + Send>>;

    /// Streams `[read_offset, read_offset + read_limit)` of the blob (to the end when
    /// the limit is 0), in messages of at most [`READ_CHUNK_BYTES`], read in windows
    /// ([`Cache::read_windows`]): every byte of the blob is read and hashed, and the
    /// last window is sent only once the digest checked, so a blob of one window is
    /// checked whole before a byte is sent. A larger blob whose bytes fail the digest
    /// ends its stream UNAVAILABLE in place of its last window. Each message is cut
    /// from its window as the stream is polled, so an open Read holds one window and
    /// one message.
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
        let end = match limit {
            0 => digest.size_bytes,
            n => digest.size_bytes.min(offset.saturating_add(n)),
        };
        let read = self.cache.read_windows(&digest, offset, end).await?;
        self.recorder.call(&metadata, &caller, &instance, &|| {
            CallKind::Downloaded(read.range_len())
        });
        // Each message is cut from the window when it is asked for: one Read holds its
        // window and the message being sent, not every message of the window at once.
        let messages = stream::unfold((read, Bytes::new()), |(mut read, mut window)| async move {
            while window.is_empty() {
                match read.next().await? {
                    Ok(next) => window = next,
                    Err(e) => return Some((Err(Status::from(e)), (read, Bytes::new()))),
                }
            }
            let message = window.split_to(window.len().min(READ_CHUNK_BYTES));
            let message = ReadResponse {
                data: message.to_vec(),
            };
            Some((Ok(message), (read, window)))
        });
        Ok(Response::new(Box::pin(messages)))
    }

    /// Receives a blob from offset 0, verifies it, stores it, and only then answers
    /// with its size. A blob already durable is answered at once. Past the first
    /// message, nothing is read until [`Cache::admit`] has admitted what the write
    /// holds: the blob's size for one of at most [`MAX_UNSTAGED_BLOB_BYTES`], received
    /// whole; [`MAX_UNSTAGED_BLOB_BYTES`] for a larger one, whose messages are written
    /// onto the write tier (or, with no tier or no room on it, to the store) as they
    /// arrive, hashed as they pass, and stored only if they hash to the digest. Each call is counted and timed in the
    /// cache's metrics (`kbf_bytestream_write_*`).
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
        // The cheapest upload is the one we skip.
        if self.cache.is_durable(&digest).await? {
            self.cache.metrics().write_present.add(1);
            timed.ok();
            return Ok(committed(size));
        }
        if size > MAX_UNSTAGED_BLOB_BYTES {
            self.write_streamed(first, stream, digest).await?;
            timed.bytes(size);
        } else {
            self.write_whole(first, stream, digest, &timed).await?;
        }
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

impl<M: MetaLog, O: ObjectStore + 'static> ByteStreamService<M, O> {
    /// Receives a blob of more than [`MAX_UNSTAGED_BLOB_BYTES`] from `first` on and
    /// stores it as its messages arrive, admitted for [`MAX_UNSTAGED_BLOB_BYTES`]:
    /// onto the write tier when it has room for it ([`Cache::stage_streamed`], a
    /// message here, flushed to the store later), else straight to the store
    /// ([`Cache::store_streamed`], a message here, a part in the store). Either way the
    /// bytes are hashed as they pass and the blob is present only if they hash to its
    /// digest.
    async fn write_streamed(
        &self,
        first: WriteRequest,
        stream: Streaming<WriteRequest>,
        digest: kbf_types::Digest,
    ) -> Result<(), Status> {
        let admitted = self.cache.admit(MAX_UNSTAGED_BLOB_BYTES).await;
        let refused = Arc::new(Mutex::new(None));
        let body = messages(first, stream, digest.size_bytes, Arc::clone(&refused));
        // Onto the write tier when it has room for the blob (flushed to the store
        // later), else straight to the store.
        let cache = Arc::clone(&self.cache);
        let size = digest.size_bytes;
        let onto_tier = tokio::task::spawn_blocking(move || cache.stream(size)).await;
        let stored = match onto_tier.ok().flatten() {
            Some(stream) => self.cache.stage_streamed(digest, stream, body).await,
            None => self.cache.store_streamed(digest, body).await,
        };
        drop(admitted);
        let refused = refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        refused.map_or(Ok(()), Err)?;
        Ok(stored?)
    }

    /// Receives a blob of at most [`MAX_UNSTAGED_BLOB_BYTES`] from `first` on into
    /// memory, admitted for its size before anything past `first` is read, verifies it
    /// and stores it with others.
    async fn write_whole(
        &self,
        first: WriteRequest,
        mut stream: Streaming<WriteRequest>,
        digest: kbf_types::Digest,
        timed: &crate::metrics::Request<'_>,
    ) -> Result<(), Status> {
        let (name, size) = (first.resource_name.clone(), digest.size_bytes);
        // Admitted before anything past the first message is read, so every byte held
        // in `received` is counted under `--write-bytes-in-flight`; a Write over the
        // bound waits here. The first message is held like a batch, under the message
        // size limit.
        let admitted = self.cache.admit(size).await;
        let mut buffering = self.cache.buffering();
        // Allocated at the admitted size, so a growing buffer never holds more than
        // was admitted (`size` is at most `MAX_UNSTAGED_BLOB_BYTES`, checked above).
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
        Ok(())
    }
}

/// A `Write`'s messages from `first` on as the bytes of a blob of `size`: each message
/// checked as the in-memory path checks it (the same resource, the next offset, no more
/// than `size` bytes in all) and its data handed on, until `finish_write`. The first
/// refusal is left in `refused` and ends the bytes with an error.
fn messages(
    first: WriteRequest,
    stream: Streaming<WriteRequest>,
    size: u64,
    refused: Arc<Mutex<Option<Status>>>,
) -> BodyStream {
    struct Receiving {
        next: Option<WriteRequest>,
        stream: Streaming<WriteRequest>,
        name: String,
        received: u64,
        finished: bool,
    }
    let refuse = move |status: Status| {
        let why = status.message().to_owned();
        refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(status);
        Err(ObjectStoreError::Body(why))
    };
    let receiving = Receiving {
        name: first.resource_name.clone(),
        next: Some(first),
        stream,
        received: 0,
        finished: false,
    };
    stream::unfold(receiving, move |mut r| {
        let refuse = refuse.clone();
        async move {
            if r.finished {
                return None;
            }
            let message = match r.next.take() {
                Some(message) => message,
                None => match r.stream.message().await {
                    Ok(Some(message)) => message,
                    // The stream ended without `finish_write`, or broke.
                    ended => {
                        r.finished = true;
                        let ended = ended.err().unwrap_or_else(|| {
                            Status::invalid_argument("write stream ended without finish_write")
                        });
                        return Some((refuse(ended), r));
                    }
                },
            };
            let wrong = if !message.resource_name.is_empty() && message.resource_name != r.name {
                Some(format!(
                    "write stream changed resource from {:?} to {:?}",
                    r.name, message.resource_name
                ))
            } else if u64::try_from(message.write_offset).ok() != Some(r.received) {
                Some(format!(
                    "write_offset {} but {} bytes were received; uploads start at 0 and \
                     do not resume",
                    message.write_offset, r.received
                ))
            } else if r.received + message.data.len() as u64 > size {
                Some(format!("more than the {size} bytes the resource names"))
            } else {
                None
            };
            if let Some(why) = wrong {
                r.finished = true;
                return Some((refuse(Status::invalid_argument(why)), r));
            }
            r.received += message.data.len() as u64;
            r.finished = message.finish_write;
            Some((Ok(Bytes::from(message.data)), r))
        }
    })
    .boxed()
}
