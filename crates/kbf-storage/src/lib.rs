//! The storage part of the farm, as calls: [`Storage`], the surface every caller of the
//! cache goes through (`docs/design/architecture-v2.md`, sections 3 and 13 P2).
//!
//! CAS reads and writes, FindMissing, the action cache and the `Segments` service, over
//! one store. Today the one implementation is `kbf-front`'s adapter over its own cache
//! (`kbf_front::CacheStorage`), and `kbf-server`'s in-process store
//! (`kbf_server::LocalStore`, what `kbf-all-in-one`'s daemon reads and writes through)
//! holds an `Arc<dyn Storage>`. The cache's code moves into this crate later; the trait
//! is the boundary it moves behind, so the callers do not change when it does. A storage
//! process of its own serves one over gRPC, and [`service::RemoteStorage`] is the
//! implementation that calls it ([`service`]).
//!
//! Object safe and asynchronous: each call returns a boxed future ([`Answer`]), so a
//! caller holds `Arc<dyn Storage>` and never names the metadata log or the object store
//! behind it.
//!
//! What every implementation keeps (the cache's promises, `kbf-front`'s crate docs):
//! - **FindMissing never omits a digest**, and answers in the order asked.
//! - **Present means durable.** A blob is reported present, and a write answered, only
//!   once its bytes are stored and indexed.
//! - **Every byte is verified,** on write and on read.
//! - **Only a daemon writes the action cache**, and only once every blob the entry
//!   needs is held.
//!
//! This crate depends on no frontend crate: not the scheduler (`kbf-sched`) and not
//! authorization (`kbf-auth`). `crates/kbf-it/tests/layering.rs` checks it.

