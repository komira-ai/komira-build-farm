//! The cache the front serves: CAS blobs and action-cache entries, kept by a
//! [`MetaLog`] (what exists, where) over an [`ObjectStore`] (the bytes, packed into
//! segments by `kbf-segments`).

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use futures::future;
use kbf_meta::{
    ActionAnswer, ActionRecord, ActionWriteError, Applied, BlobAnswer, Closure, Collected, Command,
    Epoch, MetaState, ObjectId, Retention, Role, Touch, UnreachableReason,
};
use kbf_objstore::{
    Capabilities, KeyError, KeyPrefix, MemoryStore, ObjectKey, ObjectStore, ObjectStoreError,
};
use kbf_proto::reapi;
use kbf_segments::{MAX_SEGMENT_BYTES, SegmentError, WriteError};
use kbf_types::{Digest, FarmTime};
use prost::Message;
use tonic::Status;

use crate::input_tree::{Horizon, InputTreeConfig, InputTrees};
use crate::meta_log::{MemoryMetaLog, MetaLog, MetaLogError};
use crate::metrics::CacheMetrics;
use crate::wire;

/// The most [`ObjectStore::may_hold`] checks one action-cache hit keeps in flight. A
/// hit needs one check per stored object its entry names; over a local store each is
/// a blocking-pool hop plus an open and a header read, so they run side by side.
/// At most 30, the most `futures::future::join_all` joins in place.
pub const HIT_CHECKS_IN_FLIGHT: usize = 16;

