//! A blob streamed through the storage boundary: [`Storage::stream`] over
//! [`CacheStorage`], onto the write tier when the cache has one, else to the object
//! store. A caller that writes through the trait (the storage node, a replica) gets a
//! stream wherever the cache can take one, its chunks written as they come, and a
//! stream whose bytes are not the digest it ends as stores nothing.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use kbf_front::{Cache, CacheStorage, MemoryMetaLog, SegmentsConfig};
use kbf_meta::ServerId;
use kbf_objstore::MemoryStore;
use kbf_segments::RecordHasher;
use kbf_storage::{PutStream, Storage, StorageError};
use kbf_store::StdFs;
use kbf_tier::TierOptions;
use kbf_types::Digest;

const MIB: usize = 1 << 20;

/// A blob larger than a store part, a tier write and a gRPC message: 20 MiB.
const LARGE: usize = 20 * MIB;

type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

/// `len` bytes that repeat only every 251 bytes and differ for each `seed`, so a chunk
/// lost, repeated or moved changes the blob.
fn patterned(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + seed * 7) % 251) as u8)
        .collect()
}

fn digest_and_crc(bytes: &[u8]) -> (Digest, u32) {
    let mut hasher = RecordHasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// A directory of its own for a write tier.
fn tier_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-front-storage-stream")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tier directory");
    dir
}

/// An in-memory cache with no write tier, made on the test's runtime.
fn cold() -> Arc<MemoryCache> {
    Arc::new(Cache::memory())
}

/// An in-memory cache with a write tier of 256 MiB in a directory of its own.
async fn tiered(name: &str) -> Arc<MemoryCache> {
    let fs = StdFs::new(tier_dir(name)).expect("tier fs");
    let (cache, _) = Cache::memory()
        .attach_tier(fs, TierOptions::new(256 << 20), ServerId::new(1), None)
        .await
        .expect("attach the tier");
    Arc::new(cache)
}

fn storage(cache: &Arc<MemoryCache>) -> Arc<dyn Storage> {
    Arc::new(CacheStorage::new(
        Arc::clone(cache),
        SegmentsConfig::DEFAULT,
    ))
}

/// The committed bytes on `cache`'s write tier not yet flushed.
fn on_tier(cache: &MemoryCache) -> u64 {
    let tier = cache.write_tier().expect("a tier");
    tier.unflushed(Instant::now()).bytes
}

/// A stream for `size` bytes, opened off the runtime as the trait asks.
async fn open(storage: &Arc<dyn Storage>, size: u64) -> Option<Box<dyn PutStream>> {
    let storage = Arc::clone(storage);
    tokio::task::spawn_blocking(move || storage.stream(size))
        .await
        .expect("stream")
}

/// Appends `bytes` to `stream` in pieces of `chunk` bytes, off the runtime.
async fn append(
    mut stream: Box<dyn PutStream>,
    bytes: Arc<Vec<u8>>,
    chunk: usize,
) -> Box<dyn PutStream> {
    tokio::task::spawn_blocking(move || {
        for piece in bytes.chunks(chunk) {
            stream.append(piece).expect("append");
        }
        stream
    })
    .await
    .expect("append")
}

/// Streams `bytes` in 1 MiB pieces and ends the stream as `digest` with `crc32c`;
/// answers what `finish` answered, and the commit's answer if it went on to one.
async fn stream_as(
    storage: &Arc<dyn Storage>,
    bytes: &Arc<Vec<u8>>,
    digest: Digest,
    crc32c: u32,
) -> Result<Result<(), StorageError>, StorageError> {
    let stream = open(storage, bytes.len() as u64)
        .await
        .expect("the cache can stream it");
    let stream = append(stream, Arc::clone(bytes), MIB).await;
    let finished = tokio::task::spawn_blocking(move || stream.finish(digest, crc32c))
        .await
        .expect("finish");
    match finished {
        Ok(streamed) => Ok(streamed.commit().await),
        Err(e) => Err(e),
    }
}

/// Asserts `digest` is not held: reported missing, and not read.
async fn not_held(storage: &Arc<dyn Storage>, digest: Digest) {
    let missing = storage.find_missing(&[digest]).await.expect("find");
    assert_eq!(missing, [digest], "{digest} is held");
    assert!(
        matches!(
            storage.read_blob(&digest).await,
            Err(StorageError::NotFound(_))
        ),
        "{digest} reads"
    );
}

/// Catches a cache with no write tier offering no stream (a replica refusing the write,
/// a large blob gathered whole), and a stream to the store that loses, repeats or moves
/// a chunk: a 20 MiB blob, in 20 appends, is committed and reads back whole under its
/// digest, with nothing on any tier.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_tier_a_large_blob_streams_to_the_store() {
    let cache = cold();
    let storage = storage(&cache);
    assert!(cache.write_tier().is_none());
    let bytes = Arc::new(patterned(1, LARGE));
    let (digest, crc32c) = digest_and_crc(&bytes);
    stream_as(&storage, &bytes, digest, crc32c)
        .await
        .expect("finish")
        .expect("commit");
    assert!(
        storage
            .find_missing(&[digest])
            .await
            .expect("find")
            .is_empty()
    );
    let read = storage.read_blob(&digest).await.expect("read");
    assert_eq!(read, Bytes::from(bytes.to_vec()));
    assert_eq!(digest_and_crc(&read).0, digest);
}

