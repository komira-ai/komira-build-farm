//! How the cache names, writes and marks its objects: keys from the writer epoch and
//! sequence, one index commit per segment, every object a segment, and unreachable
//! marks with the reason the read found.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use bytes::Bytes;
use kbf_front::{Cache, CacheError, MemoryMetaLog, MetaLog, MetaLogError, VerifiedBlob};
use kbf_meta::{
    Applied, BlobAnswer, Command, Epoch, Location, MetaState, ObjectId, Retention, StoreId,
    UnreachableReason,
};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_segments::SegmentReader;
use kbf_segments::layout::footer_len;
use kbf_types::Digest;

/// A bucket that refuses to replace a key, as `--s3-conditional-put` does, so a key
/// named twice fails the second write instead of hiding it.
fn conditional() -> SharedStore {
    SharedStore(Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    })))
}

/// One [`MetaLog`] shared by several caches, as two servers over one replicated log.
/// It counts the commands committed and keeps the `PutBlobs` batches.
#[derive(Clone)]
struct SharedLog(Arc<Counted>);

struct Counted {
    inner: MemoryMetaLog,
    commits: AtomicUsize,
    put_blob: AtomicUsize,
    batches: Mutex<Vec<usize>>,
}

impl SharedLog {
    fn new() -> Self {
        Self(Arc::new(Counted {
            inner: MemoryMetaLog::new(Retention::default()),
            commits: AtomicUsize::new(0),
            put_blob: AtomicUsize::new(0),
            batches: Mutex::new(Vec::new()),
        }))
    }

    fn log(&self) -> &MemoryMetaLog {
        &self.0.inner
    }

    fn batches(&self) -> Vec<usize> {
        self.0
            .batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl MetaLog for SharedLog {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.0.commits.fetch_add(1, Ordering::Relaxed);
        match &command {
            Command::PutBlob { .. } => {
                self.0.put_blob.fetch_add(1, Ordering::Relaxed);
            }
            Command::PutBlobs(blobs) => self
                .0
                .batches
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(blobs.len()),
            _ => {}
        }
        self.log().commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.log().query(f).await
    }
}

/// One bucket shared by several caches.
#[derive(Clone)]
struct SharedStore(Arc<MemoryStore>);

impl ObjectStore for SharedStore {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.list(prefix, after, max_keys).await
    }
}

fn blob(text: &str) -> VerifiedBlob {
    VerifiedBlob::hashed(Bytes::copy_from_slice(text.as_bytes()))
}

async fn keys<O: ObjectStore>(store: &O) -> Vec<String> {
    let prefix = KeyPrefix::default();
    let page = store
        .list(&prefix, None, PageSize::new(1000).expect("page size"))
        .await
        .expect("list");
    page.objects
        .iter()
        .map(|o| o.key.as_str().to_owned())
        .collect()
}

async fn location<M: MetaLog, O: ObjectStore>(cache: &Cache<M, O>, d: Digest) -> Location {
    match cache
        .meta()
        .query(move |s| s.blob(&d))
        .await
        .expect("query")
    {
        BlobAnswer::Present(location) => location,
        other => panic!("{d} is not present: {other:?}"),
    }
}

async fn mark_of<M: MetaLog, O: ObjectStore>(
    cache: &Cache<M, O>,
    object: ObjectId,
) -> Option<UnreachableReason> {
    cache
        .meta()
        .query(move |s| s.unreachable(object))
        .await
        .expect("query")
}

/// Catches: an object key that leaves out the epoch or the sequence, prints them in
/// another base or width, or drops the `cas/` part. The key form is what a rebuild
/// lists and parses, and what keeps two writers apart.
#[tokio::test]
async fn object_keys_are_the_epoch_and_sequence_in_sixteen_hex_digits() {
    let prefix = KeyPrefix::new("farm-a/").expect("prefix");
    let cache = Cache::new(SharedLog::new(), conditional(), prefix, Epoch::new(0x2a));
    let key = cache
        .object_key(ObjectId::new(Epoch::new(0x2a), 0xbeef))
        .expect("key");
    assert_eq!(key.as_str(), "farm-a/cas/000000000000002a/000000000000beef");
    let key = cache
        .object_key(ObjectId::new(Epoch::new(u64::MAX), 1))
        .expect("key");
    assert_eq!(key.as_str(), "farm-a/cas/ffffffffffffffff/0000000000000001");
}

