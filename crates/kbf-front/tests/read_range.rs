//! A blob read in pieces (`Cache::read_range`, `CacheStorage::read_range`, ByteStream
//! `Read`): no process holds the whole blob, and a stream that ends cleanly carried only
//! verified bytes.
//!
//! Every test runs a cache over a [`FaultyStore`], which records the length of every
//! GET it answers: what the reader pulled from the store, and how much at a time. The
//! write-tier tests run the same reads over a staged blob. Each test's comment names the
//! defect it is there to catch.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::faulty::{FaultyStore, GetFault};
use futures::StreamExt;
use kbf_front::{
    BlobStream, ByteStreamService, Cache, CacheError, CacheStorage, READ_CHUNK_BYTES,
    SegmentsConfig,
};
use kbf_meta::{Applied, Command, Location, ObjectId, StoreId, UnreachableReason};
use kbf_objstore::{Capabilities, KeyPrefix, ObjectStore};
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_storage::{Storage, StorageError};
use kbf_store::Fs;
use kbf_tier::{block_name, seg_name};
use kbf_types::Digest;
use support::{Farm, Log, TestBlob, TierCache, later};
use tonic::{Code, Request, Status};

const C: usize = READ_CHUNK_BYTES;
const GIB: u64 = 1 << 30;

/// A cache with no tier over a log and a store the test can see into.
struct Lab {
    log: Log,
    store: FaultyStore,
    cache: Arc<TierCache>,
}

impl Lab {
    async fn new() -> Self {
        let (log, store) = (Log::new(), FaultyStore::new(Capabilities::default()));
        let cache = Cache::open(log.clone(), store.clone(), KeyPrefix::default())
            .await
            .expect("open");
        Self {
            log,
            store,
            cache: Arc::new(cache),
        }
    }

    /// `len` bytes stored as one blob.
    async fn blob(&self, seed: u64, len: usize) -> TestBlob {
        let blob = TestBlob::new(seed, len);
        self.cache
            .store_blobs(vec![blob.verified()])
            .await
            .expect("stored");
        self.store.forget_gets();
        blob
    }

    /// Flips one bit of the blob's own bytes, `at` bytes into it, in the store.
    async fn flip(&self, blob: &TestBlob, at: usize) {
        let (location, _) = self.log.located(&blob.digest);
        let key = self.cache.object_key(location.object).expect("key");
        let mut bytes = self.store.stored(&key).await.expect("stored").to_vec();
        bytes[usize::try_from(location.offset).expect("offset") + at] ^= 1;
        self.store.delete(&key).await.expect("delete");
        let put = self.store.put_new(&key, Bytes::from(bytes), None).await;
        put.expect("replaced");
    }

    /// Replaces the object holding `blob` with its first `keep` bytes.
    async fn truncate(&self, blob: &TestBlob, keep: usize) {
        let (location, _) = self.log.located(&blob.digest);
        let key = self.cache.object_key(location.object).expect("key");
        let bytes = self.store.stored(&key).await.expect("stored");
        self.store.delete(&key).await.expect("delete");
        let put = self.store.put_new(&key, bytes.slice(..keep), None).await;
        put.expect("replaced");
    }
}

async fn items(stream: BlobStream) -> Vec<Result<Bytes, CacheError>> {
    stream.collect().await
}

fn lens(items: &[Result<Bytes, CacheError>]) -> Vec<Option<usize>> {
    items
        .iter()
        .map(|i| i.as_ref().ok().map(Bytes::len))
        .collect()
}

fn unreachable(item: &Result<Bytes, CacheError>, digest: Digest) -> bool {
    matches!(item, Err(CacheError::Unreachable(d)) if *d == digest)
}

fn joined(items: &[Result<Bytes, CacheError>]) -> Vec<u8> {
    let bytes: Vec<Bytes> = items.iter().filter_map(|i| i.clone().ok()).collect();
    bytes.concat()
}

