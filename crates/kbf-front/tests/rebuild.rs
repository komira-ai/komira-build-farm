//! The rebuild of a cache's index from the store ([`Cache::rebuild_index`]): the
//! objects the index never heard of (an old server's, an earlier boot's, another node's)
//! read back after it, and what it must not do on the way.

use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use kbf_front::{
    Cache, CacheError, MemoryMetaLog, MetaLog, MetaLogError, ObjectKeys, RebuildConfig,
    VerifiedBlob,
};
use kbf_meta::{Applied, BlobAnswer, Command, NodeWriter, ObjectId, Rebuild, Retention, Writer};
use kbf_objstore::footers::FooterReads;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_types::Digest;

const ROOT: &str = "kbf/";

/// One bucket for every cache of a test, which counts the listing calls and can be told
/// to fail the n-th.
#[derive(Clone)]
struct Bucket(Arc<Inner>);

struct Inner {
    store: MemoryStore,
    lists: AtomicUsize,
    /// The listing call (counting from 1) that fails; 0 for none.
    fail_list: AtomicUsize,
    /// How ranged reads go: [`READS_OK`], [`READS_FAIL`] or [`READS_GONE`].
    reads: AtomicUsize,
}

const READS_OK: usize = 0;
/// Every ranged read fails with a store error.
const READS_FAIL: usize = 1;
/// Every ranged read finds the object gone.
const READS_GONE: usize = 2;

impl Bucket {
    fn new() -> Self {
        Self(Arc::new(Inner {
            store: MemoryStore::new(Capabilities::default()),
            lists: AtomicUsize::new(0),
            fail_list: AtomicUsize::new(0),
            reads: AtomicUsize::new(READS_OK),
        }))
    }

    fn set_reads(&self, mode: usize) {
        self.0.reads.store(mode, Ordering::SeqCst);
    }

    fn lists(&self) -> usize {
        self.0.lists.load(Ordering::SeqCst)
    }

    /// The key of every object, in order.
    async fn keys(&self) -> Vec<ObjectKey> {
        let page = self
            .0
            .store
            .list(
                &KeyPrefix::default(),
                None,
                PageSize::new(1000).expect("size"),
            )
            .await
            .expect("list");
        page.objects.into_iter().map(|o| o.key).collect()
    }

    async fn bytes(&self, key: &ObjectKey) -> Bytes {
        let page = self
            .0
            .store
            .list(
                &KeyPrefix::default(),
                None,
                PageSize::new(1000).expect("size"),
            )
            .await
            .expect("list");
        let size = page
            .objects
            .iter()
            .find(|o| o.key == *key)
            .unwrap_or_else(|| panic!("{key} is gone"))
            .size;
        let range = ByteRange::new(0, size).expect("a range");
        self.0.store.get_range(key, range).await.expect("read")
    }

    async fn put(&self, key: &str, bytes: Bytes) {
        let key = ObjectKey::new(key).expect("key");
        self.0
            .store
            .put_new(&key, bytes, None)
            .await
            .expect("plant");
    }
}

