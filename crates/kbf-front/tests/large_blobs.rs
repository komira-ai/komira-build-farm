//! Blobs of any size, end to end, in bounded memory: written onto the write tier and
//! flushed to an S3 store as a multipart upload, written and read through ByteStream;
//! and the faults of a large Write over the loopback S3 service (bytes that fail their
//! digest, a client gone mid-way, no room on a write tier), each leaving no multipart
//! upload incomplete.
//!
//! A counting global allocator records the most bytes live at once during each step,
//! beyond those live when it started. The blobs are zeros: the tier's files keep runs of
//! zeros as holes ([`SparseFs`]) and the loopback S3 service counts them rather than
//! keeping them, so a blob of several GiB costs the test machine neither disk nor
//! memory. Nothing on the path a step measures knows the bytes are zeros. The tests
//! share the counters, so each runs alone ([`peak_during`]).

#![cfg(unix)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{File, OpenOptions};
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::StreamExt;
use kbf_front::{
    ByteStreamService, Cache, Connections, MAX_UNSTAGED_BLOB_BYTES, MemoryMetaLog,
    READ_CHUNK_BYTES, SegmentsConfig, SegmentsService,
};
use kbf_meta::{Retention, ServerId};
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::s3::{PART_BYTES, S3Store};
use kbf_objstore::{Capabilities, KeyPrefix};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::byte_stream_server::ByteStream as _;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::worker::segments_server::Segments as _;
use kbf_proto::worker::{BeginRequest, UploadSegment};
use kbf_segments::layout::{encode_footer, footer_len};
use kbf_segments::{IndexEntry, RecordHasher};
use kbf_store::{Fs, FsFile, StdFile, StdFs};
use kbf_tier::TierOptions;
use kbf_types::Digest;
use tonic::Code;
use tonic::transport::{Channel, Server, server::TcpIncoming};

const MIB: u64 = 1 << 20;

/// The size of a large blob in the memory steps: far over every bound below, and over
/// one part, one window and one in-memory blob.
const LARGE: u64 = 256 * MIB;

/// The most bytes a step may hold live at once beyond what was live when it started:
/// a few parts and windows (16 MiB and 8 MiB) on each side of the loopback store and
/// the gRPC stream, and room for the runtimes' own allocations. A blob held whole
/// anywhere on the path ([`LARGE`], 256 MiB) is far past it.
const BOUND: usize = 96 << 20;

/// Bytes allocated and not yet freed, and the most of them seen since the last reset.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn shrank(by: usize) {
    LIVE.fetch_sub(by, Ordering::Relaxed);
}

/// The system allocator, counted.
struct Counting;

// SAFETY: every call goes to `System` with the caller's arguments unchanged; the
// counters only observe the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract, which is `System.alloc`'s.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, so from `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `dealloc`, and the caller upholds `realloc`'s contract.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            grew(new_size);
            shrank(layout.size());
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Held by each test for its whole run: the counters are shared.
static ALONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Runs `step` and returns its answer and the most bytes live at once during it beyond
/// those live when it started.
async fn peak_during<T>(step: impl Future<Output = T>) -> (T, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = step.await;
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

/// A tier directory on the local disk whose files keep a run of zeros written at an
/// offset as a hole: the file is extended over it, nothing is written. Reads of a hole
/// are zeros, as of any file. Everything else is [`StdFs`].
#[derive(Debug)]
struct SparseFs(StdFs);

/// A file of [`SparseFs`]: positioned writes of zeros extend it.
#[derive(Debug)]
struct SparseFile {
    inner: StdFile,
    /// A second handle on the file, for its length; `None` for an appended file.
    positioned: Option<Mutex<File>>,
}

impl Fs for SparseFs {
    type File = SparseFile;

    fn list(&self) -> io::Result<Vec<String>> {
        self.0.list()
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        self.0.read(name)
    }

    fn file_len(&self, name: &str) -> io::Result<u64> {
        self.0.file_len(name)
    }

    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.0.read_at(name, offset, len)
    }

    fn create(&self, name: &str) -> io::Result<SparseFile> {
        let inner = self.0.create(name)?;
        Ok(SparseFile {
            inner,
            positioned: None,
        })
    }

    fn create_positioned(&self, name: &str) -> io::Result<SparseFile> {
        let inner = self.0.create_positioned(name)?;
        let handle = OpenOptions::new()
            .write(true)
            .open(self.0.dir().join(name))?;
        Ok(SparseFile {
            inner,
            positioned: Some(Mutex::new(handle)),
        })
    }

    fn open_append(&self, name: &str) -> io::Result<SparseFile> {
        let inner = self.0.open_append(name)?;
        Ok(SparseFile {
            inner,
            positioned: None,
        })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.0.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.0.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.0.sync_dir()
    }
}

