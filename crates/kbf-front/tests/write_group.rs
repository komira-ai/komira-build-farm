//! Write grouping (`docs/design/write-path.md`, sections 3 and 9): concurrent uploads
//! share one segment, one PUT and one `PutBlobs`; nobody is answered before their
//! segment commits; a failure stays inside its segment; a PUT is retried only under a
//! new key; large blobs skip the group; admission is held until the bytes are written.
//!
//! Every test runs the cache over a [`FaultyStore`] (which counts PUTs and fails them
//! on cue) and an [`AuditLog`] (which counts `PutBlobs` and checks each commits only
//! over bytes the store holds). Most run under a paused clock, so the linger and the
//! retry backoff are exact and nothing waits in real time. Each test names the defect
//! it is there to catch, and the mutant that was planted to see it fail.

mod common;

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::faulty::{AuditLog, CommitFault, FaultyStore, PutFault};
use common::pseudo_random;
use kbf_front::{
    Cache, CacheError, CasService, MAX_ADMISSION_BYTES, MIN_SEGMENT_UPLOAD_BYTES, MetaLog,
    PUT_BACKOFF, PUT_BACKOFF_FACTOR, VerifiedBlob, WriteConfig,
};
use kbf_meta::BlobAnswer;
use kbf_objstore::{Capabilities, KeyPrefix, ObjectKey};
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorage;
use kbf_proto::reapi::{BatchUpdateBlobsRequest, batch_update_blobs_request};
use kbf_segments::SegmentReader;
use kbf_segments::layout::{ENTRY_LEN, footer_len};
use kbf_types::Digest;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tonic::{Code, Request, Status};

type FaultyCache = Cache<AuditLog, FaultyStore>;

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// A cache over a fresh conditional-write bucket, grouping as `config` says, with
/// segments of at most `limit` bytes; and a handle on the bucket.
async fn cache(config: WriteConfig, limit: u64) -> (Arc<FaultyCache>, FaultyStore) {
    let store = FaultyStore::new(CONDITIONAL);
    let cache = Cache::open(
        AuditLog::new(store.clone()),
        store.clone(),
        KeyPrefix::default(),
    )
    .await
    .expect("open the cache")
    .with_segment_limit(limit)
    .with_writes(config);
    (Arc::new(cache), store)
}

/// The default cache: [`WriteConfig::DEFAULT`] and full-size segments.
async fn default_cache() -> (Arc<FaultyCache>, FaultyStore) {
    cache(WriteConfig::DEFAULT, kbf_segments::MAX_SEGMENT_BYTES).await
}

/// A segment limit with room for one 4 KiB blob and no more.
fn one_4k_blob() -> u64 {
    footer_len(1) + 4096
}

/// `len` bytes for `seed`, different for every seed (`pseudo_random` gives seeds `2n`
/// and `2n + 1` the same bytes).
fn bytes(seed: u64, len: usize) -> Vec<u8> {
    pseudo_random(len, 2 * seed + 1)
}

/// A blob of [`bytes`].
fn blob(seed: u64, len: usize) -> VerifiedBlob {
    VerifiedBlob::hashed(Bytes::from(bytes(seed, len)))
}

/// Stores `blobs` in a task of its own, as one caller.
fn spawn_store(
    cache: &Arc<FaultyCache>,
    blobs: Vec<VerifiedBlob>,
) -> JoinHandle<Result<(), CacheError>> {
    let cache = Arc::clone(cache);
    tokio::spawn(async move { cache.store_blobs(blobs).await })
}

async fn answer(cache: &FaultyCache, digest: Digest) -> BlobAnswer {
    cache
        .meta()
        .query(move |s| s.blob(&digest))
        .await
        .expect("query")
}

async fn present(cache: &FaultyCache, digest: Digest) -> bool {
    matches!(answer(cache, digest).await, BlobAnswer::Present { .. })
}

/// The key of the object that holds `digest`.
async fn key_of(cache: &FaultyCache, digest: Digest) -> ObjectKey {
    let BlobAnswer::Present { location: at, .. } = answer(cache, digest).await else {
        panic!("{digest} is not present");
    };
    cache.object_key(at.object).expect("key")
}

/// How many records the object under `key` holds, by its stored footer.
async fn records_in(store: &FaultyStore, key: &ObjectKey) -> usize {
    let bytes = store.stored(key).await.expect("stored");
    SegmentReader::open(&bytes)
        .expect("a segment")
        .footer()
        .entries()
        .len()
}

fn assert_audit_clean(cache: &FaultyCache) {
    assert_eq!(cache.meta().violations(), Vec::<String>::new());
}

