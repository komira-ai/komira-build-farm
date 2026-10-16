//! The local write tier through the cache (`docs/design/write-path.md`, section 6 and
//! the tier rows of section 9): writes staged and acknowledged on the local disk, read
//! back from it, flushed to the store under their own keys, and recovered at a restart
//! against the committed state.
//!
//! Every test runs a [`Farm`]: one metadata log that outlives restarts, a tier directory
//! on a `FaultFs` (crashed or restarted as a test needs) and a faulty store. Each test's
//! comment names the defect it is there to catch.

mod common;
#[path = "write_tier/reads.rs"]
mod reads;
#[path = "write_tier/stream.rs"]
mod stream;
#[path = "write_tier/support.rs"]
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::faulty::{GetFault, PutFault};
use kbf_front::{CacheError, FLUSH_BACKOFF_FIRST, FLUSH_BACKOFF_MAX, TierStartError};
use kbf_meta::{
    Applied, BlobAnswer, Command, ObjectId, ServerId, StagedPut, StagedState, TierId,
    UnreachableReason,
};
use kbf_objstore::{Capabilities, ObjectStore};
use kbf_segments::SegmentReader;
use kbf_store::Fs;
use kbf_tier::{TIER, TierError, block_name, seg_name};
use support::{CONDITIONAL, Farm, SEGMENT, TestBlob, blobs, key, later, start, verified};
use tokio::sync::oneshot;

const GIB: u64 = 1 << 30;

/// Catches: an acknowledgement before the block's bytes and name are synced (a power
/// cut then loses an acknowledged blob), and a restart that does not find staged
/// blocks again. Nothing reaches the store: the blobs come back from the disk.
#[tokio::test]
async fn acknowledged_blobs_read_back_after_a_power_cut() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 3, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    assert!(farm.store.put_keys().is_empty(), "staged, not PUT");
    for b in &blobs {
        assert_eq!(farm.log.located(&b.digest).1, cache.tier_id());
    }
    drop(cache);

    let cut = farm.fs.crash(0);
    let (cache, startup) = start(&cut, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert!(startup.recovery.lost.is_empty(), "{startup:?}");
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    assert!(farm.store.put_keys().is_empty());
}

/// Catches: a staged blob read from the store (which does not hold it yet) and its
/// object marked unreachable by the reader, through `read_blob` and through the batch
/// path; and a staged blob reported missing.
#[tokio::test]
async fn staged_blobs_are_read_from_their_block_and_never_marked() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 4, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    let digests: Vec<_> = blobs.iter().map(|b| b.digest).collect();
    let read = cache.read_blobs(&digests).await.expect("batch");
    for b in &blobs {
        assert_eq!(read[&b.digest].as_ref().expect("batch read"), &b.bytes);
    }
    assert_eq!(cache.find_missing(&digests).await.expect("find"), []);
    assert_eq!(farm.log.unreachable(object), None);
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Open));
    let metrics = cache.render_metrics();
    assert!(
        metrics.contains("\nkbf_write_tier_local_reads_total 8\n"),
        "{metrics}"
    );
}

/// Catches: a reader that marks a staged object missing because neither its block nor
/// the store has it. The object was staged when the read's query ran; the store not
/// holding it says nothing, so the answer is UNAVAILABLE and nothing is committed.
#[tokio::test]
async fn a_staged_block_the_tier_cannot_read_answers_unavailable_and_marks_nothing() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(1, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    let object = farm.log.object_of(&b.digest);
    farm.fs.remove(&block_name(object)).expect("remove");
    let read = cache.read_blob(&b.digest).await;
    assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    let batch = cache.read_blobs(&[b.digest]).await.expect("batch");
    assert!(matches!(batch[&b.digest], Err(CacheError::Unreachable(_))));
    assert_eq!(farm.log.unreachable(object), None);
    assert_eq!(farm.log.applied("object_unreachable"), 0);
}

