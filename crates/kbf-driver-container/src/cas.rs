//! Blobs the container driver stores: [`FileBlob`], a file on its way into the CAS
//! one [`CHUNK`] at a time, and [`MemoryCas`], a CAS in this process's memory.
//!
//! The driver reads and writes blobs through `kbf_daemon`'s [`Cas`] trait, the one the
//! daemon's ByteStream client of a front implements, and checks what it reads with
//! that crate's `fetch`. A file the driver stores (an output, stdout, stderr) goes to
//! [`Cas::put_chunks`] as a [`FileBlob`]: hashed, then sent one [`CHUNK`] at a time, so
//! a file of any size costs the daemon one chunk of memory, not the file.
//! `MemoryCas` holds blobs in this process, for tests and simulation.

use std::collections::BTreeMap;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex, PoisonError};

use futures::{StreamExt as _, stream};
use kbf_daemon::cas::{Cas, CasError, Chunks, WRITE_CHUNK_BYTES, digest_of, label};
use kbf_proto::reapi::Digest;
use sha2::{Digest as _, Sha256};

/// How many bytes of a [`FileBlob`] are read, hashed or sent at a time: the most of a
/// file the daemon holds in memory. One chunk fits one ByteStream write.
pub const CHUNK: usize = WRITE_CHUNK_BYTES;

/// A regular file on its way into the CAS, never in memory whole.
///
/// REAPI names a blob by its digest before its bytes are written (ByteStream's
/// resource name holds it), so the file is read twice: [`FileBlob::hash`] reads it in
/// chunks to learn the digest, then [`FileBlob::next_chunk`] reads it again, chunk by
/// chunk, for the upload. A file that shrank in between fails `next_chunk`; one whose
/// bytes changed no longer hashes to the digest, which the CAS checks on commit.
#[derive(Debug)]
pub struct FileBlob {
    file: Arc<std::fs::File>,
    digest: Digest,
    size: u64,
    /// Where the next chunk starts.
    offset: u64,
}

impl FileBlob {
    /// Hashes `file` from its start, one [`CHUNK`] at a time. `None` when it holds more
    /// than `max` bytes: at most `max + 1` are read, so a file past the limit is never
    /// read whole. Blocking; the bound is on the bytes read, not on `st_size` (a file
    /// can grow).
    pub fn hash(file: std::fs::File, max: u64) -> std::io::Result<Option<Self>> {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; CHUNK];
        let mut size = 0u64;
        loop {
            // One byte past `max` is enough to tell the file is too large.
            let want = max.saturating_add(1).saturating_sub(size).min(CHUNK as u64) as usize;
            let read = file.read_at(&mut buffer[..want], size)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size += read as u64;
            if size > max {
                return Ok(None);
            }
        }
        let digest = Digest {
            hash: hex::encode(hasher.finalize()),
            size_bytes: i64::try_from(size).unwrap_or(i64::MAX),
        };
        Ok(Some(Self {
            file: Arc::new(file),
            digest,
            size,
            offset: 0,
        }))
    }

    /// The digest the file hashed to.
    #[must_use]
    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// The file's size when it was hashed, in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The next [`CHUNK`] (or what is left, if less) of the file's bytes, read on
    /// tokio's blocking pool; `None` once all `size` bytes were returned. A file that
    /// shrank since it was hashed is an `UnexpectedEof` error.
    pub async fn next_chunk(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let left = self.size - self.offset;
        if left == 0 {
            return Ok(None);
        }
        let len = left.min(CHUNK as u64) as usize;
        let (file, offset) = (Arc::clone(&self.file), self.offset);
        let chunk = tokio::task::spawn_blocking(move || {
            let mut chunk = vec![0; len];
            file.read_exact_at(&mut chunk, offset).map(|()| chunk)
        })
        .await
        .map_err(std::io::Error::other)??;
        self.offset += len as u64;
        Ok(Some(chunk))
    }

    /// The file's bytes from where the next chunk starts, as [`Cas::put_chunks`] takes
    /// them: one [`FileBlob::next_chunk`] per item. A read that fails (the file shrank)
    /// is the last item.
    #[must_use]
    pub fn into_chunks(self) -> Chunks {
        Box::pin(stream::unfold(Some(self), |state| async move {
            let mut blob = state?;
            match blob.next_chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(blob))),
                Ok(None) => None,
                Err(e) => Some((Err(e), None)),
            }
        }))
    }

    /// Stores the file in `cas`, one chunk in memory at a time, and returns its digest.
    /// The CAS refuses bytes that no longer hash to [`FileBlob::digest`]
    /// ([`CasError::Corrupt`]); a file that shrank fails as [`CasError::Read`].
    ///
    /// # Errors
    /// The CAS refuses or fails the upload, or the file cannot be read back.
    pub async fn store(self, cas: &impl Cas) -> Result<Digest, CasError> {
        let digest = self.digest.clone();
        cas.put_chunks(digest, self.into_chunks()).await
    }
}

/// A CAS in this process's memory.
#[derive(Debug, Default)]
pub struct MemoryCas {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryCas {
    /// An empty CAS.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `bytes` and returns their digest.
    pub fn insert(&self, bytes: impl Into<Vec<u8>>) -> Digest {
        let bytes = bytes.into();
        let digest = digest_of(&bytes);
        self.lock().insert(label(&digest), bytes);
        digest
    }

    /// Replaces the bytes stored under `digest`, for tests of verification.
    pub fn corrupt(&self, digest: &Digest, bytes: impl Into<Vec<u8>>) {
        self.lock().insert(label(digest), bytes.into());
    }

