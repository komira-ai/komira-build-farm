//! The data plane's conformance checks (`kbf-cas-conformance`) through the storage
//! boundary: [`LocalDataPlane`] over a `LocalStore` built from an
//! `Arc<dyn kbf_storage::Storage>` ([`kbf_server::LocalStore::over`]), that storage
//! `kbf-front`'s adapter ([`CacheStorage`]) over an empty in-memory cache, wrapped in
//! [`Counted`], which counts the calls that reach it. So every check passes with the
//! plane knowing the store only as the trait, and [`every_call_crosses_the_boundary`]
//! shows the plane's calls arrive there.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures::StreamExt;
use kbf_all_in_one::LocalDataPlane;
use kbf_cas_conformance::{Subject, patterned};
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_server::LocalStore;
use kbf_storage::{Answer, PutStream, ReadStream, Storage};
use kbf_transfer::{Cas, Record, digest_of};
use kbf_types::Digest;

/// The calls that reached a [`Counted`] storage, by kind.
#[derive(Default)]
struct Calls {
    find_missing: AtomicUsize,
    read_blob: AtomicUsize,
    read_range: AtomicUsize,
    put: AtomicUsize,
    segments: AtomicUsize,
}

/// A storage that counts each call and passes it to the storage it wraps.
struct Counted {
    inner: Arc<dyn Storage>,
    calls: Arc<Calls>,
}

fn count(n: &AtomicUsize) {
    n.fetch_add(1, Ordering::SeqCst);
}

impl Storage for Counted {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        count(&self.calls.find_missing);
        self.inner.find_missing(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        count(&self.calls.read_blob);
        self.inner.read_blob(digest)
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        count(&self.calls.read_range);
        self.inner.read_range(digest, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        count(&self.calls.put);
        self.inner.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    fn put_streamed(
        &self,
        size: u64,
        bytes: futures::stream::BoxStream<'static, std::io::Result<Bytes>>,
        claim: futures::future::BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        self.inner.put_streamed(size, bytes, claim)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        count(&self.calls.segments);
        self.inner.segments()
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.inner.action_result(action)
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(result)
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, action, record)
    }
}

/// A new in-memory cache behind the boundary, and the calls that reach it.
fn counted() -> (Counted, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let storage = Counted {
        inner: Arc::new(CacheStorage::new(
            Arc::new(Cache::memory()),
            SegmentsConfig::DEFAULT,
        )),
        calls: Arc::clone(&calls),
    };
    (storage, calls)
}

/// A plane over a new in-memory cache behind the boundary, and the calls that reach it.
fn plane() -> (LocalDataPlane, Arc<Calls>) {
    let (storage, calls) = counted();
    let store = LocalStore::over(Arc::new(storage));
    let plane = LocalDataPlane::new(store, tokio::runtime::Handle::current());
    (plane, calls)
}

struct Seam {
    scratch: PathBuf,
}

impl Seam {
    fn new() -> Self {
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-all-in-one-storage-seam")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&scratch).expect("scratch directory");
        Self { scratch }
    }
}

impl Subject for Seam {
    type Cas = LocalDataPlane;

    async fn fresh(&self) -> Arc<LocalDataPlane> {
        Arc::new(plane().0)
    }

    fn scratch(&self) -> &Path {
        &self.scratch
    }
}

kbf_cas_conformance::tests!(Seam::new());

/// Catches a plane that reaches the cache some other way than the boundary it was
/// given: each of its kinds of call (FindMissing, a whole-blob write, a read, and an
/// output upload through `Segments`) arrives at the storage's own method.
#[tokio::test(flavor = "multi_thread")]
async fn every_call_crosses_the_boundary() {
    let (plane, calls) = plane();
    let whole = plane.put(patterned(1, 100)).await.expect("put");
    assert_eq!(plane.get(&whole).await.expect("get"), patterned(1, 100));
    let bytes = patterned(2, 1000);
    let digest = digest_of(&bytes);
    plane
        .put_records(vec![Record::bytes(bytes)])
        .await
        .expect("records");
    assert!(plane.missing(&[whole, digest]).await.is_empty());
    for (name, n) in [
        ("find_missing", &calls.find_missing),
        ("read_blob", &calls.read_blob),
        ("put", &calls.put),
        ("segments", &calls.segments),
    ] {
        assert!(
            n.load(Ordering::SeqCst) > 0,
            "no {name} call reached the storage"
        );
    }
}

/// Catches a ranged read that is not the storage's own: through the boundary, the bytes
/// `[offset, offset + limit)` of a blob arrive as the storage's stream, and the call is
/// counted.
#[tokio::test]
async fn a_ranged_read_crosses_the_boundary() {
    let (storage, calls) = counted();
    let bytes = patterned(3, 100);
    let digest = storage.put(Bytes::from(bytes.clone())).await.expect("put");
    let chunks: Vec<_> = storage
        .read_range(&digest, 10, 20)
        .await
        .expect("read_range")
        .collect()
        .await;
    let got: Vec<u8> = chunks
        .into_iter()
        .flat_map(|c| c.expect("chunk").to_vec())
        .collect();
    assert_eq!(got, bytes[10..30]);
    assert_eq!(calls.read_range.load(Ordering::SeqCst), 1);
}