/// Catches: `ObjectFlushed` committed before the PUT and its footer read-back finish
/// (a crash then leaves an index naming an object the store lacks), reads of a blob
/// whose flush is in flight failing, and a stored object that is not a valid segment
/// holding every record.
#[tokio::test]
async fn a_flush_commits_only_after_its_put_and_reads_stay_local_meanwhile() {
    let farm = Farm::new(Capabilities::default());
    let cache = Arc::new(farm.start(GIB).await);
    let blobs = blobs(1, 3, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    farm.store.on_put(PutFault::Hold {
        entered: entered_tx,
        release: release_rx,
    });
    let flushing = Arc::clone(&cache);
    let flush = tokio::spawn(async move { flushing.flush_tier(later()).await });
    let entered = tokio::time::timeout(Duration::from_secs(10), entered).await;
    entered
        .expect("the flush PUT within 10 s")
        .expect("PUT reached the store");
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    assert_eq!(farm.log.applied("object_flushed"), 0);
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    release.send(()).expect("release");
    let round = flush.await.expect("join").expect("round");
    assert_eq!(round.sealed, [object]);
    assert_eq!(round.flushed, [object]);
    assert_eq!(farm.log.staged_state(object), None);
    assert_eq!(farm.log.located(&blobs[0].digest).1, None);
    assert!(farm.has_copy(object) && !farm.has_block(object));
    let stored = farm
        .store
        .stored(&key(&cache, object))
        .await
        .expect("stored");
    let reader = SegmentReader::open(&stored).expect("a segment");
    for b in &blobs {
        assert_eq!(reader.get(&b.digest).expect("record"), Some(&b.bytes[..]));
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
}

/// Catches: a flush that trusts its PUT without reading the footer back (the store
/// kept a cut object, and `ObjectFlushed` would index it), and a retry that does not
/// PUT the same bytes under the same key.
#[tokio::test]
async fn a_put_that_stored_other_bytes_is_not_flushed_and_is_put_again() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    farm.store.on_put(PutFault::Truncate(1));
    let t0 = later();
    let round = cache.flush_tier(t0).await.expect("round");
    assert_eq!(round.flushed, []);
    assert_eq!(round.retrying, [(object, FLUSH_BACKOFF_FIRST)]);
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    let round = cache
        .flush_tier(t0 + FLUSH_BACKOFF_FIRST)
        .await
        .expect("round");
    assert_eq!(round.flushed, [object]);
    let key = key(&cache, object);
    assert_eq!(farm.store.put_keys(), [key.clone(), key.clone()]);
    let stored = farm.store.stored(&key).await.expect("stored");
    SegmentReader::open(&stored).expect("a whole segment");
}

/// Catches: a flush retried without a wait (a store outage met by a PUT every round),
/// a wait that does not grow, a footer read-back that failed taken as a flush, and a
/// retry under another key (a double flush that moves the object).
#[tokio::test]
async fn a_failing_store_is_retried_with_backoff_under_one_key() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(1, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    let object = farm.log.object_of(&b.digest);
    farm.store.on_put(PutFault::Fail);
    farm.store.on_put(PutFault::Fail);
    let t0 = later();
    let first = FLUSH_BACKOFF_FIRST;
    let round = cache.flush_tier(t0).await.expect("round");
    assert_eq!(round.retrying, [(object, first)]);
    let round = cache.flush_tier(t0 + first / 2).await.expect("round");
    assert_eq!((round.retrying, round.flushed), (vec![], vec![]));
    assert_eq!(farm.store.put_keys().len(), 1, "no attempt within the wait");
    let round = cache.flush_tier(t0 + first).await.expect("round");
    assert_eq!(round.retrying, [(object, first * 2)]);
    let round = cache
        .flush_tier(t0 + first * 3 - Duration::from_millis(1))
        .await;
    assert_eq!(round.expect("round").flushed, []);
    farm.store.on_get(GetFault::Transport);
    let round = cache.flush_tier(t0 + first * 3).await.expect("round");
    assert_eq!(
        round.retrying,
        [(object, first * 4)],
        "a failed read-back retries"
    );
    let round = cache.flush_tier(t0 + first * 7).await.expect("round");
    assert_eq!(round.flushed, [object]);
    let key = key(&cache, object);
    assert_eq!(farm.store.put_keys(), vec![key; 4]);
}

/// Catches: a block renamed to a flushed copy (and so evictable) on a flusher-local
/// flag before `ObjectFlushed` applied; and, after a restart, a reflush that fails on
/// the PUT it already made, PUTs another key, or calls a conflict where the bytes are
/// equal.
#[tokio::test]
async fn a_lost_object_flushed_commit_is_flushed_again_under_the_same_key_after_a_restart() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 3, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    let key = key(&cache, object);
    farm.log.refuse("object_flushed", 1);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, []);
    assert_eq!(round.retrying, [(object, FLUSH_BACKOFF_FIRST)]);
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    assert!(farm.has_block(object) && !farm.has_copy(object));
    assert_eq!(farm.store.put_keys(), std::slice::from_ref(&key));
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    drop(cache);

    let fs = farm.fs.restart();
    let (cache, startup) = start(&fs, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert_eq!(startup.recovery.sealed, [object]);
    let round = cache.flush_tier(Instant::now()).await.expect("round");
    assert_eq!((round.flushed, round.conflicts), (vec![object], vec![]));
    assert_eq!(farm.store.put_keys(), [key.clone(), key]);
    assert_eq!(farm.log.staged_state(object), None);
    assert_eq!(farm.log.applied("flush_conflict"), 0);
    assert!(fs.contents(&seg_name(object)).is_some());
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
}

/// Catches: a block of its own (a blob over the segment limit, sealed by its commit)
/// whose second flush, after the store kept the first PUT but lost its answer, meets
/// `AlreadyExists` and calls a conflict (a footer built two ways), and a stored record
/// whose CRC the footer gets wrong.
#[tokio::test]
async fn an_own_block_flushed_again_after_a_lost_answer_is_not_a_conflict() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let big = TestBlob::new(1, usize::try_from(SEGMENT).expect("size") + 1);
    cache
        .store_blobs(vec![big.verified()])
        .await
        .expect("stored");
    let object = farm.log.object_of(&big.digest);
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    farm.store.on_put(PutFault::LoseAnswer);
    let t0 = Instant::now();
    let round = cache.flush_tier(t0).await.expect("round");
    assert_eq!(round.retrying, [(object, FLUSH_BACKOFF_FIRST)]);
    let round = cache
        .flush_tier(t0 + FLUSH_BACKOFF_FIRST)
        .await
        .expect("round");
    assert_eq!((round.flushed, round.conflicts), (vec![object], vec![]));
    let stored = farm
        .store
        .stored(&key(&cache, object))
        .await
        .expect("stored");
    let reader = SegmentReader::open(&stored).expect("a segment");
    assert_eq!(
        reader.get(&big.digest).expect("record"),
        Some(&big.bytes[..])
    );
}

