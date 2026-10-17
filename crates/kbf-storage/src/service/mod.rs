//! The storage node's gRPC service, over any [`Storage`], and the client that calls it
//! ([`RemoteStorage`], which is a [`Storage`] again, so a caller keeps its
//! `Arc<dyn Storage>`).
//!
//! **The wire.** Blob calls are the standard protocols, because every REAPI client and
//! the daemon's transfer engine (`kbf-transfer`) already speak them:
//!
//! - `ContentAddressableStorage.FindMissingBlobs`. The batch calls and `GetTree` answer
//!   UNIMPLEMENTED: a frontend reaches single blobs through ByteStream, and the batch
//!   shapes are the frontend's to split.
//! - `ByteStream.Read`, ranged and streamed in chunks of at most
//!   [`crate::READ_CHUNK_BYTES`] from [`Storage::read_range`]; a blob is never collected.
//!   `Write`, whose bytes are held until they are complete (see [`Config`]), and
//!   `QueryWriteStatus`.
//! - `kbf.worker.v1.Segments`, the handlers of [`Storage::segments`].
//!
//! and `kbf.storage.v1.ActionStore` (`Get`, `Prepare`, `Commit`) is what they have no
//! message for. Errors are [`StorageError`]'s gRPC codes (`From<StorageError> for
//! Status`); the one exception is an action-cache write refusal, which `Prepare` and
//! `Commit` answer in the message, since a status cannot carry the blob it names.
//!
//! **The map generation** (section 6) rides on every call: a field of every
//! `kbf.storage.v1` request, and the [`MAP_GENERATION_HEADER`] metadata entry on the
//! standard calls. [`RemoteStorage`] sends it. A node given no map source ignores it; one given a
//! [`crate::replicate::MapSource`] ([`StorageService::with_map`], or
//! [`StorageService::with_chain`], which brings its own) judges every call by it,
//! before the call reads anything: STALE_MAP (FAILED_PRECONDITION with the node's
//! generation in the same entry) for a caller behind the node; for one ahead of it, the node
//! fetches the map ([`crate::replicate::MapSource::refetch`]) and goes on, or answers
//! UNAVAILABLE if it cannot get it in time. Such a node that has no map yet fetches one the
//! same way and serves nothing (UNAVAILABLE) until it has.
//!
//! **A node that is catching up** ([`CatchUp`], [`StorageService::with_catch_up`]) is
//! rebuilding its index, so a miss does not show a blob is not held: a `Read` of a blob it
//! cannot find, a `FindMissingBlobs` with any digest missing (the whole call, so a router
//! asks the next owner about every digest of it), an action-cache miss, and an
//! action-cache write refused for a closure blob not found all answer UNAVAILABLE, never
//! NOT_FOUND or FAILED_PRECONDITION. Hits and writes are served.
//!
//! **Who may call** is [`Admission`]: the role in the caller's certificate against the
//! endpoint's table, the role read by `kbf-peer`; the module docs of [`admission`] say
//! what is checked and what is planned.
//!
//! **How `Write` takes a blob in.** Where the store offers a [`crate::PutStream`]
//! ([`Storage::stream`]), each message is appended to it as it arrives and the blob is
//! never held whole; the primary of a chain streams it on to the replica too
//! ([`crate::replicate`]). Where it does not (no store hands one out yet), a node that is
//! in no chain holds the blob until it is complete, within
//! [`Config::write_bytes_in_flight`].

mod actions;
mod admission;
mod bytestream;
mod cas;
mod catch_up;
mod client;
mod generation;
mod in_process;
mod open_writes;
mod segments;
#[cfg(test)]
pub(crate) mod tests;
mod wire;
mod write;

use std::sync::Arc;
use std::time::Duration;

use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
use kbf_proto::storage::action_store_server::ActionStoreServer;
use kbf_proto::worker::segments_server::{Segments, SegmentsServer};
use tokio::sync::Semaphore;
use tonic::service::Routes;

use crate::replicate::{self, Chain, MapSource};
use crate::{Storage, StorageError};

pub use admission::{Admission, Endpoint, PeerRole, ROLE_URI_PREFIX, role_of};
pub use catch_up::CatchUp;
pub use client::RemoteStorage;
pub(crate) use client::lift;
pub use in_process::InProcess;
pub use open_writes::{OpenWrite, OpenWrites};

/// The metadata entry that carries the map generation on the standard calls.
pub const MAP_GENERATION_HEADER: &str = "kbf-map-generation";

/// The metadata entry that marks a `Write` as a copy the caller needs on this node
/// although the map does not place the blob here ([`crate::Storage::put_unowned`]).
pub const UNOWNED_HEADER: &str = "kbf-unowned-copy";

/// The largest gRPC message the service decodes and sends: a full 16 MiB batch and room
/// around it, `kbf-front`'s `MAX_MESSAGE_BYTES` (a test pins the two equal).
pub const MAX_MESSAGE_BYTES: usize = (16 << 20) + (1 << 20);

/// The size past which a `Write` is not held whole but streamed to the store as its
/// messages arrive ([`crate::Storage::put_streamed`]), `kbf-front`'s
/// `MAX_UNSTAGED_BLOB_BYTES` (a test pins the two equal). No blob is refused for its size.
pub const STREAM_FROM_BYTES: u64 = 16 << 20;

