//! Segment upload and segment-range reads against kbf-front's `Segments` service
//! (`docs/design/write-path.md`, sections 4 and 9, "Segment upload (PR 2)"), counted
//! by a wrapper that can also fail Commits.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_front::{Cache, Connections, MemoryMetaLog, SegmentsConfig, SegmentsService};
use kbf_objstore::MemoryStore;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::segments_server::{Segments, SegmentsServer};
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse, LocateRequest, LocateResponse,
    PutRangeRequest, PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use kbf_transfer::{Cas, MAX_RANGE_BYTES, Spool, TransferConfig, Transfers, digest_of};
use tokio::sync::{Barrier, watch};
use tonic::{Request, Response, Status};

use super::fake::serve;
use super::{engine, patterned};

type Service = SegmentsService<MemoryMetaLog, MemoryStore>;

/// How the test's `Segments` answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// As the front serves it.
    Serve,
    /// Every Commit fails UNAVAILABLE.
    FailCommits,
    /// The second and third Commits fail UNAVAILABLE: the second upload and its one
    /// retry (with `segment_uploads: 1`, uploads go one after another).
    FailSecondUpload,
    /// Every Begin is UNIMPLEMENTED, as a server without the service answers.
    Unimplemented,
    /// As `Unimplemented`, each Begin answered only once two have arrived.
    UnimplementedTogether,
    /// Every Locate fails UNAVAILABLE.
    FailLocate,
    /// Every ReadRange fails UNAVAILABLE.
    FailReadRange,
    /// Every ReadRange answers its bytes with the first one flipped.
    CorruptReadRange,
    /// Every Locate answer drops its last location.
    ShortLocate,
    /// Every Locate answer lists its locations in reverse order.
    ReorderLocate,
}

/// `Segments` over the front's, counting calls, answering as `mode` says.
struct Counting {
    inner: Service,
    begins: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
    read_ranges: Arc<AtomicUsize>,
    /// Records declared by every `Begin`'s footers.
    declared: Arc<AtomicUsize>,
    /// Two Begins meet here in [`Mode::UnimplementedTogether`].
    together: Arc<Barrier>,
    /// PutRange calls wait for this to open before they answer.
    range_gate: Arc<watch::Sender<bool>>,
    /// PutRange calls waiting at `range_gate` now.
    held_ranges: Arc<AtomicUsize>,
    mode: Mode,
}

#[tonic::async_trait]
impl Segments for Counting {
    async fn begin(&self, r: Request<BeginRequest>) -> Result<Response<BeginResponse>, Status> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        let records: usize = r
            .get_ref()
            .segments
            .iter()
            .map(|s| {
                kbf_segments::Footer::parse(&s.footer, s.length).map_or(0, |f| f.entries().len())
            })
            .sum();
        self.declared.fetch_add(records, Ordering::SeqCst);
        if self.mode == Mode::UnimplementedTogether {
            self.together.wait().await;
        }
        if matches!(self.mode, Mode::Unimplemented | Mode::UnimplementedTogether) {
            return Err(Status::unimplemented("no Segments here"));
        }
        self.inner.begin(r).await
    }

    async fn put_range(
        &self,
        r: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        self.held_ranges.fetch_add(1, Ordering::SeqCst);
        let _ = self.range_gate.subscribe().wait_for(|open| *open).await;
        self.held_ranges.fetch_sub(1, Ordering::SeqCst);
        self.inner.put_range(r).await
    }

    async fn commit(&self, r: Request<CommitRequest>) -> Result<Response<CommitResponse>, Status> {
        let ordinal = self.commits.fetch_add(1, Ordering::SeqCst);
        if self.mode == Mode::FailSecondUpload && (1..=2).contains(&ordinal) {
            return Err(Status::unavailable("the store timed out"));
        }
        if self.mode == Mode::FailCommits {
            return Err(Status::unavailable("the store is down"));
        }
        self.inner.commit(r).await
    }

    async fn locate(&self, r: Request<LocateRequest>) -> Result<Response<LocateResponse>, Status> {
        if self.mode == Mode::FailLocate {
            return Err(Status::unavailable("the index is down"));
        }
        let mut answer = self.inner.locate(r).await?;
        let locations = &mut answer.get_mut().locations;
        match self.mode {
            Mode::ShortLocate => drop(locations.pop()),
            Mode::ReorderLocate => locations.reverse(),
            _ => {}
        }
        Ok(answer)
    }

    async fn read_range(
        &self,
        r: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        self.read_ranges.fetch_add(1, Ordering::SeqCst);
        if self.mode == Mode::FailReadRange {
            return Err(Status::unavailable("the store is down"));
        }
        let mut answer = self.inner.read_range(r).await?;
        if self.mode == Mode::CorruptReadRange {
            answer.get_mut().bytes[0] ^= 0xff;
        }
        Ok(answer)
    }
}

