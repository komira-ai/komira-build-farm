//! Where a blob read whole goes while it is read: memory for [`crate::Router::read_blob`],
//! a file for a caller that streams ([`crate::Router::read_blob_into`]).

use std::convert::Infallible;
use std::future::Future;

use bytes::BytesMut;
use kbf_storage::StorageError;

/// What [`crate::Router::read_blob_into`] writes a blob into as it arrives, chunk by
/// chunk.
///
/// The router writes an owner's copy as it reads it and checks it against the digest at
/// the end. A copy that turns out not to be the blob (other bytes, too few, too many, a
/// read that broke part way) is discarded before the next owner's is written, so what the
/// sink holds when the read succeeds is the blob and nothing else.
pub trait BlobSink: Send {
    /// Why the sink could not take bytes, or could not discard them.
    type Error: Send;

    /// Takes the next bytes of the copy being read.
    fn write(&mut self, chunk: &[u8]) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Drops every byte written since the read began, leaving the sink as it was before.
    fn discard(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Why [`crate::Router::read_blob_into`] failed.
#[derive(Debug)]
pub enum IntoError<E> {
    /// The owners: as [`crate::Router::read_blob`] fails.
    Storage(StorageError),
    /// The sink, on a write or a discard. The read stopped there: no other owner would
    /// make the sink take bytes.
    Sink(E),
}

/// The blob in memory, for [`crate::Router::read_blob`].
#[derive(Default)]
pub(crate) struct Memory(pub BytesMut);

impl BlobSink for Memory {
    type Error = Infallible;

    async fn write(&mut self, chunk: &[u8]) -> Result<(), Infallible> {
        self.0.extend_from_slice(chunk);
        Ok(())
    }

    async fn discard(&mut self) -> Result<(), Infallible> {
        self.0.clear();
        Ok(())
    }
}