pub mod flush;
pub mod local_cache;
pub mod repair;
pub mod replicate;
pub mod service;

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kbf_meta::{ActionRecord, ActionWriteError, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_types::Digest;
use tonic::Status;
use tonic::metadata::MetadataValue;

/// The answer of a [`Storage`] call.
pub type Answer<'a, T> = BoxFuture<'a, Result<T, StorageError>>;

/// The bytes of a ranged read ([`Storage::read_range`]), in order, in chunks of at most
/// [`READ_CHUNK_BYTES`]. An item is an error if the read failed after the answer began;
/// the stream ends after one.
pub type ReadStream = BoxStream<'static, Result<Bytes, StorageError>>;

/// The largest chunk of a [`ReadStream`]: what a ByteStream `Read` sends in one message.
pub const READ_CHUNK_BYTES: usize = 1 << 20;

/// The storage service: CAS, action cache, FindMissing and `Segments` over one store.
pub trait Storage: Send + Sync {
    /// The digests among `digests` the store does not hold present and reachable, in
    /// the order asked, as `FindMissingBlobs` answers; the held ones are touched. The
    /// empty blob is never missing.
    ///
    /// # Errors
    /// The store could not answer; never a partial answer.
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>>;

    /// A whole blob, checked against its digest, after touching it.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] only if the blob is not held;
    /// [`StorageError::Unreachable`] if it is held but cannot be read.
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes>;

    /// The bytes `[offset, offset + limit)` of a blob (to its end when `limit` is 0, the
    /// ByteStream `read_limit`), as a stream of chunks of at most [`READ_CHUNK_BYTES`],
    /// after touching it. `offset` equal to the blob's size is an empty stream. A caller
    /// that lost its connection reads on from the bytes it has: `offset` is where it
    /// stopped.
    ///
    /// **What is verified.** A stream that ends cleanly delivered only bytes of a blob
    /// that hashes to its digest, from its first byte to its last, whatever the range:
    /// the digest covers the whole blob, so a range cannot be checked alone, and an
    /// implementation reads (and hashes) the rest of the blob too, the bytes before
    /// `offset` and after the limit, and compares the digest before it ends the stream.
    /// A blob that fails is [`StorageError::Unreachable`], never a short or wrong
    /// ending. It is the answer's error if it is found before the first chunk is
    /// handed out, else the stream's last item: **chunks that came before an error item
    /// were not yet vouched for**, and a caller that gets one discards what it has. A
    /// range thus costs a read and hash of the whole blob, not of the range, and a
    /// resumed read pays that again; what an implementation holds meanwhile is its own
    /// to document (the cache's is about one chunk, never the blob). A read that stops
    /// polling the stream stops reading, and the rest of the blob is not checked.
    ///
    /// Errors that are known before any byte is read are in the answer, where the code
    /// is still open to the caller. An `offset` past the end is decided from the
    /// digest's size alone, before the store is asked. Nothing is read, and so nothing
    /// is checked, when no byte is asked for (`offset` at the end).
    ///
    /// # Errors
    /// [`StorageError::OutOfRange`] if `offset` is past the end of the blob;
    /// [`StorageError::NotFound`] only if the blob is not held;
    /// [`StorageError::Unreachable`] if it is held but cannot be read; the store failed.
    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream>;

    /// Hashes `bytes` once, stores them under that digest within the store's write
    /// admission (waiting for room, never refusing for it), and answers the digest once
    /// they are durable.
    ///
    /// # Errors
    /// [`StorageError::Invalid`] for a blob over the largest the store accepts; the
    /// store failed.
    fn put(&self, bytes: Bytes) -> Answer<'_, Digest>;

    /// A stream for one blob of `size` bytes, written as its bytes arrive and never
    /// held whole in memory: [`PutStream::append`] each chunk, [`PutStream::finish`]
    /// with the digest and CRC-32C, then [`StreamedPut::commit`]. `None` when the store
    /// cannot take one (no write tier, no room on it, or `size` is 0): the caller uses
    /// [`Storage::put`]. Blocking: call it off the async runtime.
    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>>;

    /// A `Segments` service of its own over the store, with uploads no other caller
    /// can name: the handlers a remote daemon's `Begin`, `PutRange` and `Commit` reach.
    fn segments(&self) -> Arc<dyn Segments>;

    /// `GetActionResult`: the result and the digest of the blob it was read from, if
    /// the entry passes the closure check and its result blob reads back; `None` (a
    /// miss) otherwise.
    ///
    /// # Errors
    /// The store could not answer.
    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>>;

    /// The first half of an action-cache write: the closure of `result`, every blob of
    /// it checked held, and `result` stored as a blob. The action cache is not touched.
    ///
    /// # Errors
    /// [`StorageError::ActionWrite`] naming the first blob of the closure not held; a
    /// malformed result; the store failed.
    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord>;

    /// The second half: commits the entry for `action`. Only [`Role::Daemon`] may
    /// write, and the store checks again that every blob the entry needs is held.
    ///
    /// # Errors
    /// [`StorageError::ActionWrite`] if refused; the store failed.
    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()>;
}

/// One blob being written as its bytes arrive ([`Storage::stream`]). Dropped before it
/// is committed, nothing is stored.
pub trait PutStream: Send {
    /// Writes `chunk` after the bytes written so far. Blocking.
    ///
    /// # Errors
    /// More bytes than the stream was made for, or the store's write failed.
    fn append(&mut self, chunk: &[u8]) -> Result<(), StorageError>;

    /// Ends the stream as the blob `digest`, whose bytes have CRC-32C `crc32c`.
    ///
    /// # Errors
    /// The bytes written are not as many as `digest` names, or their CRC-32C is not
    /// `crc32c`: nothing is stored.
    fn finish(
        self: Box<Self>,
        digest: Digest,
        crc32c: u32,
    ) -> Result<Box<dyn StreamedPut>, StorageError>;
}

/// A streamed blob whose bytes are written and checked, not yet committed.
pub trait StreamedPut: Send {
    /// Makes the blob durable and present: present once this answers.
    ///
    /// # Errors
    /// The store's sync or its index commit failed.
    fn commit(self: Box<Self>) -> Answer<'static, ()>;
}

/// Why a [`Storage`] call failed. [`Status`] has a `From` for it, so a service answers
/// with the right gRPC code. Cloned, so one failure can answer several callers.
#[derive(Clone, Debug, thiserror::Error)]
pub enum StorageError {
    /// The blob is not held. The only error that becomes NOT_FOUND.
    #[error("blob {0} is not in the cache")]
    NotFound(Digest),
    /// The blob is held, but its object cannot be read. It is reported missing until a
    /// write heals it.
    #[error("blob {0} is held but its object cannot be read")]
    Unreachable(Digest),
    /// Bytes do not hash to the digest they were sent under.
    #[error("blob sent as {claimed} hashes to {actual}")]
    DigestMismatch {
        /// The digest the caller named.
        claimed: Digest,
        /// The digest of the bytes it sent.
        actual: Digest,
    },
    /// Bytes read for a blob do not hash to the digest that names it: a node served the
    /// wrong bytes (or too many, or too few). Never the caller's fault, and not a reason to
    /// believe the node: the router goes on to another owner.
    #[error("blob {digest} came back as bytes that hash to {actual}")]
    Corrupt {
        /// The blob asked for.
        digest: Digest,
        /// The digest of the bytes that came back: of all of them, or, for a node that
        /// sent more than `digest` names, of the first byte past its size and those before.
        actual: Digest,
    },
    /// A read starting past the end of the blob. ByteStream answers it OUT_OF_RANGE (a
    /// `read_offset` greater than the resource's size), which is not a malformed request
    /// to be fixed but one a caller may make on purpose.
    #[error("read_offset {offset} is past the end of {digest}")]
    OutOfRange {
        /// The blob.
        digest: Digest,
        /// The offset asked for.
        offset: u64,
    },
    /// A malformed request or stored message, or a blob over the largest the store
    /// accepts.
    #[error("{0}")]
    Invalid(String),
    /// An action-cache write was refused.
    #[error(transparent)]
    ActionWrite(#[from] ActionWriteError),
    /// The store could not answer: its metadata, its objects, its write tier. The same
    /// call may succeed later; never turned into "absent". What fails the same way each
    /// time (a stored object that does not parse, a key that cannot be built) is
    /// [`StorageError::Internal`] instead.
    #[error(transparent)]
    Unavailable(Arc<dyn std::error::Error + Send + Sync>),
    /// The caller's map is older than the node's (`docs/design/architecture-v2.md`,
    /// section 6, the staleness protocol): the caller refetches the map and tries again.
    /// `current` is the node's generation. On the wire it is FAILED_PRECONDITION with the
    /// generation in the [`service::MAP_GENERATION_HEADER`] metadata entry.
    #[error("STALE_MAP: the node is at map generation {current}")]
    StaleMap {
        /// The map generation the node holds.
        current: u64,
    },
    /// The store's own records disagree. A bug or a broken backend, never the caller's
    /// fault.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<StorageError> for Status {
    fn from(e: StorageError) -> Self {
        let message = e.to_string();
        match e {
            StorageError::NotFound(_) => Self::not_found(message),
            StorageError::OutOfRange { .. } => Self::out_of_range(message),
            StorageError::DigestMismatch { .. } | StorageError::Invalid(_) => {
                Self::invalid_argument(message)
            }
            StorageError::ActionWrite(ActionWriteError::NotDaemon) => {
                Self::permission_denied(message)
            }
            StorageError::ActionWrite(ActionWriteError::Absent(_)) => {
                Self::failed_precondition(message)
            }
            StorageError::ActionWrite(ActionWriteError::Unreachable(_))
            | StorageError::Unreachable(_)
            | StorageError::Unavailable(_) => Self::unavailable(message),
            StorageError::StaleMap { current } => {
                let mut status = Self::failed_precondition(message);
                status
                    .metadata_mut()
                    .insert(service::MAP_GENERATION_HEADER, MetadataValue::from(current));
                status
            }
            StorageError::Corrupt { .. } => Self::data_loss(message),
            StorageError::Internal(_) => Self::internal(message),
        }
    }
}

#[cfg(test)]
mod tests;
