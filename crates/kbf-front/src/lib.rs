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
//! **The proxy.** [`Proxy`] answers a client's CAS, ByteStream and action-cache calls from
//! storage nodes instead of the cache ([`routes_proxied`]; `kbf-server --storage`):
//! ByteStream as a raw HTTP/2 pass-through that never holds a blob, the rest at the
//! tonic level. See the `proxy` module for what it keeps and what it does not do yet.
//!
//! **The backend.** Execute reads the store through a [`Backend`], a cache in this
//! process, a storage node ([`Backend::remote`]) or the storage nodes of a map
//! ([`Backend::routed`]); `kbf frontend` runs the last two and holds no cache at all.
//! The map is a [`Routed`], shared with the proxy ([`Owners::routed`]).
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
mod backend;
mod bytestream;
mod cache;
mod capabilities;
mod cas;
mod closure;
mod cpu;
mod described;
mod execution;
mod finished_row;
mod input_tree;
mod meta_log;
pub mod metrics;
mod proxy;
mod recorder;
mod relay;
mod request_metadata;
mod routed;
mod segments;
mod storage;
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
pub use crate::backend::Backend;
pub use crate::bytestream::ByteStreamService;
pub use crate::cache::HIT_CHECKS_IN_FLIGHT;
pub use crate::cache::{
    ADOPT_RECORDS, Admitted, BATCH_READS_IN_FLIGHT, BlobRead, Cache, CacheError,
    DEFAULT_REAPI_STREAMS, DEFAULT_WORKER_STREAMS, EVICT_BATCH, EVICT_EVERY, EVICT_GRACE,
    EvictReady, EvictRound, FLUSH_BACKOFF_FIRST, FLUSH_BACKOFF_MAX, FLUSH_READ_BYTES,
    FLUSHES_PER_ROUND, FlushRound, HeldObject, Losses, MAX_ADMISSION_BYTES, MAX_DECODED_BLOB_BYTES,
    NEEDS_V3, ObjectKeys, PASS_OPENS_PER_SEC, PUT_BACKOFF, PUT_BACKOFF_FACTOR, PassReport,
    READ_WINDOW_BYTES, RebuildConfig, RebuildReport, RecordStream, STREAM_WINDOW_BYTES,
    StreamedRecord, TierStartError, TierStartup, TouchConfig, VerifiedBlob, WriteConfig,
};
pub use crate::capabilities::{CapabilitiesService, server_capabilities};
pub use crate::cas::CasService;
pub use crate::described::Described;
pub use crate::execution::{
    BOOK_CPUS_KEY, BOOK_MEM_GIB_KEY, Closer, Closing, DEFAULT_RESOURCES, Dispatch, ERROR_DOMAIN,
    ExecutionService, Finished, GPU_KEY, LEASE_KIND_KEY, LEASE_KINDS, NO_WORKER_REASON,
    OperationStream, Progress, ProgressTicker, Stage, Submission, Ticket, closing, missing_status,
    progress,
};
pub use crate::input_tree::{ENTRY_OVERHEAD, InputTreeConfig};
pub use crate::meta_log::{Leadership, MemoryMetaLog, MetaLog, MetaLogError};
pub use crate::proxy::{Direction, Meter, Method, Owner, Owners, Proxy};
pub use crate::recorder::{
    ActionLabel, DEFAULT_INVOCATION_MEMORY, ExecuteCall, RECORDER_CHANNEL, Recorder, Recording,
    Tap, listing, owner, wall_now,
};
pub use crate::relay::{Handed, Relay};
pub use crate::request_metadata::{
    CallMetadata, HEADER as REQUEST_METADATA_HEADER, Invalid, InvocationId, MAX_HEADER_BYTES,
    MAX_ID_BYTES, MAX_TEXT_BYTES, Read, ReadRequestMetadata, RequestMetadataLayer, Text,
    call_metadata,
};
pub use crate::routed::{Channels, Routed};
pub use crate::segments::{
    Connection, Connections, MAX_RANGE_BYTES, MAX_UPLOAD_RECORDS, MIN_SEGMENT_UPLOAD_BYTES,
    SegmentsConfig, SegmentsService, Tracked,
};
pub use crate::storage::CacheStorage;

