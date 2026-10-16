//! [`RemoteStorage`]: a [`Storage`] over a gRPC connection to a storage node, so a
//! caller keeps holding `Arc<dyn Storage>` whether the cache is in its process or not.
//! This is the storage client the frontend (and, over a map, the daemon) uses.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::{StreamExt as _, TryStreamExt as _, stream};
use kbf_meta::{ActionRecord, ActionWriteError, Closure, Role};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{ActionResult, FindMissingBlobsRequest};
use kbf_proto::storage::action_store_client::ActionStoreClient;
use kbf_proto::storage::{
    CommitRequest, GetRequest, PrepareRequest, Record, Refusal, Writer, prepare_response, refusal,
};
use kbf_proto::worker::segments_client::SegmentsClient;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest as SegmentsCommitRequest,
    CommitResponse as SegmentsCommitResponse, LocateRequest, LocateResponse, PutRangeRequest,
    PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use kbf_types::Digest;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};

use super::{MAP_GENERATION_HEADER, MAX_MESSAGE_BYTES, wire};
use crate::{Answer, PutStream, ReadStream, Storage, StorageError};

/// The largest message of a `Write`'s data.
const WRITE_CHUNK_BYTES: usize = 1 << 20;

/// A storage node, called over `channel`. Cheap to clone.
#[derive(Clone, Debug)]
pub struct RemoteStorage {
    channel: Channel,
    /// Shared by every clone (and so by the `Segments` handle), read once per call.
    generation: Arc<AtomicU64>,
    uploads: Arc<AtomicU64>,
}

/// What a storage node said when it failed a call, as an error.
#[derive(Debug, thiserror::Error)]
#[error("storage node: {0}")]
struct Remote(String);

impl RemoteStorage {
    /// The node `channel` reaches, every call tagged with map `generation` (0: no map
    /// seen; a one-member farm has none).
    #[must_use]
    pub fn new(channel: Channel, generation: u64) -> Self {
        Self {
            channel,
            generation: Arc::new(AtomicU64::new(generation)),
            uploads: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Tags every call made from now on with map `generation`, on this handle and on
    /// every clone of it (a map update reaches the one the caller holds). A call already
    /// begun keeps the generation it read.
    pub fn set_map_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Relaxed);
    }

    /// The generation a call made now carries.
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// `message` as a request carrying the map generation.
    fn tag<T>(&self, message: T) -> Request<T> {
        self.tag_at(message, self.generation())
    }