fn config(edit: impl FnOnce(&mut WriteConfig)) -> WriteConfig {
    let mut config = WriteConfig::DEFAULT;
    edit(&mut config);
    config
}

/// 64 callers each store one blob at once: one segment, one PUT, one `PutBlobs` of 64,
/// and every blob reads back. Under the paused clock every push lands before the 2 ms
/// linger fires, so the count is exact.
///
/// Catches: grouping not engaged (one PUT and one commit per caller, as before).
/// Mutant seen red: seal on every push.
#[tokio::test(start_paused = true)]
async fn concurrent_single_blob_stores_share_one_put_and_one_commit() {
    let (cache, store) = default_cache().await;
    let blobs: Vec<VerifiedBlob> = (0..64).map(|n| blob(n, 4096)).collect();
    let calls: Vec<_> = blobs
        .iter()
        .map(|b| spawn_store(&cache, vec![b.clone()]))
        .collect();
    for call in calls {
        call.await.expect("join").expect("store");
    }
    assert_eq!(store.put_keys().len(), 1, "PUTs");
    assert_eq!(cache.meta().batches(), [64], "PutBlobs commits");
    for b in &blobs {
        let read = cache.read_blob(&b.digest()).await.expect("read");
        assert_eq!(kbf_segments::sha256(&read), b.digest());
    }
    assert_audit_clean(&cache);
}

/// While the segment's PUT is held, and then while its `PutBlobs` is held, none of the
/// eight callers in it has returned and none of their blobs is durable. Once the
/// commit goes through, all return and all are durable.
///
/// Catches: acknowledging a caller on the PUT, or on its own push, before the commit.
/// Mutant seen red: answer the waiters before `commit`.
#[tokio::test(start_paused = true)]
async fn no_caller_returns_before_its_segment_commits() {
    let (cache, store) = default_cache().await;
    let (put_entered_tx, put_entered) = oneshot::channel();
    let (put_release, put_release_rx) = oneshot::channel();
    store.on_put(PutFault::Hold {
        entered: put_entered_tx,
        release: put_release_rx,
    });
    let (commit_entered_tx, commit_entered) = oneshot::channel();
    let (commit_release, commit_release_rx) = oneshot::channel();
    cache.meta().on_put_blobs(CommitFault::Hold {
        entered: commit_entered_tx,
        release: commit_release_rx,
    });
    let blobs: Vec<VerifiedBlob> = (0..8).map(|n| blob(100 + n, 4096)).collect();
    let calls: Vec<_> = blobs
        .iter()
        .map(|b| spawn_store(&cache, vec![b.clone()]))
        .collect();

    put_entered.await.expect("the PUT arrives");
    for (call, b) in calls.iter().zip(&blobs) {
        assert!(!call.is_finished(), "a caller returned during the PUT");
        assert!(!cache.is_durable(&b.digest()).await.expect("durable"));
    }
    put_release.send(()).expect("release the PUT");
    commit_entered.await.expect("the commit arrives");
    for (call, b) in calls.iter().zip(&blobs) {
        assert!(!call.is_finished(), "a caller returned before the commit");
        assert!(!cache.is_durable(&b.digest()).await.expect("durable"));
    }
    commit_release.send(()).expect("release the commit");
    for call in calls {
        call.await.expect("join").expect("store");
    }
    for b in &blobs {
        assert!(cache.is_durable(&b.digest()).await.expect("durable"));
    }
    assert_eq!(cache.meta().batches(), [8]);
    assert_audit_clean(&cache);
}

/// Two callers in two segments (each holds one blob). Every PUT of the first fails, so
/// after three attempts (100 ms, then 400 ms apart) its blob is UNAVAILABLE and absent;
/// the other caller's segment commits and its blob is present.
///
/// Catches: indexing a failed segment; a failure spreading to another segment; no
/// retry, or a retry without the backoff. Mutants seen red: one attempt only; no sleep
/// between attempts.
#[tokio::test(start_paused = true)]
async fn a_failed_segment_fails_only_its_own_blobs() {
    let (cache, store) = cache(WriteConfig::DEFAULT, one_4k_blob()).await;
    let (bad, good) = (blob(1, 4096), blob(2, 4096));
    store.fail_puts_holding(bad.digest());
    let start = Instant::now();
    let failing = spawn_store(&cache, vec![bad.clone()]);
    let passing = spawn_store(&cache, vec![good.clone()]);

    let err = failing.await.expect("join").unwrap_err();
    let backoffs = PUT_BACKOFF + PUT_BACKOFF * PUT_BACKOFF_FACTOR;
    assert_eq!(start.elapsed(), backoffs, "three attempts, two waits");
    assert!(matches!(err, CacheError::Store(_)), "{err:?}");
    assert_eq!(Status::from(err).code(), Code::Unavailable);
    passing.await.expect("join").expect("store");

    assert_eq!(answer(&cache, bad.digest()).await, BlobAnswer::Absent);
    assert!(present(&cache, good.digest()).await);
    let keys = store.put_keys();
    assert_eq!(keys.len(), 4, "three attempts and one good PUT: {keys:?}");
    let distinct: std::collections::BTreeSet<_> = keys.iter().collect();
    assert_eq!(distinct.len(), 4, "a retry reused a key: {keys:?}");
    assert_eq!(cache.meta().put_blobs(), [good.digest()]);
    assert_audit_clean(&cache);
}