/// A range is the bytes it names, and costs the whole blob: the pieces before the
/// offset are read and hashed and not sent, and so are the pieces after the limit.
/// Catches a read that stops at the end of the range (the digest never compared, so
/// a damaged tail is served), one that skips the pieces before the offset (the hash
/// then covers no blob), a prefix sent, and a limit counted from the wrong place.
#[tokio::test]
async fn a_range_is_read_and_hashed_whole_and_only_its_bytes_are_sent() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 2 * C + C / 2).await;
    let size = blob.bytes.len();
    let cases = [
        (0, 0, 0, size),
        (5, 10, 5, 15),
        (C as u64 - 10, 20, C - 10, C + 10),
        (C as u64, 0, C, size),
        (2 * C as u64 + 5, 0, 2 * C + 5, size),
        (3, size as u64 * 2, 3, size),
        (size as u64 - 1, 1, size - 1, size),
        (1, u64::MAX, 1, size),
    ];
    for (offset, limit, start, end) in cases {
        let before = lab.store.get_lens().len();
        let stream = lab.cache.read_range(&blob.digest, offset, limit).await;
        let chunks = items(stream.expect("answer")).await;
        assert_eq!(joined(&chunks), blob.bytes[start..end], "{offset} {limit}");
        assert!(chunks.iter().all(Result::is_ok), "{offset} {limit}");
        let pulled = lab.store.get_lens()[before..].to_vec();
        assert_eq!(pulled, [C, C, C / 2], "the whole blob: {offset} {limit}");
    }
}

/// A chunk never spans two pieces, so no chunk is over [`READ_CHUNK_BYTES`] and none is
/// the join of two reads. Catches pieces joined into larger chunks, and a range cut
/// across a piece boundary sent as one.
#[tokio::test]
async fn chunks_are_cut_at_the_pieces() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 2 * C + C / 2).await;
    let whole = items(
        lab.cache
            .read_range(&blob.digest, 0, 0)
            .await
            .expect("read"),
    )
    .await;
    assert_eq!(lens(&whole), [Some(C), Some(C), Some(C / 2)]);
    let across = lab.cache.read_range(&blob.digest, C as u64 - 10, 20).await;
    assert_eq!(
        lens(&items(across.expect("read")).await),
        [Some(10), Some(10)]
    );
}

/// The empty blob is never stored and reads as no bytes; a held blob read from its end
/// reads nothing (and so checks nothing); an absent blob is NOT_FOUND even then.
/// Catches the empty blob asked of the index, a read from the end that reads the blob,
/// and an absent blob read from its end answered as an empty stream.
#[tokio::test]
async fn nothing_is_read_when_no_byte_is_asked_for() {
    let lab = Lab::new().await;
    let empty = kbf_segments::sha256(&[]);
    let none = lab.cache.read_range(&empty, 0, 0).await.expect("empty");
    assert_eq!(items(none).await.len(), 0);
    let blob = lab.blob(1, 4096).await;
    let end = lab.cache.read_range(&blob.digest, 4096, 0).await;
    assert_eq!(items(end.expect("at the end")).await.len(), 0);
    let past = lab.cache.read_range(&blob.digest, u64::MAX, 7).await;
    assert_eq!(items(past.expect("past the end")).await.len(), 0);
    assert_eq!(
        lab.store.get_lens(),
        Vec::<usize>::new(),
        "nothing was pulled"
    );
    let absent = TestBlob::new(9, 10).digest;
    let missing = lab.cache.read_range(&absent, 10, 0).await;
    assert!(matches!(missing, Err(CacheError::NotFound(d)) if d == absent));
}