    /// `message` as a request carrying `generation`: for a call whose message carries it
    /// too, so the field and the header are one read of the generation.
    fn tag_at<T>(&self, message: T, generation: u64) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert(MAP_GENERATION_HEADER, MetadataValue::from(generation));
        request
    }

    fn bytestream(&self) -> ByteStreamClient<Channel> {
        ByteStreamClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    fn segments_client(&self) -> SegmentsClient<Channel> {
        SegmentsClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    fn actions(&self) -> ActionStoreClient<Channel> {
        ActionStoreClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }
}

/// The error a failed call is. The codes a [`Storage`] answers are kept (`at` is the
/// blob and offset of a read, which NOT_FOUND and OUT_OF_RANGE are about): a caller sees
/// what an in-process store would have said. UNAVAILABLE and every code that says
/// nothing of the request (a transport failure, a deadline) are `Unavailable`; a
/// store that is held but unreadable is UNAVAILABLE too, so it is `Unavailable` here
/// where it was `Unreachable` there.
pub(super) fn lift(status: &Status, at: Option<(Digest, u64)>) -> StorageError {
    let message = status.message().to_owned();
    match (status.code(), at) {
        (Code::NotFound, Some((digest, _))) => StorageError::NotFound(digest),
        (Code::OutOfRange, Some((digest, offset))) => StorageError::OutOfRange { digest, offset },
        (Code::InvalidArgument | Code::PermissionDenied | Code::Unauthenticated, _) => {
            StorageError::Invalid(message)
        }
        (Code::Internal | Code::Unimplemented | Code::NotFound | Code::OutOfRange, _) => {
            StorageError::Internal(message)
        }
        _ => StorageError::Unavailable(Arc::new(Remote(message))),
    }
}

/// [`lift`] for a call that is not about one blob.
fn other(status: Status) -> StorageError {
    lift(&status, None)
}

/// The refusal a node answered, as the store's error.
pub(super) fn refused(refusal: &Refusal) -> StorageError {
    let blob = wire::digest(refusal.blob.as_ref());
    let error = match (refusal::Kind::try_from(refusal.kind), blob) {
        (Ok(refusal::Kind::NotDaemon), _) => ActionWriteError::NotDaemon,
        (Ok(refusal::Kind::Absent), Ok(blob)) => ActionWriteError::Absent(blob),
        (Ok(refusal::Kind::Unreachable), Ok(blob)) => ActionWriteError::Unreachable(blob),
        _ => {
            return StorageError::Internal(format!("an unreadable refusal {refusal:?}"));
        }
    };
    error.into()
}

fn record_to_proto(record: &ActionRecord) -> Record {
    Record {
        result: Some(wire::to_proto(&record.result)),
        closure: record.closure.iter().map(wire::to_proto).collect(),
    }
}

impl Storage for RemoteStorage {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async move {
            let request = FindMissingBlobsRequest {
                instance_name: String::new(),
                blob_digests: digests.iter().map(wire::to_proto).collect(),
                digest_function: 0,
            };
            let mut client = ContentAddressableStorageClient::new(self.channel.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES);
            let response = client
                .find_missing_blobs(self.tag(request))
                .await
                .map_err(other)?;
            response
                .into_inner()
                .missing_blob_digests
                .iter()
                .map(|d| wire::digest(Some(d)))
                .collect::<Result<Vec<_>, _>>()
                .map_err(other)
        })
    }

    /// The whole blob, collected from the stream of [`Storage::read_range`]; its length
    /// is checked against the digest's size.
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async move {
            let chunks: Vec<Bytes> = Storage::read_range(self, digest, 0, 0)
                .await?
                .try_collect()
                .await?;
            let blob = chunks.concat();
            if blob.len() as u64 != digest.size_bytes {
                return Err(StorageError::Internal(format!(
                    "{digest} came back as {} bytes",
                    blob.len()
                )));
            }
            Ok(Bytes::from(blob))
        })
    }

    /// The node verifies the blob as a whole before its first byte (the trait's rule);
    /// this is that stream, a chunk per message.
    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            let at = Some((*digest, offset));
            let request = ReadRequest {
                resource_name: format!("blobs/{}/{}", digest.hash_hex(), digest.size_bytes),
                read_offset: i64::try_from(offset).unwrap_or(i64::MAX),
                read_limit: i64::try_from(limit).unwrap_or(i64::MAX),
            };
            let response = self
                .bytestream()
                .read(self.tag(request))
                .await
                .map_err(|s| lift(&s, at))?;
            Ok(response
                .into_inner()
                .map(move |message| match message {
                    Ok(read) => Ok(Bytes::from(read.data)),
                    Err(status) => Err(lift(&status, at)),
                })
                .boxed())
        })
    }

    /// Hashed here, sent in messages of at most 1 MiB, and answered once the node holds
    /// it.
    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        Box::pin(async move {
            let digest = kbf_segments::sha256(&bytes);
            let upload = self.uploads.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "uploads/storage-client-{}-{upload}/blobs/{}/{}",
                std::process::id(),
                digest.hash_hex(),
                digest.size_bytes
            );
            // One message per chunk; an empty blob is one message with no data.
            let count = bytes.len().div_ceil(WRITE_CHUNK_BYTES).max(1);
            let requests: Vec<WriteRequest> = (0..count)
                .map(|i| {
                    let start = i * WRITE_CHUNK_BYTES;
                    WriteRequest {
                        resource_name: if i == 0 { name.clone() } else { String::new() },
                        write_offset: start as i64,
                        finish_write: i + 1 == count,
                        data: bytes[start..bytes.len().min(start + WRITE_CHUNK_BYTES)].to_vec(),
                    }
                })
                .collect();
            let response = self
                .bytestream()
                .write(self.tag(stream::iter(requests)))
                .await
                .map_err(other)?;
            let committed = response.into_inner().committed_size;
            if committed as u64 != digest.size_bytes {
                return Err(StorageError::Internal(format!(
                    "{digest} was acknowledged as {committed} bytes"
                )));
            }
            Ok(digest)
        })
    }

    /// Always `None`: a streamed write over the wire is [`Storage::put`]'s messages.
    fn stream(&self, _size: u64) -> Option<Box<dyn PutStream>> {
        None
    }

    /// Refused: the service has no streamed write to the object store; a caller stores
    /// the blob with [`Storage::put`].
    fn put_streamed(
        &self,
        _size: u64,
        _bytes: futures::stream::BoxStream<'static, std::io::Result<Bytes>>,
        _claim: futures::future::BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        Box::pin(async {
            Err(StorageError::Invalid(
                "this store takes no streamed put".to_owned(),
            ))
        })
    }

    fn segments(&self) -> Arc<dyn Segments> {
        Arc::new(self.clone())
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        Box::pin(async move {
            let generation = self.generation();
            let request = GetRequest {
                map_generation: generation,
                action: Some(wire::to_proto(action)),
            };
            let response = self
                .actions()
                .get(self.tag_at(request, generation))
                .await
                .map_err(other)?;
            let Some(entry) = response.into_inner().entry else {
                return Ok(None);
            };
            let digest = wire::digest(entry.result_digest.as_ref()).map_err(other)?;
            let result = entry.result.unwrap_or_default();
            Ok(Some((digest, result)))
        })
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        Box::pin(async move {
            let generation = self.generation();
            let request = PrepareRequest {
                map_generation: generation,
                result: Some(result.clone()),
            };
            let response = self
                .actions()
                .prepare(self.tag_at(request, generation))
                .await
                .map_err(other)?;
            match response.into_inner().outcome {
                Some(prepare_response::Outcome::Record(record)) => Ok(ActionRecord {
                    result: wire::digest(record.result.as_ref()).map_err(other)?,
                    closure: record
                        .closure
                        .iter()
                        .map(|d| wire::digest(Some(d)))
                        .collect::<Result<Closure, _>>()
                        .map_err(other)?,
                }),
                Some(prepare_response::Outcome::Refused(refusal)) => Err(refused(&refusal)),
                None => Err(StorageError::Internal("an empty Prepare answer".to_owned())),
            }
        })
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        Box::pin(async move {
            let writer = match role {
                Role::Daemon => Writer::Daemon,
                Role::Client => Writer::Client,
            };
            let generation = self.generation();
            let request = CommitRequest {
                map_generation: generation,
                action: Some(wire::to_proto(&action)),
                record: Some(record_to_proto(&record)),
                writer: writer as i32,
            };
            let response = self
                .actions()
                .commit(self.tag_at(request, generation))
                .await
                .map_err(other)?;
            match response.into_inner().refused {
                None => Ok(()),
                Some(refusal) => Err(refused(&refusal)),
            }
        })
    }
}

/// The `Segments` calls of a node, forwarded: what [`Storage::segments`] is for a
/// remote store. The uploads are the node's, each named by the id it answered.
#[tonic::async_trait]
impl Segments for RemoteStorage {
    async fn begin(
        &self,
        request: Request<BeginRequest>,
    ) -> Result<Response<BeginResponse>, Status> {
        let mut client = self.segments_client();
        client.begin(self.tag(request.into_inner())).await
    }

    async fn put_range(
        &self,
        request: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        let mut client = self.segments_client();
        client.put_range(self.tag(request.into_inner())).await
    }

    async fn commit(
        &self,
        request: Request<SegmentsCommitRequest>,
    ) -> Result<Response<SegmentsCommitResponse>, Status> {
        let mut client = self.segments_client();
        client.commit(self.tag(request.into_inner())).await
    }

    async fn locate(
        &self,
        request: Request<LocateRequest>,
    ) -> Result<Response<LocateResponse>, Status> {
        let mut client = self.segments_client();
        client.locate(self.tag(request.into_inner())).await
    }

    async fn read_range(
        &self,
        request: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        let mut client = self.segments_client();
        client.read_range(self.tag(request.into_inner())).await
    }
}