/// One `BatchUpdateBlobs` whose two verified blobs land in two segments, the second of
/// which fails, with a blob that fails verification between them: each blob gets its
/// own status, in request order (OK, INVALID_ARGUMENT, UNAVAILABLE).
///
/// The failing blob comes last so that pairing the store's answers with every response
/// (the invalid one included) moves the failure onto the invalid blob and leaves the
/// failed blob answered OK; with the failing blob first that shift lands on slots whose
/// status it does not change.
///
/// Catches: one status smeared over the batch; the store's answers shifted by the
/// invalid blob; the call counted ok in `kbf_batch_update_*` when only some of its
/// blobs were stored. Mutants seen red: answer every verified blob with the first
/// failure; zip the store's answers with every response instead of the verified ones
/// (`[OK, UNAVAILABLE, OK]`); count the call ok when any blob stored (`any` for `all`).
#[tokio::test(start_paused = true)]
async fn a_batch_over_a_failed_and_a_good_segment_answers_each_blob() {
    let (cache, store) = cache(WriteConfig::DEFAULT, one_4k_blob()).await;
    let (bad, good) = (blob(3, 4096), blob(4, 4096));
    store.fail_puts_holding(bad.digest());
    let wrong = blob(5, 4096);
    let request = |b: &VerifiedBlob, data: Vec<u8>| batch_update_blobs_request::Request {
        digest: Some(kbf_front::digest_to_proto(&b.digest())),
        data,
        ..Default::default()
    };
    let requests = vec![
        request(&good, bytes(4, 4096)),
        // `wrong`'s digest, other bytes.
        request(&wrong, bytes(6, 4096)),
        request(&bad, bytes(3, 4096)),
    ];
    let service = CasService::new(Arc::clone(&cache));
    let response = service
        .batch_update_blobs(Request::new(BatchUpdateBlobsRequest {
            requests,
            ..Default::default()
        }))
        .await
        .expect("BatchUpdateBlobs")
        .into_inner();
    let codes: Vec<i32> = response
        .responses
        .iter()
        .map(|r| r.status.as_ref().expect("status").code)
        .collect();
    let want = [Code::Ok, Code::InvalidArgument, Code::Unavailable].map(i32::from);
    assert_eq!(codes, want);
    let calls = &cache.metrics().batch_update;
    assert_eq!(
        (calls.total.get(), calls.errors.get()),
        (1, 1),
        "a batch with a blob that failed to store counted as ok"
    );
    assert_eq!(answer(&cache, bad.digest()).await, BlobAnswer::Absent);
    assert!(present(&cache, good.digest()).await);
    assert_audit_clean(&cache);
}

/// The PUT succeeds and the `PutBlobs` is refused: every one of the four callers in the
/// segment fails UNAVAILABLE, none of the blobs is present, and the object is left in
/// the store as an orphan.
///
/// Catches: acknowledging on the PUT alone. Mutant seen red: ignore the commit's error.
#[tokio::test(start_paused = true)]
async fn a_refused_commit_fails_every_caller_in_the_segment() {
    let (cache, store) = default_cache().await;
    cache.meta().on_put_blobs(CommitFault::Refuse);
    let blobs: Vec<VerifiedBlob> = (0..4).map(|n| blob(200 + n, 4096)).collect();
    let calls: Vec<_> = blobs
        .iter()
        .map(|b| spawn_store(&cache, vec![b.clone()]))
        .collect();
    for call in calls {
        let err = call.await.expect("join").unwrap_err();
        assert!(matches!(err, CacheError::Meta(_)), "{err:?}");
        assert_eq!(Status::from(err).code(), Code::Unavailable);
    }
    for b in &blobs {
        assert_eq!(answer(&cache, b.digest()).await, BlobAnswer::Absent);
    }
    let keys = store.put_keys();
    assert_eq!(keys.len(), 1);
    assert!(
        store.stored(&keys[0]).await.is_some(),
        "the orphan is stored"
    );
    assert_eq!(cache.meta().batches(), [4]);
    assert!(cache.meta().put_blobs().is_empty());
}

