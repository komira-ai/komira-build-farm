//! A registered storage node's object keys (`docs/design/architecture-v2.md`, section 6):
//! `<root>cas/<member>/<boot>/<seq>`, apart from every other writer's and from the
//! epoch writers' `<root><log id>/cas/<epoch>/<seq>`.

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use bytes::Bytes;
use kbf_front::{
    Cache, CacheError, MemoryMetaLog, MetaLog, MetaLogError, ObjectKeys, VerifiedBlob,
};
use kbf_meta::{
    Applied, Command, Epoch, MetaState, NodeWriter, ObjectId, Retention, StaleWriter, Writer,
};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

const ROOT: &str = "kbf/";

/// One bucket shared by several nodes, which records every key a write named.
#[derive(Clone)]
struct Bucket(Arc<(MemoryStore, Mutex<Vec<String>>)>);

impl Bucket {
    /// A bucket that replaces a key it is given again, so a key named twice shows as
    /// changed bytes, not as a refused write.
    fn new() -> Self {
        Self(Arc::new((
            MemoryStore::new(Capabilities::default()),
            Mutex::new(Vec::new()),
        )))
    }

    fn written(&self) -> Vec<String> {
        self.0
            .1
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn bytes(&self, key: &str) -> Bytes {
        let key = ObjectKey::new(key).expect("key");
        let page = self
            .0
            .0
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
            .find(|o| o.key == key)
            .unwrap_or_else(|| panic!("{key} is gone"))
            .size;
        let range = ByteRange::new(0, size).expect("a range");
        self.0.0.get_range(&key, range).await.expect("read")
    }
}

impl ObjectStore for Bucket {
    fn capabilities(&self) -> Capabilities {
        self.0.0.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0
            .1
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(key.as_str().to_owned());
        self.0.0.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.0.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.0.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.0.list(prefix, after, max_keys).await
    }
}

/// One metadata log for several opens, as successive processes over one directory.
#[derive(Clone)]
struct SharedLog(Arc<MemoryMetaLog>);

impl MetaLog for SharedLog {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.0.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.0.query(f).await
    }
}

fn node(member: u64, boot: u64) -> NodeWriter {
    let n = |v| NonZeroU64::new(v).expect("not zero");
    NodeWriter::new(n(member), n(boot))
}

/// The keys of a server over the log with id `log`: its own epoch prefix and the root.
fn keys(log: u64) -> ObjectKeys {
    ObjectKeys {
        legacy: KeyPrefix::new(format!("{ROOT}{log:016x}/")).expect("prefix"),
        root: KeyPrefix::new(ROOT).expect("root"),
    }
}

fn blob(text: &str) -> VerifiedBlob {
    VerifiedBlob::hashed(Bytes::copy_from_slice(text.as_bytes()))
}

/// Opens `writer`'s cache over a log of its own and `bucket`, and stores `n` blobs one
/// at a time (a segment each). Returns the keys it wrote, in order.
async fn write_as(bucket: &Bucket, writer: NodeWriter, n: usize) -> Vec<String> {
    let before = bucket.written().len();
    let log = MemoryMetaLog::new(Retention::default());
    let cache = Cache::open_node(log, bucket.clone(), keys(0xaa), writer)
        .await
        .expect("open as a node writer");
    assert_eq!(cache.writer(), Writer::Node(writer));
    for i in 0..n {
        let text = format!("{writer:?} wrote blob {i}");
        cache.store_blobs(vec![blob(&text)]).await.expect("store");
    }
    bucket.written()[before..].to_vec()
}

