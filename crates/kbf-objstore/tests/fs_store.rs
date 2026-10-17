//! `FsStore`, the object store in a local directory: the conformance suite, what it
//! keeps across a reopen, its lock, concurrent writers of one key, and how it reports
//! files that are not what it wrote and failing file system calls.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::{
    ByteRange, Floor, FsStore, FsStoreError, KeyPrefix, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize, Room, Space, conformance, objects_source,
};
use sha2::{Digest, Sha256};

/// A floor every file system the tests run on meets: one byte.
const FLOOR: Floor = Floor::Bytes(1);

/// A fresh, absent directory for `name`, under the target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-objstore-fs")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).expect("a valid key")
}

/// Where the module docs say `key`'s object file is: `objects/<hh>/<sha256 of key>`.
fn file_of(dir: &Path, key: &ObjectKey) -> PathBuf {
    let name = hex::encode(Sha256::digest(key.as_str().as_bytes()));
    dir.join("objects").join(&name[..2]).join(name)
}

fn all(len: u64) -> ByteRange {
    ByteRange::new(0, len).expect("a non-empty range")
}

/// Catches any break in the base contract or in the conditional writes the store
/// claims (`put_new` refusing an existing key), and a store that claims Object Lock or
/// accepts a retention date it cannot keep. The server trusts a store only once it
/// passes.
#[tokio::test]
async fn passes_conformance_with_conditional_writes() {
    let dir = scratch("conformance");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    let caps = store.capabilities();
    assert!(caps.conditional_put && !caps.object_lock, "{caps:?}");
    let prefix = KeyPrefix::new("conformance/").expect("a prefix");
    let report = conformance::run(&store, &prefix).await;
    assert!(report.passed(), "{report}");
    assert_eq!(report.skipped().count(), 0, "{report}");
    // The run deleted what it wrote: the bucket is empty.
    let left = store
        .list(&KeyPrefix::default(), None, PageSize::MAX)
        .await
        .expect("list");
    assert!(left.objects.is_empty(), "{:?}", left.objects);
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a store that does not outlive its process (objects kept in memory, or under
/// a name the next open does not read), and an open that keeps a file a crash left in
/// `tmp/`: it must be removed, never read or listed as an object.
#[tokio::test]
async fn objects_survive_a_reopen_and_torn_writes_do_not() {
    let dir = scratch("reopen");
    let k = key("kbf/0123456789abcdef/cas/0000000000000001/0000000000000001");
    let body = Bytes::from_static(b"the bytes of one segment");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    store.put_new(&k, body.clone(), None).await.expect("put");
    drop(store);
    let torn = dir.join("tmp").join("0");
    std::fs::write(&torn, b"kbf-obj1 a write cut short").expect("plant");

    let store = FsStore::open(&dir, FLOOR).expect("reopen");
    assert!(!torn.exists(), "a torn temporary file outlived the open");
    let got = store.get_range(&k, all(100)).await.expect("read");
    assert_eq!(got, body);
    let everything = KeyPrefix::new("").expect("the empty prefix");
    let page = store
        .list(&everything, None, PageSize::MAX)
        .await
        .expect("list");
    assert_eq!(page.objects.len(), 1, "{page:?}");
    assert_eq!(page.objects[0].key, k);
    assert_eq!(page.objects[0].size, body.len() as u64);
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a paged listing of a directory that loses, repeats or misorders keys across
/// pages (pages of 2 over 9 keys), lists a key outside the prefix, or lists a file that
/// is not an object: a write in flight in `tmp/` (even one with a whole header) or a
/// foreign file in a shard directory.
#[tokio::test]
async fn a_paged_listing_is_the_written_key_set_and_nothing_else() {
    let dir = scratch("paged");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    let mut want = Vec::new();
    for i in [5, 0, 8, 3, 1, 7, 2, 6, 4] {
        let k = key(&format!("cas/{i:02}"));
        store
            .put_new(&k, Bytes::from(vec![b'x'; i + 1]), None)
            .await
            .expect("put");
        want.push((k.as_str().to_owned(), i as u64 + 1));
    }
    want.sort();
    store
        .put_new(&key("other/00"), Bytes::from_static(b"o"), None)
        .await
        .expect("put");
    let in_flight = [b"kbf-obj1\0\x06cas/99".as_slice(), b"body"].concat();
    std::fs::write(dir.join("tmp").join("7"), in_flight).expect("plant tmp");
    std::fs::write(
        dir.join("objects").join("00").join("stray"),
        b"not an object",
    )
    .expect("plant a foreign file");

    let cas = KeyPrefix::new("cas/").expect("prefix");
    let mut got = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .list(&cas, after.as_ref(), PageSize::new(2).expect("size"))
            .await
            .expect("list");
        assert!(page.objects.len() <= 2, "{page:?}");
        got.extend(
            page.objects
                .into_iter()
                .map(|o| (o.key.as_str().to_owned(), o.size)),
        );
        match page.next {
            Some(t) => after = Some(t),
            None => break,
        }
    }
    assert_eq!(got, want);
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a fresh listing [`ListToken::after`] a key served from the kept keys of an
/// earlier listing of the same prefix that was abandoned after its first page: a
/// rebuild resuming from the last key it finished would miss every object written
/// since that listing began. The abandoned listing's own next page may still be cut
/// from its keys (the contract lets a listing miss recent writes); a fresh token reads
/// the directory again.
#[tokio::test]
async fn a_fresh_listing_after_a_key_sees_objects_written_since_an_abandoned_one() {
    let dir = scratch("fresh-after");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    for name in ["cas/a", "cas/b", "cas/c"] {
        store
            .put_new(&key(name), Bytes::from_static(b"x"), None)
            .await
            .expect("put");
    }
    let cas = KeyPrefix::new("cas/").expect("prefix");
    let size = PageSize::new(1).expect("size");
    let first = store.list(&cas, None, size).await.expect("list");
    assert_eq!(first.objects[0].key.as_str(), "cas/a");
    assert!(first.next.is_some(), "{first:?}");
    // The listing is abandoned here; a new object lands after its first page.
    store
        .put_new(&key("cas/bb"), Bytes::from_static(b"x"), None)
        .await
        .expect("put");
    let fresh = store
        .list(&cas, Some(&ListToken::after(&key("cas/b"))), size)
        .await
        .expect("list");
    let keys: Vec<&str> = fresh.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["cas/bb"]);
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches two stores sharing one directory: the second would empty `tmp/` under the
/// first's writes. The open is refused, naming the lock file, until the first store is
/// dropped.
#[test]
fn a_second_open_is_refused_until_the_first_store_closes() {
    let dir = scratch("lock");
    let first = FsStore::open(&dir, FLOOR).expect("open");
    let Err(refused) = FsStore::open(&dir, FLOOR) else {
        panic!("a second open of a locked directory succeeded");
    };
    assert!(
        matches!(refused, FsStoreError::Locked { .. }),
        "{refused:?}"
    );
    let text = refused.to_string();
    assert!(
        text.contains(&format!("lock {}", dir.join("LOCK").display())),
        "{text}"
    );
    drop(first);
    drop(FsStore::open(&dir, FLOOR).expect("open after the first closed"));
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a write that replaces an existing object (a rename where a link belongs):
/// of concurrent writers of one key, exactly one succeeds, every other is answered
/// `AlreadyExists`, and the object holds the winner's bytes, whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_concurrent_writers_of_one_key_exactly_one_wins() {
    let dir = scratch("race");
    let store = Arc::new(FsStore::open(&dir, FLOOR).expect("open"));
    for round in 0..20 {
        let k = key(&format!("race/{round}"));
        let writers: Vec<_> = (0..4u8)
            .map(|w| {
                let (store, k) = (Arc::clone(&store), k.clone());
                let body = Bytes::from(vec![w; 64 << 10]);
                tokio::spawn(async move { (body.clone(), store.put_new(&k, body, None).await) })
            })
            .collect();
        let mut won = Vec::new();
        for writer in writers {
            match writer.await.expect("a writer") {
                (body, Ok(())) => won.push(body),
                (_, Err(ObjectStoreError::AlreadyExists(_))) => {}
                (_, Err(e)) => panic!("round {round}: {e}"),
            }
        }
        assert_eq!(
            won.len(),
            1,
            "round {round}: {} writers succeeded",
            won.len()
        );
        let got = store.get_range(&k, all(64 << 10)).await.expect("read");
        assert_eq!(got, won[0], "round {round}: not the winner's bytes");
    }
    assert_eq!(
        std::fs::read_dir(dir.join("tmp")).expect("tmp").count(),
        0,
        "a temporary file was left behind"
    );
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a read that trusts the file under a key's name: a file holding another
/// key's object (moved, or a hash bug) is refused as a protocol break, never returned
/// as the key's bytes; and a file too short for its header (a final name whose bytes
/// never reached the disk) or of zero bytes is no object: the read is `NotFound`,
/// `may_hold` is false, and a listing skips it. A whole object `may_hold`.
#[tokio::test]
async fn a_file_that_is_not_the_key_s_object_is_refused() {
    let dir = scratch("foreign");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    let (a, b, c, z) = (key("a"), key("b"), key("c"), key("z"));
    store
        .put_new(&a, Bytes::from_static(b"object a"), None)
        .await
        .expect("put a");
    assert!(store.may_hold(&a).await.expect("may_hold a"));
    std::fs::copy(file_of(&dir, &a), file_of(&dir, &b)).expect("copy a's file to b's name");
    let Err(ObjectStoreError::Protocol(why)) = store.get_range(&b, all(8)).await else {
        panic!("a's object read as b's");
    };
    assert!(why.contains("holds object a, not b"), "{why}");
    assert!(!store.may_hold(&b).await.expect("may_hold b"));

    std::fs::write(file_of(&dir, &c), b"kbf-obj1").expect("a header cut short");
    std::fs::write(file_of(&dir, &z), b"").expect("a file of zero bytes");
    for k in [&c, &z] {
        let read = store.get_range(k, all(8)).await;
        assert!(
            matches!(&read, Err(ObjectStoreError::NotFound(nf)) if nf == k),
            "{k}: {read:?}"
        );
        assert!(!store.may_hold(k).await.expect("may_hold"), "{k}");
    }
    assert!(!store.may_hold(&key("never")).await.expect("may_hold"));
    let listed = store
        .list(&KeyPrefix::new("").expect("prefix"), None, PageSize::MAX)
        .await
        .expect("list");
    let keys: Vec<&str> = listed.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["a"], "{listed:?}");
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a write past the free-space floor: with a floor no file system meets the
/// write is refused as a transport error that says how much is free and that room is
/// being made, and nothing is stored; with the tiny floor the same write is stored.
#[tokio::test]
async fn a_write_under_the_free_space_floor_is_refused() {
    let dir = scratch("floor");
    let k = key("k");
    let body = Bytes::from_static(b"body");
    let full = FsStore::open(&dir, Floor::Bytes(u64::MAX)).expect("open");
    let Err(e @ ObjectStoreError::Transport(_)) = full.put_new(&k, body.clone(), None).await else {
        panic!("a write under the floor was stored");
    };
    let text = e.to_string();
    assert!(text.contains("bytes free, under the floor of"), "{text}");
    assert!(text.contains("being evicted to make room"), "{text}");
    assert!(!full.may_hold(&k).await.expect("may_hold"));
    drop(full);
    let roomy = FsStore::open(&dir, FLOOR).expect("open");
    roomy.put_new(&k, body.clone(), None).await.expect("put");
    assert_eq!(roomy.get_range(&k, all(4)).await.expect("read"), body);
    drop(roomy);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a shard scan that reports the file's length rather than the object's (the
/// header counted), misses an object, reports one in the wrong shard, or takes a file
/// with no header for an object; and a floor check that disagrees with the write's.
#[tokio::test]
async fn a_scan_reads_each_shard_s_headers() {
    let dir = scratch("scan");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    assert!(!store.under_floor().expect("statvfs"));
    let before = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
    let keys = [key("a"), key("bb"), key("a/longer/key")];
    for (k, len) in keys.iter().zip([0, 5, 300]) {
        store
            .put_new(k, Bytes::from(vec![1; len]), None)
            .await
            .expect("put");
    }
    let headerless = file_of(&dir, &key("never-written"));
    std::fs::write(&headerless, b"kbf").expect("plant a file with no header");

    let mut found = Vec::new();
    let mut unreadable = 0;
    for shard in 0..=u8::MAX {
        let scan = store.scan(shard).await.expect("scan");
        assert_eq!(scan.opened(), scan.objects.len() + scan.unreadable);
        unreadable += scan.unreadable;
        for object in scan.objects {
            let file = file_of(&dir, &object.key);
            assert_eq!(
                file.parent(),
                Some(dir.join("objects").join(format!("{shard:02x}")).as_path())
            );
            assert!(object.mtime >= before, "{object:?}");
            found.push((object.key.as_str().to_owned(), object.len));
        }
    }
    found.sort();
    let want = [("a", 0), ("a/longer/key", 300), ("bb", 5)].map(|(k, n)| (k.to_owned(), n));
    assert_eq!(found, want);
    assert_eq!(unreadable, 1);
    assert_eq!(FsStore::SHARDS, 256);
    drop(store);
    let full = FsStore::open(&dir, Floor::Bytes(u64::MAX)).expect("open");
    assert!(full.under_floor().expect("statvfs"));
    drop(full);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a file system failure reported as anything but a transport error that names
/// the operation and the path (a missing object, or a success), on every call: the
/// directory a key's object goes in replaced by a file (so creating, reading,
/// removing and listing there fail), and `tmp/` removed (so a write cannot start).
#[tokio::test]
async fn file_system_failures_name_the_operation_and_the_path() {
    let dir = scratch("failures");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    let k = key("k");
    let object = file_of(&dir, &k);
    let shard = object.parent().expect("a shard").to_owned();
    std::fs::remove_dir(&shard).expect("remove the shard");
    std::fs::write(&shard, b"not a directory").expect("plant a file");
    let transport = |what: &str, r: Result<(), ObjectStoreError>, op: &str, path: &Path| {
        let Err(e @ ObjectStoreError::Transport(_)) = r else {
            panic!("{what}: {r:?}");
        };
        let text = e.to_string();
        let want = format!("request to the store failed: {op} {}: ", path.display());
        assert!(text.starts_with(&want), "{what}: {text}");
    };
    let body = Bytes::from_static(b"body");
    let put = store.put_new(&k, body.clone(), None).await;
    transport("put", put, "link", &object);
    let get = store.get_range(&k, all(4)).await.map(drop);
    transport("get", get, "open", &object);
    transport("delete", store.delete(&k).await, "remove", &object);
    let may_hold = store.may_hold(&k).await.map(drop);
    transport("may_hold", may_hold, "open", &object);
    let everything = KeyPrefix::new("").expect("prefix");
    let list = store.list(&everything, None, PageSize::MAX).await.map(drop);
    transport("list", list, "list", &shard);
    assert_eq!(
        std::fs::read_dir(dir.join("tmp")).expect("tmp").count(),
        0,
        "a failed write left its temporary file"
    );

    std::fs::remove_dir(dir.join("tmp")).expect("remove tmp");
    let put = store.put_new(&key("other"), body, None).await;
    transport("put without tmp", put, "create", &dir.join("tmp").join("1"));
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches an open that reports a directory it cannot create as anything but an I/O
/// error naming the operation and the path.
#[test]
fn an_open_that_cannot_create_the_directory_says_where() {
    let dir = scratch("unopenable");
    std::fs::create_dir_all(&dir).expect("scratch");
    let file = dir.join("a-file");
    std::fs::write(&file, b"not a directory").expect("plant");
    let Err(e) = FsStore::open(&file.join("store"), FLOOR) else {
        panic!("a store opened under a file");
    };
    assert!(matches!(e, FsStoreError::Io { .. }), "{e:?}");
    let text = e.to_string();
    let want = format!("create {}: ", file.join("store").display());
    assert!(text.starts_with(&want), "{text}");
    std::fs::remove_dir_all(&dir).expect("remove");
}

const GIB: u64 = 1 << 30;

/// Catches a default floor that is not the file system's 5%, at least 1 GiB, never
/// more than the file system (mutants: `Floor::KeepFree` a fixed 1 GiB, or any rule
/// but `kbf_threads::disk::keep_free`, which the daemon's caches keep too; there, no
/// minimum, a percentage other than 5, the cap dropped), and a store that reads it
/// from anywhere but its space source or
/// applies it to anything but the write: on a fake 100 GiB file system the floor is
/// 5 GiB, on a 4 GiB one 1 GiB, on a 512 MiB one all of it; a write that would leave
/// less than the floor free is refused, one that leaves exactly the floor is stored.
#[tokio::test]
async fn keep_free_is_5_percent_and_at_least_1_gib() {
    let keep_free = |total| Floor::KeepFree.bytes(total);
    assert_eq!(keep_free(100 * GIB), 5 * GIB);
    assert_eq!(keep_free(4 * GIB), GIB);
    assert_eq!(keep_free(20 * GIB), GIB);
    assert_eq!(keep_free(21 * GIB), 21 * GIB / 20);
    assert_eq!(keep_free(GIB / 2), GIB / 2);
    assert_eq!(Floor::Bytes(7).bytes(100 * GIB), 7);

    let dir = scratch("keep-free");
    let available = Arc::new(AtomicU64::new(5 * GIB + 4));
    let fake = Arc::clone(&available);
    let space = Arc::new(move |_: &Path| -> std::io::Result<Space> {
        Ok(Space {
            total: 100 * GIB,
            available: fake.load(Ordering::Relaxed),
        })
    });
    let store = FsStore::open_with_space(&dir, Floor::KeepFree, space).expect("open");
    assert_eq!(
        store.room().expect("room"),
        Room {
            available: 5 * GIB + 4,
            floor: 5 * GIB
        }
    );
    assert!(!store.under_floor().expect("room"));
    let body = Bytes::from_static(b"body");
    store
        .put_new(&key("fits"), body.clone(), None)
        .await
        .expect("leaves the floor");
    available.store(5 * GIB + 3, Ordering::Relaxed);
    let refused = store.put_new(&key("does-not"), body, None).await;
    assert!(
        matches!(refused, Err(ObjectStoreError::Transport(_))),
        "{refused:?}"
    );
    available.store(5 * GIB - 1, Ordering::Relaxed);
    assert!(store.under_floor().expect("room"));
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a refused write that wakes no evictor (mutant: no notify in the floor
/// check), so a full store waits for the evictor's next tick while every upload fails:
/// a waiter on `refused_full` wakes at the refusal, and a refusal with no one waiting
/// yet wakes the next waiter at once. A stored write wakes no one.
#[tokio::test]
async fn a_refused_write_wakes_refused_full() {
    let dir = scratch("refused-full");
    let available = Arc::new(AtomicU64::new(GIB));
    let fake = Arc::clone(&available);
    let space = Arc::new(move |_: &Path| -> std::io::Result<Space> {
        Ok(Space {
            total: 100 * GIB,
            available: fake.load(Ordering::Relaxed),
        })
    });
    let store = FsStore::open_with_space(&dir, Floor::Bytes(GIB / 2), space).expect("open");
    let body = Bytes::from_static(b"body");
    store
        .put_new(&key("stored"), body.clone(), None)
        .await
        .expect("put");
    let quiet = tokio::time::timeout(Duration::from_millis(50), store.refused_full()).await;
    assert!(quiet.is_err(), "a stored write woke the evictor");

    available.store(GIB / 2, Ordering::Relaxed);
    let waiter = tokio::spawn({
        let store = store.clone();
        async move { store.refused_full().await }
    });
    tokio::task::yield_now().await;
    let refused = store.put_new(&key("refused"), body.clone(), None).await;
    assert!(refused.is_err());
    tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("the waiter woke")
        .expect("no panic");
    let refused = store.put_new(&key("again"), body, None).await;
    assert!(refused.is_err());
    tokio::time::timeout(Duration::from_secs(10), store.refused_full())
        .await
        .expect("a refusal before the wait wakes it");
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a fake file system for eviction tests that does not count the store's own
/// files as used (mutant: nothing counted): what it reports free is
/// its size less every object file's bytes, headers included, and a deleted file
/// gives its bytes back.
#[tokio::test]
async fn a_file_system_of_objects_counts_the_object_files() {
    let dir = scratch("of-objects");
    let total = 1 << 20;
    let store =
        FsStore::open_with_space(&dir, Floor::Bytes(0), objects_source(total)).expect("open");
    assert_eq!(store.room().expect("room").available, total);
    let keys = [key("a"), key("b"), key("c")];
    for k in &keys {
        store
            .put_new(k, Bytes::from(vec![0; 1000]), None)
            .await
            .expect("put");
    }
    let files: u64 = keys
        .iter()
        .map(|k| std::fs::metadata(file_of(&dir, k)).expect("a file").len())
        .sum();
    assert!(files > 3000, "{files}");
    assert_eq!(store.room().expect("room").available, total - files);
    store.delete(&keys[0]).await.expect("delete");
    let one = std::fs::metadata(file_of(&dir, &keys[1]))
        .expect("a file")
        .len();
    assert_eq!(store.room().expect("room").available, total - 2 * one);
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches: a streamed write that failed (its body broke, or ended short) leaving its
/// file in `tmp/` until the next open, or anything under its key; and a streamed write
/// that is not written piece by piece to one file (the object reads back other than
/// sent).
#[tokio::test]
async fn a_failed_streamed_write_leaves_no_file() {
    use futures::StreamExt;
    let dir = scratch("streamed");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    let pieces = |tail: Option<ObjectStoreError>| {
        let mut out: Vec<Result<Bytes, ObjectStoreError>> = vec![
            Ok(Bytes::from_static(b"first ")),
            Ok(Bytes::from_static(b"second")),
        ];
        out.extend(tail.map(Err));
        futures::stream::iter(out).boxed()
    };
    let k = key("streamed/ok");
    store.put_stream(&k, 12, pieces(None)).await.expect("put");
    assert_eq!(store.get_range(&k, all(12)).await.unwrap(), "first second");
    let broke = ObjectStoreError::Body("the source broke".into());
    for (name, len, tail) in [
        ("broke", 12, Some(broke)),
        ("short", 13, None),
        ("long", 11, None),
    ] {
        let k = key(&format!("streamed/{name}"));
        let wrote = store.put_stream(&k, len, pieces(tail)).await;
        assert!(
            matches!(wrote, Err(ObjectStoreError::Body(_))),
            "{name}: {wrote:?}"
        );
        assert!(!file_of(&dir, &k).exists(), "{name}: an object file");
    }
    let left = std::fs::read_dir(dir.join("tmp")).expect("tmp").count();
    assert_eq!(left, 0, "files left in tmp/");
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}