/// Catches: two writers over one log and one bucket naming the same key. That happens
/// if `AllocEpoch` hands both the same epoch, or if the key drops the epoch, and with
/// conditional writes the second writer's upload then fails `AlreadyExists` (without
/// them it would overwrite the first writer's live segment). Each cache's objects must
/// sit under its own epoch, and every blob must read back through either cache.
#[tokio::test]
async fn two_caches_over_one_log_and_bucket_never_name_the_same_object() {
    let log = SharedLog::new();
    let store = conditional();
    let a = Cache::open(log.clone(), store.clone(), KeyPrefix::default())
        .await
        .expect("open a");
    let b = Cache::open(log.clone(), store.clone(), KeyPrefix::default())
        .await
        .expect("open b");
    let (first, second) = (a.epoch(), b.epoch());
    assert!(second > first, "{first:?} then {second:?}");

    let mut digests = Vec::new();
    for n in 0..3 {
        for (cache, name) in [(&a, "a"), (&b, "b")] {
            let blob = blob(&format!("{name} wrote blob {n}"));
            digests.push(blob.digest());
            cache.store_blobs(vec![blob]).await.expect("store");
        }
    }
    let keys = keys(&store).await;
    assert_eq!(keys.len(), 6, "{keys:?}");
    for (cache, count) in [(&a, 3), (&b, 3)] {
        let epoch = format!("cas/{:016x}/", cache.epoch().get());
        assert_eq!(
            keys.iter().filter(|k| k.starts_with(&epoch)).count(),
            count,
            "{keys:?}"
        );
    }
    for d in digests {
        assert_eq!(
            a.read_blob(&d).await.expect("read").len() as u64,
            d.size_bytes
        );
        assert_eq!(
            b.read_blob(&d).await.expect("read").len() as u64,
            d.size_bytes
        );
    }
}

/// Catches: a segment of N blobs committed as N log entries (one `PutBlob` each), which
/// a durable log turns into N fsyncs, or as a batch that leaves a blob out. A store of
/// five blobs into one segment is exactly one `PutBlobs` of five, and nothing else is
/// committed; a store that spans two segments is one batch per segment.
#[tokio::test]
async fn each_written_segment_is_one_put_blobs_commit() {
    let log = SharedLog::new();
    let cache = Cache::open(log.clone(), conditional(), KeyPrefix::default())
        .await
        .expect("open");
    let opened = log.0.commits.load(Ordering::Relaxed);
    let blobs: Vec<_> = (0..5).map(|n| blob(&format!("packed blob {n}"))).collect();
    cache.store_blobs(blobs.clone()).await.expect("store");
    assert_eq!(log.0.commits.load(Ordering::Relaxed) - opened, 1);
    assert_eq!(log.0.put_blob.load(Ordering::Relaxed), 0);
    assert_eq!(log.batches(), [5]);
    let objects: Vec<_> = {
        let mut seen = Vec::new();
        for b in &blobs {
            seen.push(location(&cache, b.digest()).await.object);
        }
        seen
    };
    assert!(objects.iter().all(|o| *o == objects[0]), "{objects:?}");

    // Room for two 20-byte records per segment: five blobs make three segments.
    let small = Cache::open(log.clone(), conditional(), KeyPrefix::default())
        .await
        .expect("open")
        .with_segment_limit(footer_len(2) + 40);
    let blobs: Vec<_> = (0..5)
        .map(|n| blob(&format!("twenty-byte blob #{n:02}")))
        .collect();
    assert!(blobs.iter().all(|b| b.digest().size_bytes == 20));
    small.store_blobs(blobs).await.expect("store");
    assert_eq!(log.batches(), [5, 2, 2, 1]);
}

/// Catches: a blob too large to share a segment stored as raw bytes with no footer (no
/// digest or CRC in the store, so nothing could rebuild its index entry), or at an
/// offset other than 0. It must be an object of its own that parses as a segment of one
/// record, and read back.
#[tokio::test]
async fn a_blob_too_large_to_share_a_segment_is_a_segment_of_one_record() {
    let store = conditional();
    let cache = Cache::open(SharedLog::new(), store.clone(), KeyPrefix::default())
        .await
        .expect("open")
        .with_segment_limit(footer_len(1) + 64);
    let large = VerifiedBlob::hashed(Bytes::from(vec![7u8; 300]));
    let small = blob("fits");
    cache
        .store_blobs(vec![small.clone(), large.clone()])
        .await
        .expect("store");

    let at = location(&cache, large.digest()).await;
    assert_eq!(at.offset, 0);
    assert_ne!(at.object, location(&cache, small.digest()).await.object);
    let key = cache.object_key(at.object).expect("key");
    let whole = store
        .get_range(&key, ByteRange::new(0, 1 << 20).expect("range"))
        .await
        .expect("get");
    let reader = SegmentReader::open(&whole).expect("a segment");
    assert_eq!(reader.footer().entries().len(), 1);
    assert_eq!(
        reader.get(&large.digest()).expect("valid").map(<[u8]>::len),
        Some(300)
    );
    let read = cache.read_blob(&large.digest()).await.expect("read");
    assert_eq!(read.as_ref(), &[7u8; 300][..]);
}