impl ObjectStore for Bucket {
    fn capabilities(&self) -> Capabilities {
        self.0.store.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.store.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        match self.0.reads.load(Ordering::SeqCst) {
            READS_OK => self.0.store.get_range(key, range).await,
            READS_GONE => Err(ObjectStoreError::NotFound(key.clone())),
            _ => Err(ObjectStoreError::Protocol("a scripted read failure".into())),
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.store.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let call = self.0.lists.fetch_add(1, Ordering::SeqCst) + 1;
        if self.0.fail_list.load(Ordering::SeqCst) == call {
            return Err(ObjectStoreError::Protocol(
                "a scripted listing failure".into(),
            ));
        }
        self.0.store.list(prefix, after, max_keys).await
    }
}

/// The keys of a cache over the log with id `log`: its epochs under `<root><log id>/`,
/// a node's under the root.
fn keys(log: u64) -> ObjectKeys {
    ObjectKeys {
        legacy: KeyPrefix::new(format!("{ROOT}{log:016x}/")).expect("prefix"),
        root: KeyPrefix::new(ROOT).expect("root"),
    }
}

fn node(member: u64, boot: u64) -> NodeWriter {
    let n = |v| NonZeroU64::new(v).expect("not zero");
    NodeWriter::new(n(member), n(boot))
}

fn blob(text: &str) -> VerifiedBlob {
    VerifiedBlob::hashed(Bytes::copy_from_slice(text.as_bytes()))
}

/// The digest and bytes of `n` blobs named `<tag> <i>`.
fn blobs(tag: &str, n: usize) -> Vec<(Digest, Bytes)> {
    (0..n)
        .map(|i| {
            let bytes = Bytes::from(format!("{tag} blob {i}"));
            (kbf_segments::sha256(&bytes), bytes)
        })
        .collect()
}

/// The metadata log of every cache of a test, so every cache is one type
/// (`Cache<Log, Bucket>`). Clones share the log, as successive processes over one
/// directory do; one can answer each command of one kind with another command's outcome,
/// as a broken log would.
#[derive(Clone)]
struct Log {
    log: Arc<MemoryMetaLog>,
    /// The label ([`kbf_front::metrics::COMMANDS`]) of the command it misanswers.
    misanswers: Option<&'static str>,
}

impl Log {
    fn new() -> Self {
        Self {
            log: Arc::new(MemoryMetaLog::new(Retention::default())),
            misanswers: None,
        }
    }

    fn misanswering(label: &'static str) -> Self {
        Self {
            misanswers: Some(label),
            ..Self::new()
        }
    }
}

impl MetaLog for Log {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        let label = kbf_front::metrics::COMMANDS[kbf_front::metrics::command_label(&command)];
        if self.misanswers == Some(label) {
            return Ok(Applied::Ticked(kbf_types::FarmTime::default()));
        }
        self.log.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&kbf_meta::MetaState) -> R + Send,
        R: Send,
    {
        self.log.query(f).await
    }
}

fn fresh_log() -> Log {
    Log::new()
}

/// Stores `blobs` one segment each through a node writer's cache over a log of its own
/// (which the caller then loses), under `keys`.
async fn write_as_node(
    bucket: &Bucket,
    keys: ObjectKeys,
    writer: NodeWriter,
    blobs: &[(Digest, Bytes)],
) {
    let cache = Cache::open_node(fresh_log(), bucket.clone(), keys, writer)
        .await
        .expect("open as a node");
    for (_, bytes) in blobs {
        let one = VerifiedBlob::hashed(bytes.clone());
        cache.store_blobs(vec![one]).await.expect("store");
    }
}

/// Stores `blobs` one segment each as an old server over a log with id `log` does: an
/// epoch writer, `<root><log id>/cas/<epoch>/<seq>`.
async fn write_as_old_server(bucket: &Bucket, log: u64, blobs: &[(Digest, Bytes)]) {
    let cache = Cache::open_under(fresh_log(), bucket.clone(), keys(log))
        .await
        .expect("open as a server");
    for (_, bytes) in blobs {
        let one = VerifiedBlob::hashed(bytes.clone());
        cache.store_blobs(vec![one]).await.expect("store");
    }
}

/// A new node's cache over a new log (id `log`) and the bucket.
async fn new_node(bucket: &Bucket, log: u64, writer: NodeWriter) -> Cache<Log, Bucket> {
    Cache::open_node(fresh_log(), bucket.clone(), keys(log), writer)
        .await
        .expect("open as a node")
}

/// Every blob reads back byte for byte, and none is reported missing.
async fn assert_holds<M: MetaLog, O: ObjectStore>(cache: &Cache<M, O>, blobs: &[(Digest, Bytes)]) {
    let digests: Vec<Digest> = blobs.iter().map(|(d, _)| *d).collect();
    assert_eq!(cache.find_missing(&digests).await.expect("find"), []);
    for (digest, bytes) in blobs {
        let got = cache.read_blob(digest).await.expect("read");
        assert_eq!(&got, bytes, "{digest}");
    }
}

fn small() -> RebuildConfig {
    RebuildConfig {
        reads: FooterReads {
            concurrency: NonZeroUsize::new(4).expect("not zero"),
            page: PageSize::new(2).expect("in range"),
            tail: FooterReads::default().tail,
        },
        batch: NonZeroUsize::new(2).expect("not zero"),
    }
}