/// Catches: a key holding another object answered by marking the object corrupt
/// (reads of good local bytes would stop), by flushing anyway, or by retrying it
/// forever; and its block evicted.
#[tokio::test]
async fn a_key_holding_another_object_is_a_conflict_and_the_blobs_stay_local() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    let key = key(&cache, object);
    let other = Bytes::from_static(b"some other object under this key");
    farm.store.put_new(&key, other, None).await.expect("put");
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.conflicts, round.flushed), (vec![object], vec![]));
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Conflict));
    assert_eq!(farm.log.unreachable(object), None);
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    let digests: Vec<_> = blobs.iter().map(|b| b.digest).collect();
    assert_eq!(cache.find_missing(&digests).await.expect("find"), []);
    let round = cache
        .flush_tier(later() + FLUSH_BACKOFF_MAX)
        .await
        .expect("round");
    assert_eq!(round, kbf_front::FlushRound::default(), "not retried");
    assert_eq!(
        farm.store.put_keys().len(),
        2,
        "the test's PUT and one flush"
    );
    assert!(farm.has_block(object));
    let metrics = cache.render_metrics();
    assert!(
        metrics.contains("\nkbf_write_tier_flush_conflicts_total 1\n"),
        "{metrics}"
    );
    // With the block gone, the store's object under the key is not this one: neither
    // its short bytes at the first record nor the missing range of the second marks
    // the staged object.
    farm.fs.remove(&block_name(object)).expect("remove");
    for b in &blobs {
        let read = cache.read_blob(&b.digest).await;
        assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    }
    assert_eq!(farm.log.unreachable(object), None);
}

/// Catches: a record damaged on the tier's disk flushed to the store (its footer would
/// carry the CRC of the damaged bytes), the object left readable, a damaged record
/// that takes its sound neighbours down with it, and a drop nobody sees. The damaged
/// record is never PUT, counted and dropped (its blob unavailable until a new upload
/// heals it); the sound record of the same block is staged again as an object of its
/// own, flushed by the next round, and readable throughout.
#[tokio::test]
async fn a_record_damaged_on_the_tier_is_dropped_and_the_sound_one_is_flushed() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let (at, _) = farm.log.located(&blobs[1].digest);
    let offset = usize::try_from(at.offset).expect("offset") + 7;
    farm.fs.flip(&block_name(at.object), offset);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.corrupt, round.flushed), (vec![at.object], vec![]));
    assert_eq!(round.dropped, [blobs[1].digest]);
    assert!(farm.store.put_keys().is_empty(), "nothing uploaded");
    assert_eq!(
        farm.log.unreachable(at.object),
        Some(UnreachableReason::Corrupt)
    );
    assert_eq!(farm.log.answer(&blobs[1].digest), BlobAnswer::Unavailable);
    assert!(!farm.has_block(at.object));
    let metrics = cache.render_metrics();
    assert!(
        metrics.contains("\nkbf_tier_flush_digest_mismatch_total 1\n"),
        "{metrics}"
    );

    // The sound record was staged again, in an object of its own, and reads as before.
    let (now, tier) = farm.log.located(&blobs[0].digest);
    assert_ne!(now.object, at.object);
    assert_eq!(tier, cache.tier_id());
    assert_eq!(
        cache.read_blob(&blobs[0].digest).await.expect("sound"),
        blobs[0].bytes
    );
    let again = cache.flush_tier(later()).await.expect("round");
    assert_eq!((again.flushed, again.dropped), (vec![now.object], vec![]));
    assert_eq!(farm.store.put_keys().len(), 1, "one object, the sound one");
    let stored = farm
        .store
        .stored(&key(&cache, now.object))
        .await
        .expect("stored");
    let reader = kbf_segments::SegmentReader::open(&stored).expect("a segment");
    assert_eq!(
        reader.get(&blobs[0].digest).expect("record"),
        Some(&blobs[0].bytes[..])
    );
    assert_eq!(reader.get(&blobs[1].digest).expect("absent"), None);
    assert_eq!(
        cache.read_blob(&blobs[0].digest).await.expect("flushed"),
        blobs[0].bytes
    );

    cache.store_blobs(verified(&blobs)).await.expect("again");
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("healed"), b.bytes);
    }
}

/// Catches: a block of nothing but damaged records that stages an empty object again
/// (or fails the round), and a count that is per object instead of per record. Every
/// record of the block is flipped: both are dropped and counted, nothing is staged
/// again, nothing is PUT.
#[tokio::test]
async fn a_block_of_only_damaged_records_leaves_nothing_behind() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let (at, _) = farm.log.located(&blobs[0].digest);
    for b in &blobs {
        let (there, _) = farm.log.located(&b.digest);
        let offset = usize::try_from(there.offset).expect("offset") + 1;
        farm.fs.flip(&block_name(at.object), offset);
    }
    let round = cache.flush_tier(later()).await.expect("round");
    let mut dropped = round.dropped.clone();
    dropped.sort();
    let mut want: Vec<_> = blobs.iter().map(|b| b.digest).collect();
    want.sort();
    assert_eq!(dropped, want);
    assert_eq!(round.corrupt, [at.object]);
    let metrics = cache.render_metrics();
    assert!(
        metrics.contains("\nkbf_tier_flush_digest_mismatch_total 2\n"),
        "{metrics}"
    );
    let again = cache.flush_tier(later()).await.expect("round");
    assert_eq!(again, kbf_front::FlushRound::default());
    assert!(farm.store.put_keys().is_empty());
    for b in &blobs {
        assert_eq!(farm.log.answer(&b.digest), BlobAnswer::Unavailable);
    }
}