/// A blob that does not hash to its digest ends the stream in an error item, and the
/// error comes before the last piece is handed out: the stream is the pieces that
/// precede the last, then the error, never the last piece and then the error. The
/// object is marked corrupt (the blob is then reported missing until a write heals it)
/// and the error is UNAVAILABLE, as `read_blob` answers a damaged blob. Catches a
/// mismatch ignored (a clean end), the check made after the last piece is sent, no
/// mark, and another code.
#[tokio::test]
async fn a_damaged_blob_ends_the_stream_in_an_error_item() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 2 * C + C / 2).await;
    lab.flip(&blob, C + 5).await;
    let object = lab.log.located(&blob.digest).0.object;
    let stream = lab.cache.read_range(&blob.digest, 0, 0).await;
    let chunks = items(stream.expect("the first chunk is handed out")).await;
    assert_eq!(lens(&chunks), [Some(C), Some(C), None]);
    assert!(unreachable(&chunks[2], blob.digest), "{:?}", chunks[2]);
    let status = Status::from(chunks[2].clone().expect_err("the error item"));
    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(
        lab.log.unreachable(object),
        Some(UnreachableReason::Corrupt)
    );
    let missing = lab.cache.find_missing(&[blob.digest]).await;
    assert_eq!(missing.expect("find"), [blob.digest]);
}

/// The scheme's price, pinned: a range that does not touch the damaged byte is not
/// served clean either. Reading the undamaged first bytes, the caller is handed them,
/// and the stream then ends in the error, because the rest of the blob is read and
/// hashed before it ends. Catches a check of the range's own bytes only (a clean end
/// for the undamaged range, and a damaged blob served in pieces).
#[tokio::test]
async fn a_range_clear_of_the_damage_still_ends_in_the_error() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 2 * C + C / 2).await;
    lab.flip(&blob, 2 * C + 100).await;
    let stream = lab.cache.read_range(&blob.digest, 7, 100).await;
    let chunks = items(stream.expect("answer")).await;
    assert_eq!(lens(&chunks), [Some(100), None]);
    assert_eq!(chunks[0].as_ref().expect("chunk"), &blob.bytes[7..107]);
    assert!(unreachable(&chunks[1], blob.digest), "{:?}", chunks[1]);
}

/// A blob of one piece is checked before its only chunk is handed out, so the error is
/// the answer's (where a caller still chooses the code), not an item. Catches a damaged
/// small blob handed out first.
#[tokio::test]
async fn a_damaged_blob_of_one_piece_is_the_answers_error() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 4096).await;
    lab.flip(&blob, 4095).await;
    let object = lab.log.located(&blob.digest).0.object;
    let read = lab.cache.read_range(&blob.digest, 100, 10).await;
    assert!(matches!(read, Err(CacheError::Unreachable(d)) if d == blob.digest));
    assert_eq!(
        lab.log.unreachable(object),
        Some(UnreachableReason::Corrupt)
    );
}

/// An object the store lost, found on the first GET or after some chunks went out, is
/// marked missing and answers UNAVAILABLE, and a store that fails is the store's error
/// and marks nothing. Catches a lost object served short, marked corrupt, or left
/// unmarked, and a transport failure marking an object that may be fine.
#[tokio::test]
async fn an_object_lost_or_failing_mid_stream_ends_the_stream_in_an_error() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 2 * C + C / 2).await;
    let object = lab.log.located(&blob.digest).0.object;

    let stream = lab.cache.read_range(&blob.digest, 0, 0).await;
    let mut stream = stream.expect("answer");
    assert!(stream.next().await.expect("first").is_ok());
    lab.store.on_get(GetFault::Transport);
    let failed = stream.next().await.expect("second");
    assert!(matches!(failed, Err(CacheError::Store(_))), "{failed:?}");
    assert!(stream.next().await.is_none(), "the stream ends after it");
    assert_eq!(lab.log.unreachable(object), None);

    let stream = lab.cache.read_range(&blob.digest, 0, 0).await;
    let mut stream = stream.expect("answer");
    lab.store.on_get(GetFault::NotFound);
    assert!(stream.next().await.expect("first").is_ok());
    let gone = stream.next().await.expect("second");
    assert!(unreachable(&gone, blob.digest), "{gone:?}");
    assert_eq!(
        lab.log.unreachable(object),
        Some(UnreachableReason::Missing)
    );
}

