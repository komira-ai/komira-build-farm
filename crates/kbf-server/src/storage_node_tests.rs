//! A registered node's cache ([`open_registered`]): its keys across a wiped metadata
//! directory, and what it refuses before it opens.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_front::{ObjectKeys, VerifiedBlob};
use kbf_log::MachineVersion;
use kbf_meta::Retention;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

use super::{one_voter, open_among, open_registered, prefix_root};
use crate::RaftMetaLog;
use crate::join::Joined;

/// One bucket for every start of a test, which records the keys written to it.
#[derive(Clone, Default)]
struct Bucket(Arc<(Mutex<Vec<String>>, BucketStore)>);

struct BucketStore(MemoryStore);

impl Default for BucketStore {
    /// A bucket that refuses a key twice, as `conditional-put=true` does.
    fn default() -> Self {
        Self(MemoryStore::new(Capabilities {
            conditional_put: true,
            object_lock: false,
        }))
    }
}

impl Bucket {
    fn written(&self) -> Vec<String> {
        self.0
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ObjectStore for Bucket {
    fn capabilities(&self) -> Capabilities {
        self.0.1.0.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(key.as_str().to_owned());
        self.0.1.0.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.1.0.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.1.0.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.1.0.list(prefix, after, max_keys).await
    }
}

fn joined(member_id: u64, boot: u64) -> Joined {
    Joined {
        member_id,
        boot,
        interval: Duration::from_secs(1),
        generation: 1,
    }
}

/// A fresh, empty metadata directory for `name`.
fn meta_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-node-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A start of a registered node over a new log in `dir`, as [`super::assemble`] opens
/// it: the keys from the log's prefix, then [`open_registered`]. Writes `n` blobs one at
/// a time and returns the keys written.
async fn start_and_write(dir: &Path, bucket: &Bucket, who: &Joined, n: usize) -> Vec<String> {
    let before = bucket.written().len();
    let meta = RaftMetaLog::open_dir(dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    let prefix = meta.store_prefix();
    let keys = ObjectKeys {
        root: prefix_root(&prefix, meta.log_id()).expect("a root"),
        legacy: prefix,
    };
    let cache = open_registered(meta, bucket.clone(), keys, who)
        .await
        .expect("open as the registration");
    for i in 0..n {
        let text = format!("boot {} blob {i}", who.boot);
        let blob = VerifiedBlob::hashed(Bytes::from(text));
        cache.store_blobs(vec![blob]).await.expect("store");
    }
    bucket.written()[before..].to_vec()
}

/// Catches: a node whose keys repeat after its metadata directory is wiped (the new log
/// starts its epochs, and any counter it keeps, from 1 again). The node registers
/// (member 7, boot 1), writes, loses its directory, registers again (the state store
/// gives boot 2) and writes into the same conditional bucket: every write succeeds and
/// the two key sets are disjoint, under the root, not the logs' prefixes. Mutant seen
/// red: the writer's boot taken from a fresh local `AllocEpoch` (both starts name
/// boot 1, and the second start's first PUT is refused).
#[tokio::test]
async fn a_wiped_node_never_reuses_its_keys() {
    let bucket = Bucket::default();
    let (first_dir, wiped_dir) = (meta_dir("first"), meta_dir("wiped"));
    let first = start_and_write(&first_dir, &bucket, &joined(7, 1), 3).await;
    let second = start_and_write(&wiped_dir, &bucket, &joined(7, 2), 3).await;
    let first_set: BTreeSet<&String> = first.iter().collect();
    assert_eq!((first.len(), second.len()), (3, 3), "{first:?} {second:?}");
    assert!(
        second.iter().all(|k| !first_set.contains(k)),
        "{first:?} {second:?}"
    );
    for (keys, prefix) in [
        (&first, "kbf/cas/0000000000000007/0000000000000001/"),
        (&second, "kbf/cas/0000000000000007/0000000000000002/"),
    ] {
        assert!(keys.iter().all(|k| k.starts_with(prefix)), "{keys:?}");
    }
    for dir in [first_dir, wiped_dir] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Catches: a registration of member or boot zero taken (no `NodeWriter` can name it,
/// and a default-filled answer would make every node one writer), a log left below the
/// version that takes `SetWriter`, and a second start as a boot the log has set. Each
/// refusal comes before anything is written.
#[tokio::test]
async fn a_registration_the_log_cannot_take_refuses_the_open() {
    let dir = meta_dir("refusals");
    let bucket = Bucket::default();
    for bad in [joined(0, 1), joined(7, 0)] {
        let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let keys = ObjectKeys::under(meta.store_prefix());
        let refused = open_registered(meta, bucket.clone(), keys, &bad)
            .await
            .err();
        let refused = refused.map(|e| e.to_string()).unwrap_or_default();
        assert!(refused.contains("neither may be zero"), "{refused}");
    }
    let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    assert_eq!(
        meta.machine_version(),
        MachineVersion::V1,
        "a refusal raised nothing"
    );
    let keys = ObjectKeys::under(meta.store_prefix());
    let cache = open_registered(meta, bucket.clone(), keys, &joined(7, 3))
        .await
        .expect("open");
    drop(cache);
    let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("open the log again");
    assert_eq!(meta.machine_version(), MachineVersion::V4);
    let keys = ObjectKeys::under(meta.store_prefix());
    let refused = open_registered(meta, bucket.clone(), keys, &joined(7, 3))
        .await
        .err();
    let refused = refused.map(|e| e.to_string()).unwrap_or_default();
    assert!(refused.contains("is not after boot 3"), "{refused}");
    assert!(bucket.written().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

/// Catches: a node root that keeps the log id (node keys would sit under one log's
/// prefix, and a wiped node's new log would put them elsewhere) or that takes a prefix
/// not made of a root and the log id.
#[test]
fn the_root_is_the_logs_prefix_less_its_id() {
    let log = kbf_log::FarmLogId::new(std::num::NonZeroU64::new(0xab).expect("not zero"));
    let prefix = KeyPrefix::new("bucket/kbf/00000000000000ab/").expect("prefix");
    let root = prefix_root(&prefix, log).expect("a root");
    assert_eq!(root.as_str(), "bucket/kbf/");
    let other = KeyPrefix::new("bucket/kbf/00000000000000cd/").expect("prefix");
    let refused = prefix_root(&other, log).err().map(|e| e.to_string());
    assert_eq!(
        refused.as_deref(),
        Some(
            "the log's key prefix bucket/kbf/00000000000000cd/ does not end in its id 00000000000000ab"
        )
    );
}

/// Catches: a registered node that raises a log of a cell of several voters to version
/// 4 (a replica that does not run it would refuse the log from then on).
#[test]
fn only_a_single_voter_log_is_raised_for_a_node() {
    assert_eq!(one_voter(1), Ok(()));
    assert_eq!(
        one_voter(3).err().as_deref(),
        Some("a registered storage node's metadata log has 3 voters; it must have one")
    );
}

/// Catches: a registered node's open that does not refuse a log of several voters
/// (mutant: the [`one_voter`] call dropped from the open): the open fails with the
/// voters' message before the log is raised or anything is written. A log this version
/// creates has one voter, so the count is the open's argument.
#[tokio::test]
async fn a_log_of_several_voters_refuses_the_open_before_it_is_raised() {
    let dir = meta_dir("voters");
    let bucket = Bucket::default();
    let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    let keys = ObjectKeys::under(meta.store_prefix());
    let one = std::num::NonZeroU64::new(1).expect("not zero");
    let refused = open_among(meta, bucket.clone(), keys, (one, one), 3)
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        refused.as_deref(),
        Some("a registered storage node's metadata log has 3 voters; it must have one")
    );
    assert!(bucket.written().is_empty());
    let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("open the log again");
    assert_eq!(meta.machine_version(), MachineVersion::V1, "nothing raised");
    drop(meta);
    let _ = std::fs::remove_dir_all(dir);
}
