//! The daemon half's input cache filled over the in-process plane, as `kbf-all-in-one`
//! runs it: [`LocalDataPlane`] over a `LocalStore` whose storage ([`Watched`]) wraps
//! `kbf-front`'s adapter over an in-memory cache and watches the calls. A fill reads
//! the blob as the storage's stream and writes each chunk into the entry's file before
//! it takes many more (never the whole blob between the halves), and two leases that
//! need one input at once read it from storage once, into one file.

#![cfg(target_os = "linux")]

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_all_in_one::LocalDataPlane;
use kbf_daemon::cache::{Cache, CacheConfig};
use kbf_front::{CacheStorage, SegmentsConfig};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::{ActionResult, Directory, FileNode};
use kbf_proto::worker::segments_server::Segments;
use kbf_server::LocalStore;
use kbf_storage::{Answer, PutStream, READ_CHUNK_BYTES, ReadStream, Storage, StorageError};
use kbf_transfer::{Cas as _, CasError, label, proto_digest};
use kbf_types::Digest;
use prost::Message as _;

/// What [`Watched`] saw.
#[derive(Default)]
struct Seen {
    /// Whole-blob reads of the watched blob.
    whole: AtomicUsize,
    /// Streamed reads of the watched blob.
    streams: AtomicUsize,
    /// The most bytes the stream had handed out beyond what the fills' files held,
    /// measured before each chunk.
    lag: AtomicU64,
    /// Chunks of streams of the watched blob handed out.
    handed: AtomicUsize,
    /// A stream of the watched blob was dropped.
    dropped: AtomicBool,
}

/// Marks [`Seen::dropped`] when the stream holding it is dropped.
struct OnDrop(Arc<Seen>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}