/// An object shorter than the blob it is indexed for is damaged, whether the store
/// answers a short range or refuses a range past its end. Catches a short read taken
/// for a whole piece (the position moves on by the wrong amount and the read never
/// ends or hashes the wrong bytes), and the store's InvalidRange taken for an error to
/// retry.
#[tokio::test]
async fn an_object_shorter_than_its_blob_is_damaged() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 2 * C + C / 2).await;
    let object = lab.log.located(&blob.digest).0.object;
    lab.truncate(&blob, C + 10).await;
    let stream = lab.cache.read_range(&blob.digest, 0, 0).await;
    let chunks = items(stream.expect("answer")).await;
    assert_eq!(lens(&chunks), [Some(C), None]);
    assert!(unreachable(&chunks[1], blob.digest), "{:?}", chunks[1]);
    assert_eq!(
        lab.log.unreachable(object),
        Some(UnreachableReason::Corrupt)
    );

    let other = lab.blob(3, 2 * C).await;
    let object = lab.log.located(&other.digest).0.object;
    lab.truncate(&other, C).await;
    let stream = lab.cache.read_range(&other.digest, 0, 0).await;
    let chunks = items(stream.expect("answer")).await;
    assert_eq!(lens(&chunks), [Some(C), None]);
    assert_eq!(
        lab.log.unreachable(object),
        Some(UnreachableReason::Missing)
    );
}

/// An index entry that names no readable range (an offset so large the range overflows)
/// is the cache's own inconsistency, INTERNAL, whatever the object holds. Catches a
/// panic or a wrapped offset read from the wrong place.
#[tokio::test]
async fn an_unreadable_range_is_an_internal_error() {
    let lab = Lab::new().await;
    let Applied::Epoch(epoch) = lab.log.commit(Command::AllocEpoch) else {
        panic!("an epoch");
    };
    let blob = TestBlob::new(1, 4096);
    let location = Location {
        store: StoreId::CONFIGURED,
        object: ObjectId::new(epoch, 1),
        offset: u64::MAX - 10,
    };
    let put = lab.log.commit(Command::PutBlob {
        digest: blob.digest,
        location,
    });
    assert!(matches!(put, Applied::Blob(Ok(_))), "{put:?}");
    let read = lab.cache.read_range(&blob.digest, 0, 0).await;
    assert!(
        matches!(read, Err(CacheError::Internal(_))),
        "{:?}",
        read.err()
    );
}

/// Blobs packed into one segment are read at their own offsets and checked on their own
/// bytes. Catches a read from the segment's start and a hash over the neighbour.
#[tokio::test]
async fn blobs_in_one_segment_are_read_at_their_own_offsets() {
    let lab = Lab::new().await;
    let blobs = support::blobs(1, 3, 40_000);
    let stored = lab.cache.store_blobs(support::verified(&blobs)).await;
    stored.expect("stored");
    let objects: Vec<_> = blobs.iter().map(|b| lab.log.located(&b.digest).0).collect();
    assert_eq!(objects[0].object, objects[2].object, "one segment");
    assert_ne!(objects[2].offset, 0);
    for b in &blobs {
        let stream = lab.cache.read_range(&b.digest, 10, 0).await;
        assert_eq!(joined(&items(stream.expect("read")).await), b.bytes[10..]);
    }
    lab.flip(&blobs[1], 3).await;
    let first = lab.cache.read_range(&blobs[0].digest, 0, 0).await;
    assert_eq!(joined(&items(first.expect("sound")).await), blobs[0].bytes);
    let damaged = lab.cache.read_range(&blobs[1].digest, 0, 0).await;
    assert!(damaged.is_err(), "the damaged neighbour is not served");
}