/// On a store with conditional writes, the first two PUTs are kept by the store but
/// answered as failed. The third succeeds: one `PutBlobs`, three different keys, and
/// the caller is answered after the linger and both backoffs.
///
/// Each of the three attempts is observed in `kbf_segment_put_seconds`.
///
/// Catches: a retry under the same key (it would fail `AlreadyExists` on the kept
/// first PUT); timing only the PUT that succeeded. Mutants seen red: allocate the
/// object id once, outside the retry loop; observe `segment_put` only on success.
#[tokio::test(start_paused = true)]
async fn a_put_retried_twice_takes_a_new_key_each_time() {
    let (cache, store) = default_cache().await;
    store.on_put(PutFault::LoseAnswer);
    store.on_put(PutFault::LoseAnswer);
    let b = blob(7, 4096);
    let start = Instant::now();
    cache.store_blobs(vec![b.clone()]).await.expect("store");
    let waited = WriteConfig::DEFAULT.linger + PUT_BACKOFF * (1 + PUT_BACKOFF_FACTOR);
    assert_eq!(start.elapsed(), waited);

    let keys = store.put_keys();
    assert_eq!(keys.len(), 3, "{keys:?}");
    assert!(keys[0] != keys[1] && keys[1] != keys[2] && keys[0] != keys[2]);
    for orphan in &keys[..2] {
        assert!(store.stored(orphan).await.is_some(), "{orphan}");
    }
    assert_eq!(cache.meta().batches(), [1]);
    assert_eq!(
        cache.metrics().segment_put.count(),
        3,
        "one per PUT attempt"
    );
    assert_eq!(key_of(&cache, b.digest()).await, keys[2]);
    let read = cache.read_blob(&b.digest()).await.expect("read");
    assert_eq!(kbf_segments::sha256(&read), b.digest());
    assert!(store.script_done());
    assert_audit_clean(&cache);
}

/// A PUT the store answers 5xx is tried again; a 4xx answer is not (it would only be
/// refused again), and fails the segment at once.
///
/// Catches: retrying every error, or none. Mutants seen red: `transient` always true;
/// the 5xx arm dropped; the range moved by one at either end.
#[tokio::test(start_paused = true)]
async fn only_a_put_that_did_not_reach_the_store_or_got_5xx_is_retried() {
    for (status, retried) in [
        (499, false),
        (500, true),
        (503, true),
        (599, true),
        (600, false),
        (403, false),
    ] {
        let (cache, store) = default_cache().await;
        store.on_put(PutFault::Status(status));
        let b = blob(8, 4096);
        let stored = cache.store_blobs(vec![b.clone()]).await;
        assert_eq!(stored.is_ok(), retried, "{status}: {stored:?}");
        assert_eq!(store.put_keys().len(), 1 + usize::from(retried), "{status}");
        assert_eq!(present(&cache, b.digest()).await, retried, "{status}");
    }
}

/// A lone write is sealed at the linger, not left waiting for company: under the
/// paused clock it returns exactly the linger after it started, with the default 2 ms
/// and with 50 ms.
///
/// Catches: waiting forever for a full segment (the timeout fires instead); sealing on
/// a timer other than the linger. Mutant seen red: never seal on the linger.
#[tokio::test(start_paused = true)]
async fn a_lone_write_is_sealed_at_the_linger() {
    for linger in [WriteConfig::DEFAULT.linger, Duration::from_millis(50)] {
        let (cache, store) = cache(
            config(|c| c.linger = linger),
            kbf_segments::MAX_SEGMENT_BYTES,
        )
        .await;
        let b = blob(9, 4096);
        let start = Instant::now();
        tokio::time::timeout(Duration::from_secs(60), cache.store_blobs(vec![b.clone()]))
            .await
            .expect("sealed before the timeout")
            .expect("store");
        assert_eq!(start.elapsed(), linger);
        assert_eq!(store.put_keys().len(), 1);
        assert!(present(&cache, b.digest()).await);
    }
}