/// As [`small`], reading one footer at a time, so a failing listing call comes right after
/// the objects before it were handled (with more reads in flight the stream goes on
/// listing while earlier footers are still being read, and a listing error is delivered
/// ahead of the results queued behind it).
fn serial() -> RebuildConfig {
    let mut config = small();
    config.reads.concurrency = NonZeroUsize::MIN;
    config
}

async fn rebuild_of<M: MetaLog, O: ObjectStore>(cache: &Cache<M, O>) -> Rebuild {
    cache.meta().query(|s| s.rebuild()).await.expect("query")
}

/// Catches: a rebuild that does not index the keys of the old server (the P8 cutover):
/// objects of an epoch writer of a log that is not this node's own, whose epoch alone
/// (every log counts from 1) names nothing. A new node over a new log rebuilds over
/// 7 old segments of two old logs (the same epoch and seqs in both) and reads every blob
/// back with its bytes; a blob of each old log with the same text-derived name does not
/// get confused with the other's. Mutant seen red: `ObjectKeys::object` answering `None`
/// for another log's key (every segment skipped as foreign).
#[tokio::test]
async fn old_layout_segments_are_adopted_and_read() {
    let bucket = Bucket::new();
    let (first, second) = (blobs("old log one", 4), blobs("old log two", 3));
    write_as_old_server(&bucket, 0x11, &first).await;
    write_as_old_server(&bucket, 0x22, &second).await;
    assert_eq!(bucket.keys().await.len(), 7);

    let node = new_node(&bucket, 0x33, node(7, 1)).await;
    assert_eq!(
        node.find_missing(&[first[0].0]).await.expect("find"),
        [first[0].0],
        "nothing is indexed before the rebuild"
    );
    let report = node.rebuild_index(small()).await.expect("rebuild");
    assert_eq!(report.objects, 7);
    assert_eq!(report.segments, 7);
    assert_eq!(report.inserted, 7);
    assert_eq!(report.resumed_after, None);
    assert_eq!(rebuild_of(&node).await, Rebuild::Done);
    assert_holds(&node, &first).await;
    assert_holds(&node, &second).await;
}

/// Catches: a rebuild that leaves out the objects of a node writer: this node's own
/// earlier boot (its metadata directory was wiped) and another member's. The new boot
/// finds both, indexes them, and a read of each gives its bytes. Mutant seen red: the
/// listing under the log's own prefix instead of the root (nothing of a node writer is
/// found).
#[tokio::test]
async fn a_wiped_node_finds_its_earlier_boots_and_other_members() {
    let bucket = Bucket::new();
    let (mine, theirs) = (blobs("my boot 1", 3), blobs("member 8", 2));
    write_as_node(&bucket, keys(0xaa), node(7, 1), &mine).await;
    write_as_node(&bucket, keys(0xbb), node(8, 1), &theirs).await;

    let node = new_node(&bucket, 0xcc, node(7, 2)).await;
    let report = node.rebuild_index(small()).await.expect("rebuild");
    assert_eq!((report.segments, report.own), (5, 0));
    assert_holds(&node, &mine).await;
    assert_holds(&node, &theirs).await;
}

/// The object the index holds `digest` at, if it holds it present.
async fn object_of(cache: &Cache<Log, Bucket>, digest: Digest) -> Option<ObjectId> {
    let found = cache.meta().query(move |s| match s.blob(&digest) {
        BlobAnswer::Present { location, .. } => Some(location.object),
        BlobAnswer::Unavailable | BlobAnswer::Absent => None,
    });
    found.await.expect("query")
}

/// Catches: an adoption that replaces a blob written while the rebuild was due: the new
/// node stores a blob (its own object) that an old segment also holds; after the rebuild
/// the blob is still at the node's own object, and the old segment's copy was counted as
/// kept. Mutant seen red: `adopt_segment` inserting without the digest check (the blob's
/// location is the old object).
#[tokio::test]
async fn adopt_never_replaces_a_newer_write() {
    let bucket = Bucket::new();
    let old = blobs("shared", 3);
    write_as_old_server(&bucket, 0x11, &old).await;

    let writer = node(7, 3);
    let node = new_node(&bucket, 0x33, writer).await;
    node.store_blobs(vec![blob("shared blob 1")])
        .await
        .expect("a write during catch-up");
    let own = object_of(&node, old[1].0).await.expect("indexed");
    assert_eq!(own.writer(), Writer::Node(writer));

    let report = node.rebuild_index(small()).await.expect("rebuild");
    // The node's own segment is its writer's: left to its puts. The three old ones are
    // adopted, one record of which was held already.
    assert_eq!((report.own, report.segments), (1, 3));
    assert_eq!((report.inserted, report.kept), (2, 1));
    assert_eq!(object_of(&node, old[1].0).await, Some(own), "not replaced");
    assert_holds(&node, &old).await;
}

