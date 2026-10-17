//! [`LocalDataPlane`] as the one adapter from the daemon's `Cas` to the storage boundary
//! (`kbf storage-and-daemon`'s data plane), one suite run over both kinds of storage:
//!
//! - **in-process**: the plane over `kbf-front`'s [`CacheStorage`] in this process, as
//!   `kbf storage-and-daemon` gives it the storage node's;
//! - **remote**: the same storage served by the storage service on a loopback socket
//!   (`kbf_server::bind_storage`, plain text) and the plane over a [`RemoteStorage`]
//!   to it, as a daemon-only node will read a storage node.
//!
//! The storage the plane sees is wrapped in [`Seen`], which counts its whole-blob reads,
//! its ranged reads and the chunks they deliver; the storage served can cut a read short
//! (with an error, or cleanly), change a byte of it, or run it past the blob ([`Damage`]). Each check runs on both arms, and the
//! conformance suite (`kbf-cas-conformance`) runs on the remote arm (the in-process arm
//! is tests/storage_seam.rs's).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_all_in_one::LocalDataPlane;
use kbf_cas_conformance::{Subject, patterned};
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_server::{LocalStore, StorageListener};
use kbf_storage::service::RemoteStorage;
use kbf_storage::{Answer, PutStream, READ_CHUNK_BYTES, ReadStream, Storage, StorageError};
use kbf_transfer::{Cas as _, CasError, digest_of, label};
use kbf_types::Digest;

/// What the served storage does to the bytes of a ranged read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Damage {
    /// Nothing: the cache's own read.
    None,
    /// The first chunk, then an error item (the store failed mid-read).
    Cut,
    /// Every chunk, the first with one byte changed; the stream ends cleanly.
    Flip,
    /// The first chunk; the stream ends cleanly, short.
    Short,
    /// Every chunk, then [`EXTRA_CHUNKS`] more of zeros: a stream that runs past the blob.
    Long,
}

/// How many chunks past the blob a [`Damage::Long`] read sends.
const EXTRA_CHUNKS: usize = 8;

/// The storage served: a cache, its ranged reads damaged as `damage` says.
struct Served {
    inner: Arc<dyn Storage>,
    damage: Damage,
}

/// The chunks of `stream` as `damage` says, for the blob `digest`.
fn damaged(stream: ReadStream, damage: Damage, digest: Digest) -> ReadStream {
    match damage {
        Damage::None => stream,
        Damage::Cut => stream
            .take(1)
            .chain(futures::stream::iter([Err(StorageError::Unreachable(
                digest,
            ))]))
            .boxed(),
        Damage::Flip => stream
            .enumerate()
            .map(|(i, chunk)| {
                chunk.map(|bytes| {
                    let mut bytes = bytes.to_vec();
                    bytes[0] ^= u8::from(i == 0);
                    Bytes::from(bytes)
                })
            })
            .boxed(),
        Damage::Short => stream.take(1).boxed(),
        Damage::Long => {
            let zeros = Bytes::from(vec![0; READ_CHUNK_BYTES]);
            let extra = futures::stream::iter(vec![Ok(zeros); EXTRA_CHUNKS]);
            stream.chain(extra).boxed()
        }
    }
}

/// A storage that passes each call to the one it wraps (`$inner`), but for
/// `read_blob` and `read_range`, which the wrapper defines (`$($own)*`).
macro_rules! passing {
    ($ty:ty, $($own:tt)*) => {
        impl Storage for $ty {
            $($own)*

            fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
                self.inner.find_missing(digests)
            }

            fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
                self.inner.put(bytes)
            }

            fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
                self.inner.stream(size)
            }

            fn segments(&self) -> Arc<dyn Segments> {
                self.inner.segments()
            }

            fn action_result<'a>(
                &'a self,
                action: &'a Digest,
            ) -> Answer<'a, Option<(Digest, ActionResult)>> {
                self.inner.action_result(action)
            }

            fn prepare_action_result<'a>(
                &'a self,
                result: &'a ActionResult,
            ) -> Answer<'a, ActionRecord> {
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
    };
}

passing! {
    Served,
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        self.inner.read_blob(digest)
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            let stream = self.inner.read_range(digest, offset, limit).await?;
            Ok(damaged(stream, self.damage, *digest))
        })
    }
}

/// The storage as the plane sees it, counting its reads.
struct Seen {
    inner: Arc<dyn Storage>,
    /// `read_blob` calls: a blob held whole.
    whole: AtomicUsize,
    /// `read_range` calls.
    ranges: AtomicUsize,
    /// Chunks the ranged reads delivered.
    chunks: Arc<AtomicUsize>,
}

passing! {
    Seen,
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        self.whole.fetch_add(1, Ordering::SeqCst);
        self.inner.read_blob(digest)
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        self.ranges.fetch_add(1, Ordering::SeqCst);
        let chunks = Arc::clone(&self.chunks);
        Box::pin(async move {
            let stream = self.inner.read_range(digest, offset, limit).await?;
            Ok(stream
                .inspect(move |_| {
                    chunks.fetch_add(1, Ordering::SeqCst);
                })
                .boxed())
        })
    }
}

