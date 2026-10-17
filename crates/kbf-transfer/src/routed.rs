//! [`RoutedCas`]: the daemon's data plane over the storage map.
//!
//! The same [`Cas`] a runtime uses against [`crate::Transfers`], but each blob goes to
//! the storage nodes that own it (`kbf_route::Router`): reads from the owners in rank
//! order (callers of one blob at once share one read), writes to the first owner that is
//! not down, FindMissing split by primary. A daemon holds a connection to a frontend for
//! work, and to storage nodes for bytes.
//!
//! What it keeps from the engine: at most `--transfer-in-flight` requests at once for the
//! process, FindMissing in calls of at most `--transfer-find-missing-chunk` digests, a
//! failed FindMissing answering "missing", and every blob checked against its digest.
//!
//! What it does not have yet:
//! - **Segment uploads.** [`Cas::put_records`] is the trait's default, a blob at a time,
//!   because a segment upload goes to one node and an action's outputs have different
//!   owners; one segment session per owner is the next step.
//! - **Streamed writes.** [`Cas::put_chunks`] is the trait's default, which gathers the
//!   chunks, because the storage client has no streamed write ([`kbf_storage::Storage`]
//!   takes a blob whole).
//! - **A byte budget** for reads into memory ([`Cas::get`]); `--transfer-bytes-in-flight`
//!   does not apply. Reads into a file stream, and hold one chunk; a copy that turns out
//!   not to be the blob is cut off the file and the next owner is read from the start, and
//!   a read that fails or is dropped part way leaves the file as it was.

use std::future::Future;
use std::io::{self, Seek as _, SeekFrom, Write as _};
use std::os::fd::AsFd as _;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::future::join_all;
use kbf_proto::reapi::Digest;
use kbf_route::{BlobSink, IntoError, Router};
use kbf_storage::StorageError;
use kbf_types::{DigestFunction, LeaseId};
use tokio::io::AsyncSeekExt as _;
use tokio::sync::Semaphore;

use crate::cas::{Cas, CasError, label};
use crate::config::TransferConfig;
use crate::spool::to_proto;

/// The daemon's data plane over the storage map. Cheap to clone through its `Arc`.
pub struct RoutedCas {
    router: Router,
    /// Requests in flight for the whole process.
    requests: Semaphore,
    in_flight: usize,
    find_missing_chunk: usize,
}

impl RoutedCas {
    /// A data plane calling the nodes through `router`, within the limits `config` names.
    #[must_use]
    pub fn new(router: Router, config: &TransferConfig) -> Self {
        let in_flight = config.in_flight.clamp(1, Semaphore::MAX_PERMITS);
        Self {
            router,
            requests: Semaphore::new(in_flight),
            in_flight,
            find_missing_chunk: config.find_missing_chunk.max(1),
        }
    }

    /// Runs `call` once a request permit is free. The semaphore is never closed.
    async fn limited<T>(&self, call: impl Future<Output = T>) -> T {
        let _permit = self.requests.acquire().await;
        call.await
    }
}

/// A file a blob is read into, from the offset it had when the read began, through a
/// handle of its own on the caller's open file: its writes move the caller's offset, and
/// the caller's handle never has one of them in flight.
///
/// Unless the read succeeded (`kept`), dropping it cuts the file back to where
/// the read began and goes back there, at once: a read that fails, and a read its caller
/// drops part way, leave the file as it was. A write already running when it is dropped
/// finishes first; one that has not begun writes nothing.
struct FileSink {
    handle: Arc<Mutex<Handle>>,
    start: u64,
    kept: bool,
}

/// The sink's handle on the file, and whether the sink has been dropped.
struct Handle {
    file: std::fs::File,
    /// Set when the sink is dropped: no write lands after the file is cut back.
    cut: bool,
}

impl Handle {
    /// Cuts the file back to `to` and goes back there.
    fn rewind(&mut self, to: u64) -> io::Result<()> {
        self.file.set_len(to)?;
        self.file.seek(SeekFrom::Start(to)).map(drop)
    }
}

impl FileSink {
    /// A sink writing into `file` from `start`, its offset once nothing is in flight on it.
    fn open(file: &tokio::fs::File, start: u64) -> io::Result<Self> {
        let file = std::fs::File::from(file.as_fd().try_clone_to_owned()?);
        Ok(Self {
            handle: Arc::new(Mutex::new(Handle { file, cut: false })),
            start,
            kept: false,
        })
    }