/// A front with the CAS services and `Segments` answering as `mode` says.
struct Front {
    cache: Arc<Cache<MemoryMetaLog, MemoryStore>>,
    engine: Arc<Transfers>,
    begins: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
    read_ranges: Arc<AtomicUsize>,
    declared: Arc<AtomicUsize>,
    range_gate: Arc<watch::Sender<bool>>,
    held_ranges: Arc<AtomicUsize>,
}

impl Front {
    async fn start(mode: Mode) -> Self {
        Self::with(mode, TransferConfig::default()).await
    }

    /// The front, and an engine with `config`.
    async fn with(mode: Mode, config: TransferConfig) -> Self {
        let cache = Arc::new(Cache::memory());
        let (begins, commits, read_ranges, declared, held_ranges) = Default::default();
        let range_gate = Arc::new(watch::channel(true).0);
        let mut routes = kbf_front::routes(Arc::clone(&cache));
        {
            let counting = Counting {
                inner: SegmentsService::new(
                    Arc::clone(&cache),
                    SegmentsConfig::DEFAULT,
                    Connections::new(),
                ),
                begins: Arc::clone(&begins),
                commits: Arc::clone(&commits),
                read_ranges: Arc::clone(&read_ranges),
                declared: Arc::clone(&declared),
                together: Arc::new(Barrier::new(2)),
                range_gate: Arc::clone(&range_gate),
                held_ranges: Arc::clone(&held_ranges),
                mode,
            };
            routes = routes.add_service(
                SegmentsServer::new(counting)
                    .max_decoding_message_size(kbf_front::MAX_MESSAGE_BYTES)
                    .max_encoding_message_size(kbf_front::MAX_MESSAGE_BYTES),
            );
        }
        let addr = serve(routes).await;
        Self {
            cache,
            engine: engine(addr, config),
            begins,
            commits,
            read_ranges,
            declared,
            range_gate,
            held_ranges,
        }
    }

    /// Whether every one of `digests` is present.
    async fn all_present(&self, digests: &[Digest]) -> bool {
        let digests: Vec<kbf_types::Digest> = digests
            .iter()
            .map(|d| kbf_front::digest_from_proto(Some(d)).expect("digest"))
            .collect();
        let present = self.cache.present(&digests).await.expect("present");
        present.iter().all(|&p| p)
    }
}

/// A scratch directory of this test binary's own.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-transfer-segments")
        .join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// A spool of `blobs`, and their digests.
async fn spooled(
    name: &str,
    blobs: impl IntoIterator<Item = Vec<u8>>,
) -> (Arc<Spool>, Vec<Digest>) {
    let spool = Arc::new(Spool::create(&scratch(name)).expect("spool"));
    let mut digests = Vec::new();
    for blob in blobs {
        digests.push(spool.put(blob).await.expect("spool put"));
    }
    (spool, digests)
}

