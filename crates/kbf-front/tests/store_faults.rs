//! The cache over an object store that fails: the index never runs ahead of the store,
//! and a read that could not reach the store is never taken for a lost object.
//!
//! Every test runs the cache over a [`FaultyStore`] and an [`AuditLog`], which checks,
//! as each `PutBlob` commits, that the store already holds the blob at that location.
//! Each test's comment names the defect in the cache it is there to catch.

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::faulty::{AuditLog, FaultyStore, GetFault, PutFault};
use common::pseudo_random;
use kbf_front::{Cache, CacheError, MetaLog, VerifiedBlob};
use kbf_meta::BlobAnswer;
use kbf_objstore::{Capabilities, KeyPrefix};
use kbf_segments::{MAX_SEGMENT_BYTES, SegmentWriter};
use kbf_types::Digest;
use tokio::sync::oneshot;
use tonic::{Code, Status};

type FaultyCache = Cache<AuditLog, FaultyStore>;

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// A cache over a fresh faulty bucket, and a handle on that bucket.
fn cache(capabilities: Capabilities) -> (Arc<FaultyCache>, FaultyStore) {
    let store = FaultyStore::new(capabilities);
    let cache = Cache::new(
        AuditLog::new(store.clone()),
        store.clone(),
        KeyPrefix::default(),
    );
    (Arc::new(cache), store)
}

/// 4 KiB of bytes for `seed`, and their digest.
struct TestBlob {
    bytes: Bytes,
    digest: Digest,
}

impl TestBlob {
    fn new(seed: u64) -> Self {
        let bytes = Bytes::from(pseudo_random(4096, seed));
        let digest = kbf_segments::sha256(&bytes);
        Self { bytes, digest }
    }

    fn verified(&self) -> VerifiedBlob {
        VerifiedBlob::new(self.digest, self.bytes.clone()).expect("verified")
    }
}

/// A whole segment holding `blobs` in this order, as the cache would write one.
fn segment_of(blobs: &[&TestBlob]) -> Bytes {
    let mut writer = SegmentWriter::new(MAX_SEGMENT_BYTES);
    for b in blobs {
        writer.push(&b.bytes).expect("push");
    }
    Bytes::from(writer.finish())
}

async fn answer(cache: &FaultyCache, digest: Digest) -> BlobAnswer {
    cache
        .meta()
        .query(move |s| s.blob(&digest))
        .await
        .expect("query")
}

/// Asserts the index holds nothing for `b` and every read path says so.
async fn assert_absent(cache: &FaultyCache, b: &TestBlob) {
    assert_eq!(answer(cache, b.digest).await, BlobAnswer::Absent);
    assert_eq!(
        cache.find_missing(&[b.digest]).await.expect("find"),
        vec![b.digest]
    );
    assert!(!cache.is_durable(&b.digest).await.expect("durable"));
    assert!(matches!(
        cache.read_blob(&b.digest).await,
        Err(CacheError::NotFound(_))
    ));
}

/// Asserts no `PutBlob` committed ahead of its bytes.
fn assert_audit_clean(cache: &FaultyCache) {
    assert_eq!(cache.meta().violations(), Vec::<String>::new());
}

/// A PUT the store refuses, and a footer read-back that does not reach the store,
/// commit nothing: the upload fails UNAVAILABLE and the blob stays absent. Then a
/// retry stores it.
///
/// Catches: committing `PutBlob` before (or regardless of) the PUT and the footer read.
#[tokio::test]
async fn a_failed_put_or_footer_read_commits_nothing() {
    let (cache, store) = cache(Capabilities::default());
    let b = TestBlob::new(1);

    store.on_put(PutFault::Fail);
    let err = cache.store_blobs(vec![b.verified()]).await.unwrap_err();
    assert!(matches!(err, CacheError::Store(_)), "{err:?}");
    assert_eq!(Status::from(err).code(), Code::Unavailable);
    assert_absent(&cache, &b).await;

    // The PUT lands; the first GET of the footer read-back fails.
    store.on_get(GetFault::Transport);
    let err = cache.store_blobs(vec![b.verified()]).await.unwrap_err();
    assert_eq!(Status::from(err).code(), Code::Unavailable);
    assert_absent(&cache, &b).await;
    assert!(store.script_done());

    assert!(cache.meta().put_blobs().is_empty());
    cache.store_blobs(vec![b.verified()]).await.expect("retry");
    assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    assert_eq!(cache.meta().put_blobs(), vec![b.digest]);
    assert_audit_clean(&cache);
}