/// A storage that passes every call to `inner` and watches the reads of one blob:
/// before it hands out each chunk of a stream of it, it measures how far the bytes in
/// `tmp` (the input cache's fill files) are behind the bytes handed out so far.
struct Watched {
    inner: Arc<dyn Storage>,
    blob: Mutex<Option<Digest>>,
    tmp: PathBuf,
    seen: Arc<Seen>,
    /// When set, what a streamed read of any blob answers instead.
    script: Mutex<Option<Vec<Result<Bytes, StorageError>>>>,
    /// When set, a stream of the watched blob hands out each chunk only for a permit.
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

impl Watched {
    fn watches(&self, digest: &Digest) -> bool {
        *self.blob.lock().expect("lock") == Some(*digest)
    }
}

/// The bytes of the files in `dir`.
fn bytes_in(dir: &Path) -> u64 {
    let files = std::fs::read_dir(dir).into_iter().flatten().flatten();
    files
        .filter_map(|f| f.metadata().ok())
        .map(|m| m.len())
        .sum()
}

impl Storage for Watched {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_missing(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        if self.watches(digest) {
            self.seen.whole.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.read_blob(digest)
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        let watched = self.watches(digest);
        let (tmp, seen) = (self.tmp.clone(), Arc::clone(&self.seen));
        let gate = self.gate.lock().expect("lock").clone();
        if let Some(script) = self.script.lock().expect("lock").take() {
            return Box::pin(async move { Ok(futures::stream::iter(script).boxed()) });
        }
        Box::pin(async move {
            let stream = self.inner.read_range(digest, offset, limit).await?;
            if !watched {
                return Ok(stream);
            }
            seen.streams.fetch_add(1, Ordering::SeqCst);
            let gated = stream.then(move |chunk| {
                let gate = gate.clone();
                async move {
                    if let Some(gate) = gate {
                        gate.acquire().await.expect("a permit").forget();
                    }
                    chunk
                }
            });
            let mut handed = 0;
            let on_drop = OnDrop(Arc::clone(&seen));
            Ok(gated
                .map(move |chunk| {
                    let seen = &on_drop.0;
                    seen.handed.fetch_add(1, Ordering::SeqCst);
                    let lag = handed - bytes_in(&tmp).min(handed);
                    seen.lag.fetch_max(lag, Ordering::SeqCst);
                    handed += chunk.as_ref().map_or(0, |c| c.len() as u64);
                    chunk
                })
                .boxed())
        })
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
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

/// One test's input cache in `dir/cache` (leases under `dir/leases`), and a plane
/// over a [`Watched`] in-memory store, its calls run on this runtime.
struct Kit {
    dir: PathBuf,
    cache: Arc<Cache>,
    plane: LocalDataPlane,
    storage: Arc<Watched>,
}

impl Kit {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-all-in-one-one-cache")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["cache", "leases"] {
            std::fs::create_dir_all(dir.join(sub)).expect("mkdir");
        }
        let config = CacheConfig::new(dir.join("cache"), dir.join("leases"), 1 << 30);
        let cache = Cache::open(config).expect("open");
        let storage = Arc::new(Watched {
            inner: Arc::new(CacheStorage::new(
                Arc::new(kbf_front::Cache::memory()),
                SegmentsConfig::DEFAULT,
            )),
            blob: Mutex::new(None),
            tmp: dir.join("cache/v1/tmp"),
            seen: Arc::default(),
            script: Mutex::new(None),
            gate: Mutex::new(None),
        });
        let store = LocalStore::over(Arc::clone(&storage) as Arc<dyn Storage>);
        let plane = LocalDataPlane::new(store, tokio::runtime::Handle::current());
        Self {
            dir,
            cache,
            plane,
            storage,
        }
    }

    /// Stores `blob` (watched) and an input root holding it as `input.bin`; the root.
    async fn input(&self, blob: Vec<u8>) -> kbf_proto::reapi::Digest {
        let digest = self.plane.put(blob).await.expect("put");
        let parsed = kbf_types::Digest::parse(
            kbf_types::DigestFunction::Sha256,
            &kbf_transfer::label(&digest),
        )
        .expect("a digest");
        *self.storage.blob.lock().expect("lock") = Some(parsed);
        let root = Directory {
            files: vec![FileNode {
                name: "input.bin".to_owned(),
                digest: Some(digest),
                ..FileNode::default()
            }],
            ..Directory::default()
        };
        self.plane.put(root.encode_to_vec()).await.expect("put")
    }

    /// Writes `root` into a fresh lease `name` through the input cache, over the plane.
    async fn lease(&self, root: &kbf_proto::reapi::Digest, name: &str) -> PathBuf {
        let at = self.dir.join("leases").join(name);
        std::fs::create_dir_all(&at).expect("mkdir");
        kbf_daemon::tree::materialize_cached(&self.plane, &self.cache, root, &at, &[])
            .await
            .expect("written");
        at
    }
}

/// A blob of twelve chunks of a stream and five bytes, none of whose runs of 251 bytes
/// repeats a neighbour's, larger than any buffer of the read path.
fn big() -> Vec<u8> {
    (0..12 * READ_CHUNK_BYTES + 5)
        .map(|i| (i % 251) as u8)
        .collect()
}

/// Catches a fill over the in-process plane that holds the blob whole between the
/// halves (the plane's `read_to` gathering the stream, or the plane's whole-blob
/// `get`, before writing: the "buffer" mutants), or that writes it anywhere but the
/// entry's file, or that lets more than two chunks sit between the halves (a wider
/// channel: the "channel(3)" mutant). A 12 MiB input reaches the lease's file through
/// one streamed read and no whole-blob read; before each chunk is handed out (the pump
/// holding none then), the fill's file holds all but at most three chunks of what came
/// before: one in the channel between the halves, one the fill holds while the file
/// takes it and one the file's writer is still writing. A channel of three would let
/// five lag, and a gathered blob eleven.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fill_streams_the_input_into_its_file() {
    let kit = Kit::new("stream");
    let blob = big();
    let root = kit.input(blob.clone()).await;
    let at = kit.lease(&root, "one").await;
    assert_eq!(std::fs::read(at.join("input.bin")).expect("read"), blob);
    let seen = &kit.storage.seen;
    assert_eq!(seen.whole.load(Ordering::SeqCst), 0, "a whole-blob read");
    assert_eq!(seen.streams.load(Ordering::SeqCst), 1);
    let lag = seen.lag.load(Ordering::SeqCst);
    assert!(
        lag <= 3 * READ_CHUNK_BYTES as u64,
        "the fill lagged {lag} bytes"
    );
    kit.cache.stop();
}

/// Catches two leases that need one input at once fetching it twice through the
/// plane, or keeping two files of it: one streamed read, one entry, and both leases'
/// names are that entry's inode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_leases_needing_one_input_read_it_once_into_one_file() {
    let kit = Kit::new("twice");
    let blob = big();
    let root = kit.input(blob.clone()).await;
    let (one, two) = tokio::join!(kit.lease(&root, "one"), kit.lease(&root, "two"));
    assert_eq!(kit.storage.seen.streams.load(Ordering::SeqCst), 1);
    let digest = proto_digest(&kbf_segments::sha256(&blob));
    let entry = kit
        .dir
        .join("cache/v1/blobs")
        .join(&digest.hash[..2])
        .join(format!("{}-{}", digest.hash, digest.size_bytes));
    let ino = |path: &Path| std::fs::metadata(path).expect("stat").ino();
    assert_eq!(ino(&one.join("input.bin")), ino(&entry));
    assert_eq!(ino(&two.join("input.bin")), ino(&entry));
    kit.cache.stop();
}