/// Catches: an object that is not a whole segment, or not one of the cache's, ending or
/// poisoning the rebuild, or being adopted. Under the root: a 10-byte object (shorter
/// than a trailer), 100 bytes of nothing (a torn tail), a valid segment under a key that
/// is not an object name of the cache, and a valid segment of an epoch of the cache's
/// own log that it never allocated. The 3 real segments are all indexed, the four others
/// counted (2 skipped, 2 foreign), and the rebuild completes. Mutants seen red: a skip
/// returned as an error (the rebuild never completes); the unallocated-epoch segment
/// adopted (the log refuses it, and the rebuild fails on the refusal).
#[tokio::test]
async fn torn_and_foreign_objects_are_skipped_not_fatal() {
    let bucket = Bucket::new();
    let good = blobs("good", 3);
    write_as_old_server(&bucket, 0x11, &good).await;
    let real = bucket.keys().await.remove(0);
    let segment = bucket.bytes(&real).await;
    bucket
        .put(
            "kbf/cas/0000000000000009/0000000000000001/0000000000000001",
            Bytes::from_static(b"0123456789"),
        )
        .await;
    bucket
        .put(
            "kbf/cas/0000000000000009/0000000000000001/0000000000000002",
            Bytes::from(vec![7_u8; 100]),
        )
        .await;
    bucket
        .put("kbf/notes/a-segment-in-a-strange-place", segment.clone())
        .await;
    // The cache's own log is 0x33; epoch 9 was never allocated by it.
    bucket
        .put(
            "kbf/0000000000000033/cas/0000000000000009/0000000000000001",
            segment,
        )
        .await;

    let node = new_node(&bucket, 0x33, node(7, 1)).await;
    let report = node.rebuild_index(small()).await.expect("rebuild");
    assert_eq!(report.objects, 7);
    assert_eq!(report.segments, 3);
    assert_eq!(report.skipped, 2, "short and torn");
    assert_eq!(report.foreign, 2, "a strange key; an unallocated epoch");
    assert_eq!(rebuild_of(&node).await, Rebuild::Done);
    assert_holds(&node, &good).await;
}

/// Catches: a rebuild that stops after the first page of the listing (pages of 2 over 9
/// segments: 4 full pages and a short one), or one that does not keep where it was. Its
/// listing fails on the third call, after 4 objects: the rebuild reports the failure, is
/// not done, and the log holds the key of the fourth object (a batch of 2 committed
/// twice); the next call resumes after exactly that key (5 objects left), finishes, and a
/// third lists nothing. Mutants seen red: progress not committed (the second call starts
/// from the top: 9 objects, `resumed_after` empty); `RebuildDone` committed before the
/// listing (the second call is `already_done`, with 5 segments not indexed); stopping
/// after the first page (4 segments held at most).
#[tokio::test]
async fn a_rebuild_resumes_after_its_last_progress_and_finishes_once() {
    let bucket = Bucket::new();
    let old = blobs("old", 9);
    write_as_old_server(&bucket, 0x11, &old).await;
    let all = bucket.keys().await;

    let node = new_node(&bucket, 0x33, node(7, 1)).await;
    // Calls so far: none by the node. Fail the third listing call of the rebuild.
    let before = bucket.lists();
    bucket.0.fail_list.store(before + 3, Ordering::SeqCst);
    let failed = node.rebuild_index(serial()).await.err();
    assert!(matches!(failed, Some(CacheError::Store(_))), "{failed:?}");
    assert_eq!(
        rebuild_of(&node).await,
        Rebuild::Pending {
            after: Some(all[3].as_str().to_owned())
        },
        "four objects were handled, in two batches"
    );
    let missing = node
        .find_missing(&old.iter().map(|(d, _)| *d).collect::<Vec<_>>())
        .await
        .expect("find");
    assert_eq!(missing.len(), 5, "the first four are indexed");

    let report = node.rebuild_index(serial()).await.expect("resumed");
    assert_eq!(report.resumed_after.as_deref(), Some(all[3].as_str()));
    assert_eq!(report.objects, 5);
    assert_eq!(rebuild_of(&node).await, Rebuild::Done);
    assert_holds(&node, &old).await;

    let lists = bucket.lists();
    let again = node.rebuild_index(serial()).await.expect("again");
    assert!(again.already_done);
    assert_eq!(again.objects, 0);
    assert_eq!(bucket.lists(), lists, "a finished rebuild lists nothing");
}