impl FsFile for SparseFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.append(bytes)
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let Some(handle) = &self.positioned else {
            return self.inner.write_at(offset, bytes);
        };
        // One writer at a time, so a hole never cuts bytes another wrote past it.
        let handle = handle.lock().unwrap_or_else(PoisonError::into_inner);
        let zeros = bytes.chunks(4096).all(|c| c == &[0u8; 4096][..c.len()]);
        if !zeros {
            return handle.write_all_at(bytes, offset);
        }
        let end = offset + bytes.len() as u64;
        if handle.metadata()?.len() < end {
            handle.set_len(end)?;
        }
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.inner.truncate(len)
    }

    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
}

/// Caches started by this binary so far: each tier directory is one cache's alone.
static STARTED: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty directory under Cargo's temporary directory for this target.
fn scratch(name: &str) -> PathBuf {
    let n = STARTED.fetch_add(1, Ordering::SeqCst);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-front-large-blobs")
        .join(format!("{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

type LargeCache = Cache<MemoryMetaLog, S3Store>;

/// A cache over the loopback S3 service `s3`, with a write tier in `dir` of `budget`
/// bytes.
async fn cache(s3: &FakeS3, dir: &Path, budget: u64) -> Arc<LargeCache> {
    let meta = MemoryMetaLog::new(Retention::default());
    let store = s3.store(Capabilities {
        conditional_put: true,
        object_lock: false,
    });
    let cache = Cache::open(meta, store, KeyPrefix::default())
        .await
        .expect("open");
    let fs = SparseFs(StdFs::new(dir.join("tier")).expect("tier directory"));
    let (cache, _) = cache
        .attach_tier(fs, TierOptions::new(budget), ServerId::new(1), None)
        .await
        .expect("attach the tier");
    Arc::new(cache)
}

/// A cache over the loopback S3 service `s3` with no write tier.
async fn cold_cache(s3: &FakeS3) -> Arc<LargeCache> {
    let meta = MemoryMetaLog::new(Retention::default());
    let store = s3.store(Capabilities {
        conditional_put: true,
        object_lock: false,
    });
    let cache = Cache::open(meta, store, KeyPrefix::default())
        .await
        .expect("open");
    Arc::new(cache)
}

/// Waits (up to a minute) until `s3` holds no multipart upload begun and neither
/// completed nor aborted: an upload a failed write left is aborted from a task of its
/// own.
async fn settled(s3: &FakeS3) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while s3.incomplete_uploads() > 0 {
        assert!(
            Instant::now() < deadline,
            "{} incomplete",
            s3.incomplete_uploads()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The digest and CRC-32C of `len` zero bytes.
fn zeros_digest(len: u64) -> (Digest, u32) {
    let zeros = vec![0u8; MIB as usize];
    let mut hasher = RecordHasher::new();
    let mut left = len;
    while left > 0 {
        let n = left.min(MIB);
        hasher.update(&zeros[..n as usize]);
        left -= n;
    }
    hasher.finish()
}

/// Streams `len` zero bytes onto the tier as the in-process plane does, and commits
/// them: staged, not yet flushed. Returns the blob's digest.
async fn stage_zeros(cache: &Arc<LargeCache>, len: u64) -> Digest {
    let (digest, crc32c) = zeros_digest(len);
    let cache = Arc::clone(cache);
    let streamed = tokio::task::spawn_blocking(move || {
        let mut stream = cache.stream(len).expect("a stream onto the tier");
        let zeros = vec![0u8; MIB as usize];
        let mut left = len;
        while left > 0 {
            let n = left.min(MIB);
            stream.append(&zeros[..n as usize]).expect("append");
            left -= n;
        }
        stream.finish(digest, crc32c).expect("finish")
    })
    .await
    .expect("joined");
    streamed.commit().await.expect("commit");
    digest
}

/// Long past any seal delay: every block due.
fn later() -> Instant {
    Instant::now() + Duration::from_secs(3600)
}

/// Flushes every staged object, and checks the round flushed one.
async fn flush(cache: &LargeCache) {
    let round = cache.flush_tier(later()).await.expect("seal round");
    assert!(round.retrying.is_empty(), "{round:?}");
    let round = if round.flushed.is_empty() {
        cache.flush_tier(later()).await.expect("flush round")
    } else {
        round
    };
    assert_eq!(round.flushed.len(), 1, "{round:?}");
    assert!(round.retrying.is_empty(), "{round:?}");
}

/// Removes every file of the tier directory under `dir`: what the tier kept of a flushed
/// object, so reads go to the store.
fn forget_local_copies(dir: &Path) {
    for entry in std::fs::read_dir(dir.join("tier")).expect("tier directory") {
        let path = entry.expect("entry").path();
        if path.file_name().is_some_and(|n| n != "TIER") {
            std::fs::remove_file(&path).expect("remove");
        }
    }
}

/// Catches the cap on a blob's size restored (5 GiB, S3's limit for one PUT) or a
/// flush that PUTs a block in one request: a blob of 5 GiB and one byte is streamed
/// onto the tier, flushed as a multipart upload of 16 MiB parts with none left
/// incomplete, and read back from the store through ByteStream, its digest checked by
/// the cache before the last window is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_over_5_gib_round_trips_through_the_tier_a_multipart_flush_and_a_read() {
    let _alone = ALONE.lock().await;
    let size = (5 << 30) + 1;
    let s3 = FakeS3::start().await;
    let dir = scratch("over-5-gib");
    let cache = cache(&s3, &dir, 6 << 30).await;
    let digest = stage_zeros(&cache, size).await;
    flush(&cache).await;
    let stats = s3.stats();
    let parts = (size + 2 * MIB).div_ceil(PART_BYTES);
    assert_eq!(
        (stats.single_puts, stats.parts, stats.completed),
        (0, parts, 1),
        "{stats:?}"
    );
    assert!(stats.largest_body <= PART_BYTES, "{stats:?}");
    assert_eq!(s3.incomplete_uploads(), 0);
    forget_local_copies(&dir);
    let gets = s3.requests().len();
    let mut client = serve(&cache).await;
    assert_eq!(bytestream_read(&mut client, &digest).await, (size, true));
    assert!(s3.requests().len() > gets, "read from the store");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches a flush that reads a block whole before it PUTs it (a 256 MiB block held at
/// once): the flush of one holds at most [`BOUND`].
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_flush_holds_a_part_not_the_block() {
    let _alone = ALONE.lock().await;
    let s3 = FakeS3::start().await;
    let dir = scratch("flush");
    let cache = cache(&s3, &dir, 1 << 30).await;
    stage_zeros(&cache, LARGE).await;
    let ((), peak) = peak_during(flush(&cache)).await;
    println!("a {LARGE}-byte flush: {peak} bytes live at once at most");
    assert!(peak < BOUND, "{peak} bytes live at once");
    assert_eq!(s3.stats().completed, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The ByteStream services over `cache` on a loopback port, and a client of them.
async fn serve(cache: &Arc<LargeCache>) -> ByteStreamClient<Channel> {
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("local address");
    let routes = kbf_front::routes(Arc::clone(cache));
    tokio::spawn(async move {
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming)
            .await
            .expect("serve");
    });
    let channel = Channel::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    ByteStreamClient::new(channel)
        .max_decoding_message_size(kbf_front::MAX_MESSAGE_BYTES)
        .max_encoding_message_size(kbf_front::MAX_MESSAGE_BYTES)
}

/// The messages of a ByteStream Write of `digest`, `len` zero bytes in messages of 1 MiB
/// made as they are sent; with `flip_last`, the last byte is 1, so the bytes are not
/// the digest's.
fn zero_messages(
    digest: &Digest,
    len: u64,
    flip_last: bool,
) -> impl futures::Stream<Item = WriteRequest> + Send + 'static {
    let name = write_name(digest);
    let messages = len.div_ceil(MIB);
    futures::stream::iter(0..messages).map(move |i| {
        let at = i * MIB;
        let n = MIB.min(len - at);
        let last = i + 1 == messages;
        let mut data = vec![0; n as usize];
        if last & flip_last {
            data[n as usize - 1] = 1;
        }
        WriteRequest {
            resource_name: if i == 0 { name.clone() } else { String::new() },
            write_offset: i64::try_from(at).expect("offset"),
            finish_write: last,
            data,
        }
    })
}

/// The resource name a ByteStream Write of `digest` names.
fn write_name(digest: &Digest) -> String {
    format!(
        "main/uploads/7d5e6f1a-large/blobs/{}/{}",
        digest.hash_hex(),
        digest.size_bytes
    )
}

/// A ByteStream Write of `digest`, `len` zero bytes, in messages of 1 MiB made as they
/// are sent.
async fn write_zeros(client: &mut ByteStreamClient<Channel>, digest: &Digest, len: u64) {
    let answer = client
        .write(zero_messages(digest, len, false))
        .await
        .expect("written");
    assert_eq!(answer.into_inner().committed_size, len as i64);
}

/// Catches a ByteStream Write held whole until it is verified (a 256 MiB receive
/// buffer), or one admitted for its whole size: a large Write, onto the write tier and
/// (with no tier) to the store as a multipart upload, holds at most [`BOUND`] at once
/// on both ends of the loopback, and is then present and reads back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_bytestream_write_holds_a_part_not_the_blob() {
    let _alone = ALONE.lock().await;
    for tier in [true, false] {
        let s3 = FakeS3::start().await;
        let dir = scratch("write");
        let cache = if tier {
            cache(&s3, &dir, 1 << 30).await
        } else {
            cold_cache(&s3).await
        };
        let mut client = serve(&cache).await;
        let (digest, _) = zeros_digest(LARGE);
        let ((), peak) = peak_during(write_zeros(&mut client, &digest, LARGE)).await;
        println!("a {LARGE}-byte ByteStream Write (tier {tier}): {peak} bytes live at once");
        assert!(peak < BOUND, "{peak} bytes live at once");
        let stats = s3.stats();
        assert_eq!(stats.single_puts, 0, "{stats:?}");
        assert!(
            cache
                .find_missing(&[digest])
                .await
                .expect("find")
                .is_empty()
        );
        assert_eq!(bytestream_read(&mut client, &digest).await, (LARGE, true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Catches a large ByteStream Write sent past the write tier straight to the store
/// (mutant: `store_streamed` whether or not there is a tier), or one staged whatever
/// its bytes hash to (mutant: the digest check skipped): with a tier, a 40 MiB Write
/// whose last byte is not its digest's is refused INVALID_ARGUMENT and leaves nothing
/// to flush; a sound one is present at once while the store holds nothing of it (no
/// PUT, no part), and only the flush writes it there, as one multipart upload; it then
/// reads back from the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_write_with_a_tier_lands_on_the_tier_first() {
    let _alone = ALONE.lock().await;
    let s3 = FakeS3::start().await;
    let dir = scratch("write-tier");
    let cache = cache(&s3, &dir, 1 << 30).await;
    let mut client = serve(&cache).await;
    let len = 40 * MIB;
    let (digest, _) = zeros_digest(len);
    let refused = client
        .write(zero_messages(&digest, len, true))
        .await
        .expect_err("the bytes fail the digest");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    let round = cache.flush_tier(later()).await.expect("seal round");
    assert!(
        round.flushed.is_empty() & round.retrying.is_empty(),
        "{round:?}"
    );
    assert_eq!(cache.find_missing(&[digest]).await.expect("find"), [digest]);

    write_zeros(&mut client, &digest, len).await;
    assert!(
        cache
            .find_missing(&[digest])
            .await
            .expect("find")
            .is_empty()
    );
    let stats = s3.stats();
    assert_eq!(
        (stats.single_puts, stats.parts, stats.completed),
        (0, 0, 0),
        "on the tier, not yet in the store: {stats:?}"
    );
    flush(&cache).await;
    let stats = s3.stats();
    assert_eq!((stats.single_puts, stats.completed), (0, 1), "{stats:?}");
    assert_eq!(s3.incomplete_uploads(), 0);
    forget_local_copies(&dir);
    assert_eq!(bytestream_read(&mut client, &digest).await, (len, true));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reads `digest` through ByteStream, and returns how many bytes came and whether
/// every one was zero.
async fn bytestream_read(client: &mut ByteStreamClient<Channel>, digest: &Digest) -> (u64, bool) {
    let request = ReadRequest {
        resource_name: format!("main/blobs/{}/{}", digest.hash_hex(), digest.size_bytes),
        read_offset: 0,
        read_limit: 0,
    };
    let mut messages = client.read(request).await.expect("read").into_inner();
    let (mut got, mut zeros) = (0u64, true);
    while let Some(message) = messages.message().await.expect("a message") {
        assert!(message.data.len() <= READ_CHUNK_BYTES);
        got += message.data.len() as u64;
        zeros &= message
            .data
            .chunks(4096)
            .all(|c| c == &[0u8; 4096][..c.len()]);
    }
    (got, zeros)
}

/// Catches a ByteStream Read that reads a blob whole before it sends it (a 256 MiB
/// buffer): a large Read, from the store, holds at most [`BOUND`] at once on both ends
/// of the loopback, and its bytes come back whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_bytestream_read_holds_a_window_not_the_blob() {
    let _alone = ALONE.lock().await;
    let s3 = FakeS3::start().await;
    let dir = scratch("read");
    let cache = cache(&s3, &dir, 1 << 30).await;
    let digest = stage_zeros(&cache, LARGE).await;
    flush(&cache).await;
    forget_local_copies(&dir);
    let mut client = serve(&cache).await;
    let (read, peak) = peak_during(bytestream_read(&mut client, &digest)).await;
    println!("a {LARGE}-byte ByteStream Read: {peak} bytes live at once at most");
    assert_eq!(read, (LARGE, true));
    assert!(peak < BOUND, "{peak} bytes live at once");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches a multipart upload completed before the blob's bytes are checked against its
/// digest (mutant: the footer sent, and the upload completed, whatever the bytes hashed
/// to), or one left open when they fail: a ByteStream Write of 40 MiB whose last byte
/// is not its digest's is refused INVALID_ARGUMENT after its parts were sent; the
/// upload is aborted, never completed, none is left incomplete, and the blob is not
/// held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_write_whose_bytes_fail_its_digest_leaves_nothing_in_the_store() {
    let _alone = ALONE.lock().await;
    let s3 = FakeS3::start().await;
    let cache = cold_cache(&s3).await;
    let mut client = serve(&cache).await;
    let len = 40 * MIB;
    let (digest, _) = zeros_digest(len);
    let refused = client
        .write(zero_messages(&digest, len, true))
        .await
        .expect_err("the bytes fail the digest");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    settled(&s3).await;
    let stats = s3.stats();
    assert_eq!(
        (stats.single_puts, stats.completed, stats.aborted),
        (0, 0, 1),
        "{stats:?}"
    );
    assert!(
        stats.parts >= 2,
        "streamed in parts before the check: {stats:?}"
    );
    assert_eq!(cache.find_missing(&[digest]).await.expect("find"), [digest]);
}

/// Catches a Write a client dropped mid-way completed or left open in the store, and a
/// cap on a blob's size (mutant: the 5 GiB cap restored, which refuses the Write at its
/// first message): a Write of a blob over 5 GiB, its client gone once a part of it
/// reached the store, aborts its upload, leaves none incomplete, and stores nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_its_client_dropped_mid_way_leaves_nothing_in_the_store() {
    use futures::SinkExt as _;
    let _alone = ALONE.lock().await;
    let s3 = FakeS3::start().await;
    let cache = cold_cache(&s3).await;
    let mut client = serve(&cache).await;
    let digest = Digest::new(kbf_types::DigestFunction::Sha256, [7; 32], (5 << 30) + 1);
    let (mut sender, messages) = futures::channel::mpsc::channel::<WriteRequest>(4);
    let write = tokio::spawn(async move { client.write(messages).await });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut at = 0u64;
    while s3.stats().parts == 0 {
        assert!(Instant::now() < deadline, "no part reached the store");
        let message = WriteRequest {
            resource_name: if at == 0 {
                write_name(&digest)
            } else {
                String::new()
            },
            write_offset: i64::try_from(at).expect("offset"),
            finish_write: false,
            data: vec![0; MIB as usize],
        };
        if sender.send(message).await.is_err() {
            panic!("the Write ended: {:?}", write.await);
        }
        at += MIB;
    }
    write.abort();
    drop(sender);
    settled(&s3).await;
    let stats = s3.stats();
    assert_eq!((stats.completed, stats.aborted), (0, 1), "{stats:?}");
    assert!(
        at > MAX_UNSTAGED_BLOB_BYTES,
        "{at} bytes sent before a part"
    );
    assert_eq!(cache.find_missing(&[digest]).await.expect("find"), [digest]);
}

/// Catches an output over 16 MiB refused when the server has no write tier, or one with
/// no room for it (mutant: the refusal restored, RESOURCE_EXHAUSTED at `Begin`): a
/// segment upload `Begin` declaring a 17 MiB record is answered FAILED_PRECONDITION,
/// the answer at which the daemon sends the record by ByteStream; that Write streams it
/// to the store as a multipart upload, completed, none left incomplete, and the blob is
/// held and reads back. Without a tier, and with a full one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_room_on_a_tier_an_output_over_16_mib_is_stored() {
    let _alone = ALONE.lock().await;
    for full_tier in [false, true] {
        let s3 = FakeS3::start().await;
        let dir = scratch("no-room");
        let cache = if full_tier {
            cache(&s3, &dir, 10_000).await
        } else {
            cold_cache(&s3).await
        };
        let segments = SegmentsService::new(
            Arc::clone(&cache),
            SegmentsConfig::DEFAULT,
            Connections::new(),
        );
        let len = 17 * MIB;
        let (digest, crc32c) = zeros_digest(len);
        let entry = IndexEntry {
            digest,
            offset: 0,
            crc32c,
        };
        let begin = BeginRequest {
            segments: vec![UploadSegment {
                length: len + footer_len(1),
                footer: encode_footer(len, &[entry]),
            }],
        };
        let answer = segments.begin(tonic::Request::new(begin)).await;
        let refused = answer.expect_err("sent to ByteStream");
        assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
        let mut client = serve(&cache).await;
        write_zeros(&mut client, &digest, len).await;
        settled(&s3).await;
        let stats = s3.stats();
        assert_eq!((stats.single_puts, stats.completed), (0, 1), "{stats:?}");
        assert!(
            cache
                .find_missing(&[digest])
                .await
                .expect("find")
                .is_empty()
        );
        assert_eq!(bytestream_read(&mut client, &digest).await, (len, true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The most bytes one ByteStream Read may hold live at once: one window
/// (`READ_WINDOW_BYTES`, 8 MiB), one message (`READ_CHUNK_BYTES`, 1 MiB) and room for
/// the rest. A window copied into its messages all at once (16 MiB) is past it.
const ONE_READ_BOUND: usize = 12 << 20;

/// Catches a ByteStream Read that cuts a whole window into its messages at once (a
/// window and a copy of it, 16 MiB, per open Read): a Read of a 64 MiB blob from the
/// tier, its messages taken one at a time from the service itself (no transport
/// buffering), holds at most [`ONE_READ_BOUND`] at once, and its bytes come whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bytestream_read_holds_one_window_and_one_message() {
    let _alone = ALONE.lock().await;
    let s3 = FakeS3::start().await;
    let dir = scratch("read-lazy");
    let cache = cache(&s3, &dir, 1 << 30).await;
    let len = 64 * MIB;
    let digest = stage_zeros(&cache, len).await;
    let service = ByteStreamService::new(Arc::clone(&cache));
    let request = tonic::Request::new(ReadRequest {
        resource_name: format!("main/blobs/{}/{}", digest.hash_hex(), digest.size_bytes),
        read_offset: 0,
        read_limit: 0,
    });
    let (got, peak) = peak_during(async {
        let mut messages = service.read(request).await.expect("read").into_inner();
        let mut got = 0u64;
        while let Some(message) = messages.next().await {
            let message = message.expect("a message");
            assert!(message.data.len() <= READ_CHUNK_BYTES);
            got += message.data.len() as u64;
        }
        got
    })
    .await;
    println!("a {len}-byte ByteStream Read, message by message: {peak} bytes live at once");
    assert_eq!(got, len);
    assert!(peak < ONE_READ_BOUND, "{peak} bytes live at once");
    let _ = std::fs::remove_dir_all(&dir);
}