/// The most blob data one `BatchUpdateBlobs` or `BatchReadBlobs` call may carry,
/// advertised in the capabilities. Larger blobs go through ByteStream. A request is
/// decoded whole before its handler runs, so one stream may hold up to
/// [`MAX_MESSAGE_BYTES`] (17 MiB) of decode buffer: the decode memory is that times the
/// streams a connection may open (`--reapi-max-streams`, `--worker-max-streams`), about
/// 3.4 GiB for 200, which `--write-bytes-in-flight` does not cover
/// (`docs/design/write-path.md`).
pub const MAX_BATCH_TOTAL_BYTES: usize = 16 << 20;

/// The largest gRPC message the cache services accept or send: a full batch plus room
/// for the digests and statuses around it.
pub const MAX_MESSAGE_BYTES: usize = MAX_BATCH_TOTAL_BYTES + (1 << 20);

/// No blob is too large for the cache, on any path, and none is held whole in memory
/// past [`MAX_UNSTAGED_BLOB_BYTES`]: a ByteStream `Write` of a larger blob is handed to
/// the store as it arrives (a multipart upload on S3), a segment upload's large records
/// and the in-process plane's large outputs are written to the write tier as they
/// arrive, the tier's flusher reads a block in pieces, and every read of a blob is
/// served in windows of [`READ_WINDOW_BYTES`]. A digest's size is an `i64`, so no blob
/// is over `i64::MAX` bytes; sizes are added as `u64`, saturating.
///
/// The largest blob a daemon's data plane holds in memory on its way into the cache:
/// 16 MiB. A larger one is never held whole: with a write tier that has room for it, it
/// is written onto the tier as it arrives (a segment upload's range, the in-process
/// plane's stream); without one, or when the tier is full, it is streamed to the object
/// store as it arrives (a ByteStream `Write`, the in-process plane's
/// `Cache::store_streamed_claimed`). A segment upload that would have to hold one in
/// memory until its commit is answered FAILED_PRECONDITION at `Begin`
/// ([`unstaged_refusal`]), and the daemon sends its records by ByteStream instead. No
/// blob is refused for its size. 16 MiB is where the write group already writes a blob
/// as a segment of its own (`--write-own-segment-bytes`), four PutRange messages, and
/// well under the 32 MiB upload a daemon batches small files into, so small outputs
/// keep the in-memory path unchanged.
pub const MAX_UNSTAGED_BLOB_BYTES: u64 = 16 << 20;

/// Why a segment upload holding the blob `blob` (its digest), over
/// [`MAX_UNSTAGED_BLOB_BYTES`], is not taken: no write tier has room for it, and it is
/// not held in memory instead. The blob itself is stored by a ByteStream `Write`.
#[must_use]
pub fn unstaged_refusal(blob: &str) -> String {
    format!(
        "{blob} is over {MAX_UNSTAGED_BLOB_BYTES} bytes and no write tier has room for it: \
         send it by a ByteStream Write, which streams it to the object store"
    )
}

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

/// What keeps open Execute streams going: the drain that ends them and the tick that
/// re-sends them while their action waits.
pub struct OpenStreams {
    /// Ends the streams when closed ([`closing`]).
    pub closing: Closing,
    /// Re-sends each at every tick ([`progress`]).
    pub progress: Progress,
}

/// [`routes_recorded`] with the client's CAS, ByteStream and action-cache calls answered
/// by the storage nodes of `proxy` ([`Proxy`]), and `Execution` over `backend`: a cache
/// in this process (`kbf-server --storage`, which still holds one) or a storage node
/// ([`Backend::remote`], which is what `kbf frontend` runs).
pub fn routes_proxied<D>(
    proxy: &Proxy,
    backend: impl Into<Backend>,
    dispatch: Arc<D>,
    streams: OpenStreams,
    authorizers: Arc<Authorizers>,
    recorder: &Recorder,
    default_memory: u64,
) -> Routes
where
    D: Dispatch,
{
    let OpenStreams { closing, progress } = streams;
    let execution = ExecutionServer::new(
        ExecutionService::with_authorizers(backend, dispatch, closing, Arc::clone(&authorizers))
            .recording(recorder.clone())
            .with_progress(progress)
            .with_default_memory(default_memory),
    )
    .max_decoding_message_size(MAX_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let capabilities = CapabilitiesService::new(true, authorizers);
    proxy
        .routes()
        .add_service(CapabilitiesServer::new(capabilities))
        .add_service(execution)
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