/// A segment sealed because it filled up does not take the next segment with it when
/// its own linger timer fires. With a 1 s linger and room for one 4 KiB blob: `a` opens
/// segment 0 at t = 0; `b`, stored at t = 0.5 s, does not fit, so segment 0 is sealed
/// and `b` opens segment 1. At t = 1 s (segment 0's deadline) segment 1 is still open:
/// one PUT only. `b` returns exactly one linger after it was stored, at t = 1.5 s.
///
/// Catches: a linger timer that seals whichever segment is open, so a segment can be
/// sealed before its oldest blob has waited the linger. Mutant seen red: the timer
/// takes the open segment without checking its id (`open.take()` for
/// `open.take_if(|open| open.id == id)`).
#[tokio::test(start_paused = true)]
async fn a_full_segments_linger_does_not_seal_the_next_segment() {
    let linger = Duration::from_secs(1);
    let (cache, store) = cache(config(|c| c.linger = linger), one_4k_blob()).await;
    let (a, b) = (blob(18, 4096), blob(19, 4096));
    let start = Instant::now();
    let first = spawn_store(&cache, vec![a.clone()]);
    tokio::time::sleep(linger / 2).await;
    let second = spawn_store(&cache, vec![b.clone()]);
    first.await.expect("join").expect("a");
    assert_eq!(
        start.elapsed(),
        linger / 2,
        "a's full segment is written at once"
    );
    assert_eq!(store.put_keys().len(), 1);

    tokio::time::sleep_until(start + linger + Duration::from_millis(1)).await;
    assert!(!second.is_finished(), "b sealed at a's deadline");
    assert_eq!(store.put_keys().len(), 1);
    second.await.expect("join").expect("b");
    assert_eq!(start.elapsed(), linger + linger / 2);
    assert_ne!(
        key_of(&cache, a.digest()).await,
        key_of(&cache, b.digest()).await
    );
    assert_audit_clean(&cache);
}

/// With a 1 s linger and own segments from 64 KiB, a small write opens a segment and
/// waits; a 64 KiB blob stored after it returns at once, in an object of its own, while
/// the small write is still waiting; a blob one byte smaller joins the open segment and
/// returns with it at the linger.
///
/// Catches: large blobs held up by small ones. Mutants seen red: `>` for `>=` at the
/// threshold; no own-segment path.
#[tokio::test(start_paused = true)]
async fn a_blob_at_the_own_segment_size_skips_the_linger() {
    let own = 64 << 10;
    let linger = Duration::from_secs(1);
    let (cache, store) = cache(
        config(|c| {
            c.linger = linger;
            c.own_segment_bytes = own;
        }),
        kbf_segments::MAX_SEGMENT_BYTES,
    )
    .await;
    let small = blob(10, 4096);
    let under = blob(11, own as usize - 1);
    let large = blob(12, own as usize);
    let start = Instant::now();
    let waiting = spawn_store(&cache, vec![small.clone()]);
    let joining = spawn_store(&cache, vec![under.clone()]);
    cache.store_blobs(vec![large.clone()]).await.expect("store");
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert!(!waiting.is_finished() && !joining.is_finished());
    let large_key = key_of(&cache, large.digest()).await;
    assert_eq!(records_in(&store, &large_key).await, 1);

    waiting.await.expect("join").expect("small");
    joining.await.expect("join").expect("under");
    assert_eq!(start.elapsed(), linger);
    let shared = key_of(&cache, small.digest()).await;
    assert_eq!(key_of(&cache, under.digest()).await, shared);
    assert_ne!(shared, large_key);
    assert_eq!(store.put_keys().len(), 2);
    assert_audit_clean(&cache);
}

/// The same digest from two callers (and twice from one of them): one record in one
/// segment, one `PutBlobs` naming it once, and every caller answered OK.
///
/// Catches: duplicate records or entries; a caller whose push was a no-op never
/// answered. Mutant seen red: drop the waiter of a no-op push.
#[tokio::test(start_paused = true)]
async fn the_same_digest_from_two_callers_is_one_record() {
    let (cache, store) = default_cache().await;
    let b = blob(13, 4096);
    let twice = spawn_store(&cache, vec![b.clone(), b.clone()]);
    let once = spawn_store(&cache, vec![b.clone()]);
    twice.await.expect("join").expect("twice");
    once.await.expect("join").expect("once");
    assert_eq!(store.put_keys().len(), 1);
    assert_eq!(cache.meta().batches(), [1]);
    assert_eq!(
        records_in(&store, &key_of(&cache, b.digest()).await).await,
        1
    );
    assert_audit_clean(&cache);
}