/// Where the plane's storage is.
#[derive(Clone, Copy, Debug)]
enum Arm {
    /// In this process.
    InProcess,
    /// Behind the storage service, on a loopback socket.
    Remote,
}

/// `storage` served by the storage service on a loopback port, for as long as the
/// runtime runs; a client to it.
async fn remote(storage: Arc<dyn Storage>) -> RemoteStorage {
    let listener = StorageListener {
        listen: "127.0.0.1:0".parse().expect("address"),
        tls: None,
        service: kbf_storage::service::Config {
            write_bytes_in_flight: 1 << 30,
        },
    };
    let bound = kbf_server::bind_storage(storage, listener, std::future::pending())
        .expect("bind the storage service");
    tokio::spawn(bound.serving);
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", bound.addr))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    RemoteStorage::new(channel, 0)
}

/// A plane on `arm` over a new in-memory cache whose reads are damaged as `damage`
/// says, and what the plane's storage saw.
async fn plane(arm: Arm, damage: Damage) -> (LocalDataPlane, Arc<Seen>) {
    let cache = CacheStorage::new(Arc::new(Cache::memory()), SegmentsConfig::DEFAULT);
    let served: Arc<dyn Storage> = Arc::new(Served {
        inner: Arc::new(cache),
        damage,
    });
    let reached: Arc<dyn Storage> = match arm {
        Arm::InProcess => served,
        Arm::Remote => Arc::new(remote(served).await),
    };
    let seen = Arc::new(Seen {
        inner: reached,
        whole: AtomicUsize::new(0),
        ranges: AtomicUsize::new(0),
        chunks: Arc::new(AtomicUsize::new(0)),
    });
    let store = LocalStore::over(Arc::clone(&seen) as Arc<dyn Storage>);
    let plane = LocalDataPlane::new(store, tokio::runtime::Handle::current());
    (plane, seen)
}

/// A file to read a blob into, new and empty, under this test binary's scratch.
async fn file(name: &str) -> (tokio::fs::File, PathBuf) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-all-in-one-storage-plane")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).expect("scratch");
    let path = dir.join(name);
    let file = tokio::fs::File::create(&path).await.expect("create");
    (file, path)
}

/// A blob larger than any chunk or buffer of the read path: four chunks of
/// [`READ_CHUNK_BYTES`] (the last short), more than a ByteStream message or the
/// input cache's copy buffer.
fn large() -> Vec<u8> {
    patterned(9, 3 * READ_CHUNK_BYTES + 7)
}

/// Catches a plane that reads a blob whole into memory to fill a file (the trait's
/// default `read_to`, through `get`: one `read_blob`), that reads it as one piece, or
/// that writes other bytes than the blob's: a blob of three chunks and seven bytes,
/// stored through the plane, is read into a file through one ranged read in exactly
/// four chunks, never a whole-blob read, and the file holds the blob. And a `get` that
/// has the storage hold the blob whole (`read_blob`, which over a `RemoteStorage`
/// collects a stream with no bound): it streams too, and answers the blob.
async fn a_large_blob_streams_into_its_file(arm: Arm) {
    let (plane, seen) = plane(arm, Damage::None).await;
    let blob = large();
    let digest = plane.put(blob.clone()).await.expect("put");
    assert_eq!(digest, digest_of(&blob));
    let (mut into, path) = file(&format!("large-{arm:?}")).await;
    plane.read_to(&digest, &mut into).await.expect("read_to");
    drop(into);
    assert!(std::fs::read(&path).expect("read") == blob, "other bytes");
    assert_eq!(seen.whole.load(Ordering::SeqCst), 0, "a whole-blob read");
    assert_eq!(seen.ranges.load(Ordering::SeqCst), 1);
    let chunks = seen.chunks.load(Ordering::SeqCst);
    assert_eq!(chunks, blob.len().div_ceil(READ_CHUNK_BYTES), "{arm:?}");
    assert!(
        plane.get(&digest).await.expect("get") == blob,
        "get: other bytes"
    );
    assert_eq!(
        seen.whole.load(Ordering::SeqCst),
        0,
        "get: a whole-blob read"
    );
    assert_eq!(seen.ranges.load(Ordering::SeqCst), 2);
}

/// Catches an absent blob answered as another failure (a daemon would retry a blob that
/// is not there, or not ask for it again): `read_to` and `get` both answer
/// [`CasError::Missing`] naming it, and the file is left empty.
async fn an_absent_blob_is_missing(arm: Arm) {
    let (plane, _) = plane(arm, Damage::None).await;
    let digest = digest_of(b"never stored");
    let (mut into, path) = file(&format!("absent-{arm:?}")).await;
    let read = plane.read_to(&digest, &mut into).await;
    assert_eq!(read, Err(CasError::Missing(label(&digest))));
    assert_eq!(
        plane.get(&digest).await,
        Err(CasError::Missing(label(&digest)))
    );
    assert_eq!(std::fs::metadata(&path).expect("stat").len(), 0);
}

