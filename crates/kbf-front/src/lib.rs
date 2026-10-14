//! The front end: the gRPC services that clients and daemons talk to,
//! authentication, and the HTTP endpoints for health and status.
//!
//! The cache half: `Capabilities`, `ContentAddressableStorage`, `ByteStream` and
//! `ActionCache`, served by [`routes`] over one [`Cache`]. The execution half adds
//! `Execution` over a [`Dispatch`], the scheduler's seam ([`routes_with_execution`]). The cache keeps its
//! index and action cache in a [`MetaLog`] and its bytes in a
//! [`kbf_objstore::ObjectStore`], packed into `kbf-segments` segments. Both are traits:
//! [`Cache::memory`] runs them in this process ([`MemoryMetaLog`] and the fake store),
//! and `kbf-server`'s Raft log and the S3 store slot in behind the same traits.
//!
//! The promises the services keep (`ARCHITECTURE.md#cache-traffic` and
//! `docs/design/storage.md`):
//! - **FindMissingBlobs never omits a digest.** A digest that does not parse fails the
//!   call; a blob held at an unreachable object is reported missing.
//! - **Present means durable.** A blob is reported present, and an upload acknowledged,
//!   only after its bytes are in the store and committed to the index;
//!   `QueryWriteStatus` never reports bytes a stream has merely buffered.
//! - **Never serve a hit whose files are missing.** `GetActionResult` answers through
//!   the closure check; a missing or unreachable output turns the hit into a miss.
//! - **Every byte is verified,** on upload and on read.
//! - **Clients never write the action cache:** `UpdateActionResult` is
//!   PERMISSION_DENIED.
//! - **Work never run:** Execute answers a hit from the action cache and joins a
//!   running twin before anything is queued
//!   (`ARCHITECTURE.md#executing-an-action`).
//!
//! **Authorization.** Every service asks a `kbf_auth` authorizer before it serves a
//! call ([`routes_with_authorizers`]; which call asks which is on each service and in
//! `docs/reapi-auth.md`). The caller is whoever the server's authentication layer
//! ([`kbf_auth::AuthenticateLayer`]) found, read from the request's extensions; with no
//! layer it is empty metadata. [`routes`] and [`routes_with_execution`] allow every
//! call.
//!
//! **The build a call belongs to.** [`RequestMetadataLayer`], beside the
//! authentication layer, reads each call's REAPI `RequestMetadata` header into a
//! [`CallMetadata`] ([`call_metadata`]); its `tool_invocation_id` is the invocation id
//! (`docs/design/invocations.md`). A bad header is treated as absent and never fails a
//! call. With a [`Recorder`] ([`routes_recorded`]), each call that carries an id is
//! recorded: Execute (its cached, joined or queued answer), GetActionResult (hit or
//! miss), FindMissingBlobs, and the bytes of the batch and ByteStream calls. Recording
//! never waits: a full channel drops the event and marks its record incomplete.

mod action_cache;
mod bytestream;
mod cache;
mod capabilities;
mod cas;
mod execution;
mod meta_log;
pub mod metrics;
mod recorder;
mod request_metadata;
mod segments;
mod wire;

use std::sync::Arc;

use kbf_auth::Authorizers;
use kbf_objstore::ObjectStore;
use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use kbf_proto::reapi::action_cache_server::ActionCacheServer;
use kbf_proto::reapi::capabilities_server::CapabilitiesServer;
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
use kbf_proto::reapi::execution_server::ExecutionServer;
use tonic::service::Routes;

pub use crate::action_cache::ActionCacheService;
pub use crate::bytestream::ByteStreamService;
pub use crate::cache::{
    Admitted, BATCH_READS_IN_FLIGHT, CONNECTION_WINDOW_BYTES, Cache, CacheError,
    FLUSH_BACKOFF_FIRST, FLUSH_BACKOFF_MAX, FLUSHES_PER_ROUND, FlushRound, MAX_ADMISSION_BYTES,
    MAX_CONCURRENT_STREAMS, MESSAGE_BYTES_IN_FLIGHT, PUT_BACKOFF, PUT_BACKOFF_FACTOR,
    READ_BYTES_IN_FLIGHT, READ_COPIES, ResponseHold, STREAM_WINDOW_BYTES, TierStartError,
    TierStartup, TouchConfig, VerifiedBlob, WriteConfig,
};
pub use crate::capabilities::{CapabilitiesService, server_capabilities};
pub use crate::cas::CasService;
pub use crate::execution::{
    BOOK_CPUS_KEY, BOOK_MEM_GIB_KEY, Closer, Closing, DEFAULT_RESOURCES, Dispatch, ERROR_DOMAIN,
    ExecutionService, Finished, GPU_KEY, LEASE_KIND_KEY, LEASE_KINDS, NO_WORKER_REASON,
    OperationStream, Stage, Submission, Ticket, closing, missing_status,
};
pub use crate::meta_log::{Leadership, MemoryMetaLog, MetaLog, MetaLogError};
pub use crate::recorder::{
    DEFAULT_INVOCATION_MEMORY, ExecuteCall, RECORDER_CHANNEL, Recorder, Recording, listing, owner,
    wall_now,
};
pub use crate::request_metadata::{
    CallMetadata, HEADER as REQUEST_METADATA_HEADER, Invalid, InvocationId, MAX_HEADER_BYTES,
    MAX_ID_BYTES, MAX_TEXT_BYTES, Read, ReadRequestMetadata, RequestMetadataLayer, Text,
    call_metadata,
};
pub use crate::segments::{
    Connection, Connections, MAX_RANGE_BYTES, MAX_UPLOAD_RECORDS, MIN_SEGMENT_UPLOAD_BYTES,
    SegmentsConfig, SegmentsService, Tracked,
};