/// Catches: a sound record whose new block cannot be committed (the log refuses the
/// `PutStaged`) that fails the round, retries an object that already left the queue,
/// or is reported as staged. The damaged record is still dropped and counted, the
/// object is marked corrupt, and both blobs are unavailable until a new upload heals
/// them: the failure costs availability, never correctness.
#[tokio::test]
async fn a_sound_record_that_cannot_be_staged_again_is_unavailable_until_uploaded() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let (at, _) = farm.log.located(&blobs[1].digest);
    let offset = usize::try_from(at.offset).expect("offset") + 7;
    farm.fs.flip(&block_name(at.object), offset);
    farm.log.refuse("put_staged", 1);
    let round = cache.flush_tier(later()).await.expect("not an error");
    assert_eq!(
        (round.corrupt, round.dropped),
        (vec![at.object], vec![blobs[1].digest])
    );
    assert!(round.retrying.is_empty(), "{:?}", round.retrying);
    assert_eq!(farm.log.applied("put_staged"), 1, "only the first upload");
    for b in &blobs {
        assert_eq!(farm.log.answer(&b.digest), BlobAnswer::Unavailable);
    }
    let again = cache.flush_tier(later()).await.expect("round");
    assert_eq!(again, kbf_front::FlushRound::default());
    assert!(farm.store.put_keys().is_empty());
    cache.store_blobs(verified(&blobs)).await.expect("again");
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("healed"), b.bytes);
    }
}

/// Catches: writes refused (or lost) at the budget instead of going to the store; a
/// staged block evicted to make room; disk use over the budget; a tier that never
/// takes writes again once a flush frees room (flushed copies not evicted for a
/// reservation); and a write acknowledged while both the tier and the store refuse it.
#[tokio::test]
async fn at_the_budget_writes_go_to_the_store_and_no_staged_block_is_evicted() {
    let farm = Farm::new(Capabilities::default());
    let budget = 10_000;
    let cache = farm.start(budget).await;
    let b = blobs(1, 5, 4096);
    for blob in &b[..3] {
        cache
            .store_blobs(vec![blob.verified()])
            .await
            .expect("stored");
    }
    let staged = farm.log.object_of(&b[0].digest);
    assert_eq!(farm.log.object_of(&b[1].digest), staged);
    assert_eq!(farm.log.located(&b[1].digest).1, cache.tier_id());
    assert_eq!(
        farm.log.located(&b[2].digest).1,
        None,
        "past the budget: PUT"
    );
    assert_eq!(farm.store.put_keys().len(), 1);
    assert!(farm.disk_bytes() <= budget, "{}", farm.disk_bytes());
    assert!(farm.has_block(staged));
    let metrics = cache.render_metrics();
    assert!(
        metrics.contains("\nkbf_write_tier_full_total 1\n"),
        "{metrics}"
    );

    // The direct path tries a PUT `put_attempts` times; the store refuses them all.
    for _ in 0..kbf_front::WriteConfig::DEFAULT.put_attempts.get() {
        farm.store.on_put(PutFault::Fail);
    }
    let refused = cache.store_blobs(vec![b[3].verified()]).await;
    let status = tonic::Status::from(refused.expect_err("tier full, store down"));
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert_eq!(farm.log.answer(&b[3].digest), BlobAnswer::Absent);

    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [staged]);
    cache
        .store_blobs(vec![b[4].verified()])
        .await
        .expect("stored");
    assert_eq!(farm.log.located(&b[4].digest).1, cache.tier_id());
    assert!(!farm.has_copy(staged), "the copy made room");
    assert!(farm.disk_bytes() <= budget, "{}", farm.disk_bytes());
}

/// Catches: a flushed copy evicted most recently read first, and a read of an
/// evicted copy that marks its object or fails instead of reading the store.
#[tokio::test]
async fn flushed_copies_are_evicted_least_recently_read_first() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(10_000).await;
    let b = blobs(1, 3, 4096);
    let mut objects = Vec::new();
    for blob in &b[..2] {
        cache
            .store_blobs(vec![blob.verified()])
            .await
            .expect("stored");
        let object = farm.log.object_of(&blob.digest);
        assert_eq!(
            cache.flush_tier(later()).await.expect("round").flushed,
            [object]
        );
        objects.push(object);
    }
    assert_eq!(
        cache.read_blob(&b[0].digest).await.expect("read"),
        b[0].bytes
    );
    cache
        .store_blobs(vec![b[2].verified()])
        .await
        .expect("stored");
    assert!(farm.has_copy(objects[0]), "read last: kept");
    assert!(!farm.has_copy(objects[1]), "least recently read: evicted");
    assert_eq!(
        cache.read_blob(&b[1].digest).await.expect("from the store"),
        b[1].bytes
    );
    assert_eq!(farm.log.unreachable(objects[1]), None);
    let metrics = cache.render_metrics();
    let local = "\nkbf_write_tier_local_reads_total 1\n";
    assert!(
        metrics.contains(local),
        "only the first read was local: {metrics}"
    );
}