/// A blob the cache already holds is answered at once and joins no segment. `b` is
/// stored; then a caller stores `b` again and `c` in one call: only `c` is written, in
/// a segment of one record, and the second call is answered `Ok` for both. A blob
/// stored twice in a row takes the same path.
///
/// Catches: a blob already stored written again (a second PUT and a second record, one
/// more byte of storage for every re-upload), and a held blob left out of the answer
/// (the call hangs or fails). Mutant seen red: `held` ignored in `submit` (the second
/// call PUTs a segment of two records). This is also the one test that takes the
/// held arm in this file's cache type, which the coverage ratchet counts per type.
#[tokio::test(start_paused = true)]
async fn a_blob_already_stored_is_answered_and_not_written_again() {
    let (cache, store) = default_cache().await;
    let (b, c) = (blob(30, 4096), blob(31, 4096));
    cache.store_blobs(vec![b.clone()]).await.expect("first");
    assert_eq!(store.put_keys().len(), 1);

    cache
        .store_blobs(vec![b.clone(), c.clone()])
        .await
        .expect("second");
    assert_eq!(store.put_keys().len(), 2, "PUTs: only `c` is new");
    assert_eq!(cache.meta().batches(), [1, 1], "PutBlobs commits");
    assert_eq!(
        records_in(&store, &key_of(&cache, c.digest()).await).await,
        1
    );
    assert_ne!(
        key_of(&cache, b.digest()).await,
        key_of(&cache, c.digest()).await
    );

    cache.store_blobs(vec![b.clone()]).await.expect("third");
    assert_eq!(store.put_keys().len(), 2, "PUTs: nothing was new");
    assert_eq!(cache.meta().batches(), [1, 1]);
    assert_audit_clean(&cache);
}

/// With two PUT slots, six segments sealed while every PUT is held: exactly two PUTs
/// reach the store; once they are let go every caller succeeds, and no more than two
/// were ever in flight.
///
/// Catches: unbounded concurrent PUTs. Mutant seen red: no PUT slot.
#[tokio::test(start_paused = true)]
async fn segment_puts_are_bounded_by_the_puts_in_flight() {
    let two = NonZeroUsize::new(2).expect("two");
    let (cache, store) = cache(config(|c| c.puts_in_flight = two), one_4k_blob()).await;
    store.hold_puts();
    let calls: Vec<_> = (0..6)
        .map(|n| spawn_store(&cache, vec![blob(300 + n, 4096)]))
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(store.puts_in_flight(), 2);
    store.release_puts();
    for call in calls {
        call.await.expect("join").expect("store");
    }
    assert_eq!(store.puts_in_flight_peak(), 2);
    assert_eq!(store.put_keys().len(), 6);
}

/// A caller that goes away while its blob waits in the open segment (the linger has
/// not fired) changes nothing: at the linger the segment is still written, and the
/// blob indexed.
///
/// Catches: a segment written only while a caller waits on it. Mutant seen red: skip
/// the write when every waiter is gone.
#[tokio::test(start_paused = true)]
async fn a_caller_that_goes_away_before_the_seal_changes_nothing() {
    let linger = Duration::from_secs(1);
    let (cache, store) = cache(
        config(|c| c.linger = linger),
        kbf_segments::MAX_SEGMENT_BYTES,
    )
    .await;
    let b = blob(17, 4096);
    let call = spawn_store(&cache, vec![b.clone()]);
    // The caller runs until it waits on its segment's answer: nothing it awaits first
    // (the index query) is pending.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(!call.is_finished());
    assert!(store.put_keys().is_empty(), "sealed before the linger");
    call.abort();
    assert!(call.await.expect_err("aborted").is_cancelled());

    tokio::time::sleep(linger * 2).await;
    assert_eq!(store.put_keys().len(), 1);
    assert!(present(&cache, b.digest()).await);
    assert_audit_clean(&cache);
}