/// Pulls only as many pieces as the reader polls. A reader that takes chunks slowly
/// sees the store asked for one piece per chunk, never ahead; the answer holds the
/// first piece only. Catches a read that collects the pieces before it answers or
/// reads ahead of the consumer.
#[tokio::test]
async fn the_store_is_pulled_only_as_fast_as_the_stream_is_polled() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 2 * C + C / 2).await;
    let storage = CacheStorage::new(Arc::clone(&lab.cache), SegmentsConfig::DEFAULT);
    let mut stream = storage.read_range(&blob.digest, 0, 0).await.expect("read");
    let mut pulled = Vec::new();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        pulled.push(lab.store.get_lens().len());
        assert!(stream.next().await.expect("a chunk").is_ok());
    }
    assert_eq!(
        pulled,
        [1, 1, 2],
        "one piece for the answer, then one per poll"
    );
    assert_eq!(lab.store.get_lens().len(), 3);
    assert!(stream.next().await.is_none());
}

/// The memory bound: a 32 MiB blob read by a slow consumer holds a piece or two, not
/// the blob. No GET is over [`READ_CHUNK_BYTES`], and the bytes pulled from the store
/// and not yet taken by the consumer never exceed one piece. Catches the blob read in
/// one GET, the pieces collected before the first is sent, and a read-ahead.
#[tokio::test]
async fn a_large_blob_is_never_held_whole() {
    let lab = Lab::new().await;
    let size = 32 << 20;
    let blob = lab.blob(1, size).await;
    let storage = CacheStorage::new(Arc::clone(&lab.cache), SegmentsConfig::DEFAULT);
    let mut stream = storage.read_range(&blob.digest, 0, 0).await.expect("read");
    let (mut taken, mut chunks, mut worst) = (0usize, 0usize, 0usize);
    loop {
        tokio::time::sleep(Duration::from_millis(1)).await;
        let pulled: usize = lab.store.get_lens().iter().sum();
        worst = worst.max(pulled - taken);
        let Some(chunk) = stream.next().await else {
            break;
        };
        taken += chunk.expect("chunk").len();
        chunks += 1;
    }
    assert_eq!((taken, chunks), (size, size / C));
    assert_eq!(lab.store.get_lens().iter().max().copied(), Some(C));
    assert!(worst <= C, "{worst} bytes pulled and not yet taken");
}

/// ByteStream `Read` over the same read: the messages come as the stream is polled and
/// no GET is larger than a piece, so the blob is not buffered to answer a call. Catches
/// `Read` reading the whole blob (`read_blob`) and slicing it, and a service that
/// collects its messages before it answers.
#[tokio::test]
async fn bytestream_read_does_not_buffer_the_blob() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 8 * C).await;
    let service = ByteStreamService::new(Arc::clone(&lab.cache));
    let request = ReadRequest {
        resource_name: format!(
            "blobs/{}/{}",
            blob.digest.hash_hex(),
            blob.digest.size_bytes
        ),
        read_offset: 0,
        read_limit: 0,
    };
    let response = service.read(Request::new(request)).await.expect("read");
    let mut stream = response.into_inner();
    let mut received = 0;
    while let Some(message) = stream.next().await {
        let data = message.expect("message").data;
        assert!(data.len() <= C, "a message of {} bytes", data.len());
        assert_eq!(data, blob.bytes[received..received + data.len()]);
        received += data.len();
        let pulled: usize = lab.store.get_lens().iter().sum();
        assert_eq!(pulled, received, "bytes pulled from the store, bytes sent");
    }
    assert_eq!(received, 8 * C);
    assert_eq!(lab.store.get_lens(), [C; 8]);
}

/// A damaged blob reaches the ByteStream client as an error status after its messages:
/// UNAVAILABLE, the code a damaged blob has everywhere. Catches the service turning the
/// error item into a clean end of the stream.
#[tokio::test]
async fn bytestream_read_of_a_damaged_blob_ends_in_unavailable() {
    let lab = Lab::new().await;
    let blob = lab.blob(1, 3 * C).await;
    lab.flip(&blob, 0).await;
    let service = ByteStreamService::new(Arc::clone(&lab.cache));
    let request = ReadRequest {
        resource_name: format!(
            "blobs/{}/{}",
            blob.digest.hash_hex(),
            blob.digest.size_bytes
        ),
        read_offset: 0,
        read_limit: 0,
    };
    let response = service.read(Request::new(request)).await.expect("read");
    let messages: Vec<_> = response.into_inner().collect().await;
    let codes: Vec<_> = messages
        .iter()
        .map(|m| m.as_ref().map(|r| r.data.len()).map_err(Status::code))
        .collect();
    assert_eq!(codes, [Ok(C), Ok(C), Err(Code::Unavailable)]);
}

