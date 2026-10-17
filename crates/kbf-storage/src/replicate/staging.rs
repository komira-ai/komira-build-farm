//! A blob being written to this node's store as it arrives: hashed on the way, appended
//! to a [`PutStream`] by a thread of its own, and committed only when the caller says so.

use bytes::Bytes;
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{PutStream, StorageError, StreamedPut};

/// Chunks the writer thread holds ahead of the store.
const QUEUE: usize = 2;

type Worker = JoinHandle<Result<Box<dyn StreamedPut>, StorageError>>;

/// One blob on its way into the store. Dropped before [`Staging::finish`] and
/// committed, nothing is stored.
pub(crate) struct Staging {
    tx: mpsc::Sender<Bytes>,
    worker: Worker,
}

impl Staging {
    /// Starts writing a blob that claims to be `claimed`: its chunks, in order, through
    /// `put`. The writer blocks on the store, so it has a thread of its own.
    pub(crate) fn start(put: Box<dyn PutStream>, claimed: Digest) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        let worker = tokio::task::spawn_blocking(move || write(put, claimed, rx));
        Self { tx, worker }
    }

    /// Hands the next chunk to the writer, waiting while it is behind.
    ///
    /// # Errors
    /// Why the writer ended, if it did: the store's write failed.
    pub(crate) async fn push(&mut self, chunk: Bytes) -> Result<(), StorageError> {
        if self.tx.send(chunk).await.is_ok() {
            return Ok(());
        }
        // The writer returns early only for an error, or a panic.
        Err(match (&mut self.worker).await {
            Ok(Err(why)) => why,
            Ok(Ok(_)) | Err(_) => StorageError::Internal("the blob writer ended early".into()),
        })
    }

    /// Ends the blob and waits for the writer: the bytes are checked against the digest
    /// the blob claimed and written, not yet present ([`StreamedPut::commit`]).
    ///
    /// # Errors
    /// [`StorageError::DigestMismatch`] if the bytes are not the blob it claimed (nothing
    /// is stored); the store's write failed.
    pub(crate) async fn finish(self) -> Result<Box<dyn StreamedPut>, StorageError> {
        let Self { tx, worker } = self;
        drop(tx);
        match worker.await {
            Ok(written) => written,
            Err(panicked) => Err(StorageError::Internal(format!(
                "the blob writer panicked: {panicked}"
            ))),
        }
    }
}

/// The writer thread: appends each chunk, and once the chunks end, checks the bytes
/// against `claimed` and finishes the put.
fn write(
    mut put: Box<dyn PutStream>,
    claimed: Digest,
    mut chunks: mpsc::Receiver<Bytes>,
) -> Result<Box<dyn StreamedPut>, StorageError> {
    let mut hash = Sha256::new();
    let mut crc = 0;
    let mut size = 0;
    while let Some(chunk) = chunks.blocking_recv() {
        hash.update(&chunk);
        crc = crc32c::crc32c_append(crc, &chunk);
        size += chunk.len() as u64;
        put.append(&chunk)?;
    }
    let actual = Digest::new(DigestFunction::Sha256, hash.finalize().into(), size);
    if actual != claimed {
        return Err(StorageError::DigestMismatch { claimed, actual });
    }
    put.finish(claimed, crc)
}
