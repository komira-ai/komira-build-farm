//! [`Taking`]: the in-process plane's [`Intake`], which takes each output file's bytes as
//! the walk reads them to hash them, so a file is read once, whatever its size.
//!
//! The walk hands each chunk it reads to the plane ([`IntakeFile::write`]) as it hashes
//! it, holding one chunk itself (its per-file bound). What the plane does with it
//! depends on the file's size:
//!
//! - **A large file** (over [`MAX_UNSTAGED_BLOB_BYTES`], 16 MiB) is streamed, never held
//!   in memory, whatever its size ([`LocalStore::stream`](kbf_server::LocalStore::stream)):
//!   onto the server's write tier when it has room for it, each chunk written to a block
//!   of the file's own as it arrives; else (no tier, or no room) straight to the object
//!   store, each chunk copied into a bounded queue (the walk waits when it is full) and
//!   handed to the store's write (a multipart upload on S3), hashed as it passes. When
//!   the walk finishes the file (its digest known), the server checks the length and
//!   CRC-32C, the plane asks FindMissing for it, and commits it unless it is held
//!   already (or one with its digest was taken already): then the bytes on the tier are
//!   dropped, or the upload to the store aborted. On the tier, the flusher checks the
//!   digest before it PUTs the block; to the store, the commit claims the digest and
//!   the store completes the object only if the bytes hash to it. A file the walk drops
//!   unfinished stores nothing.
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
use futures::future::BoxFuture;
use kbf_front::MAX_UNSTAGED_BLOB_BYTES;
use kbf_storage::{PutStream, StorageError, StreamedPut};
use kbf_transfer::{
    Cas, CasError, DEFAULT_SEGMENT_UPLOADS, Intake, IntakeFile, MAX_UPLOAD_RECORDS, Record,
    SEGMENT_UPLOAD_BYTES, Source,
};
use tokio::task::JoinHandle;

use crate::LocalDataPlane;

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
        finished: Result<Box<dyn StreamedPut>, StorageError>,
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
            let stream = self.0.plane.store().stream(size)?;
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

/// One file streamed onto the write tier, or to the store, as the walk reads it:
/// nothing of it is held.
struct StreamedFile {
    /// The stream, or why it stopped (a write the tier refused): then nothing more is
    /// written, and the finish answers that.
    stream: Result<Box<dyn PutStream>, StorageError>,
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

#[cfg(test)]
#[path = "intake_tests.rs"]
mod tests;
