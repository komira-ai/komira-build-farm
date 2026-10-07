//! [`CasStore`]: the daemon's CAS as the store `kbf-outputs` writes outputs to.

use std::io::Read as _;
use std::sync::Arc;

use futures::stream;
use kbf_daemon::cas::{Cas, Chunks, WRITE_CHUNK_BYTES};
use kbf_outputs::{Store, StoreError};
use kbf_proto::reapi::Digest;

/// A `kbf_daemon::Cas` as a `kbf_outputs::Store`. A file goes to
/// [`Cas::put_chunks`] one chunk at a time, each read on the blocking pool when the
/// upload asks for it, so no file is held whole.
#[derive(Debug)]
pub(crate) struct CasStore<C>(pub(crate) Arc<C>);

impl<C: Cas> Store for CasStore<C> {
    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, StoreError> {
        Ok(self.0.put(bytes).await?)
    }

    async fn put_file(&self, file: std::fs::File, digest: Digest) -> Result<Digest, StoreError> {
        Ok(self.0.put_chunks(digest, chunks(file)).await?)
    }
}

/// The rest of `file`, a [`WRITE_CHUNK_BYTES`] chunk at a time. A read that fails
/// ends the stream after yielding its error.
fn chunks(file: std::fs::File) -> Chunks {
    Box::pin(stream::unfold(Some(file), |state| async move {
        let file = state?;
        let joined = tokio::task::spawn_blocking(move || {
            let mut chunk = Vec::new();
            let mut file = file;
            (&mut file)
                .take(WRITE_CHUNK_BYTES as u64)
                .read_to_end(&mut chunk)
                .map(|_| (file, chunk))
        })
        .await;
        // A task that did not finish (a panic, a runtime shutting down) is a failed read.
        let read = joined
            .map_err(std::io::Error::other)
            .and_then(std::convert::identity);
        match read {
            Ok((_, chunk)) if chunk.is_empty() => None,
            Ok((file, chunk)) => Some((Ok(chunk), Some(file))),
            Err(e) => Some((Err(e), None)),
        }
    }))
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;

    use super::*;

    /// Catches a file streamed with bytes lost or reordered across chunks, a stream
    /// that does not end at the end of the file, and a read error swallowed rather
    /// than yielded and ending the stream.
    #[tokio::test]
    async fn a_file_is_read_in_chunks_to_its_end() {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!("chunks-{}", std::process::id()));
        let bytes: Vec<u8> = (0..(2 * WRITE_CHUNK_BYTES + 3))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&path, &bytes).expect("write");
        let read: Vec<std::io::Result<Vec<u8>>> = chunks(std::fs::File::open(&path).expect("open"))
            .collect()
            .await;
        let sizes: Vec<usize> = read
            .iter()
            .map(|c| c.as_ref().expect("chunk").len())
            .collect();
        assert_eq!(sizes, [WRITE_CHUNK_BYTES, WRITE_CHUNK_BYTES, 3]);
        let joined: Vec<u8> = read.into_iter().flat_map(|c| c.expect("chunk")).collect();
        assert_eq!(joined, bytes);
        // A file open for writing only cannot be read: one error, then the end.
        let write_only = std::fs::File::create(&path).expect("create");
        let read: Vec<std::io::Result<Vec<u8>>> = chunks(write_only).collect().await;
        assert_eq!(read.len(), 1);
        assert!(read[0].is_err());
        std::fs::remove_file(&path).expect("remove");
    }
}