/// Catches: a rebuild that treats a failed read of a footer as the object being no
/// segment (a transient store error then drops a segment's blobs for good, the rebuild
/// marked done without them): the call fails, is not done, and a retry that reads
/// successfully indexes it. The failing read is of one object's tail. Mutant seen red:
/// `SkipReason::Read` counted as skipped.
#[tokio::test]
async fn a_failed_footer_read_stops_the_rebuild_instead_of_skipping() {
    let bucket = Bucket::new();
    let old = blobs("old", 3);
    write_as_old_server(&bucket, 0x11, &old).await;
    bucket.set_reads(READS_FAIL);
    let node = new_node(&bucket, 0x33, node(7, 1)).await;
    let failed = node.rebuild_index(small()).await.err();
    assert!(matches!(failed, Some(CacheError::Store(_))), "{failed:?}");
    assert!(matches!(
        node.meta().query(|s| s.rebuild()).await.expect("query"),
        Rebuild::Pending { .. }
    ));
    bucket.set_reads(READS_OK);
    node.rebuild_index(small()).await.expect("retry");
    assert_holds(&node, &old).await;
}

/// Catches: an object deleted between the listing and the read of its footer (a sweep, a
/// peer's eviction) failing the rebuild or being adopted from nothing: it is counted gone
/// and skipped, and the rebuild completes.
#[tokio::test]
async fn an_object_gone_since_it_was_listed_is_counted_and_skipped() {
    let bucket = Bucket::new();
    write_as_old_server(&bucket, 0x11, &blobs("old", 3)).await;
    bucket.set_reads(READS_GONE);
    let node = new_node(&bucket, 0x33, node(7, 1)).await;
    let report = node.rebuild_index(small()).await.expect("rebuild");
    assert_eq!((report.objects, report.gone, report.segments), (3, 3, 0));
    assert_eq!(rebuild_of(&node).await, Rebuild::Done);
}

/// Catches: a rebuild that takes an outcome the log did not give for its command (a broken
/// log) as done: adopting, recording progress and finishing each fail the rebuild with an
/// internal error, and it is not marked done by a log that answered `Ticked`.
#[tokio::test]
async fn a_log_that_misanswers_a_rebuild_command_fails_the_rebuild() {
    for label in ["adopt_segment", "rebuild_progress", "rebuild_done"] {
        let bucket = Bucket::new();
        write_as_old_server(&bucket, 0x11, &blobs("old", 2)).await;
        let log = Log::misanswering(label);
        let node = Cache::open_node(log, bucket.clone(), keys(0x33), node(7, 1))
            .await
            .expect("open");
        let failed = node.rebuild_index(small()).await.err();
        assert!(
            matches!(failed, Some(CacheError::Internal(_))),
            "{label}: {failed:?}"
        );
    }
}