/// Catches per-blob calls and per-segment commits (mutant: one upload per segment):
/// 15,000 small outputs go as one `Begin`, one `Commit` and one `PutBlobs`, and every
/// one is present after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifteen_thousand_outputs_are_one_upload_and_one_commit() {
    let front = Front::start(Mode::Serve).await;
    let blobs = (0..15_000).map(|i| format!("output file {i}\n").into_bytes());
    let (spool, digests) = spooled("fifteen-thousand", blobs).await;
    front.engine.put_spool(spool).await.expect("put spool");
    assert_eq!(front.begins.load(Ordering::SeqCst), 1);
    assert_eq!(front.commits.load(Ordering::SeqCst), 1);
    assert_eq!(
        front.cache.metrics().segment_commit.count(),
        1,
        "one PutBlobs"
    );
    assert_eq!(
        front.cache.metrics().write.total.get(),
        0,
        "no ByteStream write"
    );
    assert!(front.all_present(&digests).await);
}

/// Catches blobs the server holds sent again (mutant: the FindMissing answer ignored;
/// design step 4.1.1): a spool of 20 blobs, 10 of them uploaded by an earlier spool,
/// declares only the 10 new ones in its `Begin`, and all 20 are present after.
#[tokio::test]
async fn blobs_the_server_holds_are_not_sent_again() {
    let front = Front::start(Mode::Serve).await;
    let (spool, held) = spooled("held-first", (0..10).map(|i| patterned(i, 500))).await;
    front.engine.put_spool(spool).await.expect("first upload");
    assert_eq!(front.declared.load(Ordering::SeqCst), 10);
    let (spool, all) = spooled("held-second", (0..20).map(|i| patterned(i, 500))).await;
    assert_eq!(all[..10], held[..]);
    front.engine.put_spool(spool).await.expect("second upload");
    assert_eq!(
        front.declared.load(Ordering::SeqCst),
        20,
        "the second upload declares only the 10 new blobs"
    );
    assert_eq!(front.begins.load(Ordering::SeqCst), 2);
    assert_eq!(
        front.cache.metrics().write.total.get(),
        0,
        "no ByteStream write"
    );
    assert!(front.all_present(&all).await);
}

/// Catches a new daemon failing against an old server (mutant: UNIMPLEMENTED is an
/// error): a front without `Segments` gets every blob by ByteStream (a large one
/// streamed from the spool), and the next spool does not try `Segments` again.
#[tokio::test]
async fn a_server_without_segments_gets_every_blob_by_bytestream() {
    let front = Front::start(Mode::Unimplemented).await;
    // One blob over `--transfer-memory-blob-bytes`, streamed from the spool.
    let blobs = (0..20)
        .map(|i| patterned(i, 100))
        .chain([patterned(99, 3 << 20)]);
    let (spool, digests) = spooled("unimplemented", blobs).await;
    front.engine.put_spool(spool).await.expect("fall back");
    assert!(front.all_present(&digests).await);
    assert_eq!(front.cache.metrics().write.total.get(), 21);
    let (spool, more) = spooled("unimplemented-2", (20..25).map(|i| patterned(i, 100))).await;
    front
        .engine
        .put_spool(spool)
        .await
        .expect("fall back again");
    assert!(front.all_present(&more).await);
    assert_eq!(
        front.begins.load(Ordering::SeqCst),
        1,
        "Segments asked once"
    );
}

/// Where the test's subscriber writes.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Catches the switch to blob-by-blob logged by every upload that meets UNIMPLEMENTED
/// (mutant: the warning not guarded by the flag's swap): two uploads whose Begins are
/// both in flight when the answers come are both UNIMPLEMENTED, and one line says so.
#[tokio::test]
async fn uploads_in_flight_together_log_unimplemented_once() {
    // The server runs on this test's one thread, so the thread's subscriber sees it.
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let front = Front::start(Mode::UnimplementedTogether).await;
    let (a, first) = spooled("together-a", (0..5).map(|i| patterned(i, 100))).await;
    let (b, second) = spooled("together-b", (5..10).map(|i| patterned(i, 100))).await;
    let both = async { tokio::join!(front.engine.put_spool(a), front.engine.put_spool(b)) };
    let (a, b) = tokio::time::timeout(Duration::from_secs(30), both)
        .await
        .expect("both Begins arrived");
    a.expect("fall back");
    b.expect("fall back");
    assert_eq!(front.begins.load(Ordering::SeqCst), 2);
    assert!(front.all_present(&first).await);
    assert!(front.all_present(&second).await);
    let text = String::from_utf8_lossy(&log.0.lock().expect("log")).into_owned();
    assert_eq!(text.matches("Segments UNIMPLEMENTED").count(), 1, "{text}");
}

