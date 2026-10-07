//! The blobs a lease reads and writes: the [`Cas`] trait and [`MemoryCas`].
//!
//! The driver fetches an action, its command and its input tree through a `Cas`, and
//! stores outputs, stdout and stderr through it. The daemon's CAS client (over
//! ByteStream, with the local cache in front) implements the same trait; `MemoryCas`
//! holds blobs in this process, for tests and simulation.
//!
//! Every blob is hashed on arrival: a `Cas` that hands back the wrong bytes is caught
//! here, not trusted ([`fetch`]).
//!
//! A file the driver stores (an output, stdout, stderr) reaches the CAS as a
//! [`FileBlob`]: hashed, then sent, one [`CHUNK`] at a time, so a file of any size
//! costs the daemon one chunk of memory, not the file.

use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex, PoisonError};

use kbf_proto::reapi::Digest;
use sha2::{Digest as _, Sha256};

/// Why a blob could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CasError {
    /// The CAS does not hold the blob.
    #[error("blob {0} is not in the CAS")]
    Missing(String),
    /// The bytes the CAS returned do not hash to the digest asked for.
    #[error("blob {0} failed verification: its bytes hash to {1}")]
    Corrupt(String, String),
    /// The file a [`FileBlob`] reads could not be read back for storing.
    #[error("blob {0} could not be read from its file: {1}")]
    Read(String, String),
}

/// A content-addressed blob store, SHA-256 only.
pub trait Cas: Send + Sync + 'static {
    /// The blob's bytes. Callers verify them ([`fetch`] does).
    fn get(&self, digest: &Digest) -> impl Future<Output = Result<Vec<u8>, CasError>> + Send;

    /// Stores `bytes` and returns their digest.
    fn put(&self, bytes: Vec<u8>) -> impl Future<Output = Result<Digest, CasError>> + Send;

    /// Stores the file `blob` reads, taking its bytes one [`FileBlob::next_chunk`] at a
    /// time, and returns its digest. The bytes must hash to [`FileBlob::digest`] (a file
    /// that changed after it was hashed does not): the CAS checks, as a ByteStream
    /// server does on commit, and answers [`CasError::Corrupt`] otherwise.
    fn put_file(&self, blob: FileBlob) -> impl Future<Output = Result<Digest, CasError>> + Send;
}

/// How many bytes of a [`FileBlob`] are read, hashed or sent at a time: the most of a
/// file the daemon holds in memory.
pub const CHUNK: usize = 1 << 20;

/// A regular file on its way into the CAS, never in memory whole.
///
/// REAPI names a blob by its digest before its bytes are written (ByteStream's
/// resource name holds it), so the file is read twice: [`FileBlob::hash`] reads it in
/// chunks to learn the digest, then [`FileBlob::next_chunk`] reads it again, chunk by
/// chunk, for the upload. A file that shrank in between fails `next_chunk`; one whose
/// bytes changed no longer hashes to the digest, which [`Cas::put_file`] checks.
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
}

/// The SHA-256 digest of `bytes`.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
    }
}

/// Reads a blob and checks its bytes against `digest`.
pub async fn fetch(cas: &impl Cas, digest: &Digest) -> Result<Vec<u8>, CasError> {
    let bytes = cas.get(digest).await?;
    let actual = digest_of(&bytes);
    if actual != *digest {
        return Err(CasError::Corrupt(
            label(digest),
            format!("{}/{}", actual.hash, actual.size_bytes),
        ));
    }
    Ok(bytes)
}

/// `hash/size`, the way REAPI resource names spell a digest.
pub(crate) fn label(digest: &Digest) -> String {
    format!("{}/{}", digest.hash, digest.size_bytes)
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

    async fn put_file(&self, mut blob: FileBlob) -> Result<Digest, CasError> {
        let expected = label(blob.digest());
        let mut bytes = Vec::new();
        while let Some(chunk) = blob
            .next_chunk()
            .await
            .map_err(|e| CasError::Read(expected.clone(), e.to_string()))?
        {
            bytes.extend_from_slice(&chunk);
        }
        let actual = digest_of(&bytes);
        if actual != *blob.digest() {
            return Err(CasError::Corrupt(expected, label(&actual)));
        }
        self.lock().insert(expected, bytes);
        Ok(actual)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    /// Catches a digest that is not REAPI's SHA-256 spelling (lowercase hex, byte size).
    #[test]
    fn digest_of_is_sha256_hex_and_size() {
        let digest = digest_of(b"abc");
        assert_eq!(
            digest.hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest.size_bytes, 3);
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
        let error = block_on(cas.put_file(blob)).expect_err("refused");
        assert!(matches!(error, CasError::Corrupt(..)), "{error}");
        assert_eq!(cas.blob(&digest_of(b"before")), None);
        assert_eq!(cas.blob(&digest_of(b"after!")), None);

        let (path, file) = file_holding("cas-shrank", b"before");
        let blob = FileBlob::hash(file, 6).expect("read").expect("fits");
        std::fs::write(&path, b"bef").expect("truncate");
        let error = block_on(cas.put_file(blob)).expect_err("refused");
        assert!(matches!(error, CasError::Read(..)), "{error}");
        assert!(error.to_string().contains("could not be read"), "{error}");

        let (_, file) = file_holding("cas-stored", b"stored");
        let blob = FileBlob::hash(file, 6).expect("read").expect("fits");
        let digest = block_on(cas.put_file(blob)).expect("stored");
        assert_eq!(cas.blob(&digest), Some(b"stored".to_vec()));
    }
}