/// Catches a read that fails after its first chunk taken for a whole blob, or for a
/// damaged one (the store failed, so the same read may pass later: UNAVAILABLE, not
/// "corrupt"): `read_to` and `get` answer [`CasError::Unavailable`], and the file holds
/// no more than the one chunk that arrived.
async fn a_read_cut_short_is_unavailable(arm: Arm) {
    let (plane, _) = plane(arm, Damage::Cut).await;
    let blob = large();
    let digest = plane.put(blob).await.expect("put");
    let (mut into, path) = file(&format!("cut-{arm:?}")).await;
    let read = plane.read_to(&digest, &mut into).await;
    assert!(
        matches!(&read, Err(CasError::Unavailable(blob, _)) if *blob == label(&digest)),
        "{arm:?}: {read:?}"
    );
    drop(into);
    let written = std::fs::metadata(&path).expect("stat").len();
    assert!(written <= READ_CHUNK_BYTES as u64, "{written}");
    let got = plane.get(&digest).await;
    assert!(
        matches!(&got, Err(CasError::Unavailable(blob, _)) if *blob == label(&digest)),
        "{arm:?}: get: {got:?}"
    );
}

/// What a damaged read answers through both reads, `read_to` and `get`, and how many
/// bytes the file holds after `read_to`.
async fn read_both(
    arm: Arm,
    damage: Damage,
) -> (kbf_proto::reapi::Digest, [Result<(), CasError>; 2], u64) {
    let (plane, _) = plane(arm, damage).await;
    let digest = plane.put(large()).await.expect("put");
    let (mut into, path) = file(&format!("{damage:?}-{arm:?}")).await;
    let read = plane.read_to(&digest, &mut into).await;
    drop(into);
    let written = std::fs::metadata(&path).expect("stat").len();
    let got = plane.get(&digest).await.map(drop);
    (digest, [read, got], written)
}

/// Catches a read whose bytes are not the blob's taken for the blob (the plane must
/// hash what it reads: a stream that ends cleanly is not proof), with one byte changed
/// or cut short with no error: `read_to` and `get` answer [`CasError::Corrupt`] naming
/// the blob.
async fn other_bytes_are_corrupt(arm: Arm) {
    for damage in [Damage::Flip, Damage::Short] {
        let (digest, answers, _) = read_both(arm, damage).await;
        for read in answers {
            assert!(
                matches!(&read, Err(CasError::Corrupt(blob, _)) if *blob == label(&digest)),
                "{arm:?} {damage:?}: {read:?}"
            );
        }
    }
}

/// Catches a read that writes on past the digest's size until the storage stops
/// sending (a storage that sends without end would fill the disk, or with `get` the
/// memory, before the hash is checked): a read that runs [`EXTRA_CHUNKS`] chunks past
/// the blob answers [`CasError::Corrupt`] as the transfer engine does ("more than N
/// bytes"), on both reads, and the file holds no byte past the blob's size.
async fn a_read_past_the_size_stops_there(arm: Arm) {
    let (digest, answers, written) = read_both(arm, Damage::Long).await;
    let size = large().len();
    let says = format!("more than {size} bytes");
    for read in answers {
        assert_eq!(read, Err(CasError::Corrupt(label(&digest), says.clone())));
    }
    assert!(written <= size as u64, "{arm:?}: wrote {written} of {size}");
}

/// Each check above, as a test on each arm.
macro_rules! on_both_arms {
    ($($check:ident)*) => {
        mod in_process {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $check() {
                    super::$check(super::Arm::InProcess).await;
                }
            )*
        }

        mod remote {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $check() {
                    super::$check(super::Arm::Remote).await;
                }
            )*
        }
    };
}

on_both_arms!(
    a_large_blob_streams_into_its_file
    an_absent_blob_is_missing
    a_read_cut_short_is_unavailable
    other_bytes_are_corrupt
    a_read_past_the_size_stops_there
);

/// The conformance suite's subject: a plane over a [`RemoteStorage`] to a new
/// in-memory cache served on a loopback socket.
struct Remote {
    scratch: PathBuf,
}

impl Remote {
    fn new() -> Self {
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-all-in-one-storage-plane-conformance")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&scratch).expect("scratch directory");
        Self { scratch }
    }
}

impl Subject for Remote {
    type Cas = LocalDataPlane;

    async fn fresh(&self) -> Arc<LocalDataPlane> {
        Arc::new(plane(Arm::Remote, Damage::None).await.0)
    }

    fn scratch(&self) -> &Path {
        &self.scratch
    }
}

mod remote_conformance {
    use super::Remote;

    kbf_cas_conformance::tests!(Remote::new());
}