/// Catches: a wrong mount or a lost `TIER` file destroying the index (the start must
/// be refused and change nothing); `--hot-store-abandon` naming another tier
/// accepted; and abandoned blobs answered `NOT_FOUND` or with other bytes instead of
/// `Unavailable` until a re-upload heals them.
#[tokio::test]
async fn a_wrong_or_missing_tier_file_refuses_the_start_and_abandon_heals() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let ours = cache.tier_id().expect("tier");
    let object = farm.log.object_of(&blobs[0].digest);
    drop(cache);

    let fs = farm.fs.restart();
    fs.remove(TIER).expect("remove TIER");
    let refused = start(&fs, &farm.log, &farm.store, GIB, None).await;
    assert!(
        matches!(
            refused,
            Err(TierStartError::Tier(TierError::WrongTier {
                found: None,
                ..
            }))
        ),
        "{refused:?}"
    );
    fs.put(TIER, b"77\n");
    let refused = start(&fs, &farm.log, &farm.store, GIB, None).await;
    assert!(
        matches!(
            refused,
            Err(TierStartError::Tier(TierError::WrongTier { found: Some(t), .. })) if t == TierId::new(77)
        ),
        "{refused:?}"
    );
    let other = Some(TierId::new(77));
    let refused = start(&fs, &farm.log, &farm.store, GIB, other).await;
    assert!(
        matches!(refused, Err(TierStartError::AbandonOther { .. })),
        "{refused:?}"
    );
    assert!(
        fs.contents(&block_name(object)).is_some(),
        "nothing changed"
    );
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Open));

    let (cache, startup) = start(&fs, &farm.log, &farm.store, GIB, Some(ours))
        .await
        .expect("abandon");
    assert_eq!(startup.abandoned, [object]);
    assert!(startup.registered);
    assert_ne!(startup.tier, ours);
    for b in &blobs {
        assert_eq!(farm.log.answer(&b.digest), BlobAnswer::Unavailable);
        let read = cache.read_blob(&b.digest).await;
        assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    }
    let digests: Vec<_> = blobs.iter().map(|b| b.digest).collect();
    assert_eq!(cache.find_missing(&digests).await.expect("find"), digests);
    assert!(fs.contents(&block_name(object)).is_none(), "swept");
    cache
        .store_blobs(verified(&blobs))
        .await
        .expect("re-upload");
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("healed"), b.bytes);
    }
}

/// Catches: one lost block file failing the whole tier (every staged object marked,
/// or the start refused) or going unreported (its blobs answered present with no
/// bytes behind them).
#[tokio::test]
async fn one_missing_block_marks_only_its_object_missing() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let size = usize::try_from(SEGMENT).expect("size") + 1;
    let (lost, kept) = (TestBlob::new(1, size), TestBlob::new(2, size));
    for b in [&lost, &kept] {
        cache.store_blobs(vec![b.verified()]).await.expect("stored");
    }
    let (lost_object, kept_object) = (
        farm.log.object_of(&lost.digest),
        farm.log.object_of(&kept.digest),
    );
    assert_ne!(lost_object, kept_object);
    drop(cache);

    let fs = farm.fs.restart();
    fs.remove(&block_name(lost_object)).expect("remove");
    let (cache, startup) = start(&fs, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert_eq!(startup.recovery.lost, [lost_object]);
    assert_eq!(
        farm.log.unreachable(lost_object),
        Some(UnreachableReason::Missing)
    );
    assert_eq!(farm.log.answer(&lost.digest), BlobAnswer::Unavailable);
    assert_eq!(farm.log.unreachable(kept_object), None);
    assert_eq!(
        cache.read_blob(&kept.digest).await.expect("read"),
        kept.bytes
    );
}

/// Catches: a restart that flushes only what it sealed itself (a block sealed before
/// the crash, whose flush had failed, would never reach the store), and one that does
/// not seal a block left unsealed.
#[tokio::test]
async fn a_restart_seals_and_flushes_what_the_last_start_left() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let (a, b) = (TestBlob::new(1, 4096), TestBlob::new(2, 4096));
    cache.store_blobs(vec![a.verified()]).await.expect("stored");
    let sealed = farm.log.object_of(&a.digest);
    farm.store.on_put(PutFault::Fail);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.sealed, round.flushed), (vec![sealed], vec![]));
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    let open = farm.log.object_of(&b.digest);
    assert_ne!(open, sealed);
    drop(cache);

    let cut = farm.fs.crash(0);
    let (cache, startup) = start(&cut, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert_eq!(startup.recovery.sealed, [sealed]);
    assert_eq!(startup.recovery.unsealed, [open]);
    let round = cache.flush_tier(Instant::now()).await.expect("round");
    assert_eq!(round.sealed, [open]);
    assert_eq!(round.flushed, [sealed, open]);
    for blob in [&a, &b] {
        assert_eq!(farm.log.located(&blob.digest).1, None);
        assert_eq!(
            cache.read_blob(&blob.digest).await.expect("read"),
            blob.bytes
        );
    }
}

/// Catches: a block synced but never committed (its `PutStaged` failed) acknowledged,
/// or left on disk and counted against the budget after the next start.
#[tokio::test]
async fn a_block_whose_put_staged_failed_is_swept_at_the_next_start() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(1, 4096);
    farm.log.refuse("put_staged", 1);
    let refused = cache.store_blobs(vec![b.verified()]).await;
    assert!(matches!(refused, Err(CacheError::Meta(_))), "{refused:?}");
    assert_eq!(farm.log.answer(&b.digest), BlobAnswer::Absent);
    let blocks = |fs: &kbf_store::FaultFs| {
        let names = fs.list().expect("list");
        names.into_iter().filter(|n| n.ends_with(".blk")).count()
    };
    assert_eq!(blocks(&farm.fs), 1, "written and synced, never committed");
    drop(cache);

    let cut = farm.fs.crash(0);
    let (cache, startup) = start(&cut, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert_eq!(startup.recovery.removed.len(), 1);
    assert_eq!(blocks(&cut), 0);
    assert_eq!(cache.write_tier().expect("tier").usage().used, 0);
}

