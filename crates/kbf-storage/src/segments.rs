//! `kbf.worker.v1.Segments`: a daemon uploads an action's outputs as whole segments
//! with one commit, and reads blobs as ranges of the objects that hold them
//! (`docs/design/write-path.md`, section 4).
//!
//! Served on the worker listener only, behind its admission (kbf-server wraps this
//! service as it wraps the blob services); this type authorizes nothing itself.
//!
//! - **Begin** checks every footer and refuses an upload over [`MAX_UPLOAD_RECORDS`].
//!   With a write tier, each segment's records get a range of their own on it, written
//!   as they arrive; a segment the tier has no room for (or every segment, without a
//!   tier) is held in memory until the commit. Such a segment may not hold a record
//!   over [`MAX_UNSTAGED_BLOB_BYTES`]: `Begin` answers FAILED_PRECONDITION
//!   ([`unstaged_refusal`](crate::unstaged_refusal)), and the client sends the
//!   upload's records blob by blob through ByteStream, whose `Write` streams a large
//!   blob to the store. No record is refused for its size. The bytes held in memory may not pass
//!   `--segment-upload-max-bytes`, and are admitted by [`Cache::admit`]
//!   (`--write-bytes-in-flight`, the bound ByteStream and batch uploads share), waiting
//!   (never refusing) while it is spent. An upload whose records all go to the tier
//!   holds none, whatever its size.
//! - **PutRange** writes a range of one segment's records. Once Commit was called,
//!   every PutRange is refused, so the bytes received are the bytes stored.
//!   *Without a write tier* the range also hashes (SHA-256 and CRC-32C) what it
//!   completed or extended before it answers: each record's hash advances over the
//!   bytes received unbroken from its start, whichever order the ranges come in, on
//!   the cache's hashing pool (`--cpu-threads`, blocking pool, off the runtime
//!   workers), and a record that does not hash to its digest is refused at once, since
//!   the PUT is the only copy. *With a write tier* nothing is hashed here: the daemon's
//!   digests are trusted when the upload is acknowledged, and the flusher checks every
//!   record before the PUT (`docs/design/write-path.md`, section 6.7).
//! - **Commit** runs in a task of its own, detached from the call and the connection,
//!   until it has indexed or failed. It refuses a record not wholly received (and, with
//!   no tier, one not hashed to its digest) and never waits for any hashing of its own.
//!   With a write tier attached, each segment is staged on it as a block of its own
//!   (its records synced to the tier's disk, one `PutStaged` for its blobs): the Commit
//!   is answered without an object-store PUT, and the flusher PUTs the object later,
//!   with a footer it builds from its own hashing. A segment the tier does not take (at
//!   its budget, a disk error before the commit) is hashed then, if it was not, and PUT
//!   and read back as a grouped segment is (a `--write-puts-in-flight` slot, the PUT
//!   retried under a new key) with a footer built here (the daemon's CRC-32C values
//!   never reach the store), and one `PutBlobs` commits the records of every PUT
//!   segment. A second Commit waits for the first and returns its answer.
//! - **ReadRange** is served from the write tier while the object is staged on it (or
//!   has a flushed copy there), else from the store.
//! - An upload belongs to the connection that began it ([`Connections`]). When that
//!   connection closes, or no call names the upload for `--segment-upload-idle-secs`, an
//!   upload still receiving is dropped and its memory returned at once; a committing
//!   one is kept until its commit ends.

mod connection;
mod upload;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_meta::{Epoch, ObjectId};
use kbf_objstore::ObjectStore;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, BlobLocation, CommitRequest, CommitResponse, LocateRequest,
    LocateResponse, ObjectRef, PutRangeRequest, PutRangeResponse, ReadRangeRequest,
    ReadRangeResponse,
};
use kbf_segments::RecordHasher;
use kbf_types::Digest;
use tokio::sync::watch;
use tokio::time::Instant;
use tonic::{Request, Response, Status};

pub use self::connection::{Connection, Connections, Tracked};
use self::upload::{Declared, Held, Put, Receiving, TierWrites, advance, declare};
use crate::cache::{Admitted, BuiltSegment, Cache, TierRange};
use crate::meta_log::MetaLog;
use crate::{MAX_UNSTAGED_BLOB_BYTES, unstaged_refusal, wire};

/// The most records one upload may hold: bounds `Begin`'s message (16,384 entries of
/// 52 bytes, about 850 KiB) and the upload's one `PutBlobs`.
pub const MAX_UPLOAD_RECORDS: usize = 16_384;