/// Catches: a node key that leaves out the member or the boot (two members, or two
/// boots of one member, both counting seq from 1, would then name the same objects),
/// or that is not exactly `<root>cas/<member>/<boot>/<seq>` in 16 hex digits each; and
/// a key that does not parse back to its object, or parses in a spelling the cache
/// never writes. Mutant seen red: the boot dropped from the key (boot 2's keys equal
/// boot 1's).
#[tokio::test]
async fn object_keys_carry_member_and_boot() {
    let k = keys(0xaa);
    let id = ObjectId::of(Writer::Node(node(7, 0x2b)), 3);
    let key = k.key(id).expect("key");
    assert_eq!(
        key.as_str(),
        "kbf/cas/0000000000000007/000000000000002b/0000000000000003"
    );
    assert_eq!(k.object(&key), Some(id));
    let epoch = ObjectId::new(Epoch::new(5), 3);
    let old = k.key(epoch).expect("key");
    assert_eq!(
        old.as_str(),
        "kbf/00000000000000aa/cas/0000000000000005/0000000000000003"
    );
    assert_eq!(k.object(&old), Some(epoch));
    // Another log's epoch writer: named with its log id, and only if it is not the
    // cache's own log (the `00..aa` above, an epoch writer plain).
    let other = ObjectId::of(
        Writer::Legacy(kbf_meta::LegacyWriter::new(
            NonZeroU64::new(0xbb).expect("not zero"),
            Epoch::new(5),
        )),
        3,
    );
    let other_key = k.key(other).expect("key");
    assert_eq!(
        other_key.as_str(),
        "kbf/00000000000000bb/cas/0000000000000005/0000000000000003"
    );
    assert_eq!(k.object(&other_key), Some(other));
    for bad in [
        "kbf/cas/0000000000000000/000000000000002b/0000000000000003",
        "kbf/cas/0000000000000007/0000000000000000/0000000000000003",
        "kbf/cas/0000000000000007/000000000000002B/0000000000000003",
        "kbf/cas/7/2b/3",
        "kbf/cas/0000000000000007/000000000000002b",
        "kbf/00000000000000bb/cas/0000000000000000/0000000000000003",
        "kbf/00000000000000bb/cas/0000000000000005/00000000000003",
        "kbf/00000000000000BB/cas/0000000000000005/0000000000000003",
        "other/cas/0000000000000007/000000000000002b/0000000000000003",
    ] {
        assert_eq!(k.object(&ObjectKey::new(bad).expect("key")), None, "{bad}");
    }

    let bucket = Bucket::new();
    let seven = write_as(&bucket, node(7, 1), 3).await;
    let eight = write_as(&bucket, node(8, 1), 3).await;
    let reboot = write_as(&bucket, node(7, 2), 3).await;
    for (keys, prefix) in [
        (&seven, "kbf/cas/0000000000000007/0000000000000001/"),
        (&eight, "kbf/cas/0000000000000008/0000000000000001/"),
        (&reboot, "kbf/cas/0000000000000007/0000000000000002/"),
    ] {
        assert_eq!(keys.len(), 3, "{keys:?}");
        for (seq, key) in keys.iter().enumerate() {
            assert_eq!(*key, format!("{prefix}{:016x}", seq + 1), "the same seqs");
        }
    }
    let all: BTreeSet<&String> = seven.iter().chain(&eight).chain(&reboot).collect();
    assert_eq!(all.len(), 9, "no key named twice");
}

/// Catches: a node writer whose keys can land on an object an old server wrote. The
/// bucket holds old-layout objects of a log whose id equals the member id, at the epoch
/// equal to the boot and the very seqs the node will number; the node writes N
/// segments. No write may name an existing key, and every old object must read back
/// byte for byte. Mutant seen red: the node key reusing the old layout,
/// `<root><member>/cas/<boot>/<seq>` (the node's writes replace the old objects).
#[tokio::test]
async fn no_key_collision_with_old_server_objects() {
    const N: u64 = 4;
    let bucket = Bucket::new();
    let (member, boot) = (0x7_u64, 0x2_u64);
    let mut old = Vec::new();
    for seq in 1..=N + 2 {
        let key = format!("{ROOT}{member:016x}/cas/{boot:016x}/{seq:016x}");
        let bytes = Bytes::from(format!("an old server's object {seq}"));
        bucket
            .0
            .0
            .put_new(
                &ObjectKey::new(key.as_str()).expect("key"),
                bytes.clone(),
                None,
            )
            .await
            .expect("seed");
        old.push((key, bytes));
    }
    let written = write_as(
        &bucket,
        node(member, boot),
        usize::try_from(N).expect("small"),
    )
    .await;
    assert_eq!(written.len() as u64, N, "{written:?}");
    for key in &written {
        assert!(
            old.iter().all(|(o, _)| o != key),
            "{key} was an old object's key"
        );
    }
    for (key, bytes) in &old {
        assert_eq!(&bucket.bytes(key).await, bytes, "{key} changed");
    }
}

/// Catches: a node cache that opens over a log that has set its boot, or a later one,
/// already (a state store that handed the boot out again, or an earlier process still
/// registered): it would number objects from seq 1 over keys that boot wrote. The
/// refusal is the log's, and the first open stays the only one.
#[tokio::test]
async fn a_boot_set_before_refuses_the_open() {
    let log = SharedLog(Arc::new(MemoryMetaLog::new(Retention::default())));
    let open = |writer| {
        Cache::open_node(
            log.clone(),
            MemoryStore::new(Capabilities::default()),
            keys(1),
            writer,
        )
    };
    let first = open(node(3, 4)).await.expect("the first open");
    for again in [node(3, 4), node(3, 2)] {
        let refused = open(again).await.err();
        assert_eq!(
            refused.map(|e| e.to_string()),
            Some(
                CacheError::StaleWriter(StaleWriter {
                    writer: again,
                    current: NonZeroU64::new(4).expect("not zero"),
                })
                .to_string()
            )
        );
    }
    assert_eq!(first.writer(), Writer::Node(node(3, 4)));
    // Not the caller's fault, and the same on every retry: INTERNAL on both surfaces.
    let stale = CacheError::StaleWriter(StaleWriter {
        writer: node(3, 4),
        current: NonZeroU64::new(4).expect("not zero"),
    });
    assert_eq!(
        tonic::Status::from(stale.clone()).code(),
        tonic::Code::Internal
    );
    assert!(matches!(
        kbf_storage::StorageError::from(stale),
        kbf_storage::StorageError::Internal(_)
    ));
}