/// Catches: unflushed bytes or blocks miscounted (reserved but uncommitted bytes
/// counted, flushed ones still counted), and an oldest age that is not the oldest
/// block's (the newest one's would hide a stuck block from the alert).
#[tokio::test]
async fn the_tier_reports_unflushed_bytes_and_the_oldest_blocks_age() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let small = TestBlob::new(1, 4096);
    let big = TestBlob::new(2, usize::try_from(SEGMENT).expect("size") + 1);
    cache
        .store_blobs(vec![small.verified()])
        .await
        .expect("stored");
    let after_small = Instant::now();
    std::thread::sleep(Duration::from_millis(20));
    cache
        .store_blobs(vec![big.verified()])
        .await
        .expect("stored");
    let at = Instant::now();
    let tier = cache.write_tier().expect("tier");
    let unflushed = tier.unflushed(at);
    assert_eq!(unflushed.bytes, 4096 + big.digest.size_bytes);
    assert_eq!(unflushed.blocks, 2);
    let oldest = unflushed.oldest.expect("an unflushed block");
    assert!(oldest >= at.duration_since(after_small), "{oldest:?}");
    let metrics = cache.render_metrics();
    let bytes = format!("\nkbf_write_tier_unflushed_bytes {}\n", unflushed.bytes);
    assert!(metrics.contains(&bytes), "{metrics}");
    assert!(
        metrics.contains("\nkbf_write_tier_unflushed_blocks 2\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("\nkbf_write_tier_budget_bytes 1073741824\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("\nkbf_write_tier_oldest_unflushed_seconds 0."),
        "{metrics}"
    );

    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2);
    let unflushed = cache.write_tier().expect("tier").unflushed(Instant::now());
    assert_eq!(
        (unflushed.bytes, unflushed.blocks, unflushed.oldest),
        (0, 0, None)
    );
    let metrics = cache.render_metrics();
    assert!(
        metrics.contains("\nkbf_write_tier_unflushed_bytes 0\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("\nkbf_write_tier_flushes_total 2\n"),
        "{metrics}"
    );
}

/// Catches: a cache without a tier that stages, flushes or reports tier metrics: the
/// tier is off unless attached, and then a blob is in the store when acknowledged.
#[tokio::test]
async fn without_a_tier_nothing_is_staged_or_flushed() {
    let farm = Farm::new(Capabilities::default());
    let cache = kbf_front::Cache::open(
        farm.log.clone(),
        farm.store.clone(),
        kbf_objstore::KeyPrefix::default(),
    )
    .await
    .expect("open");
    let b = TestBlob::new(1, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    assert_eq!(farm.log.located(&b.digest).1, None);
    assert_eq!(farm.store.put_keys().len(), 1);
    assert_eq!(cache.tier_id(), None);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round, kbf_front::FlushRound::default());
    assert!(!cache.render_metrics().contains("kbf_write_tier"));
    assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
}

/// Catches: a staged record damaged on the local disk served, or answered without
/// marking its object (the local copy is the only one: its blobs must answer
/// `Unavailable` and heal by re-upload); a disk read error on a staged block taken
/// for a lost object (marked) instead of an unavailable read; and the marked object's
/// open block left taking ranges (the next upload's `PutStaged` would be refused, so
/// it would fail instead of staging in a new block) or left on the disk counting
/// against the budget until the next start.
#[tokio::test]
async fn a_staged_record_the_disk_damaged_or_failed_to_read_is_never_served() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let (at, _) = farm.log.located(&blobs[0].digest);
    farm.fs.fail_at(farm.fs.ops(), kbf_store::Fault::Error);
    let read = cache.read_blob(&blobs[0].digest).await;
    assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    assert_eq!(
        farm.log.unreachable(at.object),
        None,
        "a read error marks nothing"
    );
    let offset = usize::try_from(at.offset).expect("offset");
    farm.fs.flip(&block_name(at.object), offset);
    let read = cache.read_blob(&blobs[0].digest).await;
    assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    assert_eq!(
        farm.log.unreachable(at.object),
        Some(UnreachableReason::Corrupt)
    );
    assert_eq!(farm.log.answer(&blobs[1].digest), BlobAnswer::Unavailable);
    let name = block_name(at.object);
    assert_eq!(farm.fs.contents(&name), None, "the marked block is kept");
    let next = TestBlob::new(9, 4096);
    cache
        .store_blobs(vec![next.verified()])
        .await
        .expect("stored");
    let (now_at, staged_on) = farm.log.located(&next.digest);
    assert_ne!(now_at.object, at.object, "staged into the marked object");
    assert!(staged_on.is_some(), "staged, not sent direct");
}

/// Catches: a blob staged on another server's tier read from this server's disk (at
/// another object's offsets) or from the store, and its object marked by that read.
#[tokio::test]
async fn a_blob_staged_on_another_servers_tier_is_unavailable_here() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let theirs = TierId::new(999);
    let added = farm.log.commit(Command::AddTier {
        server: ServerId::new(2),
        tier: theirs,
    });
    assert_eq!(added, Applied::Tier(Ok(())));
    let Applied::Epoch(epoch) = farm.log.commit(Command::AllocEpoch) else {
        panic!("an epoch");
    };
    let b = TestBlob::new(1, 4096);
    let object = ObjectId::new(epoch, 1);
    let put = StagedPut {
        object,
        end: 4096,
        seal: false,
        blobs: vec![(b.digest, 0)],
    };
    let staged = farm.log.commit(Command::PutStaged {
        tier: theirs,
        objects: vec![put],
    });
    assert!(matches!(staged, Applied::Staged(Ok(_))), "{staged:?}");
    let read = cache.read_blob(&b.digest).await;
    assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    let batch = cache.read_blobs(&[b.digest]).await.expect("batch");
    assert!(matches!(batch[&b.digest], Err(CacheError::Unreachable(_))));
    assert_eq!(farm.log.unreachable(object), None);
}