/// The most blob data one `BatchUpdateBlobs` or `BatchReadBlobs` call may carry,
/// advertised in the capabilities. Larger blobs go through ByteStream. A client takes
/// the smaller of this and its own setting, so this is a ceiling, not a batch size:
/// what a batch of this size holds in memory is bounded by [`MESSAGE_BYTES_IN_FLIGHT`]
/// and [`READ_BYTES_IN_FLIGHT`], not by this.
pub const MAX_BATCH_TOTAL_BYTES: usize = 1 << 30;

/// The largest gRPC message the cache services (CAS and ByteStream) accept or send: a
/// full batch plus room for the digests and statuses around it. It stays under the
/// 2 GiB a protobuf message cannot reach.
pub const MAX_MESSAGE_BYTES: usize = MAX_BATCH_TOTAL_BYTES + (1 << 20);

/// The largest gRPC message the services that carry no blob batches (Execution, Worker,
/// Segments) accept or send. Their messages are small; a limit of the cache's size here
/// would let a stream of one of them make the server hold a gigabyte for nothing.
pub const MAX_CONTROL_MESSAGE_BYTES: usize = (4 << 20) + (1 << 20);

/// The largest blob the cache accepts. A ByteStream Write is held in memory until it
/// is verified, so without a cap a client could make the front buffer whatever size
/// its resource name claims. Chunked uploads (planned,
/// `docs/design/storage.md#segments`) come later and will stream larger blobs instead;
/// until then anything bigger is refused before a byte is buffered.
pub const MAX_BLOB_BYTES: u64 = 1 << 30;

/// The most data in one ByteStream `ReadResponse`.
pub const READ_CHUNK_BYTES: usize = 1 << 20;

/// The four cache services over `cache`, ready for a tonic server. Every call is
/// allowed.
pub fn routes<M, O>(cache: Arc<Cache<M, O>>) -> Routes
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let authorizers = Arc::new(Authorizers::allow_all());
    cache_routes(
        cache,
        CapabilitiesService::cache_only(),
        &authorizers,
        &Recorder::off(),
    )
}

/// The cache services and `Execution` over `cache` and `dispatch`, with capabilities
/// that advertise execution. Open Execute and WaitExecution streams end UNAVAILABLE
/// when `closing`'s [`Closer`] closes. Every call is allowed.
pub fn routes_with_execution<M, O, D>(
    cache: Arc<Cache<M, O>>,
    dispatch: Arc<D>,
    closing: Closing,
) -> Routes
where
    M: MetaLog,
    O: ObjectStore + 'static,
    D: Dispatch,
{
    routes_with_authorizers(cache, dispatch, closing, Arc::new(Authorizers::allow_all()))
}

/// [`routes_with_execution`], each call authorized by `authorizers` before it is
/// served.
pub fn routes_with_authorizers<M, O, D>(
    cache: Arc<Cache<M, O>>,
    dispatch: Arc<D>,
    closing: Closing,
    authorizers: Arc<Authorizers>,
) -> Routes
where
    M: MetaLog,
    O: ObjectStore + 'static,
    D: Dispatch,
{
    routes_recorded(cache, dispatch, closing, authorizers, &Recorder::off())
}

/// [`routes_with_authorizers`], each call that carries an invocation id sent to
/// `recorder`.
pub fn routes_recorded<M, O, D>(
    cache: Arc<Cache<M, O>>,
    dispatch: Arc<D>,
    closing: Closing,
    authorizers: Arc<Authorizers>,
    recorder: &Recorder,
) -> Routes
where
    M: MetaLog,
    O: ObjectStore + 'static,
    D: Dispatch,
{
    let execution = ExecutionServer::new(
        ExecutionService::with_authorizers(
            Arc::clone(&cache),
            dispatch,
            closing,
            Arc::clone(&authorizers),
        )
        .recording(recorder.clone()),
    )
    .max_decoding_message_size(MAX_CONTROL_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_CONTROL_MESSAGE_BYTES);
    let capabilities = CapabilitiesService::new(true, Arc::clone(&authorizers));
    cache_routes(cache, capabilities, &authorizers, recorder).add_service(execution)
}

fn cache_routes<M, O>(
    cache: Arc<Cache<M, O>>,
    capabilities: CapabilitiesService,
    authorizers: &Arc<Authorizers>,
    recorder: &Recorder,
) -> Routes
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let cas = CasService::with_authorizers(Arc::clone(&cache), Arc::clone(authorizers))
        .recording(recorder.clone());
    let bytestream =
        ByteStreamService::with_authorizers(Arc::clone(&cache), Arc::clone(authorizers))
            .recording(recorder.clone());
    let action_cache = ActionCacheService::with_authorizers(cache, Arc::clone(authorizers))
        .recording(recorder.clone());
    Routes::new(CapabilitiesServer::new(capabilities))
        .add_service(
            ContentAddressableStorageServer::new(cas)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ByteStreamServer::new(bytestream)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(ActionCacheServer::new(action_cache))
}

/// A REAPI digest as kbf's [`kbf_types::Digest`], or INVALID_ARGUMENT naming why not.
///
/// # Errors
/// The digest is absent, has a negative size or is not lowercase SHA-256 hex.
pub fn digest_from_proto(
    d: Option<&kbf_proto::reapi::Digest>,
) -> Result<kbf_types::Digest, tonic::Status> {
    wire::digest(d)
}

/// kbf's [`kbf_types::Digest`] as a REAPI digest.
#[must_use]
pub fn digest_to_proto(d: &kbf_types::Digest) -> kbf_proto::reapi::Digest {
    wire::digest_to_proto(d)
}
