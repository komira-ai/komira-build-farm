//! BatchReadBlobs against a store whose GETs take time: the batch's blobs are read
//! concurrently (bounded), blobs that lie back to back in one segment come from one
//! range GET, and every failure of such a shared GET still answers each blob alone.
//!
//! The service is called in process (no transport) on a paused tokio clock, so a GET's
//! 10 ms is virtual time and the timings below are exact, not measured.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::{Cache, CasService, MemoryMetaLog, MetaLog, MetaLogError, VerifiedBlob};
use kbf_meta::{Applied, Command, Location, MetaState, ObjectId, Retention, StoreId};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorage;
use kbf_proto::reapi::{self, BatchReadBlobsRequest, FindMissingBlobsRequest};
use kbf_types::FarmTime;
use tonic::{Code, Request};

/// How long each GET takes once [`Store::slow`] is set, as the design's test C-15 says.
const GET_TIME: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
struct Script {
    /// GETs made.
    gets: usize,
    /// GETs in flight now, and the most seen at once.
    in_flight: usize,
    most_in_flight: usize,
    /// The next this many GETs fail as a reset connection would.
    fail_next: usize,
    /// Every GET that covers this offset (of any object) answers it with its bits
    /// flipped.
    flip: Option<u64>,
}

/// A memory bucket that counts its GETs, can make each take [`GET_TIME`], and can
/// fail or corrupt them on cue. Clones share one bucket and one script.
#[derive(Clone)]
struct Store {
    inner: Arc<MemoryStore>,
    slow: Arc<AtomicBool>,
    script: Arc<Mutex<Script>>,
}

impl Store {
    fn new() -> Self {
        Self {
            inner: Arc::new(MemoryStore::new(Capabilities::default())),
            slow: Arc::default(),
            script: Arc::default(),
        }
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn gets(&self) -> usize {
        self.script().gets
    }
}

impl ObjectStore for Store {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let (fail, flip) = {
            let mut s = self.script();
            s.gets += 1;
            s.in_flight += 1;
            s.most_in_flight = s.most_in_flight.max(s.in_flight);
            let fail = s.fail_next > 0;
            s.fail_next = s.fail_next.saturating_sub(1);
            (fail, s.flip)
        };
        if self.slow.load(Ordering::Relaxed) {
            tokio::time::sleep(GET_TIME).await;
        }
        self.script().in_flight -= 1;
        if fail {
            let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "test");
            return Err(ObjectStoreError::Transport(Arc::new(reset)));
        }
        let bytes = self.inner.get_range(key, range).await?;
        match flip.and_then(|at| at.checked_sub(range.offset())) {
            Some(at) if at < bytes.len() as u64 => {
                let mut flipped = bytes.to_vec();
                flipped[usize::try_from(at).expect("small")] ^= 0xff;
                Ok(Bytes::from(flipped))
            }
            _ => Ok(bytes),
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

/// A memory log whose queries can be made to fail, as when no leader answers.
#[derive(Clone)]
struct Log {
    inner: Arc<MemoryMetaLog>,
    down: Arc<AtomicBool>,
}

impl MetaLog for Log {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.inner.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        if self.down.load(Ordering::Relaxed) {
            return Err(MetaLogError::Unavailable("down for the test".to_owned()));
        }
        self.inner.query(f).await
    }
}

struct Farm {
    cache: Arc<Cache<Log, Store>>,
    store: Store,
    log: Log,
    cas: CasService<Log, Store>,
}

impl Farm {
    async fn new() -> Self {
        let store = Store::new();
        let log = Log {
            inner: Arc::new(MemoryMetaLog::new(Retention::default())),
            down: Arc::default(),
        };
        let cache = Cache::open(log.clone(), store.clone(), KeyPrefix::default())
            .await
            .expect("open");
        let cache = Arc::new(cache);
        let authorizers = Arc::new(Authorizers::allow_all());
        let cas = CasService::with_authorizers(Arc::clone(&cache), authorizers);
        Self {
            cache,
            store,
            log,
            cas,
        }
    }

    /// Stores `blobs` together, in one segment, back to back in this order.
    async fn store(&self, blobs: &[&Blob]) {
        let blobs = blobs.iter().map(|b| b.verified.clone()).collect();
        self.cache.store_blobs(blobs).await.expect("store");
    }

