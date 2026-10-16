//! [`Taking`]: the in-process plane's [`Intake`], which takes each output file's bytes as
//! the walk reads them to hash them, so a file is read once, whatever its size.
//!
//! The walk hands each chunk it reads to the plane ([`IntakeFile::write`]) as it hashes
//! it, holding one chunk itself (its per-file bound). What the plane does with it
//! depends on the file's size:
//!
//! - **A large file** (over [`MAX_UNSTAGED_BLOB_BYTES`], 16 MiB) is streamed, never held
//!   in memory, whatever its size:
//!   - onto the server's write tier when it has one with room for it
//!     ([`LocalStore::stream`](kbf_server::LocalStore::stream)): each chunk is written to
//!     a block of the file's own as it arrives, from the walk's buffer. When the walk
//!     finishes it (its digest known), the server checks the length and CRC-32C, the
//!     plane asks FindMissing for it, and commits it unless it is held already (the
//!     bytes on the tier are then dropped). The flusher checks its digest before it PUTs
//!     the block, as for any staged upload;
//!   - else (no write tier, or no room on it) straight to the object store
//!     ([`LocalStore::put_streamed`](kbf_server::LocalStore::put_streamed)), as a
//!     ByteStream `Write` of a large blob is: each chunk is copied into a bounded queue
//!     ([`COLD_CHUNKS_IN_FLIGHT`] chunks; the walk waits when it is full) and handed to
//!     the store as it comes (a multipart upload on S3), hashed as it passes. When the
//!     walk finishes the file, its digest is claimed (or withdrawn, if one with it was
//!     taken already or the cache holds it): the server completes the upload only if
//!     the bytes hash to it. A file the walk drops unfinished withdraws its upload,
//!     which is aborted.
//! - **A small file** is kept in memory: when the walk finishes it, its bytes become a
//!   [`Record`] of the plane's batch ([`Source::Bytes`], no copy). A batch of
//!   [`SEGMENT_UPLOAD_BYTES`] or [`MAX_UPLOAD_RECORDS`] goes to [`Cas::put_records`] at
//!   once, on the server half's runtime, while the walk goes on: FindMissing for its
//!   digests, then the uploads the transfer engine would plan for the missing ones
//!   (`Begin`, `PutRange`, `Commit`), whose bytes are those buffers (written through to
//!   the write tier when there is one). No more than [`DEFAULT_SEGMENT_UPLOADS`] batches
//!   are in flight; a walk that finishes a file past that waits for one to end.
//!
//! Two files with one digest are stored once. [`Intake::finish`] sends the rest and
//! waits for every batch and every streamed file.

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::future::{BoxFuture, FutureExt as _};
use futures::stream::StreamExt as _;
use kbf_front::MAX_UNSTAGED_BLOB_BYTES;
use kbf_server::{LocalStream, LocalStreamed};
use kbf_storage::StorageError;
use kbf_transfer::{
    Cas, CasError, DEFAULT_SEGMENT_UPLOADS, Intake, IntakeFile, MAX_UPLOAD_RECORDS, Record,
    SEGMENT_UPLOAD_BYTES, Source,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::LocalDataPlane;

/// The most chunks of a large file streamed to the object store held in the queue
/// between the walk and the store: the walk waits for room past it.
pub(crate) const COLD_CHUNKS_IN_FLIGHT: usize = 2;

/// One walk's intake into a [`LocalDataPlane`].
pub(crate) struct Taking(Arc<Shared>);

struct Shared {
    plane: LocalDataPlane,
    /// A batch of this many bytes is sent ([`SEGMENT_UPLOAD_BYTES`]).
    batch_bytes: u64,
    /// The most batches in flight ([`DEFAULT_SEGMENT_UPLOADS`]).
    uploads: usize,
    state: Mutex<State>,
    /// Signalled whenever a batch's upload ends.
    ended: Condvar,
}

#[derive(Default)]
struct State {
    /// Files finished and not yet sent.
    batch: Vec<Record>,
    /// Their bytes.
    bytes: u64,
    /// Every digest taken: a second file with one is not stored again.
    seen: BTreeSet<kbf_types::Digest>,
    /// Batches sent whose upload has not ended.
    in_flight: usize,
    uploads: Vec<JoinHandle<Result<(), CasError>>>,
}

impl State {
    /// The batch so far, counted in flight.
    fn take(&mut self) -> Vec<Record> {
        self.bytes = 0;
        self.in_flight += 1;
        std::mem::take(&mut self.batch)
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Taking {
    /// The intake of a walk into `plane`.
    pub(crate) fn new(plane: LocalDataPlane) -> Self {
        Self::with(plane, SEGMENT_UPLOAD_BYTES, DEFAULT_SEGMENT_UPLOADS)
    }

    /// [`Taking::new`], sending a batch of `batch_bytes` with at most `uploads` in flight.
    fn with(plane: LocalDataPlane, batch_bytes: u64, uploads: usize) -> Self {
        Self(Arc::new(Shared {
            plane,
            batch_bytes,
            uploads,
            state: Mutex::default(),
            ended: Condvar::new(),
        }))
    }
}

impl Shared {
    /// Adds a finished file to the batch, and sends the batch once it is full, after
    /// waiting for room among the batches in flight.
    fn add(self: &Arc<Self>, record: Record) {
        let batch = {
            let mut state = lock(&self.state);
            if !state.seen.insert(record.digest) {
                return;
            }
            state.bytes += record.digest.size_bytes;
            state.batch.push(record);
            // Both parts are looked at, whichever is true: no short-circuit to cover.
            let full =
                (state.bytes >= self.batch_bytes) | (state.batch.len() >= MAX_UPLOAD_RECORDS);
            if !full {
                return;
            }
            while state.in_flight >= self.uploads {
                state = self
                    .ended
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            state.take()
        };
        self.send(batch);
    }

    /// Stores `batch` on the server half's runtime. Called with the state unlocked: a
    /// runtime that is gone drops the upload, and its [`Ended`] with it, at once.
    fn send(self: &Arc<Self>, batch: Vec<Record>) {
        let (plane, ended) = (self.plane.clone(), Ended(Arc::clone(self)));
        let upload = self.plane.server().spawn(async move {
            let _ended = ended;
            plane.put_records(batch).await
        });
        lock(&self.state).uploads.push(upload);
    }
}

/// Counts a batch's upload ended when it is dropped, whether it ran or not.
struct Ended(Arc<Shared>);

impl Drop for Ended {
    fn drop(&mut self) {
        lock(&self.0.state).in_flight -= 1;
        self.0.ended.notify_all();
    }
}

impl Shared {
    /// Stores the streamed file `finished` (or answers why it could not be), unless one
    /// with its digest was taken already, on the server half's runtime: FindMissing,
    /// then the commit if it is missing.
    fn store_streamed(
        self: &Arc<Self>,
        digest: kbf_types::Digest,
        finished: Result<LocalStreamed, StorageError>,
    ) {
        if !lock(&self.state).seen.insert(digest) {
            return;
        }
        let store = self.plane.store().clone();
        let upload = self.plane.server().spawn(async move {
            let streamed = finished.map_err(not_stored)?;
            let held = store
                .find_missing(&[digest])
                .await
                .is_ok_and(|m| m.is_empty());
            if held {
                return Ok(());
            }
            streamed.commit().await.map_err(not_stored)
        });
        lock(&self.state).uploads.push(upload);
    }
}

/// A streamed file the server refused or could not store.
fn not_stored(e: StorageError) -> CasError {
    CasError::Unavailable("outputs".to_owned(), e.to_string())
}

impl Intake for Taking {
    fn file(&self, size: u64) -> Option<Box<dyn IntakeFile>> {
        if size > MAX_UNSTAGED_BLOB_BYTES {
            // Streamed onto the write tier, or to the store when the tier is missing or
            // has no room for it: never held in memory, never refused.
            let Some(stream) = self.0.plane.store().stream(size) else {
                return Some(Box::new(ColdFile::start(&self.0, size)));
            };
            return Some(Box::new(StreamedFile {
                stream: Ok(stream),
                shared: Arc::clone(&self.0),
            }));
        }
        Some(Box::new(TakenFile {
            bytes: Vec::with_capacity(usize::try_from(size).unwrap_or(0)),
            shared: Arc::clone(&self.0),
        }))
    }

    fn finish(&self) -> BoxFuture<'_, Result<(), CasError>> {
        Box::pin(async move {
            let batch = {
                let mut state = lock(&self.0.state);
                (!state.batch.is_empty()).then(|| state.take())
            };
            if let Some(batch) = batch {
                self.0.send(batch);
            }
            let uploads = std::mem::take(&mut lock(&self.0.state).uploads);
            let mut answer = Ok(());
            for upload in uploads {
                let done = upload.await.unwrap_or_else(|gone| {
                    Err(CasError::Unavailable(
                        "outputs".to_owned(),
                        format!("the server half did not answer: {gone}"),
                    ))
                });
                answer = answer.and(done);
            }
            answer
        })
    }
}

/// One file's bytes, as the walk reads them.
struct TakenFile {
    bytes: Vec<u8>,
    shared: Arc<Shared>,
}

impl IntakeFile for TakenFile {
    fn write(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
    }

    fn finish(self: Box<Self>, digest: kbf_types::Digest, crc32c: u32) {
        let Self { bytes, shared } = *self;
        shared.add(Record {
            digest,
            crc32c,
            source: Source::Bytes(Bytes::from(bytes)),
        });
    }
}

/// One file streamed onto the write tier as the walk reads it: nothing of it is held.
struct StreamedFile {
    /// The stream, or why it stopped (a write the tier refused): then nothing more is
    /// written, and the finish answers that.
    stream: Result<LocalStream, StorageError>,
    shared: Arc<Shared>,
}

impl IntakeFile for StreamedFile {
    fn write(&mut self, chunk: &[u8]) {
        if let Ok(stream) = &mut self.stream
            && let Err(e) = stream.append(chunk)
        {
            self.stream = Err(e);
        }
    }

    fn finish(self: Box<Self>, digest: kbf_types::Digest, crc32c: u32) {
        let Self { stream, shared } = *self;
        let finished = stream.and_then(|stream| stream.finish(digest, crc32c));
        shared.store_streamed(digest, finished);
    }
}

/// One file streamed straight to the object store as the walk reads it
/// ([`LocalStore::put_streamed`](kbf_server::LocalStore::put_streamed)), its upload
/// running on the server half's runtime from the file's first chunk: what this holds is
/// the queue between them.
struct ColdFile {
    /// The queue of the file's chunks to the upload; `None` once the upload stopped
    /// taking them (it failed, and answers why).
    bytes: Option<mpsc::Sender<std::io::Result<Bytes>>>,
    /// Where the file's digest is claimed once it is finished; dropped, it withdraws
    /// the file.
    claim: oneshot::Sender<kbf_types::Digest>,
    shared: Arc<Shared>,
}

impl ColdFile {
    /// Starts the upload of a file of `size` bytes, counted among `shared`'s uploads.
    fn start(shared: &Arc<Shared>, size: u64) -> Self {
        let (bytes, queued) = mpsc::channel(COLD_CHUNKS_IN_FLIGHT);
        let (claim, claimed) = oneshot::channel();
        let body = futures::stream::unfold(queued, |mut queued| async move {
            let chunk = queued.recv().await?;
            Some((chunk, queued))
        })
        .boxed();
        let store = shared.plane.store().clone();
        let claim_of = store.clone();
        // The file's digest, unless the cache holds the blob already: then the upload
        // is withdrawn as for a second file of one digest.
        let claimed = async move {
            let digest = claimed.await.ok()?;
            let missing = claim_of.find_missing(&[digest]).await;
            let held = missing.is_ok_and(|m| m.is_empty());
            (!held).then_some(digest)
        }
        .boxed();
        let upload = shared.plane.server().spawn(async move {
            let stored = store.put_streamed(size, body, claimed).await;
            stored.map(drop).map_err(not_stored)
        });
        lock(&shared.state).uploads.push(upload);
        Self {
            bytes: Some(bytes),
            claim,
            shared: Arc::clone(shared),
        }
    }
}

impl IntakeFile for ColdFile {
    /// Queues a copy of `chunk`, waiting for room: the walk's hashing thread, never the
    /// async runtime, calls it.
    fn write(&mut self, chunk: &[u8]) {
        let copy = Ok(Bytes::copy_from_slice(chunk));
        let sent = self.bytes.as_ref().map(|queue| queue.blocking_send(copy));
        if let Some(Err(_)) = sent {
            self.bytes = None;
        }
    }

    fn finish(self: Box<Self>, digest: kbf_types::Digest, _crc32c: u32) {
        let Self {
            bytes,
            claim,
            shared,
        } = *self;
        drop(bytes);
        // The first file of a digest claims it; a second withdraws its own upload.
        if lock(&shared.state).seen.insert(digest) {
            let _ = claim.send(digest);
        }
    }
}

#[cfg(test)]
#[path = "intake_tests.rs"]
mod tests;