/// Catches a stream that bypasses the write tier when it has room (the blob written
/// straight to the store), or loses bytes on it: the blob is committed onto the tier,
/// all of its bytes unflushed there, and reads back whole.
#[tokio::test(flavor = "multi_thread")]
async fn with_a_tier_a_large_blob_lands_on_the_tier() {
    let cache = tiered("lands").await;
    let storage = storage(&cache);
    let bytes = Arc::new(patterned(2, LARGE));
    let (digest, crc32c) = digest_and_crc(&bytes);
    assert_eq!(on_tier(&cache), 0);
    stream_as(&storage, &bytes, digest, crc32c)
        .await
        .expect("finish")
        .expect("commit");
    assert!(
        on_tier(&cache) >= LARGE as u64,
        "{} on the tier",
        on_tier(&cache)
    );
    let read = storage.read_blob(&digest).await.expect("read");
    assert_eq!(read, Bytes::from(bytes.to_vec()));
}

/// What a stream is ended as, wrongly, in [`a_wrong_finish_is_refused_and_stores_nothing`].
#[derive(Clone, Copy, Debug)]
enum Wrong {
    /// The bytes' size, another blob's hash.
    Hash,
    /// The bytes' hash, one byte more.
    Size,
    /// The bytes' digest, another CRC-32C.
    Crc,
}

/// Ends a stream of `bytes` as `wrong` says, on `storage` over `cache`, and asserts
/// `finish` refuses it and nothing is held: not the digest named, not the bytes' own,
/// and nothing on the tier.
async fn refused(cache: &Arc<MemoryCache>, wrong: Wrong) {
    let storage = storage(cache);
    let bytes = Arc::new(patterned(3, 3 * MIB + 17));
    let (actual, crc32c) = digest_and_crc(&bytes);
    let (digest, crc) = match wrong {
        Wrong::Hash => {
            let other = digest_and_crc(&patterned(4, bytes.len())).0;
            assert_eq!(other.size_bytes, actual.size_bytes);
            (other, crc32c)
        }
        Wrong::Size => (
            Digest {
                size_bytes: actual.size_bytes + 1,
                ..actual
            },
            crc32c,
        ),
        Wrong::Crc => (actual, crc32c ^ 1),
    };
    let answer = stream_as(&storage, &bytes, digest, crc).await;
    assert!(
        answer.is_err(),
        "{wrong:?}: finish took it (the commit answered {answer:?})"
    );
    not_held(&storage, digest).await;
    not_held(&storage, actual).await;
    if cache.write_tier().is_some() {
        assert_eq!(on_tier(cache), 0, "{wrong:?}: bytes committed to the tier");
    }
}

/// Catches a `finish` that takes a stream whose bytes are not the blob it ends as: a
/// hash that is not the bytes' (the digest the size check cannot see), a size that is
/// not theirs, a CRC-32C that is not theirs. Each is refused at `finish`, before any
/// commit, with a write tier and without one, and stores nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_finish_is_refused_and_stores_nothing() {
    for wrong in [Wrong::Hash, Wrong::Size, Wrong::Crc] {
        refused(&cold(), wrong).await;
        refused(&tiered(&format!("wrong-{wrong:?}")).await, wrong).await;
    }
}

/// Catches a mismatch answered as another error than `DigestMismatch` (a client told
/// to retry what can never pass), naming the wrong digests.
#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_hash_is_a_digest_mismatch() {
    let storage = storage(&cold());
    let bytes = Arc::new(patterned(5, 1000));
    let (actual, crc32c) = digest_and_crc(&bytes);
    let claimed = digest_and_crc(&patterned(6, 1000)).0;
    let answer = stream_as(&storage, &bytes, claimed, crc32c).await;
    assert!(
        matches!(answer, Err(StorageError::DigestMismatch { claimed: c, actual: a }) if c == claimed && a == actual),
        "{answer:?}"
    );
}

/// Catches a stream offered for an empty blob (the trait's caller puts it whole), and
/// one offered by a cache made outside a runtime, which cannot stream at all.
#[test]
fn no_stream_for_nothing_or_without_a_runtime() {
    let outside = Arc::new(Cache::memory());
    assert!(storage(&outside).stream(1).is_none());
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let inside = runtime.block_on(async { cold() });
    assert!(storage(&inside).stream(0).is_none());
    assert!(storage(&inside).stream(1).is_some());
}

/// Catches an append past the size the stream was made for taken (a blob longer than
/// its reservation written past it), with a write tier and without: refused as the cache
/// refuses it (`Invalid` to the store, the tier's out-of-range write on the tier), and
/// nothing is held.
#[tokio::test(flavor = "multi_thread")]
async fn an_append_past_the_size_is_refused() {
    for cache in [cold(), tiered("past").await] {
        let storage = storage(&cache);
        let bytes = patterned(7, 11);
        let stream = open(&storage, 10).await.expect("the cache can stream it");
        let appended = tokio::task::spawn_blocking(move || {
            let mut stream = stream;
            stream.append(&bytes)
        })
        .await
        .expect("append");
        assert!(appended.is_err(), "{appended:?}");
        not_held(&storage, digest_and_crc(&patterned(7, 10)).0).await;
    }
}