    /// Each answer of one BatchReadBlobs call for `digests`: its code and bytes.
    async fn batch_read(&self, digests: Vec<reapi::Digest>) -> Vec<(Code, Vec<u8>)> {
        let request = Request::new(BatchReadBlobsRequest {
            digests,
            ..BatchReadBlobsRequest::default()
        });
        let response = self.cas.batch_read_blobs(request).await.expect("call");
        response
            .into_inner()
            .responses
            .into_iter()
            .map(|r| (Code::from(r.status.expect("status").code), r.data))
            .collect()
    }

    async fn missing(&self, blobs: &[&Blob]) -> BTreeSet<String> {
        let request = Request::new(FindMissingBlobsRequest {
            blob_digests: blobs.iter().map(|b| proto(b)).collect(),
            ..FindMissingBlobsRequest::default()
        });
        let response = self.cas.find_missing_blobs(request).await.expect("call");
        let missing = response.into_inner().missing_blob_digests;
        missing.into_iter().map(|d| d.hash).collect()
    }
}

/// A blob and its bytes.
struct Blob {
    verified: VerifiedBlob,
    data: Vec<u8>,
}

impl Blob {
    fn size(&self) -> u64 {
        self.verified.digest().size_bytes
    }
}

fn blob(text: &str) -> Blob {
    let data = text.as_bytes().to_vec();
    let verified = VerifiedBlob::hashed(Bytes::from(data.clone()));
    Blob { verified, data }
}

fn proto(b: &Blob) -> reapi::Digest {
    kbf_front::digest_to_proto(&b.verified.digest())
}

/// What BatchReadBlobs answers for `b` when it is served.
fn ok(b: &Blob) -> (Code, Vec<u8>) {
    (Code::Ok, b.data.clone())
}

/// A hundred blobs, each in a segment of its own (so no two can share a GET), in a
/// farm whose GETs from now on take [`GET_TIME`] each.
async fn hundred_apart() -> (Farm, Vec<Blob>) {
    let farm = Farm::new().await;
    let blobs: Vec<Blob> = (0..100).map(|i| blob(&format!("blob {i}"))).collect();
    for b in &blobs {
        farm.store(&[b]).await;
    }
    farm.store.slow.store(true, Ordering::Relaxed);
    farm.store.script().most_in_flight = 0;
    (farm, blobs)
}

/// Test C-15 (`docs/design/worker-cache.md#10-test-plan`). Catches (mutant "revert to
/// the serial loop", `buffer_unordered(1)`): a batch read that waits for each blob's
/// GET before it starts the next, so a 100-blob batch costs 100 store round trips. At
/// 10 ms a GET that is 1,000 ms; 32 at a time it is 4 rounds, 40 ms.
#[tokio::test(start_paused = true)]
async fn a_hundred_blob_batch_read_is_not_a_hundred_gets_in_a_row() {
    let (farm, blobs) = hundred_apart().await;
    let gets = farm.store.gets();
    let started = tokio::time::Instant::now();
    let answers = farm.batch_read(blobs.iter().map(proto).collect()).await;
    let took = started.elapsed();
    let want: Vec<(Code, Vec<u8>)> = blobs.iter().map(ok).collect();
    assert_eq!(answers, want, "every blob answered, in request order");
    assert_eq!(farm.store.gets() - gets, 100, "one GET per blob");
    assert!(
        took < 100 * GET_TIME / 4,
        "a 100-blob batch took {took:?}; one GET after another takes {:?}",
        100 * GET_TIME
    );
}

/// Catches (mutant "unbounded", `buffer_unordered(usize::MAX)` or a `join_all`): one
/// BatchReadBlobs call opening a GET per blob at once, so a client's batch sets how
/// many requests the store sees; and (mutant "bound 1") no concurrency at all. The
/// design's bound is 32 (`docs/design/worker-cache.md#9-rollout`, stage S1).
#[tokio::test(start_paused = true)]
async fn a_batch_read_keeps_at_most_thirty_two_gets_in_flight() {
    let (farm, blobs) = hundred_apart().await;
    let answers = farm.batch_read(blobs.iter().map(proto).collect()).await;
    assert!(answers.iter().all(|(code, _)| *code == Code::Ok));
    assert_eq!(farm.store.script().most_in_flight, 32);
}

/// Catches (mutant "never extend a run"): blobs stored back to back in one segment
/// read with one GET each instead of one GET for the lot; a run that spans a record
/// the batch did not ask for (a gap must split it); and answers that lose request
/// order or a digest asked twice. The empty blob needs no GET, a blob never stored is
/// NOT_FOUND with no GET, and a digest that does not parse fails alone.
#[tokio::test]
async fn blobs_back_to_back_in_one_segment_are_read_with_one_get() {
    let farm = Farm::new().await;
    let (a, b, c) = (blob("first"), blob("second"), blob("third"));
    farm.store(&[&a, &b, &c]).await;
    let empty = blob("");
    let mut malformed = proto(&a);
    malformed.size_bytes = -1;

    let gets = farm.store.gets();
    let asked = vec![
        proto(&c),
        proto(&a),
        malformed,
        proto(&b),
        proto(&empty),
        proto(&a),
    ];
    let answers = farm.batch_read(asked).await;
    let invalid = (Code::InvalidArgument, Vec::new());
    let want = [
        ok(&c),
        ok(&a),
        invalid,
        ok(&b),
        (Code::Ok, Vec::new()),
        ok(&a),
    ];
    assert_eq!(answers, want);
    assert_eq!(farm.store.gets() - gets, 1, "three records back to back");

    let gets = farm.store.gets();
    let never = blob("never stored");
    let answers = farm
        .batch_read(vec![proto(&a), proto(&never), proto(&c)])
        .await;
    let not_found = (Code::NotFound, Vec::new());
    assert_eq!(answers, [ok(&a), not_found, ok(&c)]);
    assert_eq!(farm.store.gets() - gets, 2, "the second record is a gap");
}

/// Catches (mutant "fail every blob of a failed joined GET"): one failed GET for a
/// run of blobs failing them all, where reading each alone (as `read_blob` would)
/// still serves them.
#[tokio::test]
async fn a_failed_joined_get_reads_each_blob_alone() {
    let farm = Farm::new().await;
    let (a, b, c) = (blob("one"), blob("two"), blob("three"));
    farm.store(&[&a, &b, &c]).await;
    let gets = farm.store.gets();
    farm.store.script().fail_next = 1;
    let answers = farm.batch_read(vec![proto(&a), proto(&b), proto(&c)]).await;
    assert_eq!(answers, [ok(&a), ok(&b), ok(&c)]);
    assert_eq!(farm.store.gets() - gets, 4, "the joined GET, then one each");
}

/// Catches (mutant "no digest check on a record of a joined GET"): bytes damaged in
/// the store served for one blob of a run because the GET as a whole succeeded. The
/// damaged blob must fail UNAVAILABLE and mark its object unreachable (so all three
/// are then reported missing and get uploaded again), while its neighbours, whose
/// bytes check, are served. And (mutant "an unreachable blob answers NOT_FOUND"): the
/// next batch answers all three UNAVAILABLE without reading the store.
#[tokio::test]
async fn a_damaged_record_in_a_joined_get_fails_alone_and_marks_its_object() {
    let farm = Farm::new().await;
    let (a, b, c) = (blob("left"), blob("damaged"), blob("right"));
    farm.store(&[&a, &b, &c]).await;
    farm.store.script().flip = Some(a.size() + 1);
    let answers = farm.batch_read(vec![proto(&a), proto(&b), proto(&c)]).await;
    assert_eq!(answers, [ok(&a), (Code::Unavailable, Vec::new()), ok(&c)]);
    let all: BTreeSet<String> = [&a, &b, &c].iter().map(|x| proto(x).hash).collect();
    assert_eq!(farm.missing(&[&a, &b, &c]).await, all);

    // Once marked, every blob of the object answers UNAVAILABLE (never NOT_FOUND, which
    // a client would read as "never uploaded"), with no GET.
    let gets = farm.store.gets();
    let answers = farm.batch_read(vec![proto(&a), proto(&b), proto(&c)]).await;
    let unavailable = (Code::Unavailable, Vec::new());
    assert_eq!(
        answers,
        [unavailable.clone(), unavailable.clone(), unavailable]
    );
    assert_eq!(farm.store.gets(), gets);
}

/// Catches (mutant "no store check before a joined GET"): blobs indexed in a store
/// this cache does not have read from the configured store anyway. Each must fail
/// INTERNAL with no GET made, as `read_blob` fails one.
#[tokio::test]
async fn blobs_in_another_store_are_internal_and_never_read() {
    let farm = Farm::new().await;
    let (a, b) = (blob("elsewhere one"), blob("elsewhere two"));
    let object = ObjectId::of(farm.cache.writer(), 99);
    let mut offset = 0;
    for x in [&a, &b] {
        let location = Location {
            store: StoreId::new(1),
            object,
            offset,
        };
        offset += x.size();
        let digest = x.verified.digest();
        let put = Command::PutBlob { digest, location };
        let applied = farm.log.commit(put).await.expect("commit");
        assert!(matches!(applied, Applied::Blob(Ok(_))), "{applied:?}");
    }
    let gets = farm.store.gets();
    let answers = farm.batch_read(vec![proto(&a), proto(&b)]).await;
    let internal = (Code::Internal, Vec::new());
    assert_eq!(answers, [internal.clone(), internal]);
    assert_eq!(farm.store.gets(), gets);
}

/// Catches (mutant "`?` on the batch's metadata query"): a log that cannot answer
/// failing the whole call, or answering some blob as NOT_FOUND (which a client reads
/// as "upload it again"). Each blob answers UNAVAILABLE, as one read alone would.
#[tokio::test]
async fn a_batch_read_with_the_log_down_answers_each_blob_unavailable() {
    let farm = Farm::new().await;
    let (a, b) = (blob("stored a"), blob("stored b"));
    farm.store(&[&a, &b]).await;
    farm.log.down.store(true, Ordering::Relaxed);
    let answers = farm.batch_read(vec![proto(&a), proto(&b)]).await;
    let unavailable = (Code::Unavailable, Vec::new());
    assert_eq!(answers, [unavailable.clone(), unavailable]);
}

/// Farm time `n` days after the epoch.
fn days(n: u64) -> FarmTime {
    FarmTime::from_millis(n * 24 * 60 * 60 * 1000)
}

/// Catches (mutant "batch read without the touch", `touch_for(&[])`): blobs that are
/// only ever read through BatchReadBlobs (buck2 reads small inputs that way) not
/// counting as used, so they are collected at the end of their TTL while builds still
/// read them. A blob batch-read on day 5 outlives a collection on day 10; its unread
/// neighbour, stored the same day, does not.
#[tokio::test]
async fn each_batch_read_is_a_touch() {
    let farm = Farm::new().await;
    let (read, unread) = (blob("batch-read on day 5"), blob("never read"));
    farm.store(&[&read]).await;
    farm.store(&[&unread]).await;
    farm.cache.tick(days(5)).await.expect("tick");
    assert_eq!(farm.batch_read(vec![proto(&read)]).await, [ok(&read)]);
    farm.cache.tick(days(10)).await.expect("tick");
    farm.cache.collect().await.expect("collect");
    let (read, unread) = (read.verified.digest(), unread.verified.digest());
    let missing = farm
        .cache
        .find_missing(&[read, unread])
        .await
        .expect("find");
    assert_eq!(missing, [unread]);
}

/// Catches (`read[&d]` indexing the batch's answers): a digest that parsed but that
/// `read_blobs` gave no answer for panicking the call, which drops the client's
/// connection. That happens only if the index holds two digests at one location (a
/// corrupt log; the segment writer never makes one): the batch then keeps one record
/// per offset. The dropped digest answers INTERNAL; the call still answers every blob.
#[tokio::test]
async fn two_digests_indexed_at_one_location_do_not_panic_the_call() {
    let farm = Farm::new().await;
    let (a, b) = (blob("same place one"), blob("same place two"));
    let location = Location {
        store: StoreId::CONFIGURED,
        object: ObjectId::of(farm.cache.writer(), 99),
        offset: 0,
    };
    for x in [&a, &b] {
        let digest = x.verified.digest();
        let put = Command::PutBlob { digest, location };
        let applied = farm.log.commit(put).await.expect("commit");
        assert!(matches!(applied, Applied::Blob(Ok(_))), "{applied:?}");
    }
    // The batch keeps the record of the larger digest (it is inserted last).
    let (dropped, kept) = if a.verified.digest() < b.verified.digest() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    let answers = farm.batch_read(vec![proto(dropped), proto(kept)]).await;
    assert_eq!(answers[0], (Code::Internal, Vec::new()), "{answers:?}");
    assert_eq!(answers.len(), 2);
}