/// Catches the plane's pump reading on for a fill that is gone (a lease killed while
/// its inputs are written drops the fill: the "pump ignores a closed channel" mutant
/// reads the blob to its end for nobody) or keeping the storage's stream open: with the
/// stream handing out a chunk per permit, the fill is dropped once its first chunk is in
/// its file; the pump takes the one chunk it was waiting for, finds nobody to give it to
/// and drops the stream, two of thirteen chunks handed out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fill_dropped_midway_stops_the_storages_stream() {
    let kit = Kit::new("dropped");
    let digest = kit.plane.put(big()).await.expect("put");
    let parsed = kbf_types::Digest::parse(kbf_types::DigestFunction::Sha256, &label(&digest));
    *kit.storage.blob.lock().expect("lock") = Some(parsed.expect("a digest"));
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    *kit.storage.gate.lock().expect("lock") = Some(Arc::clone(&gate));
    let path = kit.dir.join("dropped");
    let mut file = tokio::fs::File::create(&path).await.expect("create");
    let seen = Arc::clone(&kit.storage.seen);
    {
        let read = kit.plane.read_to(&digest, &mut file);
        let first = async {
            while std::fs::metadata(&path).map_or(0, |m| m.len()) < READ_CHUNK_BYTES as u64 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        tokio::select! {
            read = read => panic!("the read ended on one chunk: {read:?}"),
            () = first => {}
        }
    }
    assert_eq!(seen.handed.load(Ordering::SeqCst), 1);
    gate.add_permits(100);
    for _ in 0..1000 {
        if seen.dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        seen.dropped.load(Ordering::SeqCst),
        "the stream is still open"
    );
    assert_eq!(seen.handed.load(Ordering::SeqCst), 2);
    kit.cache.stop();
}

/// `read_to` of `digest` over `kit`'s plane into a new file `name` opened for writing
/// (`writable`) or reading only.
async fn read_to(
    kit: &Kit,
    digest: &kbf_proto::reapi::Digest,
    name: &str,
    writable: bool,
) -> Result<(), CasError> {
    let path = kit.dir.join(name);
    std::fs::write(&path, b"").expect("create");
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(writable)
        .open(&path)
        .await
        .expect("open");
    kit.plane.read_to(digest, &mut file).await
}

/// Catches the plane's streamed read taking a damaged or cut stream for the blob: bytes
/// that hash to another digest are `Corrupt`, a stream that ends in an error is
/// `Unavailable` (the storage's UNAVAILABLE), a blob the storage lacks is `Missing`, a
/// digest that is not one is `Missing` without a call, and a file that cannot be
/// written is `Write`, whether a write or the final flush finds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_read_answers_what_went_wrong() {
    let kit = Kit::new("errors");
    let blob = big();
    let held = kit.plane.put(blob.clone()).await.expect("put");
    let small = kit.plane.put(b"small".to_vec()).await.expect("put");
    let wrong = Bytes::from_static(b"other bytes");
    let unreachable = StorageError::Unreachable(kbf_segments::sha256(b"small"));
    for (script, want) in [
        (vec![Ok(wrong)], "Corrupt"),
        (
            vec![Ok(Bytes::from_static(b"sm")), Err(unreachable)],
            "Unavailable",
        ),
    ] {
        *kit.storage.script.lock().expect("lock") = Some(script);
        let read = read_to(&kit, &small, "scripted", true).await;
        assert!(
            format!("{read:?}").starts_with(&format!("Err({want}(")),
            "{read:?}"
        );
    }
    let absent = proto_digest(&kbf_segments::sha256(b"absent"));
    assert_eq!(
        read_to(&kit, &absent, "absent", true).await,
        Err(CasError::Missing(label(&absent)))
    );
    let bad = kbf_proto::reapi::Digest {
        hash: "not hex".to_owned(),
        size_bytes: 3,
    };
    assert_eq!(
        read_to(&kit, &bad, "bad", true).await,
        Err(CasError::Missing(label(&bad)))
    );
    for digest in [&small, &held] {
        let read = read_to(&kit, digest, "read-only", false).await;
        assert!(matches!(read, Err(CasError::Write(..))), "{read:?}");
    }
    assert_eq!(read_to(&kit, &held, "whole", true).await, Ok(()));
    assert_eq!(std::fs::read(kit.dir.join("whole")).expect("read"), blob);
    kit.cache.stop();
}

/// Catches a streamed read over a server half that is gone hanging or taken for an
/// answer: it fails `Unavailable`.
#[test]
fn a_streamed_read_over_a_gone_server_half_is_unavailable() {
    let gone = tokio::runtime::Runtime::new().expect("runtime");
    let kit = gone.block_on(async { Kit::new("gone") });
    let digest = gone.block_on(kit.plane.put(b"held".to_vec())).expect("put");
    drop(gone);
    let caller = tokio::runtime::Runtime::new().expect("runtime");
    let read = caller.block_on(read_to(&kit, &digest, "gone", true));
    assert!(matches!(read, Err(CasError::Unavailable(..))), "{read:?}");
    kit.cache.stop();
}