/// A caller that goes away while its segment's PUT is held changes nothing either, and
/// its admission is held until the segment is settled: with an 8 KiB bound, a later
/// 1-byte admission waits while the PUT is held, and gets in once it is let go.
///
/// Catches: admission released when the caller goes rather than when its bytes are
/// written. Mutant seen red: the admissions not carried with the segment.
#[tokio::test(start_paused = true)]
async fn a_caller_that_goes_away_keeps_its_bytes_admitted_until_they_are_written() {
    let (cache, store) = cache(
        config(|c| c.bytes_in_flight = 8 << 10),
        kbf_segments::MAX_SEGMENT_BYTES,
    )
    .await;
    store.hold_puts();
    let b = blob(14, 8 << 10);
    let call = {
        let cache = Arc::clone(&cache);
        let b = b.clone();
        tokio::spawn(async move {
            let admitted = cache.admit(8 << 10).await;
            cache.store_admitted(vec![b], admitted).await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(store.puts_in_flight(), 1, "the segment's PUT is held");
    call.abort();
    assert!(call.await.expect_err("aborted").is_cancelled());

    let more = tokio::time::timeout(Duration::from_secs(1), cache.admit(1)).await;
    assert!(
        more.is_err(),
        "admitted while the bytes are still unwritten"
    );
    store.release_puts();
    tokio::time::timeout(Duration::from_secs(1), cache.admit(1))
        .await
        .expect("admitted once the segment is settled");
    assert!(present(&cache, b.digest()).await);
    assert_audit_clean(&cache);
}

/// One admission of the largest upload (a 5 GiB blob and its footer, over the
/// `u32::MAX` permits a semaphore hands out at once) is counted in full, and one asking
/// for more than the bound is counted as the bound: what `kbf-server`'s check of
/// `--segment-upload-max-bytes` against `--write-bytes-in-flight` relies on, so no
/// `Begin` holds more than it is counted for. Takes no memory: admission counts bytes,
/// it does not allocate them. [`MAX_ADMISSION_BYTES`] is what the semaphore holds.
///
/// Catches: an admission clamped to `u32::MAX` (a 5 GiB upload counted as 4 GiB).
#[tokio::test]
async fn one_admission_counts_all_its_bytes() {
    assert_eq!(
        MAX_ADMISSION_BYTES,
        tokio::sync::Semaphore::MAX_PERMITS as u64
    );
    let (cache, _) = cache(
        config(|c| c.bytes_in_flight = 16 << 30),
        kbf_segments::MAX_SEGMENT_BYTES,
    )
    .await;
    let full = cache.admit(MIN_SEGMENT_UPLOAD_BYTES).await;
    assert_eq!(cache.bytes_admitted(), MIN_SEGMENT_UPLOAD_BYTES);
    drop(full);
    let over = cache.admit(20 << 30).await;
    assert_eq!(cache.bytes_admitted(), 16 << 30);
    drop(over);
    assert_eq!(cache.bytes_admitted(), 0);
}

/// Catches two admissions taken in parts each holding a part the other waits for
/// (mutant: no lock around an admission over `u32::MAX`): under an 8 GiB bound, two
/// admissions of the largest upload asked at once would each take 4 GiB - 1 and wait
/// forever for the rest; both are admitted, one after the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_large_admissions_do_not_wait_on_each_other() {
    let (cache, _) = cache(
        config(|c| c.bytes_in_flight = 8 << 30),
        kbf_segments::MAX_SEGMENT_BYTES,
    )
    .await;
    let cache = Arc::new(cache);
    let both = (0..2).map(|_| {
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            let admitted = cache.admit(MIN_SEGMENT_UPLOAD_BYTES).await;
            tokio::task::yield_now().await;
            drop(admitted);
        })
    });
    let joined = tokio::time::timeout(Duration::from_secs(30), futures::future::join_all(both))
        .await
        .expect("both admitted");
    assert!(joined.iter().all(Result::is_ok));
    assert_eq!(cache.bytes_admitted(), 0);
}

/// A `BatchUpdateBlobs` waits for admission of its total before it stores anything:
/// with all but one byte less than its 4 KiB total of the 8 KiB bound held elsewhere,
/// it does not finish, and once the bytes are released it stores its blob.
///
/// Catches: a batch stored past the bound, and a batch admitted for less than its
/// total (any amount up to the 4095 bytes left free would go through). Mutants seen
/// red: no admission in BatchUpdateBlobs; `admit(1)` in place of `admit(total)`.
#[tokio::test(start_paused = true)]
async fn a_batch_waits_for_admission_of_its_total() {
    const BOUND: u64 = 8 << 10;
    const TOTAL: u64 = 4096;
    let (cache, _store) = cache(
        config(|c| c.bytes_in_flight = BOUND),
        kbf_segments::MAX_SEGMENT_BYTES,
    )
    .await;
    // Leaves TOTAL - 1 bytes free: enough for any admission smaller than the total.
    let held = cache.admit(BOUND - TOTAL + 1).await;
    let b = blob(15, TOTAL as usize);
    let call = {
        let service = CasService::new(Arc::clone(&cache));
        let request = BatchUpdateBlobsRequest {
            requests: vec![batch_update_blobs_request::Request {
                digest: Some(kbf_front::digest_to_proto(&b.digest())),
                data: bytes(15, TOTAL as usize),
                ..Default::default()
            }],
            ..Default::default()
        };
        tokio::spawn(async move { service.batch_update_blobs(Request::new(request)).await })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!call.is_finished(), "stored past the bound");
    assert!(!present(&cache, b.digest()).await);
    drop(held);
    let response = call.await.expect("join").expect("call").into_inner();
    assert_eq!(
        response.responses[0].status.as_ref().expect("status").code,
        0
    );
    assert!(present(&cache, b.digest()).await);
}