/// The storage boundary reports the damaged blob as it reports one found by
/// `read_blob`: [`StorageError::Unreachable`], in the answer for a blob of one piece
/// and as the last item otherwise. Catches the cache's error converted to another
/// variant, and a stream error dropped at the boundary.
#[tokio::test]
async fn the_storage_boundary_reports_a_damaged_blob_as_unreachable() {
    let lab = Lab::new().await;
    let big = lab.blob(1, 2 * C).await;
    lab.flip(&big, 5).await;
    let storage = CacheStorage::new(Arc::clone(&lab.cache), SegmentsConfig::DEFAULT);
    let stream = storage.read_range(&big.digest, 0, 0).await.expect("read");
    let chunks: Vec<_> = stream.collect().await;
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].is_ok());
    assert!(matches!(&chunks[1], Err(StorageError::Unreachable(d)) if *d == big.digest));
    let small = lab.blob(3, 100).await;
    lab.flip(&small, 5).await;
    let read = storage.read_range(&small.digest, 0, 0).await;
    assert!(matches!(read, Err(StorageError::Unreachable(d)) if d == small.digest));
}

fn staged_farm() -> Farm {
    Farm::new(Capabilities::default())
}

async fn staged(farm: &Farm, seed: u64, len: usize) -> (Arc<TierCache>, TestBlob) {
    let cache = Arc::new(farm.start(GIB).await);
    let blob = TestBlob::new(seed, len);
    cache
        .store_blobs(vec![blob.verified()])
        .await
        .expect("stored");
    assert!(farm.log.located(&blob.digest).1.is_some(), "staged");
    farm.store.forget_gets();
    (cache, blob)
}

fn local_reads(cache: &TierCache) -> String {
    let metrics = cache.render_metrics();
    let line = metrics
        .lines()
        .find(|l| l.starts_with("kbf_write_tier_local_reads_total"));
    line.expect("the counter").to_owned()
}

/// A blob staged on the tier, the only copy, is streamed from its block: no GET of the
/// store (it does not hold the blob yet), one local read counted per stream, and the
/// object never marked. Catches a staged blob streamed from the store (marked missing,
/// or not found), and a count per piece.
#[tokio::test]
async fn a_staged_blob_is_streamed_from_its_block() {
    let farm = staged_farm();
    let (cache, blob) = staged(&farm, 1, 2 * C + C / 2).await;
    let stream = cache.read_range(&blob.digest, 3, 0).await;
    let chunks = items(stream.expect("read")).await;
    assert_eq!(joined(&chunks), blob.bytes[3..]);
    assert_eq!(farm.store.get_lens(), Vec::<usize>::new());
    assert_eq!(local_reads(&cache), "kbf_write_tier_local_reads_total 1");
    let object = farm.log.object_of(&blob.digest);
    assert_eq!(farm.log.unreachable(object), None);
}