/// Catches a failed upload given up on, or retried forever (mutants: no retry; no
/// fallback): a Commit that always fails is tried twice, then every blob goes by
/// ByteStream and is present.
#[tokio::test]
async fn a_failing_commit_is_retried_once_then_blobs_go_one_by_one() {
    let front = Front::start(Mode::FailCommits).await;
    let (spool, digests) = spooled("failing-commit", (0..10).map(|i| patterned(i, 1000))).await;
    front.engine.put_spool(spool).await.expect("fall back");
    assert_eq!(
        front.begins.load(Ordering::SeqCst),
        2,
        "the upload and one retry"
    );
    assert_eq!(front.commits.load(Ordering::SeqCst), 2);
    assert!(front.all_present(&digests).await);
    assert_eq!(front.cache.metrics().write.total.get(), 10);
}

/// Catches an upload that loses its progress to one failed commit (the farm node's cold
/// upload: one 1 GiB upload, one failed PUT, every blob sent again): ten blobs of
/// 10 MiB are four uploads of at most 32 MiB; the second fails twice, and only its three
/// blobs go by ByteStream while the other seven stay committed by their own commits
/// (mutant: `SEGMENT_UPLOAD_BYTES` back at 1 GiB + 1 MiB, one upload, no fall back).
#[tokio::test]
async fn a_failed_upload_keeps_the_ones_committed_before_it() {
    // One upload at a time: which Commit is "the second" is only defined then.
    let config = TransferConfig {
        segment_uploads: 1,
        ..TransferConfig::default()
    };
    let front = Front::with(Mode::FailSecondUpload, config).await;
    let (spool, digests) = spooled("second-fails", (0..10).map(|i| patterned(i, 10 << 20))).await;
    front.engine.put_spool(spool).await.expect("put spool");
    assert_eq!(
        front.begins.load(Ordering::SeqCst),
        5,
        "four uploads and the second's retry"
    );
    assert_eq!(front.commits.load(Ordering::SeqCst), 5);
    assert_eq!(
        front.cache.metrics().write.total.get(),
        3,
        "only the failed upload's blobs are sent again"
    );
    assert!(front.all_present(&digests).await);
}

/// Catches fills still one call per blob (mutant: no coalescing): the 1,000 files of a
/// tree, uploaded as segments, read back through `Locate` and `ReadRange` in at most
/// ceil(bytes / 4 MiB) reads, every blob checked; a blob never uploaded is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_of_a_thousand_files_reads_back_in_few_ranges() {
    let front = Front::start(Mode::Serve).await;
    let blobs: Vec<Vec<u8>> = (0..1000)
        .map(|i| patterned(i, 2000 + (i * 37) % 9000))
        .collect();
    let total: u64 = blobs.iter().map(|b| b.len() as u64).sum();
    let (spool, digests) = spooled("tree", blobs.clone()).await;
    front.engine.put_spool(spool).await.expect("upload");
    let absent = kbf_transfer::digest_of(b"never uploaded");
    let mut asked = digests.clone();
    asked.push(absent.clone());
    let located = front
        .engine
        .scope(&kbf_types::LeaseId::new(1, 1))
        .read_located(&asked)
        .await;
    let located = located.expect("read");
    assert_eq!(located.missing, vec![absent]);
    assert_eq!(located.blobs.len(), 1000);
    for (digest, bytes) in &located.blobs {
        let i = digests.iter().position(|d| d == digest).expect("asked");
        assert_eq!(bytes, &blobs[i]);
    }
    let reads = front.read_ranges.load(Ordering::SeqCst) as u64;
    assert!(
        reads <= total.div_ceil(MAX_RANGE_BYTES),
        "{reads} reads for {total} bytes"
    );
}