/// A PUT the store committed but whose answer was lost commits nothing, and a retry on
/// a store with conditional writes goes to a new key rather than failing
/// AlreadyExists on the orphan.
///
/// Catches: committing `PutBlob` before the PUT answers; reusing an object id on retry.
#[tokio::test]
async fn a_lost_put_answer_commits_nothing_and_the_retry_takes_a_new_key() {
    let (cache, store) = cache(CONDITIONAL);
    let b = TestBlob::new(2);

    store.on_put(PutFault::LoseAnswer);
    let err = cache.store_blobs(vec![b.verified()]).await.unwrap_err();
    assert_eq!(Status::from(err).code(), Code::Unavailable);
    assert_absent(&cache, &b).await;
    let orphan = store.put_keys()[0].clone();
    assert!(store.stored(&orphan).await.is_some(), "the lost PUT stored");

    cache.store_blobs(vec![b.verified()]).await.expect("retry");
    let keys = store.put_keys();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1], "the retry reused the orphan's key");
    assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    assert_audit_clean(&cache);
}

/// While a PUT is in flight the blob is not reported: FindMissing lists it, it is not
/// durable, a read is NOT_FOUND. Only once the PUT answers does the upload return and
/// the blob appear.
///
/// Catches: committing `PutBlob` before the PUT completes (the in-flight window is
/// where a client would be told the blob exists while the store has no bytes).
#[tokio::test]
async fn a_blob_is_not_reported_while_its_put_is_in_flight() {
    let (cache, store) = cache(Capabilities::default());
    let b = TestBlob::new(3);
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    store.on_put(PutFault::Hold {
        entered: entered_tx,
        release: release_rx,
    });

    let upload = tokio::spawn({
        let cache = Arc::clone(&cache);
        let v = b.verified();
        async move { cache.store_blobs(vec![v]).await }
    });
    entered.await.expect("the PUT arrives");
    assert_absent(&cache, &b).await;
    assert!(!upload.is_finished(), "the upload returned before its PUT");

    release.send(()).expect("release");
    upload.await.expect("join").expect("upload");
    assert!(matches!(
        answer(&cache, b.digest).await,
        BlobAnswer::Present(_)
    ));
    assert_eq!(cache.find_missing(&[b.digest]).await.expect("find"), vec![]);
    assert_audit_clean(&cache);
}

/// A store that answers OK but holds a truncated object, no object, or some other
/// segment: the footer read back from the store does not name the blob, so nothing is
/// committed and the upload fails.
///
/// Catches: skipping the footer read-back and committing the offsets the writer chose
/// (the PUT's OK is then the only evidence, and these stores lie in it).
#[tokio::test]
async fn a_put_that_stores_other_bytes_is_never_committed() {
    let other = TestBlob::new(40);
    let lies = [
        ("truncated by one byte", PutFault::Truncate(1)),
        ("missing its footer", PutFault::Truncate(64)),
        ("not stored at all", PutFault::Drop),
        ("another segment", PutFault::Replace(segment_of(&[&other]))),
    ];
    for (i, (what, lie)) in lies.into_iter().enumerate() {
        let (cache, store) = cache(Capabilities::default());
        let b = TestBlob::new(41 + i as u64);
        store.on_put(lie);
        let err = cache.store_blobs(vec![b.verified()]).await.unwrap_err();
        assert_ne!(Status::from(err).code(), Code::Ok, "{what}");
        assert_absent(&cache, &b).await;
        assert!(cache.meta().put_blobs().is_empty(), "{what}: committed");
        assert_audit_clean(&cache);
    }
}

