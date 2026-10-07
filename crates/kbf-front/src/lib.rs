//! The front end: the gRPC services that clients and daemons talk to,
//! authentication, and the HTTP endpoints for health and status.
//!
//! This is the cache half: `Capabilities`, `ContentAddressableStorage`, `ByteStream`
//! and `ActionCache`, served by [`routes`] over one [`Cache`]. The cache keeps its
//! index and action cache in a [`MetaLog`] and its bytes in a
//! [`kbf_objstore::ObjectStore`], packed into `kbf-segments` segments. Both are traits:
//! [`Cache::memory`] runs them in this process ([`MemoryMetaLog`] and the fake store),
//! and the replicated log and the S3 store slot in behind the same traits.
//!
//! The promises the services keep (RFC sections 3.5 and 9):
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

mod action_cache;
mod bytestream;
mod cache;
mod capabilities;
mod cas;
mod meta_log;
mod wire;

use std::sync::Arc;

use kbf_objstore::ObjectStore;
use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use kbf_proto::reapi::action_cache_server::ActionCacheServer;
use kbf_proto::reapi::capabilities_server::CapabilitiesServer;
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
use tonic::service::Routes;

pub use crate::action_cache::ActionCacheService;
pub use crate::bytestream::ByteStreamService;
pub use crate::cache::{Cache, CacheError, VerifiedBlob};
pub use crate::capabilities::{CapabilitiesService, server_capabilities};
pub use crate::cas::CasService;
pub use crate::meta_log::{MemoryMetaLog, MetaLog, MetaLogError};

/// The most blob data one `BatchUpdateBlobs` or `BatchReadBlobs` call may carry,
/// advertised in the capabilities. Larger blobs go through ByteStream.
pub const MAX_BATCH_TOTAL_BYTES: usize = 4 << 20;

/// The largest gRPC message the cache services accept or send: a full batch plus room
/// for the digests and statuses around it.
pub const MAX_MESSAGE_BYTES: usize = MAX_BATCH_TOTAL_BYTES + (1 << 20);

/// The most data in one ByteStream `ReadResponse`.
pub const READ_CHUNK_BYTES: usize = 1 << 20;

/// The four cache services over `cache`, ready for a tonic server.
pub fn routes<M, O>(cache: Arc<Cache<M, O>>) -> Routes
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Routes::new(CapabilitiesServer::new(CapabilitiesService))
        .add_service(
            ContentAddressableStorageServer::new(CasService::new(Arc::clone(&cache)))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ByteStreamServer::new(ByteStreamService::new(Arc::clone(&cache)))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(ActionCacheServer::new(ActionCacheService::new(cache)))
}
