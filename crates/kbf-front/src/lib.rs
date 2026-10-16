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
mod capabilities;
mod cas;
mod described;
mod execution;
mod recorder;
mod request_metadata;

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
pub use crate::capabilities::{CapabilitiesService, server_capabilities};
pub use crate::cas::CasService;
pub use crate::described::Described;
pub use crate::execution::{
    BOOK_CPUS_KEY, BOOK_MEM_GIB_KEY, Closer, Closing, DEFAULT_RESOURCES, Dispatch, ERROR_DOMAIN,
    ExecutionService, Finished, GPU_KEY, LEASE_KIND_KEY, LEASE_KINDS, NO_WORKER_REASON,
    OperationStream, Progress, ProgressTicker, Stage, Submission, Ticket, closing, missing_status,
    progress,
};
pub use crate::recorder::{
    ActionLabel, DEFAULT_INVOCATION_MEMORY, ExecuteCall, RECORDER_CHANNEL, Recorder, Recording,
    listing, owner, wall_now,
};
pub use crate::request_metadata::{
    CallMetadata, HEADER as REQUEST_METADATA_HEADER, Invalid, InvocationId, MAX_HEADER_BYTES,
    MAX_ID_BYTES, MAX_TEXT_BYTES, Read, ReadRequestMetadata, RequestMetadataLayer, Text,
    call_metadata,
};
pub use kbf_storage::CacheStorage;
pub use kbf_storage::HIT_CHECKS_IN_FLIGHT;
pub use kbf_storage::metrics;
pub use kbf_storage::{
    Admitted, BATCH_READS_IN_FLIGHT, BlobRead, Cache, CacheError, DEFAULT_REAPI_STREAMS,
    DEFAULT_WORKER_STREAMS, EVICT_BATCH, EVICT_EVERY, EVICT_GRACE, EvictReady, EvictRound,
    FLUSH_BACKOFF_FIRST, FLUSH_BACKOFF_MAX, FLUSH_READ_BYTES, FLUSHES_PER_ROUND, FlushRound,
    HeldObject, MAX_ADMISSION_BYTES, MAX_DECODED_BLOB_BYTES, NEEDS_V3, PASS_OPENS_PER_SEC,
    PUT_BACKOFF, PUT_BACKOFF_FACTOR, PassReport, READ_WINDOW_BYTES, RecordStream,
    STREAM_WINDOW_BYTES, StreamedRecord, TierStartError, TierStartup, TouchConfig, VerifiedBlob,
    WriteConfig,
};
pub use kbf_storage::{
    Connection, Connections, MAX_RANGE_BYTES, MAX_UPLOAD_RECORDS, MIN_SEGMENT_UPLOAD_BYTES,
    SegmentsConfig, SegmentsService, Tracked,
};
pub use kbf_storage::{ENTRY_OVERHEAD, InputTreeConfig};
pub use kbf_storage::{Leadership, MemoryMetaLog, MetaLog, MetaLogError};
pub use kbf_storage::{
    MAX_BATCH_TOTAL_BYTES, MAX_MESSAGE_BYTES, MAX_UNSTAGED_BLOB_BYTES, unstaged_refusal,
};

/// The most data in one ByteStream `ReadResponse`, and in one chunk of a ranged storage
/// read.
pub const READ_CHUNK_BYTES: usize = kbf_storage::READ_CHUNK_BYTES;

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
/// when `closing`'s [`Closer`] closes; a drain refuses new Execute calls and leaves the
/// open ones. Every call is allowed.
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
    routes_recorded(
        cache,
        dispatch,
        closing,
        Progress::off(),
        authorizers,
        &Recorder::off(),
        DEFAULT_RESOURCES.memory_bytes,
    )
}

/// [`routes_with_authorizers`], each call that carries an invocation id sent to
/// `recorder`, open streams re-sent at each tick of `progress`, and an `action` that
/// names no memory size booking `default_memory` bytes ([`ExecutionService::with_default_memory`]).
pub fn routes_recorded<M, O, D>(
    cache: Arc<Cache<M, O>>,
    dispatch: Arc<D>,
    closing: Closing,
    progress: Progress,
    authorizers: Arc<Authorizers>,
    recorder: &Recorder,
    default_memory: u64,
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
        .recording(recorder.clone())
        .with_progress(progress)
        .with_default_memory(default_memory),
    )
    .max_decoding_message_size(MAX_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_MESSAGE_BYTES);
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
    kbf_storage::wire::digest(d)
}

/// kbf's [`kbf_types::Digest`] as a REAPI digest.
#[must_use]
pub fn digest_to_proto(d: &kbf_types::Digest) -> kbf_proto::reapi::Digest {
    kbf_storage::wire::digest_to_proto(d)
}