/// Catches a fill that fails, or hangs, when the server cannot locate or read (mutants:
/// a failed `Locate` or `ReadRange` returned as an error): every blob asked for comes
/// back missing, so the caller fetches it another way. Also catches bytes from a
/// range trusted unchecked (mutant: the per-blob digest check dropped): a range whose
/// bytes were altered fails the read as corrupt.
#[tokio::test]
async fn reads_by_range_fall_back_to_missing_and_refuse_altered_bytes() {
    for mode in [
        Mode::FailLocate,
        Mode::FailReadRange,
        Mode::CorruptReadRange,
    ] {
        let front = Front::start(mode).await;
        let (spool, digests) = spooled("fill", (0..5).map(|i| patterned(i, 700))).await;
        front.engine.put_spool(spool).await.expect("upload");
        let read = front
            .engine
            .scope(&kbf_types::LeaseId::new(1, 1))
            .read_located(&digests)
            .await;
        if mode == Mode::CorruptReadRange {
            let failed = read.expect_err("altered bytes");
            assert!(format!("{failed:?}").starts_with("Corrupt("), "{failed:?}");
            continue;
        }
        let read = read.expect("missing, not an error");
        assert!(read.blobs.is_empty());
        let mut missing = read.missing;
        missing.sort_by(|a, b| a.hash.cmp(&b.hash));
        let mut want = digests.clone();
        want.sort_by(|a, b| a.hash.cmp(&b.hash));
        assert_eq!(missing, want);
    }
}

/// Catches Locate answers paired to the asked digests by position (mutant: `zip` of
/// the batch and the answer): an answer one location short, or in another order,
/// still accounts for every asked digest, each read blob under its own digest; a
/// digest the answer does not name is missing, never dropped.
#[tokio::test]
async fn a_short_or_reordered_locate_answer_loses_no_digest() {
    for mode in [Mode::ShortLocate, Mode::ReorderLocate] {
        let front = Front::start(mode).await;
        let blobs: Vec<Vec<u8>> = (0..5).map(|i| patterned(i, 700)).collect();
        let (spool, digests) = spooled("locate", blobs.clone()).await;
        front.engine.put_spool(spool).await.expect("upload");
        let read = front
            .engine
            .scope(&kbf_types::LeaseId::new(1, 1))
            .read_located(&digests)
            .await
            .expect("read");
        for (digest, bytes) in &read.blobs {
            let i = digests.iter().position(|d| d == digest).expect("asked");
            assert_eq!(bytes, &blobs[i]);
        }
        let mut accounted: Vec<Digest> = read.blobs.into_iter().map(|(d, _)| d).collect();
        accounted.extend(read.missing.iter().cloned());
        accounted.sort_by(|a, b| a.hash.cmp(&b.hash));
        let mut want = digests.clone();
        want.sort_by(|a, b| a.hash.cmp(&b.hash));
        assert_eq!(accounted, want);
        let want_missing = usize::from(mode == Mode::ShortLocate);
        assert_eq!(read.missing.len(), want_missing);
    }
}

/// Catches a spool that cannot be read treated as a failed call (mutant: the spool
/// error retried and then sent blob by blob, each read failing again): when the spool
/// file is cut short after its blobs were spooled, `put_spool` fails with a read
/// error after one `Begin` and no `Commit`. Linux only: the unlinked spool file is
/// reached through `/proc/self/fd`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_spool_cut_short_fails_the_upload_without_a_retry() {
    let front = Front::start(Mode::Serve).await;
    let (spool, _) = spooled("cut-short", (0..3).map(|i| patterned(i, 900))).await;
    let marker = format!(".kbf-spool-{}-", std::process::id());
    let dir = scratch("cut-short");
    let mut cut = 0;
    for entry in std::fs::read_dir("/proc/self/fd").expect("fds") {
        let path = entry.expect("fd").path();
        let target = std::fs::read_link(&path).unwrap_or_default();
        let name = target.to_string_lossy().into_owned();
        if name.starts_with(dir.to_string_lossy().as_ref()) && name.contains(&marker) {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("reopen the spool");
            file.set_len(0).expect("cut");
            cut += 1;
        }
    }
    assert_eq!(cut, 1, "one spool file under the scratch directory");
    let failed = front
        .engine
        .put_spool(spool)
        .await
        .expect_err("the spool cannot be read");
    assert!(format!("{failed:?}").starts_with("Read("), "{failed:?}");
    assert_eq!(front.begins.load(Ordering::SeqCst), 1, "not retried");
    assert_eq!(front.commits.load(Ordering::SeqCst), 0);
}