    /// Runs `work` on the handle, on a blocking thread.
    async fn run(
        &self,
        work: impl FnOnce(&mut Handle) -> io::Result<()> + Send + 'static,
    ) -> io::Result<()> {
        let handle = Arc::clone(&self.handle);
        tokio::task::spawn_blocking(move || {
            work(&mut handle.lock().unwrap_or_else(PoisonError::into_inner))
        })
        .await?
    }
}

impl BlobSink for FileSink {
    type Error = io::Error;

    async fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        let chunk = chunk.to_vec();
        self.run(move |handle| {
            let chunk: &[u8] = if handle.cut { &[] } else { &chunk };
            handle.file.write_all(chunk)
        })
        .await
    }

    async fn discard(&mut self) -> io::Result<()> {
        let start = self.start;
        self.run(move |handle| handle.rewind(start)).await
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        let mut handle = self.handle.lock().unwrap_or_else(PoisonError::into_inner);
        handle.cut = true;
        // No one to tell: the read has failed already, or its caller has gone.
        let _ = handle.rewind(self.start);
    }
}

/// `digest` as storage names it, or `None` when it is not a SHA-256 digest with a size,
/// which no store holds.
fn parse(digest: &Digest) -> Option<kbf_types::Digest> {
    kbf_types::Digest::parse(DigestFunction::Sha256, &label(digest)).ok()
}

/// A storage failure as the data plane's: only a blob the farm does not hold is missing,
/// and only bytes that are not the blob are corrupt.
fn failed(blob: String, error: &StorageError) -> CasError {
    match error {
        StorageError::NotFound(_) => CasError::Missing(blob),
        StorageError::Corrupt { actual, .. } => CasError::Corrupt(blob, label(&to_proto(actual))),
        other => CasError::Unavailable(blob, other.to_string()),
    }
}

impl Cas for RoutedCas {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let blob = label(digest);
        let Some(parsed) = parse(digest) else {
            return Err(CasError::Missing(blob));
        };
        match self.limited(self.router.read_blob(&parsed)).await {
            Ok(bytes) => Ok(bytes.to_vec()),
            Err(e) => Err(failed(blob, &e)),
        }
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let blob = format!("of {} bytes", bytes.len());
        match self.limited(self.router.put(Bytes::from(bytes))).await {
            Ok(digest) => Ok(to_proto(&digest)),
            Err(e) => Err(failed(blob, &e)),
        }
    }

    /// Streams the blob into `file` from its offset, one chunk at a time, through
    /// [`Router::read_blob_into`]: the owners are tried as [`Cas::get`] tries them, and
    /// an owner's copy that is not the blob is cut off the file before the next owner's is
    /// written. On every error, and when the read is dropped part way, the file holds what
    /// it held before and its offset is where it was.
    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        let blob = label(digest);
        let Some(parsed) = parse(digest) else {
            return Err(CasError::Missing(blob));
        };
        // The offset is known once what the caller's handle had in flight has landed, so
        // nothing of the caller's lands among the blob's bytes.
        let opened = file
            .stream_position()
            .await
            .and_then(|start| FileSink::open(file, start));
        let mut sink = opened.map_err(|e| CasError::Write(blob.clone(), e.to_string()))?;
        let _permit = self.requests.acquire().await;
        match self.router.read_blob_into(&parsed, &mut sink).await {
            Ok(()) => {
                sink.kept = true;
                Ok(())
            }
            Err(IntoError::Storage(e)) => Err(failed(blob, &e)),
            Err(IntoError::Sink(e)) => Err(CasError::Write(blob, e.to_string())),
        }
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        // A digest that does not parse is held by no store: ask nothing about it.
        let Some(parsed) = digests.iter().map(parse).collect::<Option<Vec<_>>>() else {
            return digests.to_vec();
        };
        let calls = parsed
            .chunks(self.find_missing_chunk)
            .map(|chunk| async move {
                let asked = self.limited(self.router.find_missing(chunk)).await;
                // A call that fails answers "missing" for its digests: FindMissing only
                // saves uploads.
                asked.unwrap_or_else(|_| chunk.to_vec())
            });
        join_all(calls)
            .await
            .into_iter()
            .flatten()
            .map(|d| to_proto(&d))
            .collect()
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }
}
