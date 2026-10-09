//! The cache the front serves: CAS blobs and action-cache entries, kept by a
//! [`MetaLog`] (what exists, where) over an [`ObjectStore`] (the bytes, packed into
//! segments by `kbf-segments`).

use std::collections::BTreeSet;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use kbf_meta::{
    ActionAnswer, ActionRecord, ActionWriteError, Applied, BlobAnswer, Closure, Collected, Command,
    Epoch, Location, MetaState, ObjectId, Retention, Role, StoreId, Touch, UnreachableReason,
};
use kbf_objstore::{
    ByteRange, Capabilities, KeyError, KeyPrefix, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError,
};
use kbf_proto::reapi;
use kbf_segments::layout::{TRAILER_LEN, footer_len};
use kbf_segments::{Footer, MAX_SEGMENT_BYTES, SegmentError, SegmentWriter, WriteError};
use kbf_types::{Digest, FarmTime};
use prost::Message;
use tonic::Status;

use crate::meta_log::{MemoryMetaLog, MetaLog, MetaLogError};
use crate::wire;

/// How many times a read asks again when a collection removes what it was about to
/// touch, before it answers UNAVAILABLE.
const TOUCH_ATTEMPTS: usize = 3;

/// Why a cache operation failed. [`Status`] has a `From` for it, so a service answers
/// with the right gRPC code.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The blob is not held. The only error that becomes NOT_FOUND.
    #[error("blob {0} is not in the cache")]
    NotFound(Digest),
    /// The blob is held, but its object could not be read (missing from the store, or
    /// its bytes failed their digest). The object is marked unreachable, so the blob is
    /// reported missing until a re-upload heals it.
    #[error("blob {0} is held but its object cannot be read")]
    Unreachable(Digest),
    /// Uploaded bytes do not hash to the digest they were sent under.
    #[error("blob sent as {claimed} hashes to {actual}")]
    DigestMismatch {
        /// The digest the client named.
        claimed: Digest,
        /// The digest of the bytes it sent.
        actual: Digest,
    },
    /// A malformed request or stored message.
    #[error("{0}")]
    Invalid(String),
    /// An action-cache write was refused.
    #[error(transparent)]
    ActionWrite(#[from] ActionWriteError),
    /// The metadata log could not answer.
    #[error(transparent)]
    Meta(#[from] MetaLogError),
    /// The object store failed.
    #[error("object store: {0}")]
    Store(#[from] ObjectStoreError),
    /// A segment read back from the store did not parse.
    #[error("segment: {0}")]
    Segment(#[from] SegmentError),
    /// A blob could not be packed.
    #[error("segment: {0}")]
    Pack(#[from] WriteError),
    /// An object key could not be built.
    #[error("object key: {0}")]
    Key(#[from] KeyError),
    /// Collections kept removing what a read was about to touch.
    #[error("the index kept changing under the read; try again")]
    Contended,
    /// The cache's own records disagree (a segment read back without a blob just
    /// packed into it, an index entry with no readable range, a log answering with
    /// the wrong outcome). A bug or a broken backend, never the client's fault.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<CacheError> for Status {
    fn from(e: CacheError) -> Self {
        let message = e.to_string();
        match e {
            CacheError::NotFound(_) => Status::not_found(message),
            CacheError::DigestMismatch { .. } | CacheError::Invalid(_) => {
                Status::invalid_argument(message)
            }
            CacheError::ActionWrite(ActionWriteError::NotDaemon) => {
                Status::permission_denied(message)
            }
            CacheError::ActionWrite(ActionWriteError::Absent(_)) => {
                Status::failed_precondition(message)
            }
            CacheError::ActionWrite(ActionWriteError::Unreachable(_))
            | CacheError::Unreachable(_)
            | CacheError::Meta(_)
            | CacheError::Store(_)
            | CacheError::Segment(_)
            | CacheError::Pack(_)
            | CacheError::Key(_)
            | CacheError::Contended => Status::unavailable(message),
            CacheError::Internal(_) => Status::internal(message),
        }
    }
}

/// A blob whose bytes have been checked against its digest. The only way to store
/// bytes, so nothing reaches the store under a digest it does not have.
#[derive(Clone, Debug)]
pub struct VerifiedBlob {
    digest: Digest,
    bytes: Bytes,
}

impl VerifiedBlob {
    /// Checks `bytes` against the digest a client sent them under.
    ///
    /// # Errors
    /// [`CacheError::DigestMismatch`] if the bytes (or their length) do not match.
    pub fn new(claimed: Digest, bytes: Bytes) -> Result<Self, CacheError> {
        let actual = kbf_segments::sha256(&bytes);
        if actual != claimed {
            return Err(CacheError::DigestMismatch { claimed, actual });
        }
        Ok(Self {
            digest: claimed,
            bytes,
        })
    }

    /// Hashes bytes the server made itself.
    #[must_use]
    pub fn hashed(bytes: Bytes) -> Self {
        Self {
            digest: kbf_segments::sha256(&bytes),
            bytes,
        }
    }

    /// The blob's digest.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

/// Whether `d` is the empty blob, which every cache holds: it is reported present and
/// reads as no bytes whether or not anyone uploaded it.
fn is_empty_blob(d: &Digest) -> bool {
    d.size_bytes == 0 && *d == kbf_segments::sha256(&[])
}

/// The cache: a [`MetaLog`] for the index and action cache, an [`ObjectStore`] bucket
/// for the bytes.
///
/// Every upload is verified, packed into a segment (a blob too large to share one is a
/// segment of its own), written to the store, its footer read back, and only then
/// committed to the index, one [`Command::PutBlobs`] per segment: a blob is reported
/// present only once its bytes are durable. Every read is verified against its digest;
/// an object the store cannot produce, or whose bytes are wrong, is marked unreachable
/// (with the reason), never served and never reported absent.
#[derive(Debug)]
pub struct Cache<M, O> {
    meta: M,
    objects: O,
    prefix: KeyPrefix,
    epoch: Epoch,
    next_seq: AtomicU64,
    segment_limit: u64,
}

impl Cache<MemoryMetaLog, MemoryStore> {
    /// The single-process cache (`--store=memory`): the RFC's retention, an in-memory
    /// index and an in-memory bucket.
    #[must_use]
    pub fn memory() -> Self {
        let meta = MemoryMetaLog::new(Retention::default());
        let epoch = meta.alloc_epoch();
        Self::new(
            meta,
            MemoryStore::new(Capabilities::default()),
            KeyPrefix::default(),
            epoch,
        )
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// A cache over `meta` and `objects`, naming its objects under `prefix` in writer
    /// epoch `epoch`, which `meta` must have allocated for this cache alone
    /// ([`Command::AllocEpoch`]; [`Cache::open`] does that). Object sequence numbers
    /// count up from 1 within the epoch.
    ///
    /// Writes under an epoch `meta` never allocated are refused by the index
    /// ([`CacheError::Internal`]); two caches given one epoch would name the same keys.
    #[must_use]
    pub const fn new(meta: M, objects: O, prefix: KeyPrefix, epoch: Epoch) -> Self {
        Self {
            meta,
            objects,
            prefix,
            epoch,
            next_seq: AtomicU64::new(1),
            segment_limit: MAX_SEGMENT_BYTES,
        }
    }

    /// The same cache, packing segments of at most `limit` bytes, footer included,
    /// instead of [`MAX_SEGMENT_BYTES`]. A blob too large to share a segment of that
    /// size still gets a segment of its own.
    #[must_use]
    pub const fn with_segment_limit(mut self, limit: u64) -> Self {
        self.segment_limit = limit;
        self
    }

    /// A cache over `meta` and `objects` under `prefix`, in a writer epoch it commits
    /// to `meta` first.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn open(meta: M, objects: O, prefix: KeyPrefix) -> Result<Self, CacheError> {
        match meta.commit(Command::AllocEpoch).await? {
            Applied::Epoch(epoch) => Ok(Self::new(meta, objects, prefix, epoch)),
            other => Err(unexpected(&other)),
        }
    }

    /// The writer epoch this cache names its objects in.
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The metadata log.
    pub const fn meta(&self) -> &M {
        &self.meta
    }

    /// The object store.
    pub const fn objects(&self) -> &O {
        &self.objects
    }

    /// The key of object `id` in the store: `<prefix>cas/<epoch>/<seq>`, each as 16
    /// lowercase hex digits.
    ///
    /// # Errors
    /// [`CacheError::Key`] if the prefix and id do not form a valid key.
    pub fn object_key(&self, id: ObjectId) -> Result<ObjectKey, CacheError> {
        let (epoch, seq) = (id.epoch().get(), id.seq());
        Ok(self.prefix.key(&format!("cas/{epoch:016x}/{seq:016x}"))?)
    }

    /// Advances farm time. In memory mode the caller ticks; the replicated store's
    /// leader commits a tick each second.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn tick(&self, now: FarmTime) -> Result<(), CacheError> {
        self.meta.commit(Command::Tick(now)).await?;
        Ok(())
    }

    /// Removes every blob and action entry whose retention has run out, and says what
    /// went. Segments are not rewritten or deleted yet; their dead space is reported.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn collect(&self) -> Result<Collected, CacheError> {
        match self.meta.commit(Command::Collect).await? {
            Applied::Collected(c) => Ok(c),
            other => Err(unexpected(&other)),
        }
    }

    /// Answers `FindMissingBlobs`: every requested digest that is not present and
    /// reachable, in request order. Present blobs are touched before the answer.
    ///
    /// # Errors
    /// The metadata log is unavailable or kept changing; never a partial answer.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, CacheError> {
        let missing = self
            .touched(|s| {
                let found = s.find_missing(digests);
                (found.missing, found.touch)
            })
            .await?;
        Ok(missing.into_iter().filter(|d| !is_empty_blob(d)).collect())
    }

    /// Whether `digest` is durable: committed to the index at a reachable object. What
    /// `QueryWriteStatus` reports; bytes still arriving count for nothing.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn is_durable(&self, digest: &Digest) -> Result<bool, CacheError> {
        if is_empty_blob(digest) {
            return Ok(true);
        }
        self.touched(|s| match s.blob(digest) {
            BlobAnswer::Present(_) => (true, s.touch_for([digest])),
            BlobAnswer::Unavailable | BlobAnswer::Absent => (false, Touch::default()),
        })
        .await
    }

    /// Stores verified blobs. Blobs already present are touched, not stored again; the
    /// rest are packed into segments, written, and committed. Returns once every blob
    /// is durable.
    ///
    /// # Errors
    /// The store or the metadata log failed; nothing was reported present.
    pub async fn store_blobs(&self, blobs: Vec<VerifiedBlob>) -> Result<(), CacheError> {
        if blobs.is_empty() {
            return Ok(());
        }
        let (mut held, touch) = self
            .meta
            .query(|s| {
                let held: BTreeSet<Digest> = blobs
                    .iter()
                    .map(|b| b.digest)
                    .filter(|d| matches!(s.blob(d), BlobAnswer::Present(_)))
                    .collect();
                let touch = s.touch_for(&held);
                (held, touch)
            })
            .await?;
        if !touch.is_empty()
            && let Applied::Touched { lost } = self.meta.commit(Command::Touch(touch)).await?
        {
            // Collected between the query and the touch: store it again.
            for d in &lost.blobs {
                held.remove(d);
            }
        }
        let fresh: Vec<VerifiedBlob> = blobs
            .into_iter()
            .filter(|b| !held.contains(&b.digest))
            .collect();
        for placed in self.write_objects(fresh).await? {
            match self.meta.commit(Command::PutBlobs(placed)).await? {
                Applied::Blobs(Ok(_)) => {}
                Applied::Blobs(Err(e)) => return Err(CacheError::Internal(e.to_string())),
                other => return Err(unexpected(&other)),
            }
        }
        Ok(())
    }

    /// Reads a whole blob, verified against its digest, after touching it.
    ///
    /// # Errors
    /// [`CacheError::NotFound`] only if the blob is not held;
    /// [`CacheError::Unreachable`] if it is held but cannot be read.
    pub async fn read_blob(&self, digest: &Digest) -> Result<Bytes, CacheError> {
        if is_empty_blob(digest) {
            return Ok(Bytes::new());
        }
        let answer = self
            .touched(|s| (s.blob(digest), s.touch_for([digest])))
            .await?;
        match answer {
            BlobAnswer::Absent => Err(CacheError::NotFound(*digest)),
            BlobAnswer::Unavailable => Err(CacheError::Unreachable(*digest)),
            BlobAnswer::Present(location) => self.fetch(digest, location).await,
        }
    }

    /// Answers `GetActionResult`: the result if the entry passes the closure check
    /// (its result blob and every blob it needs present and reachable) and the result
    /// blob reads back; `None` (a miss, so the action runs again) otherwise.
    ///
    /// # Errors
    /// The metadata log or the store is unavailable.
    pub async fn action_result(
        &self,
        action: &Digest,
    ) -> Result<Option<reapi::ActionResult>, CacheError> {
        let answer = self
            .touched(|s| match s.action(action) {
                ActionAnswer::Hit { result, touch } => (Ok(result), touch),
                ActionAnswer::Miss(miss) => (Err(miss), Touch::default()),
            })
            .await?;
        let result = match answer {
            Ok(result) => result,
            Err(miss) => {
                tracing::debug!(%action, ?miss, "action cache miss");
                return Ok(None);
            }
        };
        let bytes = match self.read_blob(&result).await {
            Ok(bytes) => bytes,
            Err(CacheError::NotFound(_) | CacheError::Unreachable(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        match reapi::ActionResult::decode(bytes) {
            Ok(r) => Ok(Some(r)),
            Err(e) => {
                tracing::warn!(%action, %result, error = %e, "stored ActionResult does not decode");
                Ok(None)
            }
        }
    }

    /// Writes an action-cache entry: stores `result` as a blob and commits it with its
    /// closure (every output file, stdout and stderr, every output tree and each file
    /// the tree names). Only [`Role::Daemon`] may write, and only once every blob the
    /// entry needs is held. Clients never reach this: `UpdateActionResult` refuses them.
    ///
    /// # Errors
    /// [`CacheError::ActionWrite`] if refused; a missing tree, a malformed result, or an
    /// unavailable store or log.
    pub async fn write_action_result(
        &self,
        role: Role,
        action: Digest,
        result: &reapi::ActionResult,
    ) -> Result<(), CacheError> {
        if role != Role::Daemon {
            return Err(ActionWriteError::NotDaemon.into());
        }
        let record = self.prepare_action_result(result).await?;
        self.commit_action_record(role, action, record).await
    }

    /// The first half of [`Cache::write_action_result`]: works out the closure of
    /// `result`, checks every blob in it is held, and stores `result` as a blob. The
    /// action cache is not touched; [`Cache::commit_action_record`] writes the entry.
    ///
    /// The server runs this when a daemon reports a result, before the scheduler accepts
    /// it, and commits the entry only once the result is accepted: so a result whose
    /// outputs are not all stored is never accepted, and a result that is not accepted
    /// never reaches the action cache.
    ///
    /// # Errors
    /// [`CacheError::ActionWrite`] naming the first blob of the closure that is not
    /// held; a missing tree, a malformed result, or an unavailable store or log.
    pub async fn prepare_action_result(
        &self,
        result: &reapi::ActionResult,
    ) -> Result<ActionRecord, CacheError> {
        let closure = self.closure_of(result).await?;
        let lack = self
            .meta
            .query(|s| {
                closure.iter().find_map(|d| match s.blob(d) {
                    BlobAnswer::Present(_) => None,
                    BlobAnswer::Absent => Some(ActionWriteError::Absent(*d)),
                    BlobAnswer::Unavailable => Some(ActionWriteError::Unreachable(*d)),
                })
            })
            .await?;
        if let Some(lack) = lack {
            return Err(lack.into());
        }
        let blob = VerifiedBlob::hashed(Bytes::from(result.encode_to_vec()));
        let digest = blob.digest;
        self.store_blobs(vec![blob]).await?;
        Ok(ActionRecord {
            result: digest,
            closure,
        })
    }

    /// The second half of [`Cache::write_action_result`]: commits the action-cache
    /// entry for `action`. Only [`Role::Daemon`] may write, and the log checks again
    /// that every blob the entry needs is held.
    ///
    /// # Errors
    /// [`CacheError::ActionWrite`] if refused; an unavailable log.
    pub async fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), CacheError> {
        match self
            .meta
            .commit(Command::PutAction {
                role,
                action,
                record,
            })
            .await?
        {
            Applied::Action(outcome) => Ok(outcome?),
            other => Err(unexpected(&other)),
        }
    }

    /// Every blob an `ActionResult` needs besides itself. The empty blob is left out:
    /// it is always present.
    ///
    /// An output directory's `root_directory_digest`, when set, is required as well, so
    /// a hit never names a root `Directory` a client cannot fetch. REAPI does not make
    /// the server hold that blob; here the writer (the daemon) must upload it with the
    /// tree, or the entry is refused as absent.
    async fn closure_of(&self, result: &reapi::ActionResult) -> Result<Closure, CacheError> {
        let mut closure = Closure::new();
        for file in &result.output_files {
            insert(&mut closure, proto_digest(file.digest.as_ref())?);
        }
        for d in [&result.stdout_digest, &result.stderr_digest]
            .into_iter()
            .flatten()
        {
            insert(&mut closure, proto_digest(Some(d))?);
        }
        for dir in &result.output_directories {
            let tree_digest = proto_digest(dir.tree_digest.as_ref())?;
            insert(&mut closure, tree_digest);
            if let Some(root) = &dir.root_directory_digest {
                insert(&mut closure, proto_digest(Some(root))?);
            }
            let tree = reapi::Tree::decode(self.read_blob(&tree_digest).await?).map_err(|e| {
                CacheError::Invalid(format!("output tree {tree_digest} does not decode: {e}"))
            })?;
            for directory in tree.root.iter().chain(&tree.children) {
                for file in &directory.files {
                    insert(&mut closure, proto_digest(file.digest.as_ref())?);
                }
            }
        }
        Ok(closure)
    }

    /// Runs `read` and commits the touch it asks for, asking again if a collection
    /// removed an entry in between. The answer is returned only after its touch is
    /// committed, so nothing reported present can be collected within `min_ttl`.
    async fn touched<R, F>(&self, read: F) -> Result<R, CacheError>
    where
        F: Fn(&MetaState) -> (R, Touch) + Send + Sync,
        R: Send,
    {
        for _ in 0..TOUCH_ATTEMPTS {
            let (answer, touch) = self.meta.query(&read).await?;
            if touch.is_empty() {
                return Ok(answer);
            }
            match self.meta.commit(Command::Touch(touch)).await? {
                Applied::Touched { lost } if lost.is_empty() => return Ok(answer),
                Applied::Touched { .. } => {}
                other => return Err(unexpected(&other)),
            }
        }
        Err(CacheError::Contended)
    }

    /// Packs `blobs` into segments (a blob too large to share one gets a segment of its
    /// own), writes them, and returns where each blob is, one list per segment.
    async fn write_objects(
        &self,
        blobs: Vec<VerifiedBlob>,
    ) -> Result<Vec<Vec<(Digest, Location)>>, CacheError> {
        let mut placed = Vec::with_capacity(blobs.len());
        let mut writer = SegmentWriter::new(self.segment_limit);
        let mut packed = Vec::new();
        for blob in blobs {
            match writer.push(&blob.bytes) {
                Ok(_) => packed.push(blob.digest),
                Err(WriteError::Full { .. }) => {
                    let full = mem::replace(&mut writer, SegmentWriter::new(self.segment_limit));
                    placed.push(self.put_segment(full, &mem::take(&mut packed)).await?);
                    writer.push(&blob.bytes)?;
                    packed.push(blob.digest);
                }
                Err(WriteError::TooLarge { .. }) => placed.push(self.put_whole(blob).await?),
                Err(e) => return Err(e.into()),
            }
        }
        if !writer.is_empty() {
            placed.push(self.put_segment(writer, &packed).await?);
        }
        Ok(placed)
    }

    /// Writes one segment, reads its footer back from the store, and returns where
    /// each packed blob is, as the stored footer says.
    async fn put_segment(
        &self,
        writer: SegmentWriter,
        packed: &[Digest],
    ) -> Result<Vec<(Digest, Location)>, CacheError> {
        let segment = self.allocate();
        let key = self.object_key(segment)?;
        let body = Bytes::from(writer.finish());
        let len = body.len() as u64;
        self.objects.put_new(&key, body, None).await?;
        let footer = self.read_footer(&key, len).await?;
        packed
            .iter()
            .map(|d| {
                let entry = footer.find(d).ok_or_else(|| {
                    CacheError::Internal(format!("segment {key} came back without blob {d}"))
                })?;
                Ok((
                    *d,
                    Location {
                        store: StoreId::CONFIGURED,
                        object: segment,
                        offset: entry.offset,
                    },
                ))
            })
            .collect()
    }

    async fn read_footer(&self, key: &ObjectKey, len: u64) -> Result<Footer, CacheError> {
        let tail = |n: u64| {
            len.checked_sub(n)
                .and_then(|start| ByteRange::new(start, n))
                .ok_or_else(|| CacheError::Internal(format!("segment {key} is {len} bytes")))
        };
        let trailer = self
            .objects
            .get_range(key, tail(TRAILER_LEN as u64)?)
            .await?;
        let footer_len = Footer::len_from_trailer(&trailer)?;
        let bytes = self.objects.get_range(key, tail(footer_len)?).await?;
        Ok(Footer::parse(&bytes, len)?)
    }

    /// Stores a blob too large to share a segment as a segment of one record, so it
    /// carries a footer like every other object: its digest and CRC are in the store,
    /// and an index can be rebuilt from it. Chunking such blobs (RFC section 9.4) comes
    /// later.
    async fn put_whole(&self, blob: VerifiedBlob) -> Result<Vec<(Digest, Location)>, CacheError> {
        let mut writer = SegmentWriter::new(blob.digest.size_bytes.saturating_add(footer_len(1)));
        writer.push(&blob.bytes)?;
        self.put_segment(writer, &[blob.digest]).await
    }

    /// Reads `digest`'s bytes at `location` and checks them. An object the store does
    /// not have is marked unreachable as [`UnreachableReason::Missing`], one whose bytes
    /// are wrong as [`UnreachableReason::Corrupt`].
    async fn fetch(&self, digest: &Digest, location: Location) -> Result<Bytes, CacheError> {
        let Location {
            store,
            object,
            offset,
        } = location;
        if store != StoreId::CONFIGURED {
            return Err(CacheError::Internal(format!(
                "blob {digest} is in store {}, and this cache has only store {}",
                store.get(),
                StoreId::CONFIGURED.get()
            )));
        }
        let key = self.object_key(object)?;
        let range = ByteRange::new(offset, digest.size_bytes).ok_or_else(|| {
            CacheError::Internal(format!("blob {digest} has no readable range at {offset}"))
        })?;
        let bytes = match self.objects.get_range(&key, range).await {
            Ok(bytes) => bytes,
            Err(e @ (ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. })) => {
                tracing::error!(%digest, %key, error = %e, "held blob missing from the store");
                self.mark_unreachable(object, UnreachableReason::Missing)
                    .await?;
                return Err(CacheError::Unreachable(*digest));
            }
            Err(e) => return Err(e.into()),
        };
        // Digest equality includes the length, so a short read fails here too.
        if kbf_segments::sha256(&bytes) != *digest {
            tracing::error!(%digest, %key, "stored bytes fail their digest");
            self.mark_unreachable(object, UnreachableReason::Corrupt)
                .await?;
            return Err(CacheError::Unreachable(*digest));
        }
        Ok(bytes)
    }

    async fn mark_unreachable(
        &self,
        object: ObjectId,
        reason: UnreachableReason,
    ) -> Result<(), CacheError> {
        match self
            .meta
            .commit(Command::ObjectUnreachable { object, reason })
            .await?
        {
            Applied::Marked(Ok(())) => Ok(()),
            Applied::Marked(Err(e)) => Err(CacheError::Internal(e.to_string())),
            other => Err(unexpected(&other)),
        }
    }

    fn allocate(&self) -> ObjectId {
        ObjectId::new(self.epoch, self.next_seq.fetch_add(1, Ordering::Relaxed))
    }
}

fn insert(closure: &mut Closure, digest: Digest) {
    if !is_empty_blob(&digest) {
        closure.insert(digest);
    }
}

fn proto_digest(d: Option<&reapi::Digest>) -> Result<Digest, CacheError> {
    wire::digest(d).map_err(|s| CacheError::Invalid(s.message().to_owned()))
}

/// A [`MetaLog`] answered a command with the outcome of a different one: a broken log.
fn unexpected(applied: &Applied) -> CacheError {
    CacheError::Internal(format!(
        "metadata log answered with an unexpected outcome {applied:?}"
    ))
}