/// Issue #641, for segment uploads. With `--transfer-in-flight` 4 and 4 segment
/// streams, an upload of five 4 MiB ranges whose `PutRange` calls the server holds
/// (as a slow server holds them) must leave a permit for another lease's read: the
/// ranges hold the uploads' share (3), and the read finishes while they are held.
/// Before the fix the ranges took all 4 permits and the read waited for them. Mutant
/// seen red: no share taken by `put_range` (4 held, the read stuck).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_ranges_leave_a_permit_for_another_leases_read() {
    let config = TransferConfig {
        in_flight: 4,
        segment_streams: 4,
        ..TransferConfig::default()
    };
    let front = Front::with(Mode::Serve, config).await;
    let small = patterned(7, 300);
    let digest = front.engine.put(small.clone()).await.expect("put");
    let (spool, digests) = spooled("held-ranges", (0..5).map(|i| patterned(i, 4 << 20))).await;
    front.range_gate.send_replace(false);
    let uploader = Arc::new(front.engine.scope(&kbf_types::LeaseId::new(8, 0)));
    let upload = {
        let uploader = Arc::clone(&uploader);
        tokio::spawn(async move { uploader.put_spool(spool).await })
    };
    let share = 3;
    for _ in 0..3000 {
        if front.held_ranges.load(Ordering::SeqCst) >= share {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Room for ranges past the share to reach the server, were they allowed to.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reader = front.engine.scope(&kbf_types::LeaseId::new(8, 1));
    let read = tokio::time::timeout(Duration::from_secs(10), reader.get(&digest)).await;
    assert_eq!(
        (read.is_ok(), front.held_ranges.load(Ordering::SeqCst)),
        (true, share),
        "(read finished, ranges held) while the server holds every PutRange"
    );
    assert_eq!(
        read.ok().map(|r| r.map(|b| digest_of(&b))),
        Some(Ok(digest))
    );
    front.range_gate.send_replace(true);
    upload.await.expect("upload").expect("put spool");
    assert!(front.all_present(&digests).await);
}

/// The 17.5 s of a cold unpack, step 6: the uploads of a spool went one after another,
/// so the server's check of one never overlapped the sending of the next. With the
/// server holding every `PutRange` and `--segment-uploads-in-flight` 4, the four uploads
/// of ten 10 MiB blobs all `Begin` while none has committed; with 1 only the first does
/// (the test then opens the gate, so it ends). After the gate opens every blob is
/// present and each upload committed once. Mutant seen red: the `for` loop over uploads
/// back (2 Begins at most, the wait below times out).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uploads_of_one_spool_are_in_flight_together() {
    for (uploads, want_begins) in [(1, 1), (4, 4)] {
        let config = TransferConfig {
            segment_uploads: uploads,
            ..TransferConfig::default()
        };
        let front = Arc::new(Front::with(Mode::Serve, config).await);
        let (spool, digests) =
            spooled("uploads-together", (0..10).map(|i| patterned(i, 10 << 20))).await;
        front.range_gate.send_replace(false);
        let sending = {
            let front = Arc::clone(&front);
            tokio::spawn(async move { front.engine.put_spool(spool).await })
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        // Ranges are held: nothing commits, so Begins counted are uploads in flight.
        while front.begins.load(Ordering::SeqCst) < want_begins
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let begun = front.begins.load(Ordering::SeqCst);
        assert_eq!(begun, want_begins, "{uploads} in flight allowed");
        assert_eq!(front.commits.load(Ordering::SeqCst), 0);
        front.range_gate.send_replace(true);
        sending.await.expect("join").expect("put spool");
        assert_eq!(front.commits.load(Ordering::SeqCst), 4);
        assert!(front.all_present(&digests).await);
    }
}