/// A staged record damaged on the local disk (the only copy) ends the stream in the
/// error item and is marked corrupt, as `read_blob` marks it: its blobs are reported
/// missing until a re-upload, and the marked block is gone from the disk. Catches a
/// damaged staged record served, a stream that ends cleanly, and a block left on the
/// disk counting against the budget.
#[tokio::test]
async fn a_staged_blob_damaged_on_the_disk_is_marked_and_ends_in_the_error() {
    let farm = staged_farm();
    let (cache, blob) = staged(&farm, 1, 2 * C + C / 2).await;
    let (at, _) = farm.log.located(&blob.digest);
    let offset = usize::try_from(at.offset).expect("offset") + C + 5;
    farm.fs.flip(&block_name(at.object), offset);
    let stream = cache.read_range(&blob.digest, 0, 0).await;
    let chunks = items(stream.expect("answer")).await;
    assert_eq!(lens(&chunks), [Some(C), Some(C), None]);
    assert!(unreachable(&chunks[2], blob.digest), "{:?}", chunks[2]);
    assert_eq!(
        farm.log.unreachable(at.object),
        Some(UnreachableReason::Corrupt)
    );
    assert_eq!(farm.fs.contents(&block_name(at.object)), None);
    let missing = cache.find_missing(&[blob.digest]).await;
    assert_eq!(missing.expect("find"), [blob.digest]);
    assert_eq!(local_reads(&cache), "kbf_write_tier_local_reads_total 0");
}

/// A staged block the tier cannot read is not the store's to answer for: the store does
/// not hold the blob yet, so nothing is marked and the answer is UNAVAILABLE. Catches
/// the stream marking a staged object missing because the store lacks it.
#[tokio::test]
async fn a_staged_block_the_tier_lost_marks_nothing() {
    let farm = staged_farm();
    let (cache, blob) = staged(&farm, 1, 2 * C).await;
    let object = farm.log.object_of(&blob.digest);
    farm.fs.remove(&block_name(object)).expect("remove");
    let read = cache.read_range(&blob.digest, 0, 0).await;
    assert!(matches!(read, Err(CacheError::Unreachable(d)) if d == blob.digest));
    assert_eq!(farm.log.unreachable(object), None);
    assert_eq!(farm.log.applied("object_unreachable"), 0);
}

/// A stream that began on the tier and lost its copy goes on from the store, which the
/// flush filled, and the digest still covers every byte, whichever copy it came from.
/// Catches a switch of source that restarts, repeats or skips a piece (the digest then
/// fails), and a mixed stream counted as local.
#[tokio::test]
async fn a_stream_switches_to_the_store_when_the_tier_loses_its_copy() {
    let farm = staged_farm();
    let (cache, blob) = staged(&farm, 1, 2 * C + C / 2).await;
    let object = farm.log.object_of(&blob.digest);
    let stream = cache.read_range(&blob.digest, 0, 0).await;
    let mut stream = stream.expect("answer");
    let first = stream.next().await.expect("first").expect("chunk");
    let round = cache.flush_tier(later()).await.expect("flush");
    assert_eq!(round.flushed, [object]);
    farm.store.forget_gets();
    farm.fs.remove(&seg_name(object)).expect("remove the copy");
    let mut got = first.to_vec();
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.expect("chunk"));
    }
    assert_eq!(got, blob.bytes);
    assert_eq!(farm.store.get_lens(), [C, C / 2]);
    assert_eq!(local_reads(&cache), "kbf_write_tier_local_reads_total 0");
}

/// A flushed copy on the tier is not streamed: the store is read, and a damaged copy
/// cannot fail a stream. Catches a stream served from a flushed copy it cannot drop
/// when it is bad.
#[tokio::test]
async fn a_flushed_blob_is_streamed_from_the_store() {
    let farm = staged_farm();
    let (cache, blob) = staged(&farm, 1, 2 * C + C / 2).await;
    let object = farm.log.object_of(&blob.digest);
    let round = cache.flush_tier(later()).await.expect("flush");
    assert_eq!(round.flushed, [object]);
    farm.store.forget_gets();
    farm.fs.flip(&seg_name(object), 0);
    let stream = cache.read_range(&blob.digest, 0, 0).await;
    assert_eq!(joined(&items(stream.expect("read")).await), blob.bytes);
    assert_eq!(farm.store.get_lens(), [C, C, C / 2]);
    assert_eq!(farm.log.unreachable(object), None);
    assert_eq!(local_reads(&cache), "kbf_write_tier_local_reads_total 0");
}
