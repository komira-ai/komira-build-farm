//! `listing::objects`, the stream of every object under a prefix: the same cases over
//! every local store (the in-memory fake and a `file://` directory; the S3 store's are
//! in `s3_wire.rs` and, against real stores, in the conformance suite's listing
//! checks), that it asks for one page at a time, and that it refuses a store that
//! breaks the order it promises.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use futures::{StreamExt as _, TryStreamExt as _};
use kbf_objstore::listing::objects;
use kbf_objstore::{
    ByteRange, Capabilities, Floor, FsStore, KeyError, KeyPrefix, ListPage, ListToken, MemoryStore,
    ObjectInfo, ObjectKey, ObjectStore, ObjectStoreError, PageSize,
};

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).expect("a valid key")
}

fn prefix(p: &str) -> KeyPrefix {
    KeyPrefix::new(p).expect("a valid prefix")
}

fn size(n: u16) -> PageSize {
    PageSize::new(n).expect("in range")
}

/// The keys the shared cases write, in no order: seven under `p/` (nested under `/`
/// too), and keys either side of it that share its text.
const KEYS: [&str; 10] = [
    "p/k3", "p/k1", "p/a/b/c", "p/k0", "p/k2", "p/a/b", "p/k4", "o", "p", "q/k0",
];

/// The keys under `p/`, in key order.
const UNDER_P: [&str; 7] = ["p/a/b", "p/a/b/c", "p/k0", "p/k1", "p/k2", "p/k3", "p/k4"];

async fn write_keys<S: ObjectStore>(store: &S) {
    for (i, name) in KEYS.iter().enumerate() {
        let body = Bytes::from(vec![7; i + 1]);
        store.put_new(&key(name), body, None).await.expect("put");
    }
}

async fn listed<S: ObjectStore>(
    store: &S,
    under: &KeyPrefix,
    from: Option<ListToken>,
    page: u16,
) -> Vec<String> {
    objects(store, under, from, size(page))
        .map_ok(|o| o.key.as_str().to_owned())
        .try_collect()
        .await
        .expect("listing")
}

/// The shared cases, over `store`.
async fn every_case<S: ObjectStore>(store: &S) {
    write_keys(store).await;
    let p = prefix("p/");
    // Pages of one key, a short last page, an exact multiple, all on one page.
    for page in [1, 2, 3, 7, 1000] {
        assert_eq!(
            listed(store, &p, None, page).await,
            UNDER_P,
            "pages of {page}"
        );
    }
    // Sizes come with the keys.
    let sized: Vec<ObjectInfo> = objects(store, &p, None, size(2))
        .try_collect()
        .await
        .expect("listing");
    let sizes: Vec<u64> = sized.iter().map(|o| o.size).collect();
    assert_eq!(sizes, [6, 3, 4, 2, 5, 1, 7]);
    // The empty prefix is the whole bucket, in key order.
    let mut all: Vec<&str> = KEYS.to_vec();
    all.sort_unstable();
    assert_eq!(listed(store, &KeyPrefix::default(), None, 3).await, all);
    // A prefix that matches nothing, and one that is a key's whole text.
    assert!(listed(store, &prefix("p/z/"), None, 2).await.is_empty());
    assert!(listed(store, &prefix("n"), None, 2).await.is_empty());
    assert_eq!(listed(store, &prefix("p/a/b"), None, 1).await, UNDER_P[..2]);
    // Resumed after a key that exists and after one that does not.
    let after = |k: &str| Some(ListToken::after(&key(k)));
    assert_eq!(listed(store, &p, after("p/k1"), 2).await, UNDER_P[4..]);
    assert_eq!(listed(store, &p, after("p/k1x"), 2).await, UNDER_P[4..]);
    assert!(listed(store, &p, after("p/k4"), 2).await.is_empty());
}

/// Catches: a stream that loses or repeats a key at a page boundary (every page size
/// from 1 up, an exact multiple included), lists outside its prefix or not in key
/// order, groups keys at `/`, or drops sizes; and a resume that starts at, not after,
/// its key. Over the in-memory store.
#[tokio::test]
async fn every_case_over_the_memory_store() {
    every_case(&MemoryStore::new(Capabilities::default())).await;
}