/// Catches: a disk error before the commit (an `ENOSPC` on a write, a failed
/// `fdatasync`) answered as an error or as a stored blob, instead of sending the
/// segment to the store; and the failure not counted.
#[tokio::test]
async fn a_disk_error_before_the_commit_sends_the_segment_to_the_store() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = blobs(1, 2, 4096);
    // A new block: create, write, sync, directory sync. Fail the write.
    farm.fs.fail_at(farm.fs.ops() + 1, kbf_store::Fault::Error);
    cache
        .store_blobs(vec![b[0].verified()])
        .await
        .expect("stored");
    // The poisoned block, with nothing committed, is deleted when the next range seals
    // it; then another new block: remove, create, write, sync. Fail the sync.
    farm.fs.fail_at(farm.fs.ops() + 3, kbf_store::Fault::Error);
    cache
        .store_blobs(vec![b[1].verified()])
        .await
        .expect("stored");
    for blob in &b {
        assert_eq!(farm.log.located(&blob.digest).1, None, "in the store");
        assert_eq!(
            cache.read_blob(&blob.digest).await.expect("read"),
            blob.bytes
        );
    }
    assert_eq!(farm.store.put_keys().len(), 2);
    let metrics = cache.render_metrics();
    assert!(
        metrics.contains("\nkbf_write_tier_errors_total 2\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("\nkbf_write_tier_full_total 0\n"),
        "{metrics}"
    );
}

/// Catches: a flushed block whose rename to a copy failed left counting, or its flush
/// undone (the object is in the store and `ObjectFlushed` applied); its `.blk` is an
/// orphan the next start deletes.
#[tokio::test]
async fn a_flushed_block_that_cannot_be_renamed_is_swept_at_the_next_start() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let big = TestBlob::new(1, usize::try_from(SEGMENT).expect("size") + 1);
    cache
        .store_blobs(vec![big.verified()])
        .await
        .expect("stored");
    let object = farm.log.object_of(&big.digest);
    // The flush reads the block (one operation), then renames it.
    farm.fs.fail_at(farm.fs.ops() + 1, kbf_store::Fault::Error);
    let round = cache.flush_tier(Instant::now()).await.expect("round");
    assert_eq!(round.flushed, [object]);
    assert_eq!(farm.log.staged_state(object), None);
    assert!(farm.has_block(object) && !farm.has_copy(object));
    assert_eq!(cache.write_tier().expect("tier").usage().used, 0);
    assert_eq!(
        cache.read_blob(&big.digest).await.expect("from the store"),
        big.bytes
    );
    drop(cache);
    let fs = farm.fs.restart();
    let (_, startup) = start(&fs, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert_eq!(startup.recovery.removed, [block_name(object)]);
}

/// Catches: a log that answers a tier command with another command's outcome taken
/// for a success: a write acknowledged, a seal or flush recorded, a tier attached.
#[tokio::test]
async fn a_log_answering_with_another_commands_outcome_is_an_internal_error() {
    let farm = Farm::new(CONDITIONAL);
    farm.log.answer_wrong("add_tier", 1);
    let refused = start(&farm.fs, &farm.log, &farm.store, GIB, None).await;
    assert!(
        matches!(refused, Err(TierStartError::Cache(CacheError::Internal(_)))),
        "{refused:?}"
    );
    let cache = farm.start(GIB).await;
    let debug = format!("{cache:?}");
    assert!(
        debug.contains("Staging") && debug.contains("tier"),
        "{debug}"
    );
    let b = blobs(1, 3, 4096);
    farm.log.answer_wrong("put_staged", 1);
    let refused = cache.store_blobs(vec![b[0].verified()]).await;
    assert!(
        matches!(refused, Err(CacheError::Internal(_))),
        "{refused:?}"
    );
    assert_eq!(farm.log.answer(&b[0].digest), BlobAnswer::Absent);

    cache
        .store_blobs(vec![b[1].verified()])
        .await
        .expect("stored");
    let object = farm.log.object_of(&b[1].digest);
    farm.log.answer_wrong("seal_staged", 1);
    let refused = cache.flush_tier(later()).await;
    assert!(
        matches!(refused, Err(CacheError::Internal(_))),
        "{refused:?}"
    );
    farm.log.answer_wrong("object_flushed", 1);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.sealed, [object]);
    assert_eq!(round.retrying, [(object, FLUSH_BACKOFF_FIRST)]);

    let big = TestBlob::new(9, usize::try_from(SEGMENT).expect("size") + 1);
    cache
        .store_blobs(vec![big.verified()])
        .await
        .expect("stored");
    let other = farm.log.object_of(&big.digest);
    let taken = Bytes::from_static(b"another object under this key");
    farm.store
        .put_new(&key(&cache, other), taken, None)
        .await
        .expect("put");
    farm.log.answer_wrong("flush_conflict", 1);
    let round = cache.flush_tier(later()).await.expect("round");
    assert!(
        round.retrying.contains(&(other, FLUSH_BACKOFF_FIRST)),
        "{round:?}"
    );
    assert_eq!(farm.log.staged_state(other), Some(StagedState::Sealed));
}