    /// The blob under `digest`, unverified.
    #[must_use]
    pub fn blob(&self, digest: &Digest) -> Option<Vec<u8>> {
        self.lock().get(&label(digest)).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        // Each update is one map insert, so the map is consistent after a panic.
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Cas for MemoryCas {
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.blob(digest)
            .ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        Ok(self.insert(bytes))
    }

    /// Checks the bytes before it stores them, as a ByteStream server does on commit:
    /// bytes that do not hash to `digest` are refused and not stored.
    async fn put_chunks(&self, digest: Digest, mut chunks: Chunks) -> Result<Digest, CasError> {
        let expected = label(&digest);
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|e| CasError::Read(expected.clone(), e.to_string()))?;
            bytes.extend_from_slice(&chunk);
        }
        let actual = digest_of(&bytes);
        if actual != digest {
            return Err(CasError::Corrupt(expected, label(&actual)));
        }
        self.lock().insert(expected, bytes);
        Ok(actual)
    }
}

#[cfg(test)]
mod tests {
    use kbf_daemon::cas::fetch;

    use super::*;

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    /// Catches a round trip that loses or changes bytes.
    #[test]
    fn put_then_fetch_returns_the_bytes() {
        let cas = MemoryCas::new();
        let digest = block_on(cas.put(b"hello".to_vec())).expect("put");
        assert_eq!(block_on(fetch(&cas, &digest)).expect("fetch"), b"hello");
    }

    /// Catches a missing blob read as empty bytes.
    #[test]
    fn a_missing_blob_is_an_error() {
        let cas = MemoryCas::new();
        let digest = digest_of(b"never stored");
        assert_eq!(
            block_on(fetch(&cas, &digest)),
            Err(CasError::Missing(label(&digest)))
        );
    }

    /// Catches a flipped byte being trusted: the CAS answered, but with other bytes.
    #[test]
    fn corrupt_bytes_are_refused() {
        let cas = MemoryCas::new();
        let digest = cas.insert(b"good".to_vec());
        cas.corrupt(&digest, b"evil".to_vec());
        let error = block_on(fetch(&cas, &digest)).expect_err("refused");
        assert!(matches!(error, CasError::Corrupt(..)), "{error}");
        assert!(error.to_string().contains("failed verification"));
    }

    /// Writes `bytes` to a fresh file beside the test binary and opens it for reading.
    fn file_holding(name: &str, bytes: &[u8]) -> (std::path::PathBuf, std::fs::File) {
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit");
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write");
        let file = std::fs::File::open(&path).expect("open");
        (path, file)
    }

    /// Catches a file read whole rather than in chunks (each chunk is at most
    /// [`CHUNK`] bytes, and they add up to the file), a hash over other bytes than
    /// the file's, and the byte bound off by one or ignored: a file of exactly `max`
    /// bytes is hashed, one byte more is refused.
    #[test]
    fn a_file_blob_is_hashed_and_read_in_chunks() {
        let bytes: Vec<u8> = (0..2 * CHUNK + 3).map(|i| (i % 251) as u8).collect();
        let (_, file) = file_holding("cas-chunks", &bytes);
        let size = bytes.len() as u64;
        let refused = FileBlob::hash(file.try_clone().expect("dup"), size - 1).expect("read");
        let stored = FileBlob::hash(file.try_clone().expect("dup"), size)
            .expect("read")
            .expect("fits");
        let cas = MemoryCas::new();
        let digest = block_on(stored.store(&cas)).expect("stored");
        assert_eq!(cas.blob(&digest), Some(bytes.clone()));
        assert!(refused.is_none(), "{refused:?}");
        let mut blob = FileBlob::hash(file, size).expect("read").expect("fits");
        assert_eq!((blob.digest(), blob.size()), (&digest_of(&bytes), size));
        let mut chunks = Vec::new();
        while let Some(chunk) = block_on(blob.next_chunk()).expect("chunk") {
            chunks.push(chunk);
        }
        let sizes: Vec<usize> = chunks.iter().map(Vec::len).collect();
        assert_eq!(sizes, [CHUNK, CHUNK, 3]);
        assert_eq!(chunks.concat(), bytes);
    }

    /// Catches a file that changed between its hash and its upload being stored
    /// under the old digest (bytes changed: the CAS's check refuses it) or stored
    /// short (the file shrank: the read back fails).
    #[test]
    fn a_file_changed_after_it_was_hashed_is_not_stored() {
        let cas = MemoryCas::new();
        let (path, file) = file_holding("cas-changed", b"before");
        let blob = FileBlob::hash(file, 6).expect("read").expect("fits");
        std::fs::write(&path, b"after!").expect("rewrite");
        let error = block_on(blob.store(&cas)).expect_err("refused");
        assert!(matches!(error, CasError::Corrupt(..)), "{error}");
        assert_eq!(cas.blob(&digest_of(b"before")), None);
        assert_eq!(cas.blob(&digest_of(b"after!")), None);

        let (path, file) = file_holding("cas-shrank", b"before");
        let blob = FileBlob::hash(file, 6).expect("read").expect("fits");
        std::fs::write(&path, b"bef").expect("truncate");
        let error = block_on(blob.store(&cas)).expect_err("refused");
        assert!(matches!(error, CasError::Read(..)), "{error}");
        assert!(error.to_string().contains("could not be read"), "{error}");

        let (_, file) = file_holding("cas-stored", b"stored");
        let blob = FileBlob::hash(file, 6).expect("read").expect("fits");
        let digest = block_on(blob.store(&cas)).expect("stored");
        assert_eq!(cas.blob(&digest), Some(b"stored".to_vec()));
    }
}