/// Catches: a read that marks every failure the same way. An object the store does not
/// have is `Missing` (it may come back, and a re-probe may clear it); bytes that fail
/// their digest are `Corrupt` (they must never be re-probed into service). Also catches
/// a mark on the wrong object: the neighbour in its own segment stays unmarked.
#[tokio::test]
async fn a_read_marks_a_missing_object_missing_and_bad_bytes_corrupt() {
    let store = conditional();
    let cache = Cache::open(SharedLog::new(), store.clone(), KeyPrefix::default())
        .await
        .expect("open");
    let (gone, bad, sound) = (blob("deleted"), blob("bit flipped"), blob("sound"));
    for b in [&gone, &bad, &sound] {
        cache.store_blobs(vec![b.clone()]).await.expect("store");
    }
    let (gone_at, bad_at, sound_at) = (
        location(&cache, gone.digest()).await,
        location(&cache, bad.digest()).await,
        location(&cache, sound.digest()).await,
    );

    store
        .delete(&cache.object_key(gone_at.object).expect("key"))
        .await
        .expect("delete");
    let key = cache.object_key(bad_at.object).expect("key");
    let range = ByteRange::new(0, 1 << 20).expect("range");
    let mut bytes = store.get_range(&key, range).await.expect("get").to_vec();
    bytes[usize::try_from(bad_at.offset).expect("offset")] ^= 1;
    store.delete(&key).await.expect("delete");
    store
        .put_new(&key, Bytes::from(bytes), None)
        .await
        .expect("put");

    for d in [gone.digest(), bad.digest()] {
        let read = cache.read_blob(&d).await;
        assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    }
    assert_eq!(
        mark_of(&cache, gone_at.object).await,
        Some(UnreachableReason::Missing)
    );
    assert_eq!(
        mark_of(&cache, bad_at.object).await,
        Some(UnreachableReason::Corrupt)
    );
    assert_eq!(mark_of(&cache, sound_at.object).await, None);
    assert!(cache.read_blob(&sound.digest()).await.is_ok());
}

/// Catches: a read that ignores the store id in a location and reads the configured
/// store anyway. It would get a 404 for an object that lives elsewhere and mark it
/// missing; the cache holds one store, so the read must fail INTERNAL and mark nothing.
#[tokio::test]
async fn a_location_in_another_store_is_internal_and_marks_nothing() {
    let cache = Cache::open(SharedLog::new(), conditional(), KeyPrefix::default())
        .await
        .expect("open");
    let elsewhere = blob("in store 1");
    let location = Location {
        store: StoreId::new(1),
        object: ObjectId::new(cache.epoch(), 99),
        offset: 0,
    };
    let applied = cache
        .meta()
        .commit(Command::PutBlob {
            digest: elsewhere.digest(),
            location,
        })
        .await
        .expect("commit");
    assert!(matches!(applied, Applied::Blob(Ok(_))), "{applied:?}");

    let read = cache.read_blob(&elsewhere.digest()).await;
    assert!(matches!(read, Err(CacheError::Internal(_))), "{read:?}");
    assert_eq!(mark_of(&cache, location.object).await, None);
}

/// Catches: a cache that writes under an epoch its log never allocated, and an index
/// that accepts the result. The object may be written, but the blob must not become
/// present, and the store call fails INTERNAL (a wiring bug, never the client's).
#[tokio::test]
async fn a_cache_whose_epoch_was_never_allocated_reports_nothing_present() {
    let cache = Cache::new(
        SharedLog::new(),
        conditional(),
        KeyPrefix::default(),
        Epoch::new(1),
    );
    let b = blob("never committed");
    let stored = cache.store_blobs(vec![b.clone()]).await;
    assert!(matches!(stored, Err(CacheError::Internal(_))), "{stored:?}");
    let d = b.digest();
    let answer = cache
        .meta()
        .query(move |s| s.blob(&d))
        .await
        .expect("query");
    assert_eq!(answer, BlobAnswer::Absent);
    assert_eq!(cache.find_missing(&[d]).await.expect("find missing"), [d]);
}