/// Catches: an adopted object whose loss the log cannot record (its writer unknown to
/// the log): the object is deleted from the store after the rebuild; a read of a blob in
/// it must fail as unreachable (not internal, not served), the blob must then be reported
/// missing so a client uploads it again, and the upload must heal the entry into the
/// node's own object. Mutant seen red: `adopted_writers` not filled (the unreachable mark
/// is refused and the read fails the same way every time, the blob never reported missing).
#[tokio::test]
async fn a_lost_adopted_object_is_marked_reported_missing_and_healed() {
    let bucket = Bucket::new();
    let old = blobs("old", 2);
    write_as_old_server(&bucket, 0x11, &old).await;
    let lost = bucket.keys().await.remove(0);

    let node = new_node(&bucket, 0x33, node(7, 1)).await;
    node.rebuild_index(small()).await.expect("rebuild");
    bucket.delete(&lost).await.expect("delete");
    // Which blob was in the deleted object: the one that no longer reads.
    let mut gone = None;
    for (digest, _) in &old {
        if matches!(
            node.read_blob(digest).await,
            Err(CacheError::Unreachable(_))
        ) {
            gone = Some(*digest);
        }
    }
    let gone = gone.expect("a read of the lost object is unreachable");
    assert_eq!(node.find_missing(&[gone]).await.expect("find"), [gone]);
    let bytes = old
        .iter()
        .find(|(d, _)| *d == gone)
        .expect("blob")
        .1
        .clone();
    node.store_blobs(vec![VerifiedBlob::hashed(bytes.clone())])
        .await
        .expect("re-upload");
    assert_eq!(node.read_blob(&gone).await.expect("healed"), bytes);
}

/// Catches: a rebuild that indexes the objects its own writer is writing (the node's puts
/// do that): the node writes 3 segments before the rebuild; the rebuild counts them as
/// its own, adopts none of them, and the index is as it was. Mutant seen red: no
/// current-writer check in the log (the three counted as segments, all records kept).
#[tokio::test]
async fn a_rebuild_leaves_the_nodes_own_objects_to_its_puts() {
    let bucket = Bucket::new();
    let node = new_node(&bucket, 0x33, node(7, 1)).await;
    let mine = blobs("mine", 3);
    for (_, bytes) in &mine {
        node.store_blobs(vec![VerifiedBlob::hashed(bytes.clone())])
            .await
            .expect("store");
    }
    let report = node.rebuild_index(small()).await.expect("rebuild");
    assert_eq!((report.own, report.segments), (3, 0));
    assert_eq!((report.inserted, report.kept), (0, 0));
    assert_holds(&node, &mine).await;
}

/// Catches: a rebuild that is not repeatable across starts: a log that finished one is
/// done for good (the marker is in the log, and a second cache over it lists nothing);
/// and a cache over a log with no marker (a new or wiped one) lists.
#[tokio::test]
async fn the_marker_is_the_logs_not_the_caches() {
    let bucket = Bucket::new();
    write_as_old_server(&bucket, 0x11, &blobs("old", 2)).await;
    let log = fresh_log();
    let open = |boot| Cache::open_node(log.clone(), bucket.clone(), keys(0x33), node(7, boot));
    let first = open(1).await.expect("open");
    first.rebuild_index(small()).await.expect("rebuild");
    let lists = bucket.lists();
    let second = open(2).await.expect("a later boot over the same log");
    assert!(
        second
            .rebuild_index(small())
            .await
            .expect("again")
            .already_done
    );
    assert_eq!(bucket.lists(), lists);
    let wiped = new_node(&bucket, 0x44, node(7, 3)).await;
    assert!(
        !wiped
            .rebuild_index(small())
            .await
            .expect("rebuild")
            .already_done
    );
    assert!(bucket.lists() > lists);
}

/// Catches: an adoption command that a segment of many records does not fit in: a segment
/// of more than `ADOPT_RECORDS` records is adopted in several commands and every record
/// is indexed. Mutant seen red: only the first chunk of a segment committed.
#[tokio::test]
async fn a_segment_of_many_records_is_adopted_whole() {
    let bucket = Bucket::new();
    let n = kbf_front::ADOPT_RECORDS + 10;
    let many = blobs("many", n);
    let cache = Cache::open_under(fresh_log(), bucket.clone(), keys(0x11))
        .await
        .expect("open as a server");
    cache
        .store_blobs(
            many.iter()
                .map(|(_, b)| VerifiedBlob::hashed(b.clone()))
                .collect(),
        )
        .await
        .expect("store");
    let objects: BTreeMap<_, _> = bucket.keys().await.into_iter().map(|k| (k, ())).collect();
    assert_eq!(objects.len(), 1, "one segment");

    let node = new_node(&bucket, 0x33, node(7, 1)).await;
    let report = node.rebuild_index(small()).await.expect("rebuild");
    assert_eq!(report.inserted, n as u64);
    assert_holds(&node, &many).await;
}