/// The same over a `file://` store, whose later pages are cut from kept keys.
#[tokio::test]
async fn every_case_over_a_file_store() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-objstore-listing")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    let store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
    every_case(&store).await;
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// A listing resumed with [`ListToken::after`] a key, as a rebuild resumes after a
/// restart from the last key it handled: the store's own first page and the whole
/// stream hold exactly the keys after it, for a key at the start, the middle and the
/// end of the prefix, a key that was never written, and a key before the prefix.
async fn resumes_after_a_key<S: ObjectStore>(store: &S) {
    write_keys(store).await;
    let p = prefix("p/");
    let cases: [(&str, &[&str]); 5] = [
        ("o", &UNDER_P),
        ("p/a/b", &UNDER_P[1..]),
        ("p/k1", &UNDER_P[4..]),
        ("p/k1x", &UNDER_P[4..]),
        ("p/k4", &[]),
    ];
    for (k, rest) in cases {
        let from = ListToken::after(&key(k));
        // The store itself, not only the stream (which refuses a key not after `k`).
        let first = store.list(&p, Some(&from), size(2)).await.expect("list");
        let keys: Vec<&str> = first.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, rest[..rest.len().min(2)], "first page after {k}");
        for page in [1, 2, 1000] {
            assert_eq!(
                listed(store, &p, Some(from.clone()), page).await,
                rest,
                "after {k} in pages of {page}"
            );
        }
    }
}

/// Catches: a resume that starts at its key (an inclusive `after`) or back at the start
/// of the prefix, over the in-memory store. A resume that skipped a key would leave a
/// segment out of a rebuilt index: a false miss once the rebuild ends.
#[tokio::test]
async fn a_listing_resumed_after_a_key_lists_only_the_rest_over_the_memory_store() {
    resumes_after_a_key(&MemoryStore::new(Capabilities::default())).await;
}

/// The same over a `file://` store (its window crossing is a unit test in `fs_store.rs`).
#[tokio::test]
async fn a_listing_resumed_after_a_key_lists_only_the_rest_over_a_file_store() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-objstore-listing-resumed")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    let store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
    resumes_after_a_key(&store).await;
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches: a prefix or key with a character outside kbf's alphabet (here, non-ASCII)
/// reaching a store, where S3 and a local store would encode or compare it differently.
#[test]
fn a_prefix_or_key_outside_the_alphabet_is_refused() {
    assert!(matches!(
        KeyPrefix::new("caf\u{e9}/"),
        Err(KeyError::Character { ch: '\u{e9}', .. })
    ));
    assert!(matches!(
        ObjectKey::new("p/\u{1f600}"),
        Err(KeyError::Character {
            ch: '\u{1f600}',
            ..
        })
    ));
}

/// A store that answers `list` from a script of pages, counting the calls.
struct Scripted {
    pages: Mutex<VecDeque<Result<ListPage, ObjectStoreError>>>,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(pages: Vec<Result<ListPage, ObjectStoreError>>) -> Self {
        Self {
            pages: Mutex::new(pages.into()),
            calls: AtomicUsize::new(0),
        }
    }
}

fn page(keys: &[&str], next: Option<&str>) -> Result<ListPage, ObjectStoreError> {
    Ok(ListPage {
        objects: keys
            .iter()
            .map(|k| ObjectInfo {
                key: key(k),
                size: 1,
            })
            .collect(),
        next: next.map(|k| ListToken::after(&key(k))),
    })
}

impl ObjectStore for Scripted {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn put_new(
        &self,
        _: &ObjectKey,
        _: Bytes,
        _: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        unreachable!("the listing tests write nothing")
    }

    async fn get_range(&self, _: &ObjectKey, _: ByteRange) -> Result<Bytes, ObjectStoreError> {
        unreachable!("the listing tests read nothing")
    }

    async fn delete(&self, _: &ObjectKey) -> Result<(), ObjectStoreError> {
        unreachable!("the listing tests delete nothing")
    }

    async fn list(
        &self,
        _: &KeyPrefix,
        _: Option<&ListToken>,
        _: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.pages.lock().expect("lock").pop_front();
        next.expect("the stream asked past the scripted pages")
    }
}

/// The keys `store` streams under `p/`, and the error that ended the stream, if any.
async fn run(store: &Scripted, from: Option<ListToken>) -> (Vec<String>, Option<String>) {
    let p = prefix("p/");
    let mut stream = std::pin::pin!(objects(store, &p, from, size(2)));
    let mut keys = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(o) => keys.push(o.key.as_str().to_owned()),
            Err(e) => {
                // Nothing after an error.
                assert!(stream.next().await.is_none());
                return (keys, Some(e.to_string()));
            }
        }
    }
    (keys, None)
}

