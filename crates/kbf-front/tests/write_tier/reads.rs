//! Reads in windows over a write tier and its store, and the flusher's failures: what a
//! read does when a window cannot be had or the bytes fail their digest, and where a
//! corrupt object's sound records go. Each test's comment names the defect it is there
//! to catch.

use kbf_front::{CacheError, MAX_DECODED_BLOB_BYTES, READ_WINDOW_BYTES};
use kbf_meta::{Applied, Command, UnreachableReason};
use kbf_objstore::Capabilities;
use kbf_store::Fs as _;
use kbf_tier::{block_name, seg_name};

use super::GIB;
use super::common::faulty::GetFault;
use super::support::{Farm, TestBlob, blobs, later, verified};

/// Catches a windowed read that sends the last window of a flushed copy whose bytes
/// are damaged past its first window, or marks the object for it (the store's copy is
/// sound): the first window comes, then the read fails UNAVAILABLE, and the object is
/// not marked.
#[tokio::test]
async fn a_flushed_copy_damaged_past_its_first_window_fails_the_read_unmarked() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(41, 12 << 20);
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    let (at, _) = farm.log.located(&b.digest);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [at.object], "{round:?}");
    let damaged = usize::try_from(at.offset).expect("offset") + (10 << 20);
    farm.fs.flip(&seg_name(at.object), damaged);
    let mut read = cache
        .read_windows(&b.digest, 0, b.digest.size_bytes)
        .await
        .expect("open");
    let first = read.next().await.expect("a window").expect("the first");
    assert_eq!(first.len() as u64, READ_WINDOW_BYTES);
    let last = read.next().await.expect("an answer");
    assert!(matches!(last, Err(CacheError::Unreachable(_))), "{last:?}");
    assert!(read.next().await.is_none(), "the read ended");
    assert_eq!(farm.log.unreachable(at.object), None);
}

/// Catches a damaged flushed copy that fails a read the store can answer, or marks the
/// object: a blob of one window whose local copy is damaged is read again from the
/// store, whole and right, and nothing is marked.
#[tokio::test]
async fn a_damaged_flushed_copy_of_one_window_is_read_from_the_store() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(42, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    let (at, _) = farm.log.located(&b.digest);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [at.object], "{round:?}");
    farm.fs.flip(
        &seg_name(at.object),
        usize::try_from(at.offset).expect("offset"),
    );
    assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    assert_eq!(farm.log.unreachable(at.object), None);
}

/// Catches a read whose store cannot be reached answered as a missing or corrupt
/// object (a mark that would make the blob be uploaded again): with no local copy and
/// the store's GET failing, the read fails UNAVAILABLE with the store's error and the
/// object is not marked.
#[tokio::test]
async fn a_store_that_cannot_be_reached_fails_the_read_unmarked() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(43, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    let (at, _) = farm.log.located(&b.digest);
    cache.flush_tier(later()).await.expect("round");
    farm.fs.remove(&seg_name(at.object)).expect("remove");
    farm.store.on_get(GetFault::Transport);
    let read = cache.read_blob(&b.digest).await;
    assert!(matches!(read, Err(CacheError::Store(_))), "{read:?}");
    assert_eq!(farm.log.unreachable(at.object), None);
    assert_eq!(
        cache.read_blob(&b.digest).await.expect("then read"),
        b.bytes
    );
}

/// Catches a read of a blob the index has marked unreachable, a range outside a blob,
/// or a whole read of a blob too large to decode, answered with bytes: each is refused
/// before a window is read.
#[tokio::test]
async fn reads_the_index_or_the_range_rule_out_are_refused() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(44, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    let size = b.digest.size_bytes;
    for (from, to) in [(5, 3), (0, size + 1)] {
        let read = cache.read_windows(&b.digest, from, to).await;
        assert!(matches!(read, Err(CacheError::Invalid(_))), "{from}..{to}");
    }
    let object = farm.log.object_of(&b.digest);
    let reason = UnreachableReason::Missing;
    let marked = farm
        .log
        .commit(Command::ObjectUnreachable { object, reason });
    assert!(matches!(marked, Applied::Marked(Ok(()))), "{marked:?}");
    let read = cache.read_windows(&b.digest, 0, size).await;
    assert!(matches!(read, Err(CacheError::Unreachable(_))));

    let large = TestBlob::new(
        45,
        usize::try_from(MAX_DECODED_BLOB_BYTES).expect("size") + 1,
    );
    cache
        .store_blobs(vec![large.verified()])
        .await
        .expect("stored");
    let whole = cache.read_blob(&large.digest).await;
    assert!(matches!(whole, Err(CacheError::Invalid(_))), "{whole:?}");
}

/// Catches the sound records of a corrupt object lost when the tier has no room for a
/// block of their own: they are streamed to the store as an object of their own
/// instead, indexed, and read back; the bad record stays unavailable.
#[tokio::test]
async fn sound_records_go_to_the_store_when_the_tier_has_no_room() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(10_000).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let (at, _) = farm.log.located(&blobs[1].digest);
    let offset = usize::try_from(at.offset).expect("offset") + 7;
    farm.fs.flip(&block_name(at.object), offset);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.corrupt, [at.object], "{round:?}");
    assert_eq!(farm.store.put_keys().len(), 1, "the sound record's object");
    let (sound, staged) = farm.log.located(&blobs[0].digest);
    assert_eq!(staged, None, "in the store, not staged");
    assert_ne!(sound.object, at.object);
    let read = cache.read_blob(&blobs[0].digest).await.expect("read");
    assert_eq!(read, blobs[0].bytes);
    let bad = cache.read_blob(&blobs[1].digest).await;
    assert!(matches!(bad, Err(CacheError::Unreachable(_))), "{bad:?}");
}

/// Catches a flush whose block cannot be read taken for a flush, or ending the round:
/// the flush is retried after the backoff, and nothing is stored.
#[tokio::test]
async fn a_block_that_cannot_be_read_is_flushed_later() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(46, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    let object = farm.log.object_of(&b.digest);
    farm.fs.remove(&block_name(object)).expect("remove");
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.retrying.len(), 1, "{round:?}");
    assert_eq!(round.retrying[0].0, object);
    assert!(farm.store.put_keys().is_empty());
}