/// The index records where the stored footer says a blob is, not where the writer put
/// it. The cache sends a segment of `x` then `b`; the store holds a segment of the same
/// length with the two records the other way round, and both blobs are read at the
/// offsets its footer gives.
///
/// Catches: taking offsets from the writer instead of the footer read back (each read
/// then lands on the other blob's bytes and fails its digest).
#[tokio::test]
async fn offsets_come_from_the_footer_the_store_holds() {
    let (cache, store) = cache(Capabilities::default());
    let x = TestBlob::new(5);
    let b = TestBlob::new(50);
    let sent = segment_of(&[&x, &b]);
    let swapped = segment_of(&[&b, &x]);
    assert_eq!(sent.len(), swapped.len());
    assert_ne!(sent, swapped);
    store.on_put(PutFault::Replace(swapped));

    cache
        .store_blobs(vec![x.verified(), b.verified()])
        .await
        .expect("upload");
    for t in [&x, &b] {
        assert_eq!(cache.read_blob(&t.digest).await.expect("read"), t.bytes);
    }
    assert_audit_clean(&cache);
}

/// A read that does not reach the store (a reset connection, a 503) fails UNAVAILABLE
/// and leaves the blob present: no unreachable mark, FindMissing does not list it, and
/// the next read serves it.
///
/// Catches: marking the object unreachable on every store error (a network blip would
/// then report a held blob missing and make clients upload it again).
#[tokio::test]
async fn a_read_that_does_not_reach_the_store_marks_nothing() {
    for blip in [GetFault::Transport, GetFault::SlowDown] {
        let (cache, store) = cache(Capabilities::default());
        let b = TestBlob::new(6);
        cache.store_blobs(vec![b.verified()]).await.expect("upload");

        store.on_get(blip);
        let err = cache.read_blob(&b.digest).await.unwrap_err();
        assert!(matches!(err, CacheError::Store(_)), "{blip:?}: {err:?}");
        assert_eq!(Status::from(err).code(), Code::Unavailable, "{blip:?}");
        assert!(store.script_done());

        assert!(
            matches!(answer(&cache, b.digest).await, BlobAnswer::Present(_)),
            "{blip:?}"
        );
        assert_eq!(
            cache.find_missing(&[b.digest]).await.expect("find"),
            vec![],
            "{blip:?}"
        );
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
}

/// A store that says the object is gone (NotFound) or shorter than the blob's range
/// (InvalidRange) does get the object marked unreachable: the blob is UNAVAILABLE,
/// never NOT_FOUND, and FindMissing lists it so a client uploads it again.
///
/// Catches: the opposite over-correction, never marking anything, which would keep
/// answering for a blob whose bytes are lost.
#[tokio::test]
async fn a_store_that_says_the_object_is_gone_marks_it_unreachable() {
    for gone in [GetFault::NotFound, GetFault::InvalidRange] {
        let (cache, store) = cache(Capabilities::default());
        let b = TestBlob::new(7);
        cache.store_blobs(vec![b.verified()]).await.expect("upload");

        store.on_get(gone);
        let err = cache.read_blob(&b.digest).await.unwrap_err();
        assert!(
            matches!(err, CacheError::Unreachable(_)),
            "{gone:?}: {err:?}"
        );
        assert_eq!(Status::from(err).code(), Code::Unavailable, "{gone:?}");
        assert_eq!(answer(&cache, b.digest).await, BlobAnswer::Unavailable);
        assert_eq!(
            cache.find_missing(&[b.digest]).await.expect("find"),
            vec![b.digest],
            "{gone:?}"
        );
    }
}