/// Why a cache operation failed. [`Status`] has a `From` for it, so a service answers
/// with the right gRPC code. Cloned, so one segment's failure answers every blob in it.
#[derive(Clone, Debug, thiserror::Error)]
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
    /// The local write tier failed.
    #[error("write tier: {0}")]
    Tier(Arc<kbf_tier::TierError>),
    /// A blob could not be packed.
    #[error("segment: {0}")]
    Pack(#[from] WriteError),
    /// An object key could not be built.
    #[error("object key: {0}")]
    Key(#[from] KeyError),
    /// The cache's own records disagree (a segment read back without a blob just
    /// packed into it, an index entry with no readable range, a log answering with
    /// the wrong outcome). A bug or a broken backend, never the client's fault.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<kbf_tier::TierError> for CacheError {
    fn from(e: kbf_tier::TierError) -> Self {
        Self::Tier(Arc::new(e))
    }
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
            | CacheError::Tier(_) => Status::unavailable(message),
            CacheError::Segment(_)
            | CacheError::Pack(_)
            | CacheError::Key(_)
            | CacheError::Internal(_) => Status::internal(message),
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
/// Every upload is verified, packed into a segment shared with concurrent uploads (a
/// large blob is a segment of its own; [`WriteConfig`]), written to the store, its
/// footer read back, and only then committed to the index, one [`Command::PutBlobs`]
/// per segment: a blob is reported present only once its bytes are durable. Every read is verified against its digest;
/// an object the store cannot produce, or whose bytes are wrong, is marked unreachable
/// (with the reason), never served and never reported absent.
///
/// A read records the touch it owes and answers; the touches are committed in batches
/// off the reply path, and [`Cache::collect`] commits them before it collects
/// ([`TouchConfig`]).
///
/// Each stage of a store, each read's object GET, and each metadata commit and query
/// is timed into the cache's [`CacheMetrics`] ([`Cache::metrics`]).
#[derive(Debug)]
pub struct Cache<M, O> {
    writer: Arc<Writer<M, O>>,
    group: Arc<Group>,
    touches: Arc<Touches>,
    segment_limit: u64,
    trees: Arc<InputTrees>,
}

impl<M, O> Drop for Cache<M, O> {
    fn drop(&mut self) {
        self.touches.stop();
    }
}

impl Cache<MemoryMetaLog, MemoryStore> {
    /// The single-process cache (for tests): the default retention
    /// (`docs/design/storage.md#retention-and-touches`), an in-memory index and an
    /// in-memory bucket.
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
    /// Uploads are grouped and bounded as [`WriteConfig::DEFAULT`] says
    /// ([`Cache::with_writes`]).
    #[must_use]
    pub fn new(meta: M, objects: O, prefix: KeyPrefix, epoch: Epoch) -> Self {
        Self {
            writer: Arc::new(Writer::new(meta, objects, prefix, epoch)),
            group: Arc::new(Group::new(WriteConfig::DEFAULT)),
            touches: Arc::new(Touches::new(TouchConfig::DEFAULT)),
            segment_limit: MAX_SEGMENT_BYTES,
            trees: Arc::new(InputTrees::new(InputTreeConfig::DEFAULT)),
        }
    }

    /// The same cache, remembering input trees as `config` says instead of
    /// [`InputTreeConfig::DEFAULT`] ([`Cache::missing_inputs`]). Call it before the
    /// cache serves an Execute.
    #[must_use]
    pub fn with_input_trees(mut self, config: InputTreeConfig) -> Self {
        self.trees = Arc::new(InputTrees::new(config));
        self
    }

    /// How many decoded directories the input check keeps, and what they cost in bytes.
    pub fn input_tree_cache(&self) -> (usize, u64) {
        (self.trees.len(), self.trees.bytes())
    }

    pub(crate) fn input_trees(&self) -> &InputTrees {
        &self.trees
    }

    /// The farm time now, the farm time until which a blob reported held now stays
    /// held (now plus the retention's `min_ttl`), and the count of objects lost so far.
    pub(crate) async fn retention_horizon(&self) -> Result<Horizon, CacheError> {
        Ok(self
            .writer
            .query(|s| Horizon {
                now: s.now(),
                until: s.now().saturating_add(s.retention().min_ttl),
                losses: s.losses(),
            })
            .await?)
    }

    /// The same cache, packing segments of at most `limit` bytes, footer included,
    /// instead of [`MAX_SEGMENT_BYTES`]. A blob too large to share a segment of that
    /// size still gets a segment of its own.
    #[must_use]
    pub const fn with_segment_limit(mut self, limit: u64) -> Self {
        self.segment_limit = limit;
        self
    }

    /// The same cache, committing recorded touches as `config` says instead of
    /// [`TouchConfig::DEFAULT`]. Call it before the cache serves a read.
    #[must_use]
    pub fn with_touches(mut self, config: TouchConfig) -> Self {
        self.touches.stop();
        self.touches = Arc::new(Touches::new(config));
        self
    }

    /// How many recorded touches wait for their commit.
    pub fn touches_pending(&self) -> usize {
        self.touches.pending()
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
    pub fn epoch(&self) -> Epoch {
        self.writer.epoch
    }

    /// The metadata log.
    pub fn meta(&self) -> &M {
        &self.writer.meta
    }

    /// The object store.
    pub fn objects(&self) -> &O {
        &self.writer.objects
    }

    /// What this cache, and the services over it, have measured.
    pub fn metrics(&self) -> &CacheMetrics {
        &self.writer.metrics
    }

    /// The prefix every key this cache writes starts with.
    pub fn prefix(&self) -> &KeyPrefix {
        &self.writer.prefix
    }

    /// The key of object `id` in the store: `<prefix>cas/<epoch>/<seq>`, each as 16
    /// lowercase hex digits.
    ///
    /// # Errors
    /// [`CacheError::Key`] if the prefix and id do not form a valid key.
    pub fn object_key(&self, id: ObjectId) -> Result<ObjectKey, CacheError> {
        self.writer.object_key(id)
    }

    /// Advances farm time. In memory mode the caller ticks; the replicated store's
    /// leader commits a tick each second.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn tick(&self, now: FarmTime) -> Result<(), CacheError> {
        self.writer.commit(Command::Tick(now)).await?;
        Ok(())
    }

    /// Removes every blob and action entry whose retention has run out, and says what
    /// went. Segments are not rewritten or deleted yet; their dead space is reported.
    ///
    /// Every recorded touch is committed first, and no read records one until the
    /// collection has applied, so nothing a read reported is collected.
    ///
    /// # Errors
    /// The metadata log is unavailable; then nothing is collected.
    pub async fn collect(&self) -> Result<Collected, CacheError> {
        let _exclusive = self.touches.gate().write().await;
        touches::flush(&self.touches, &self.writer).await?;
        match self.writer.commit(Command::Collect).await? {
            Applied::Collected(c) => Ok(c),
            other => Err(unexpected(&other)),
        }
    }

    /// Answers `FindMissingBlobs`: every requested digest that is not present and
    /// reachable, in request order. The present blobs' touch is recorded, not
    /// committed: the answer waits for no metadata commit.
    ///
    /// # Errors
    /// The metadata log is unavailable; never a partial answer.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, CacheError> {
        let (missing, objects) = self
            .touched(|s| {
                let found = s.find_missing(digests);
                ((found.missing, found.objects), found.touch)
            })
            .await?;
        self.used(objects);
        Ok(missing.into_iter().filter(|d| !is_empty_blob(d)).collect())
    }

    /// Whether each of `digests` is present and reachable, in order, as
    /// FindMissingBlobs would answer, but without touching any: a page that lists a
    /// blob does not keep it.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn present(&self, digests: &[Digest]) -> Result<Vec<bool>, CacheError> {
        let found = self
            .writer
            .query(|s| {
                let held = |d: &Digest| matches!(s.blob(d), BlobAnswer::Present { .. });
                digests
                    .iter()
                    .map(|d| is_empty_blob(d) || held(d))
                    .collect()
            })
            .await?;
        Ok(found)
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
            BlobAnswer::Present { .. } => (true, s.touch_for([digest])),
            BlobAnswer::Unavailable | BlobAnswer::Absent => (false, Touch::default()),
        })
        .await
    }

    /// Stores verified blobs, with no admission ([`Cache::store_admitted`]). Blobs
    /// already present are touched, not stored again; the rest are packed into segments
    /// with concurrent uploads, written, and committed. Returns once every blob is
    /// durable.
    ///
    /// # Errors
    /// The store or the metadata log failed for some blob (the first such blob's
    /// error); that blob was not reported present.
    pub async fn store_blobs(&self, blobs: Vec<VerifiedBlob>) -> Result<(), CacheError> {
        self.store_admitted(blobs, Admitted::default()).await
    }

    /// Which of `blobs` are present, each touched. A blob collected between the query
    /// and the touch is left out, so it is stored again.
    async fn held(&self, blobs: &[VerifiedBlob]) -> Result<BTreeSet<Digest>, CacheError> {
        let (mut held, touch) = self
            .writer
            .query(|s| {
                let held: BTreeSet<Digest> = blobs
                    .iter()
                    .map(|b| b.digest)
                    .filter(|d| matches!(s.blob(d), BlobAnswer::Present { .. }))
                    .collect();
                let touch = s.touch_for(&held);
                (held, touch)
            })
            .await?;
        if !touch.is_empty()
            && let Applied::Touched { lost } = self.writer.commit(Command::Touch(touch)).await?
        {
            // Collected between the query and the touch: store it again.
            for d in &lost.blobs {
                held.remove(d);
            }
        }
        Ok(held)
    }

    /// Answers `GetActionResult`: the result, and the digest of the ActionResult blob
    /// it was read from, if the entry passes the closure check (its result blob and
    /// every blob it needs present and reachable), the store may still hold every
    /// object those blobs are in, and the result blob reads back; `None` (a miss, so
    /// the action runs again) otherwise.
    ///
    /// # Errors
    /// The metadata log or the store is unavailable.
    pub async fn action_result(
        &self,
        action: &Digest,
    ) -> Result<Option<(Digest, reapi::ActionResult)>, CacheError> {
        let answer = self
            .touched(|s| match s.action(action) {
                ActionAnswer::Hit { result, touch } => {
                    (Ok((result, s.stored_objects(action))), touch)
                }
                ActionAnswer::Miss(miss) => (Err(miss), Touch::default()),
            })
            .await?;
        let (result, objects) = match answer {
            Ok(hit) => hit,
            Err(miss) => {
                tracing::debug!(%action, ?miss, "action cache miss");
                return Ok(None);
            }
        };
        self.used(objects.iter().copied());
        if !self.store_may_hold(action, objects).await? {
            return Ok(None);
        }
        let bytes = match self.read_blob(&result).await {
            Ok(bytes) => bytes,
            Err(CacheError::NotFound(_) | CacheError::Unreachable(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        match reapi::ActionResult::decode(bytes) {
            Ok(r) => Ok(Some((result, r))),
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
            .writer
            .query(|s| {
                closure.iter().find_map(|d| match s.blob(d) {
                    BlobAnswer::Present { .. } => None,
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
            .writer
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

    /// Runs `read` and records the touch it asks for ([`TouchConfig`]); commits
    /// nothing. The query and the record happen under the gate a collection takes
    /// exclusively, so a collection either sees the touch or ran before the query.
    async fn touched<R, F>(&self, read: F) -> Result<R, CacheError>
    where
        F: FnOnce(&MetaState) -> (R, Touch) + Send,
        R: Send,
    {
        let _shared = self.touches.gate().read().await;
        let (answer, touch) = self.writer.query(read).await?;
        if !touch.is_empty() {
            self.touches.record(touch);
            self.touches.start(&self.writer);
        }
        Ok(answer)
    }

    /// Whether the store may still hold each of `objects`, the stored objects an
    /// action-cache hit on `action` needs ([`ObjectStore::may_hold`]: a local store
    /// checks its files, a remote one answers yes). Each object the store does not hold
    /// is marked unreachable as missing, so the hit becomes a miss and FindMissing
    /// reports the blobs of every lost object, rather than the client being sent to
    /// download bytes no one has. A failed check fails the call only when no object was
    /// found gone: one that was makes the answer a miss either way.
    ///
    /// The checks run in groups of [`HIT_CHECKS_IN_FLIGHT`], each group joined whole,
    /// so every check runs to its answer. A group this small is joined in place, with
    /// no task per check, so a store that answers at once pays next to nothing for
    /// the fan-out.
    async fn store_may_hold(
        &self,
        action: &Digest,
        objects: BTreeSet<ObjectId>,
    ) -> Result<bool, CacheError> {
        let mut keys = Vec::with_capacity(objects.len());
        for object in objects {
            keys.push((object, self.object_key(object)?));
        }
        let store = &self.writer.objects;
        let mut answers = Vec::with_capacity(keys.len());
        for group in keys.chunks(HIT_CHECKS_IN_FLIGHT) {
            if let [(_, key)] = group {
                answers.push(store.may_hold(key).await);
            } else {
                let checks = group.iter().map(|(_, key)| store.may_hold(key));
                answers.extend(future::join_all(checks).await);
            }
        }
        let mut missing = Vec::new();
        let mut failed = None;
        for ((object, key), held) in keys.iter().zip(answers) {
            match held {
                Ok(true) => {}
                Ok(false) => missing.push((*object, key)),
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        if missing.is_empty() {
            return failed.map_or(Ok(true), |e| Err(e.into()));
        }
        if let Some(e) = failed {
            tracing::warn!(%action, error = %e, "a store check failed on a hit that is a miss anyway");
        }
        for (object, key) in missing {
            tracing::error!(%action, %key, "an object an action-cache hit needs is gone from the store: a miss");
            self.mark_unreachable(object, UnreachableReason::Missing)
                .await?;
        }
        Ok(false)
    }

    async fn mark_unreachable(
        &self,
        object: ObjectId,
        reason: UnreachableReason,
    ) -> Result<(), CacheError> {
        self.writer.mark_unreachable(object, reason).await
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

mod batch;
mod consistency;
mod direct;
mod evict;
mod flush;
mod read;
mod stream;
mod tier;
mod touches;
mod upload;
mod verify;
mod write_group;

pub use batch::BATCH_READS_IN_FLIGHT;
pub use consistency::{
    EVICT_BATCH, EvictReady, HeldObject, NEEDS_V3, PASS_OPENS_PER_SEC, PassReport,
};
pub use evict::{EVICT_EVERY, EVICT_GRACE, EvictRound};
pub use flush::{
    FLUSH_BACKOFF_FIRST, FLUSH_BACKOFF_MAX, FLUSH_READ_BYTES, FLUSHES_PER_ROUND, FlushRound,
};
pub use read::{BlobRead, MAX_DECODED_BLOB_BYTES, READ_WINDOW_BYTES};
pub(crate) use stream::TierRange;
pub use stream::{RecordStream, StreamedRecord};
pub use tier::{TierStartError, TierStartup};
pub use touches::TouchConfig;
use touches::Touches;
pub(crate) use upload::BuiltSegment;
pub use write_group::{
    Admitted, DEFAULT_REAPI_STREAMS, DEFAULT_WORKER_STREAMS, MAX_ADMISSION_BYTES, PUT_BACKOFF,
    PUT_BACKOFF_FACTOR, STREAM_WINDOW_BYTES, WriteConfig,
};
use write_group::{Group, Writer};