/// Catches: a flushed copy whose bytes the disk damaged served, or its object marked
/// (the store holds a good copy: the read goes there and succeeds); and, once the copy
/// is gone, a store object with wrong bytes served or left unmarked.
#[tokio::test]
async fn a_damaged_flushed_copy_is_read_from_the_store() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(1, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("stored");
    let object = farm.log.object_of(&b.digest);
    assert_eq!(
        cache.flush_tier(later()).await.expect("round").flushed,
        [object]
    );
    farm.fs.flip(&seg_name(object), 0);
    assert_eq!(
        cache.read_blob(&b.digest).await.expect("from the store"),
        b.bytes
    );
    assert_eq!(farm.log.unreachable(object), None);

    farm.fs.remove(&seg_name(object)).expect("remove");
    let key = key(&cache, object);
    let mut wrong = farm.store.stored(&key).await.expect("stored").to_vec();
    wrong[0] ^= 0xff;
    farm.store
        .put_new(&key, Bytes::from(wrong), None)
        .await
        .expect("replace");
    let read = cache.read_blob(&b.digest).await;
    assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    assert_eq!(
        farm.log.unreachable(object),
        Some(UnreachableReason::Corrupt)
    );
}

/// Catches: a batch read that answers a staged blob, or any other answer, as another
/// one: every kind of answer in one `read_blobs` (the empty blob, a blob never stored,
/// one whose object is unreachable, one in the store, one staged here).
#[tokio::test]
async fn a_batch_read_answers_every_kind_of_blob() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let [flushed, staged, never] = [2, 3, 4].map(|seed| TestBlob::new(seed, 4096));
    let empty = TestBlob::new(0, 0);
    let big = |seed| TestBlob::new(seed, usize::try_from(SEGMENT).expect("size") + 1);
    let (lost_big, kept_big) = (big(5), big(6));
    for b in [&flushed, &lost_big] {
        cache.store_blobs(vec![b.verified()]).await.expect("stored");
    }
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2);
    let gone = farm.log.object_of(&lost_big.digest);
    farm.store.delete(&key(&cache, gone)).await.expect("delete");
    farm.fs.remove(&seg_name(gone)).expect("remove");
    let missing = cache.read_blob(&lost_big.digest).await;
    assert!(
        matches!(missing, Err(CacheError::Unreachable(_))),
        "{missing:?}"
    );
    for b in [&staged, &kept_big] {
        cache.store_blobs(vec![b.verified()]).await.expect("stored");
    }
    let digests = [
        empty.digest,
        never.digest,
        lost_big.digest,
        flushed.digest,
        staged.digest,
        kept_big.digest,
    ];
    let read = cache.read_blobs(&digests).await.expect("batch");
    assert_eq!(read[&empty.digest].as_ref().expect("empty"), &Bytes::new());
    assert!(matches!(read[&never.digest], Err(CacheError::NotFound(_))));
    assert!(matches!(
        read[&lost_big.digest],
        Err(CacheError::Unreachable(_))
    ));
    for b in [&flushed, &staged, &kept_big] {
        assert_eq!(read[&b.digest].as_ref().expect("read"), &b.bytes);
    }
    assert_eq!(farm.log.located(&staged.digest).1, cache.tier_id());
}

/// Catches: a blob the index places in a store this cache does not have read from the
/// tier or the configured store (its offsets belong to another store's object): it is
/// an internal error, with or without a tier.
#[tokio::test]
async fn a_blob_in_another_store_is_an_internal_error() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let Applied::Epoch(epoch) = farm.log.commit(Command::AllocEpoch) else {
        panic!("an epoch");
    };
    let b = TestBlob::new(1, 4096);
    let location = kbf_meta::Location {
        store: kbf_meta::StoreId::new(1),
        object: ObjectId::new(epoch, 1),
        offset: 0,
    };
    let put = farm.log.commit(Command::PutBlob {
        digest: b.digest,
        location,
    });
    assert!(matches!(put, Applied::Blob(Ok(_))), "{put:?}");
    let read = cache.read_blob(&b.digest).await;
    assert!(matches!(read, Err(CacheError::Internal(_))), "{read:?}");
}

/// Catches: the open segment a blob too large for it leaves behind (the group opens
/// one before it finds the blob does not fit) settled empty at its linger: an empty
/// block staged and flushed, or an empty object PUT, for nothing.
#[tokio::test]
async fn a_blob_too_large_for_a_segment_leaves_no_empty_one_behind() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let big = TestBlob::new(1, usize::try_from(SEGMENT).expect("size") + 1);
    cache
        .store_blobs(vec![big.verified()])
        .await
        .expect("stored");
    tokio::time::sleep(kbf_front::WriteConfig::DEFAULT.linger * 10).await;
    let blocks = farm.fs.list().expect("list");
    let blocks: Vec<_> = blocks.iter().filter(|n| n.ends_with(".blk")).collect();
    assert_eq!(blocks, [&block_name(farm.log.object_of(&big.digest))]);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 1, "{round:?}");
    assert_eq!(farm.store.put_keys().len(), 1);
}

/// Catches: grouped writes (write-path.md section 3) that bypass the tier, or stage
/// each caller's blob on its own: concurrent uploads share one segment, and that
/// segment is one range of the open block, staged with one `PutStaged`.
#[tokio::test]
async fn concurrent_uploads_share_one_staged_segment() {
    let farm = Farm::new(Capabilities::default());
    let cache = Arc::new(farm.start(GIB).await);
    let b = blobs(1, 8, 4096);
    let stores = b.iter().map(|blob| {
        let (cache, blob) = (Arc::clone(&cache), blob.verified());
        tokio::spawn(async move { cache.store_blobs(vec![blob]).await })
    });
    for store in stores.collect::<Vec<_>>() {
        store.await.expect("join").expect("stored");
    }
    let object = farm.log.object_of(&b[0].digest);
    for blob in &b {
        assert_eq!(farm.log.located(&blob.digest).1, cache.tier_id());
        assert_eq!(farm.log.object_of(&blob.digest), object, "one block");
    }
    assert!(farm.log.applied("put_staged") < b.len(), "grouped");
    assert!(farm.store.put_keys().is_empty());
}