/// The most bytes one `PutRange` or `ReadRange` carries.
pub const MAX_RANGE_BYTES: u64 = 4 << 20;

/// The smallest `--segment-upload-max-bytes`, the most bytes one upload may hold in
/// memory: 33 MiB, a daemon's upload of small files (32 MiB,
/// `kbf_transfer::SEGMENT_UPLOAD_BYTES`) with room to spare. No record held in memory
/// is over [`MAX_UNSTAGED_BLOB_BYTES`], and records written to the write tier as they
/// arrive are not held, so an upload of any size can be admitted.
pub const MIN_SEGMENT_UPLOAD_BYTES: u64 = 33 << 20;

/// The `--segment-upload-*` flags of the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentsConfig {
    /// The most bytes one upload may hold in memory (`--segment-upload-max-bytes`):
    /// its segments that are not written to the write tier as they arrive. The memory
    /// uploads in progress hold together is bounded by the cache's
    /// `--write-bytes-in-flight` ([`Cache::admit`]), which `kbf-server` refuses to
    /// start below this cap, so a legal `Begin` is always admitted.
    pub max_bytes: u64,
    /// How long an upload may go without a call before it is dropped
    /// (`--segment-upload-idle-secs`).
    pub idle: Duration,
}

impl SegmentsConfig {
    /// The defaults: [`MIN_SEGMENT_UPLOAD_BYTES`] and 60 seconds.
    pub const DEFAULT: Self = Self {
        max_bytes: MIN_SEGMENT_UPLOAD_BYTES,
        idle: Duration::from_secs(60),
    };
}

/// The `Segments` service over one cache.
#[derive(Debug)]
pub struct SegmentsService<M, O> {
    shared: Arc<Shared<M, O>>,
}

impl<M, O> Clone for SegmentsService<M, O> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

#[derive(Debug)]
struct Shared<M, O> {
    cache: Arc<Cache<M, O>>,
    config: SegmentsConfig,
    connections: Connections,
    /// `Begin` calls waiting for admission now.
    waiting: AtomicUsize,
    next: AtomicU64,
    uploads: Mutex<HashMap<u64, Arc<Upload>>>,
}

#[derive(Debug)]
struct Upload {
    owner: Option<Connection>,
    declared: Vec<Declared>,
    phase: Mutex<Phase>,
    last_call: Mutex<Instant>,
    /// The commit's answer, once there is one.
    done: watch::Sender<Option<Result<Vec<Digest>, Status>>>,
}

#[derive(Debug)]
enum Phase {
    Receiving {
        segments: Vec<Receiving>,
        _admitted: Admitted,
    },
    Committing,
    /// Committed or failed; the answer is in [`Upload::done`].
    Done,
    Dropped,
}

/// Counts one `Begin` waiting for admission until dropped, so a call cancelled while
/// it waits is uncounted too.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn enter(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Every lock here guards state that is whole after each statement.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Upload {
    fn touch(&self) {
        *lock(&self.last_call) = Instant::now();
    }

    /// The commit's answer, once it has one.
    async fn answer(&self) -> Result<Vec<Digest>, Status> {
        let mut done = self.done.subscribe();
        loop {
            let answer = done.borrow_and_update().clone();
            if let Some(answer) = answer {
                return answer;
            }
            // The sender lives in `self`, so `changed` never fails while `self` is
            // borrowed: it returns once a value is sent.
            let _ = done.changed().await;
        }
    }

    /// The next step of the check of record `i` of segment `index` ([`Receiving::next`]);
    /// `None` when the upload is no longer receiving (a hash job outlived its upload),
    /// which ends that record's check.
    fn check_next(
        &self,
        index: usize,
        i: usize,
        finished: Option<(RecordHasher, usize)>,
    ) -> Option<Held> {
        match &mut *lock(&self.phase) {
            Phase::Receiving { segments, .. } => {
                segments[index].next(&self.declared[index], i, finished)
            }
            _ => None,
        }
    }

    /// The refusal of segment `index`, if a record of it did not hash to its digest.
    fn refusal_at(&self, index: usize) -> Option<Status> {
        match &*lock(&self.phase) {
            Phase::Receiving { segments, .. } => segments[index].refusal(),
            _ => None,
        }
    }

    /// Drops the upload, freeing its buffers and admission even if a call still holds it,
    /// unless a commit of it is running: decided under the phase lock, so a Commit
    /// cannot start between the check and the drop.
    fn try_drop(&self) -> bool {
        let mut phase = lock(&self.phase);
        if matches!(*phase, Phase::Committing) {
            return false;
        }
        *phase = Phase::Dropped;
        true
    }
}