/// The most digests one `FindMissingBlobs` may name. `kbf-front` sets none beyond the
/// message size, and a full message could name 262 144 of them (17 MiB over the 68 bytes
/// of the smallest digest), each a lookup in the store, so the node bounds the work of
/// one call below that: 65 536, 64 times the 1 000 a transfer engine asks about by
/// default. A call over it is INVALID_ARGUMENT, not RESOURCE_EXHAUSTED: REAPI uses that
/// code for a server out of space, and a client that reads it retries; this refusal
/// cannot pass by retrying, only by splitting the call.
pub const MAX_FIND_MISSING_DIGESTS: usize = 1 << 16;

/// What the service is set up with.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How many bytes of blobs `Write` calls hold at once. A `Write` takes room for the
    /// blob it declares before it reads past the first message, and waits for it (never
    /// refusing for it), so concurrent uploads cannot hold more than this, whatever
    /// their number. A blob larger than the bound takes all of it.
    pub write_bytes_in_flight: u32,
}

/// The service: every call of the module docs, over one [`Storage`].
pub struct StorageService {
    storage: Arc<dyn Storage>,
    segments: Arc<dyn Segments>,
    admission: Admission,
    chain: Option<Arc<Chain>>,
    map: Option<Arc<dyn MapSource>>,
    catch_up: Option<CatchUp>,
    open_writes: OpenWrites,
    refetch_wait: Duration,
    write_room: Semaphore,
    write_room_bytes: u32,
}

impl StorageService {
    /// The service over `storage`, admitting callers as `admission` says.
    #[must_use]
    pub fn new(storage: Arc<dyn Storage>, admission: Admission, config: Config) -> Self {
        Self {
            segments: storage.segments(),
            storage,
            admission,
            chain: None,
            map: None,
            catch_up: None,
            open_writes: OpenWrites::new(),
            refetch_wait: replicate::REFETCH_WAIT,
            write_room: Semaphore::new(config.write_bytes_in_flight as usize),
            write_room_bytes: config.write_bytes_in_flight,
        }
    }

    /// The service judging every call by the map `map` holds ([`crate::replicate`]), with
    /// no part in a chain (a node that serves reads, or a test).
    #[must_use]
    pub fn with_map(mut self, map: Arc<dyn MapSource>) -> Self {
        self.map = Some(map);
        self
    }

    /// The service as a node of a chain ([`crate::replicate`]): it judges every call by the
    /// chain's map and, as the primary of a blob, copies it to the replica before it
    /// answers.
    ///
    /// # Errors
    /// [`StorageError::Invalid`] unless the service admits callers by their certificates
    /// ([`Admission::Certified`]): the copy a primary sends its replica must be shown to come
    /// from a storage node, or any caller could write a blob that is never copied.
    pub fn with_chain(mut self, chain: Arc<Chain>) -> Result<Self, StorageError> {
        if self.admission != Admission::Certified {
            return Err(StorageError::Invalid(
                "a node in a chain admits callers by certificate (Admission::Certified)".into(),
            ));
        }
        self.map = Some(chain.source());
        self.chain = Some(chain);
        Ok(self)
    }

    /// The service of a node whose index may be being rebuilt: while `catch_up` says so,
    /// a miss is UNAVAILABLE (the module docs). A service given none is never catching up.
    #[must_use]
    pub fn with_catch_up(mut self, catch_up: CatchUp) -> Self {
        self.catch_up = Some(catch_up);
        self
    }

    /// The service counting the writes it has open in `open_writes`, which the node's
    /// registration reports ([`OpenWrites`]). A service given none counts into a handle
    /// nobody reads.
    #[must_use]
    pub fn with_open_writes(mut self, open_writes: OpenWrites) -> Self {
        self.segments = self.storage.segments_counted(&open_writes);
        self.open_writes = open_writes;
        self
    }

    /// Whether a miss found from now on must be answered UNAVAILABLE: read before the
    /// store is asked ([`catch_up`]'s module docs say why).
    fn catching_up(&self) -> bool {
        self.catch_up.as_ref().is_some_and(CatchUp::is_catching_up)
    }

    /// How long a call routed by a map this node has not been told of waits for the node
    /// to fetch it (default [`replicate::REFETCH_WAIT`]).
    #[must_use]
    pub const fn with_refetch_wait(mut self, wait: Duration) -> Self {
        self.refetch_wait = wait;
        self
    }

    /// Whether a call routed by map generation `requested` may be served now: always, for
    /// a node given no map source ([`replicate::check`]).
    async fn check_map(&self, requested: u64) -> Result<(), StorageError> {
        match &self.map {
            Some(source) => replicate::check(&**source, requested, self.refetch_wait)
                .await
                .map(drop),
            None => Ok(()),
        }
    }

    /// The service as routes, ready to be added to a tonic server.
    #[must_use]
    pub fn routes(self) -> Routes {
        Arc::new(self).shared_routes()
    }

    /// [`Self::routes`] for a service the caller keeps a handle on, so that the process
    /// that serves it can also call it ([`Self::in_process`]).
    #[must_use]
    pub fn shared_routes(self: &Arc<Self>) -> Routes {
        let this = Arc::clone(self);
        Routes::new(
            ContentAddressableStorageServer::from_arc(Arc::clone(&this))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ByteStreamServer::from_arc(Arc::clone(&this))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            SegmentsServer::from_arc(Arc::clone(&this))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ActionStoreServer::from_arc(this)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
    }

    /// The service called in this process, without a connection ([`InProcess`]): what the
    /// daemon half of a storage-and-daemon process uses for the one member that is its own
    /// node, so that a write it makes takes the path a call from another node takes (the
    /// generation check, the chain's forward to the replica).
    #[must_use]
    pub fn in_process(self: &Arc<Self>) -> InProcess {
        InProcess::new(Arc::clone(self))
    }
}