/// `GET /metrics` on the operator API at `api`: each sample by name.
async fn scrape(api: std::net::SocketAddr) -> std::collections::BTreeMap<String, u64> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut tcp = tokio::net::TcpStream::connect(api).await.expect("connect");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut answer = String::new();
    tcp.read_to_string(&mut answer).await.expect("read");
    let samples = answer.lines().filter(|line| !line.starts_with('#'));
    samples
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect()
}

/// Catches the server half's write tier not reporting its bytes to the one cache (the
/// input cache's budget would then leave the hot store out: the "storage never
/// attached" and "counts nothing" mutants) or reporting other bytes than its blocks: a
/// server started as `kbf-all-in-one` starts its half, with one cache, stages a blob of
/// 1,000 bytes (in the open grouped block, which seals only after 30 s, so nothing is
/// flushed meanwhile); the one cache's storage bytes are then the tier's used bytes,
/// at least the blob, and `/metrics` says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hot_stores_blocks_are_the_one_caches_storage_bytes() {
    let local = Arc::new(kbf_storage::local_cache::LocalCache::new());
    let (store, api, _) = server_half("blocks", 1 << 30, Some(Arc::clone(&local))).await;
    assert_eq!(local.storage_bytes(), 0);
    store.put(vec![5; 1000]).await.expect("put");
    let staged = local.storage_bytes();
    assert!(staged >= 1000, "{staged}");
    let samples = scrape(api).await;
    for name in [
        "kbf_write_tier_used_bytes",
        "kbf_local_cache_storage_bytes",
        "kbf_local_cache_fill_bytes",
    ] {
        assert_eq!(samples.get(name), Some(&staged), "{name}: {samples:?}");
    }
}

/// A server half started as `kbf-all-in-one` starts it, in a fresh directory named
/// `name`, with `local` as its one cache: blobs of at least `own` bytes get a block of
/// their own. Its local store, its operator API, and its hot store's directory.
async fn server_half(
    name: &str,
    own: u64,
    local: Option<Arc<kbf_storage::local_cache::LocalCache>>,
) -> (LocalStore, std::net::SocketAddr, PathBuf) {
    use clap::Parser as _;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-all-in-one-one-cache")
        .join(format!("server-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let argv = [
        "kbf-server".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        "--api-listen=127.0.0.1:0".to_owned(),
        format!("--write-own-segment-bytes={own}"),
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--hot-store=file://{}", dir.join("hot").display()),
    ];
    let mut args = kbf_server::Args::try_parse_from(argv).expect("flags");
    args.local_cache = local;
    let never = || Ok(std::future::pending::<()>());
    let server = kbf_server::start(&args, never).await.expect("start");
    let (store, api) = (server.local(), server.api.expect("the API"));
    tokio::spawn(server.serve());
    (store, api, dir.join("hot"))
}

/// Catches the hot store's flushed copies decided otherwise than by the one cache: the
/// "never keep" mutant (a server with no one cache, `kbf-server` alone or an
/// all-in-one whose hot store is on another disk, would delete what it serves local
/// reads from) and the "always keep" one (a second copy beside the input cache's): a
/// server half without a one cache and one with it each upload a blob in a block of its
/// own and flush it; the first keeps a flushed copy (`.seg` under the hot store,
/// counted in `kbf_write_tier_flushed_copy_bytes`), the second neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hot_store_keeps_flushed_copies_only_without_the_one_cache() {
    for one_cache in [false, true] {
        let local = one_cache.then(|| Arc::new(kbf_storage::local_cache::LocalCache::new()));
        let name = format!("flushed-{one_cache}");
        let (store, api, hot) = server_half(&name, 1, local).await;
        store.put(vec![6; 1000]).await.expect("put");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let samples = loop {
            let samples = scrape(api).await;
            let flushes = samples.get("kbf_write_tier_flushes_total").copied();
            let used = samples.get("kbf_write_tier_used_bytes").copied();
            if flushes.unwrap_or(0) > 0 && used == Some(0) {
                break samples;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "not flushed: {samples:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let copies = std::fs::read_dir(&hot)
            .expect("hot")
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "seg"))
            .count();
        let copy_bytes = samples["kbf_write_tier_flushed_copy_bytes"];
        if one_cache {
            assert_eq!(
                (copies, copy_bytes),
                (0, 0),
                "a flushed copy beside the one cache"
            );
        } else {
            assert_eq!(copies, 1, "no flushed copy without the one cache");
            assert!(copy_bytes >= 1000, "{copy_bytes}");
        }
    }
}