/// A segment write that panics answers its callers INTERNAL instead of leaving them
/// waiting forever, and indexes nothing.
///
/// Catches: a caller left hanging on a reply channel whose sender died.
#[tokio::test(start_paused = true)]
async fn a_segment_write_that_panics_answers_internal() {
    let (cache, store) = default_cache().await;
    store.on_put(PutFault::Panic);
    let b = blob(16, 4096);
    let stored = tokio::time::timeout(Duration::from_secs(60), cache.store_blobs(vec![b.clone()]))
        .await
        .expect("answered");
    assert!(matches!(stored, Err(CacheError::Internal(_))), "{stored:?}");
    assert_eq!(answer(&cache, b.digest()).await, BlobAnswer::Absent);
}

/// 15,000 callers each store one small blob at once (1 to 8 KiB, about 67 MB in all,
/// the shape of one toolchain output) into 4 MiB segments. Segments are filled in
/// turn, so there are about ceil(bytes / room per segment) objects and as many
/// `PutBlobs`, never more than the PUT slots in flight, and every blob reads back.
///
/// Catches: per-blob PUTs and commits under load; segments sealed half-empty; a blob
/// lost between segments. Mutant seen red: seal on every push.
#[tokio::test(start_paused = true)]
async fn fifteen_thousand_concurrent_writes_fill_segments_in_turn() {
    let limit: u64 = 4 << 20;
    let (cache, store) = cache(WriteConfig::DEFAULT, limit).await;
    let blobs: Vec<VerifiedBlob> = (0..15_000u64)
        .map(|n| blob(10_000 + n, 1024 + (n as usize * 7919) % (7 << 10)))
        .collect();
    let calls: Vec<_> = blobs
        .iter()
        .map(|b| spawn_store(&cache, vec![b.clone()]))
        .collect();
    for call in calls {
        call.await.expect("join").expect("store");
    }

    // Each segment holds at most `room` bytes of records and index entries; segments
    // are filled in turn, so each but the last is short of full by less than the
    // largest record.
    let room = limit - footer_len(0);
    let bytes: u64 = blobs
        .iter()
        .map(|b| b.digest().size_bytes + ENTRY_LEN as u64)
        .sum();
    let largest = blobs
        .iter()
        .map(|b| b.digest().size_bytes)
        .max()
        .expect("blobs");
    let fewest = bytes.div_ceil(room);
    let most = bytes / (room - largest - ENTRY_LEN as u64) + 1;
    assert!(most <= fewest + 1, "{fewest} to {most}");
    let objects = store.put_keys().len() as u64;
    assert!(
        (fewest..=most).contains(&objects),
        "{objects} objects for {bytes} bytes: {fewest} to {most}"
    );
    let batches = cache.meta().batches();
    assert_eq!(batches.len() as u64, objects, "one PutBlobs per object");
    assert_eq!(batches.iter().sum::<usize>(), blobs.len());
    assert!(store.puts_in_flight_peak() <= WriteConfig::DEFAULT.puts_in_flight.get());

    let digests: Vec<Digest> = blobs.iter().map(VerifiedBlob::digest).collect();
    let read = cache.read_blobs(&digests).await.expect("read");
    assert_eq!(read.len(), digests.len());
    for (digest, bytes) in read {
        let bytes = bytes.expect("readable");
        assert_eq!(kbf_segments::sha256(&bytes), digest);
    }
    assert_audit_clean(&cache);
}

/// The defaults are the design's.
#[test]
fn the_defaults_are_the_designs() {
    let d = WriteConfig::default();
    assert_eq!(d, WriteConfig::DEFAULT);
    assert_eq!(d.linger, Duration::from_millis(2));
    assert_eq!(d.own_segment_bytes, 16 << 20);
    assert_eq!(d.bytes_in_flight, 2 << 30);
    assert_eq!(d.puts_in_flight, NonZeroUsize::new(4).expect("4"));
    assert_eq!(d.put_attempts, NonZeroU32::new(3).expect("3"));
    assert_eq!(PUT_BACKOFF, Duration::from_millis(100));
    assert_eq!(PUT_BACKOFF * PUT_BACKOFF_FACTOR, Duration::from_millis(400));
}