/// Writes the range of `bytes` at `offset`, which `put` recorded, to the write tier,
/// off the runtime, and answers `writes` whether it did (if not, the range is forgotten
/// at the next `PutRange` or `Commit`, so it can be sent again, and the call fails
/// UNAVAILABLE). Nothing of the range is held once this answers.
async fn write_to_tier(
    range: Arc<TierRange>,
    writes: Arc<TierWrites>,
    offset: u64,
    bytes: Vec<u8>,
) -> Result<(), Status> {
    let written = tokio::task::spawn_blocking(move || range.write(offset, &bytes))
        .await
        .map_err(shown)
        .and_then(|written| written.map_err(shown));
    writes.done(offset, written.is_ok());
    written.map_err(|why| {
        Status::unavailable(format!(
            "the write tier did not take the range at {offset}: {why}"
        ))
    })
}

/// `e` as a message.
fn shown(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Builds every received segment from what `Begin` declared and `PutRange` checked
/// ([`Receiving::check`]), on the blocking pool: copying footers must not hold a runtime
/// thread. A
/// check that panicked answers INTERNAL, so the upload's Commits are still answered.
async fn check_all(
    upload: Arc<Upload>,
    segments: Vec<Receiving>,
) -> Result<Vec<BuiltSegment>, Status> {
    tokio::task::spawn_blocking(move || {
        segments
            .into_iter()
            .zip(&upload.declared)
            .map(|(received, declared)| received.check(declared))
            .collect::<Result<Vec<_>, Status>>()
    })
    .await
    .unwrap_or_else(|e| Err(Status::internal(format!("checking an upload failed: {e}"))))
}

impl<M: MetaLog, O: ObjectStore + 'static> SegmentsService<M, O> {
    /// The service over `cache`, with uploads bounded as `config` says and owned by the
    /// connections `connections` tracks (the listener's accepted streams must be
    /// wrapped by [`Connections::track`], or uploads are dropped only when idle).
    #[must_use]
    pub fn new(cache: Arc<Cache<M, O>>, config: SegmentsConfig, connections: Connections) -> Self {
        Self {
            shared: Arc::new(Shared {
                cache,
                config,
                connections,
                waiting: AtomicUsize::new(0),
                next: AtomicU64::new(1),
                uploads: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// How many `Begin` calls are waiting for admission now: what a test watches to
    /// see a `Begin` wait rather than be refused.
    #[must_use]
    pub fn begins_waiting(&self) -> usize {
        self.shared.waiting.load(Ordering::SeqCst)
    }

    /// How many uploads are open (receiving, committing, or holding a commit's answer).
    #[must_use]
    pub fn open_uploads(&self) -> usize {
        lock(&self.shared.uploads).len()
    }

    /// The upload `id`, if the caller's connection owns it.
    fn upload<T>(&self, request: &Request<T>, id: u64) -> Result<Arc<Upload>, Status> {
        let caller = self.shared.connections.of(request.remote_addr());
        let upload = lock(&self.shared.uploads).get(&id).cloned();
        upload
            .filter(|u| u.owner.as_ref().map(Connection::id) == caller.as_ref().map(Connection::id))
            .ok_or_else(|| Status::not_found(format!("no upload {id} on this connection")))
    }

    /// Drops upload `id` once no commit of it is running: called when its connection
    /// closed or it went idle.
    async fn reap(shared: Arc<Shared<M, O>>, id: u64, upload: Arc<Upload>) {
        let idle = shared.config.idle;
        let closed = async {
            match &upload.owner {
                Some(owner) => owner.closed().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(closed);
        let mut closed_seen = false;
        loop {
            if !closed_seen {
                let deadline = *lock(&upload.last_call) + idle;
                tokio::select! {
                    () = &mut closed => closed_seen = true,
                    () = tokio::time::sleep_until(deadline) => {
                        if *lock(&upload.last_call) + idle > Instant::now() {
                            continue;
                        }
                    }
                }
            }
            if upload.try_drop() {
                break;
            }
            // A commit is running: it is never dropped. Once it ends, its answer is
            // kept for another idle period (or dropped at once if the connection is
            // gone).
            let _ = upload.answer().await;
        }
        lock(&shared.uploads).remove(&id);
    }

    /// Checks every record, builds and stores the segments, and commits them; then
    /// answers every Commit waiting and returns the memory. Runs detached.
    async fn commit_task(
        cache: Arc<Cache<M, O>>,
        upload: Arc<Upload>,
        segments: Vec<Receiving>,
        admitted: Admitted,
    ) {
        let answer = async {
            let built = check_all(Arc::clone(&upload), segments).await?;
            cache.store_upload(built).await.map_err(Status::from)
        }
        .await;
        drop(admitted);
        *lock(&upload.phase) = Phase::Done;
        upload.touch();
        upload.done.send_replace(Some(answer));
    }
}

#[tonic::async_trait]
impl<M: MetaLog, O: ObjectStore + 'static> Segments for SegmentsService<M, O> {
    async fn begin(
        &self,
        request: Request<BeginRequest>,
    ) -> Result<Response<BeginResponse>, Status> {
        let owner = self.shared.connections.of(request.remote_addr());
        let segments: Vec<(u64, &[u8])> = request
            .get_ref()
            .segments
            .iter()
            .map(|s| (s.length, s.footer.as_slice()))
            .collect();
        let (declared, records) = declare(&segments)?;
        if records > MAX_UPLOAD_RECORDS {
            return Err(Status::invalid_argument(format!(
                "an upload of {records} records is over the {MAX_UPLOAD_RECORDS}-record limit"
            )));
        }
        // With a write tier the daemon's digests are trusted here and checked by the
        // flusher before the PUT; without one, the only copy is the PUT, so each record
        // is hashed as its bytes arrive. With one, each segment's records are written
        // to a range of their own on it as they arrive, if it has room (else held here).
        let trusted = self.shared.cache.write_tier().is_some();
        let mut ranges = Vec::with_capacity(declared.len());
        let mut held = 0u64;
        for d in &declared {
            let range = if trusted & (d.records_len > 0) {
                self.shared.cache.reserve_range(d.records_len).await
            } else {
                None
            };
            if range.is_none() {
                // A record held in memory until the commit must be small: memory is
                // bounded per file. A larger one, with no tier to write it to, is sent
                // by ByteStream instead, which streams it to the store: the client
                // falls back to that at this answer.
                let large = d
                    .entries
                    .iter()
                    .find(|e| e.digest.size_bytes > MAX_UNSTAGED_BLOB_BYTES);
                if let Some(large) = large {
                    return Err(Status::failed_precondition(unstaged_refusal(
                        &large.digest.to_string(),
                    )));
                }
                held = held.saturating_add(d.length);
            }
            ranges.push(range);
        }
        let cap = self.shared.config.max_bytes;
        if held > cap {
            return Err(Status::invalid_argument(format!(
                "an upload that holds {held} bytes in memory is over the {cap}-byte cap \
                 (--segment-upload-max-bytes)"
            )));
        }
        // `held <= cap <= --write-bytes-in-flight` (kbf-server refuses to start
        // otherwise), so this always ends.
        let admitted = {
            let _waiting = Waiting::enter(&self.shared.waiting);
            self.shared.cache.admit(held).await
        };
        let segments: Vec<Receiving> = declared
            .iter()
            .zip(ranges)
            .map(|(d, range)| Receiving::new(d, trusted, range))
            .collect();
        let (done, _) = watch::channel(None);
        let upload = Arc::new(Upload {
            owner,
            declared,
            phase: Mutex::new(Phase::Receiving {
                segments,
                _admitted: admitted,
            }),
            last_call: Mutex::new(Instant::now()),
            done,
        });
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.shared.uploads).insert(id, Arc::clone(&upload));
        tokio::spawn(Self::reap(Arc::clone(&self.shared), id, upload));
        Ok(Response::new(BeginResponse { upload: id }))
    }

    async fn put_range(
        &self,
        request: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        let upload = self.upload(&request, request.get_ref().upload)?;
        upload.touch();
        let PutRangeRequest {
            segment,
            offset,
            bytes,
            ..
        } = request.into_inner();
        if bytes.len() as u64 > MAX_RANGE_BYTES {
            return Err(Status::invalid_argument(format!(
                "a range of {} bytes is over the {MAX_RANGE_BYTES}-byte limit",
                bytes.len()
            )));
        }
        let index = segment as usize;
        let declared = upload.declared.get(index).ok_or_else(|| {
            Status::invalid_argument(format!("no segment {segment} in the upload"))
        })?;
        let (claimed, to_tier) = {
            let mut phase = lock(&upload.phase);
            match &mut *phase {
                Phase::Receiving { segments, .. } => {
                    let (put, claimed) = segments[index].put(declared, offset, &bytes)?;
                    if put == Put::Resent {
                        tracing::debug!(segment, offset, "resent range acknowledged, not written");
                    }
                    let to_tier = (put == Put::Written).then(|| segments[index].tier());
                    (claimed, to_tier.flatten())
                }
                Phase::Committing | Phase::Done => {
                    return Err(Status::failed_precondition(
                        "the upload is committing; ranges are no longer accepted",
                    ));
                }
                Phase::Dropped => return Err(Status::not_found("the upload was dropped")),
            }
        };
        if let Some((range, writes)) = to_tier {
            write_to_tier(range, writes, offset, bytes).await?;
        }
        if !claimed.is_empty() {
            // Hash what this range completed or extended, here and now, beside the
            // other ranges' own: the commit then has nothing left to hash. The task
            // records its results itself, so a call cancelled meanwhile leaves no
            // record claimed forever.
            let task = Arc::clone(&upload);
            self.shared
                .cache
                .hashing()
                .run(Box::new(move || {
                    advance(&claimed, |i, finished| task.check_next(index, i, finished));
                }))
                .await?;
            // A mismatch is refused at once; the upload cannot commit.
            if let Some(refused) = upload.refusal_at(index) {
                return Err(refused);
            }
        }
        Ok(Response::new(PutRangeResponse {}))
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        let upload = self.upload(&request, request.get_ref().upload)?;
        upload.touch();
        {
            let mut phase = lock(&upload.phase);
            if let Phase::Receiving { segments, .. } = &mut *phase {
                segments.iter_mut().for_each(Receiving::forget_failed);
                let missing = segments
                    .iter()
                    .zip(&upload.declared)
                    .find_map(|(r, d)| r.first_missing(d));
                if let Some(digest) = missing {
                    // The upload stays open: the daemon may send what is missing.
                    return Err(Status::failed_precondition(format!(
                        "record {digest} was not wholly received"
                    )));
                }
                if let Some(refused) = segments.iter().find_map(Receiving::not_ready) {
                    return Err(refused);
                }
            }
            match std::mem::replace(&mut *phase, Phase::Committing) {
                Phase::Receiving {
                    segments,
                    _admitted: admitted,
                } => {
                    tokio::spawn(Self::commit_task(
                        Arc::clone(&self.shared.cache),
                        Arc::clone(&upload),
                        segments,
                        admitted,
                    ));
                }
                Phase::Dropped => {
                    *phase = Phase::Dropped;
                    return Err(Status::not_found("the upload was dropped"));
                }
                // Committing, or answered: wait for, or return, that answer.
                other => *phase = other,
            }
        }
        let committed = upload.answer().await?;
        Ok(Response::new(CommitResponse {
            committed: committed.iter().map(wire::digest_to_proto).collect(),
        }))
    }

    async fn locate(
        &self,
        request: Request<LocateRequest>,
    ) -> Result<Response<LocateResponse>, Status> {
        let digests = request
            .get_ref()
            .digests
            .iter()
            .map(|d| wire::digest(Some(d)))
            .collect::<Result<Vec<Digest>, Status>>()?;
        let found = self.shared.cache.locate(&digests).await?;
        let locations = digests
            .iter()
            .zip(found)
            .map(|(digest, location)| BlobLocation {
                digest: Some(wire::digest_to_proto(digest)),
                object: location.map(|l| ObjectRef {
                    epoch: l.object.epoch().get(),
                    seq: l.object.seq(),
                }),
                offset: location.map_or(0, |l| l.offset),
            })
            .collect();
        Ok(Response::new(LocateResponse { locations }))
    }

    async fn read_range(
        &self,
        request: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        let ReadRangeRequest {
            object,
            offset,
            length,
        } = request.into_inner();
        if length == 0 || length > MAX_RANGE_BYTES {
            return Err(Status::invalid_argument(format!(
                "a read of {length} bytes is outside 1..={MAX_RANGE_BYTES}"
            )));
        }
        let object = object.ok_or_else(|| Status::invalid_argument("no object named"))?;
        let id = ObjectId::new(Epoch::new(object.epoch), object.seq);
        match self
            .shared
            .cache
            .read_object_range(id, offset, length)
            .await?
        {
            Some(bytes) => Ok(Response::new(ReadRangeResponse {
                bytes: bytes.to_vec(),
            })),
            None => Err(Status::not_found(format!(
                "object {}/{} holds no {length} bytes at {offset}",
                object.epoch, object.seq
            ))),
        }
    }
}

#[cfg(test)]
#[path = "segments_tests.rs"]
mod tests;