/// Catches: a stream that lists every page before yielding (memory growing with the
/// prefix), or that asks for a page past the last: one key taken from a two-page
/// listing asks for one page, and the whole listing asks for exactly two.
#[tokio::test]
async fn the_stream_asks_for_one_page_at_a_time() {
    let store = Scripted::new(vec![
        page(&["p/a", "p/b"], Some("p/b")),
        page(&["p/c"], None),
    ]);
    let p = prefix("p/");
    let first: Vec<_> = objects(&store, &p, None, size(2))
        .take(1)
        .try_collect()
        .await
        .expect("listing");
    assert_eq!(first.len(), 1);
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    let store = Scripted::new(vec![
        page(&["p/a", "p/b"], Some("p/b")),
        page(&["p/c"], None),
    ]);
    assert_eq!(
        run(&store, None).await,
        (vec!["p/a".into(), "p/b".into(), "p/c".into()], None)
    );
    assert_eq!(store.calls.load(Ordering::SeqCst), 2);
}

/// Catches: a stream that hands on whatever a store lists. Each broken answer ends the
/// stream with a protocol error naming what broke, after the keys before it: a key
/// outside the prefix, keys out of order within a page, a page that repeats the last
/// key of the one before (a page served twice), a first key not after the key the
/// listing resumed from, and an empty page whose token is the one it was asked with
/// (a store that would page forever).
#[tokio::test]
async fn a_store_that_breaks_the_order_is_refused() {
    let protocol = |what: &str| Some(format!("store broke the protocol: listing \"p/\" {what}"));
    let outside = Scripted::new(vec![page(&["p/a", "q/b"], None)]);
    assert_eq!(
        run(&outside, None).await,
        (vec![], protocol("returned q/b, which is outside it"))
    );
    let unordered = Scripted::new(vec![page(&["p/b", "p/a"], None)]);
    assert_eq!(
        run(&unordered, None).await,
        (vec![], protocol("returned p/a after p/b: keys must ascend"))
    );
    let repeated = Scripted::new(vec![
        page(&["p/a", "p/b"], Some("p/b")),
        page(&["p/b"], None),
    ]);
    assert_eq!(
        run(&repeated, None).await,
        (
            vec!["p/a".into(), "p/b".into()],
            protocol("returned p/b after p/b: keys must ascend")
        )
    );
    let at_the_key = Scripted::new(vec![page(&["p/b", "p/c"], None)]);
    assert_eq!(
        run(&at_the_key, Some(ListToken::after(&key("p/b")))).await,
        (vec![], protocol("returned p/b after p/b: keys must ascend"))
    );
    let stuck = Scripted::new(vec![page(&["p/a"], Some("p/a")), page(&[], Some("p/a"))]);
    assert_eq!(
        run(&stuck, None).await,
        (
            vec!["p/a".into()],
            Some(
                "store broke the protocol: listing \"p/\": an empty page whose token is the one it was asked with"
                    .into()
            )
        )
    );
}

/// Catches: a stream that ends quietly on a failed page (a rebuild would take a part
/// for the whole), or that goes on past it: the keys before, then the store's error.
/// An empty page with a new token is not an error: the stream asks again.
#[tokio::test]
async fn a_failed_page_ends_the_stream_with_its_error() {
    let store = Scripted::new(vec![
        page(&["p/a"], Some("p/a")),
        page(&[], Some("p/b")),
        Err(ObjectStoreError::Service {
            status: 403,
            code: "AccessDenied".into(),
            message: "no".into(),
        }),
    ]);
    assert_eq!(
        run(&store, None).await,
        (
            vec!["p/a".into()],
            Some("store answered 403 AccessDenied: no".into())
        )
    );
    assert_eq!(store.calls.load(Ordering::SeqCst), 3);
}

/// Catches: a stream that refuses, or asks past, an empty last page (S3 may answer a
/// truncated page's token with an empty, untruncated one): the keys before it, no
/// error, and no call after it.
#[tokio::test]
async fn an_empty_last_page_ends_the_stream() {
    let store = Scripted::new(vec![page(&["p/a"], Some("p/a")), page(&[], None)]);
    assert_eq!(run(&store, None).await, (vec!["p/a".into()], None));
    assert_eq!(store.calls.load(Ordering::SeqCst), 2);
}
