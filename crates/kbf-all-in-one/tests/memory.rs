//! How much memory the whole in-process path holds while one large output is stored,
//! onto the write tier or, without one, straight to the cold store:
//! the container driver's walk, the plane and the server's upload until its commit,
//! all in this process. A counting global allocator records the most bytes live at once
//! while each test runs, beyond those live when it started (the server is started
//! before). The outputs are sparse files, so they cost the test machine no disk to
//! create.
//!
//! The tests share the counters, so each runs alone ([`peak_during`]).

#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use clap::Parser as _;
use kbf_all_in_one::LocalDataPlane;
use kbf_daemon::cpu_pool::CpuPool;
use kbf_driver_container::tree::{OutputLimits, collect};
use kbf_proto::reapi::ActionResult;
use kbf_transfer::Cas;
use kbf_types::LeaseId;

/// The walk's chunk (`kbf_driver_container::CHUNK`, 1 MiB).
const CHUNK: usize = 1 << 20;

/// The most bytes the whole path may hold live at once beyond what was live when the
/// test started: the chunk window (one chunk per hashing thread, nothing more on the
/// plane's or the server's side) and room for the runtimes' own allocations. A file
/// held whole anywhere on the path (2 GiB) is far past it.
const BOUND: usize = 16 * CHUNK;

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

/// Runs `f` with no other test of this binary running, and returns its result and the
/// most bytes live at once during it beyond those live when it started.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    static ALONE: Mutex<()> = Mutex::new(());
    let _alone = ALONE.lock().unwrap_or_else(PoisonError::into_inner);
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

/// Servers started by this binary so far: each directory is one server's alone.
static STARTED: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty directory under Cargo's temporary directory for this target.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-all-in-one-memory")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// The plane over a server started in `runtime` on the stores under `dir`, with a
/// write tier (`tier`) or without one, and the server's store. The server's handle is dropped, which stops its
/// flusher, so it does not run while the upload path is measured (the flush's own
/// memory is measured by kbf-front's `tests/large_blobs.rs`). The store works on
/// without the handle.
fn plane(
    runtime: &tokio::runtime::Runtime,
    dir: &Path,
    tier: bool,
) -> (Arc<LocalDataPlane>, kbf_server::LocalStore) {
    let n = STARTED.fetch_add(1, Ordering::SeqCst);
    let mut argv = vec![
        "kbf-server".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        format!(
            "--cold-store=file://{}",
            dir.join(format!("cold-{n}")).display()
        ),
    ];
    argv.push(if tier {
        format!(
            "--hot-store=file://{}",
            dir.join(format!("hot-{n}")).display()
        )
    } else {
        format!("--meta-dir={}", dir.join(format!("meta-{n}")).display())
    });
    let args = kbf_server::Args::try_parse_from(argv).expect("flags");
    runtime.block_on(async {
        let never = || Ok(std::future::pending::<()>());
        let server = kbf_server::start(&args, never).await.expect("start");
        let store = server.local();
        drop(server);
        let plane = LocalDataPlane::new(store.clone(), tokio::runtime::Handle::current());
        (Arc::new(plane), store)
    })
}

/// Stores the sparse output `out/large` of `size` bytes through a lease of the plane
/// over a server with a write tier (`tier`) or without one, and returns the most bytes
/// live at once while it did; checks it is held after.
fn store_one_large_output(name: &str, size: u64, tier: bool) -> usize {
    let dir = scratch(name);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let (plane, _) = plane(&runtime, &dir.join("server"), tier);
    let upper = dir.join("upper");
    std::fs::create_dir_all(upper.join("out")).expect("mkdir");
    std::fs::File::create(upper.join("out/large"))
        .and_then(|file| file.set_len(size))
        .expect("sparse file");
    let lease = plane.lease(&LeaseId::new(1, 1));
    let pool = CpuPool::new(4);
    let (result, peak) = peak_during(|| {
        let mut result = ActionResult::default();
        let collected = runtime.block_on(collect(
            &lease,
            &upper,
            &pool,
            "",
            &["out/large".to_owned()],
            OutputLimits::DEFAULT,
            &mut result,
        ));
        collected.map(|()| result)
    });
    let result = result.expect("collect");
    let digest = result.output_files[0].digest.clone().expect("a digest");
    let missing = runtime.block_on(lease.missing(&[digest]));
    assert!(missing.is_empty(), "not held");
    let _ = std::fs::remove_dir_all(&dir);
    peak
}

/// Catches a file held whole anywhere on the in-process path (the walk, the plane's
/// intake, the server's upload until its commit): a 2 GiB output streams onto the
/// write tier with no more than [`BOUND`] live at once.
#[test]
fn a_2_gib_output_streams_onto_the_tier_in_bounded_memory() {
    let peak = store_one_large_output("tier-2g", 2 << 30, true);
    println!("a 2 GiB output: {peak} bytes live at once at most");
    assert!(peak < BOUND, "{peak} bytes live at once");
}

/// Catches a large output refused without a write tier, or held whole on its way to
/// the store (mutants: the refusal restored, the stream gathered before the store): a
/// 256 MiB output streams straight to the cold store with no more than [`BOUND`] live
/// at once (the walk's chunks, the queue to the store, and the store's piece).
#[test]
fn without_a_tier_a_256_mib_output_streams_to_the_store_in_bounded_memory() {
    let peak = store_one_large_output("cold-256m", 256 << 20, false);
    println!("a 256 MiB output without a tier: {peak} bytes live at once at most");
    assert!(peak < BOUND, "{peak} bytes live at once");
}

/// Uploads one record of `size` zero bytes through the server's `Segments` handlers
/// (`Begin`, a `PutRange` per 4 MiB, `Commit`), the calls and messages a remote daemon's
/// transfer engine sends over the worker listener, to a server with a write tier; returns
/// the most bytes live at once while it did, and checks the record is held after.
fn upload_one_large_record(name: &str, size: u64) -> usize {
    use kbf_proto::worker::{BeginRequest, CommitRequest, PutRangeRequest, UploadSegment};
    use kbf_transfer::{
        MAX_RANGE_BYTES, MAX_UPLOAD_RECORDS, Record, RecordHasher, SEGMENT_UPLOAD_BYTES, Source,
        plan,
    };
    use tonic::Request;

    let dir = scratch(name);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let (_, store) = plane(&runtime, &dir.join("server"), true);
    let segments = store.segments();
    let mut hasher = RecordHasher::new();
    let zeros = vec![0u8; CHUNK];
    for _ in 0..size / CHUNK as u64 {
        hasher.update(&zeros);
    }
    drop(zeros);
    let (digest, crc32c) = hasher.finish();
    let record = Record {
        digest,
        crc32c,
        source: Source::Bytes(bytes::Bytes::new()),
    };
    let uploads = plan(
        &[record],
        kbf_segments::MAX_SEGMENT_BYTES,
        SEGMENT_UPLOAD_BYTES,
        MAX_UPLOAD_RECORDS,
    );
    let [upload] = &uploads[..] else {
        panic!("one upload");
    };
    let [segment] = &upload[..] else {
        panic!("one segment");
    };
    let (uploaded, peak) = peak_during(|| {
        runtime.block_on(async {
            let begin = BeginRequest {
                segments: vec![UploadSegment {
                    length: segment.length(),
                    footer: segment.footer(),
                }],
            };
            let id = segments
                .begin(Request::new(begin))
                .await?
                .into_inner()
                .upload;
            for (start, end) in segment.ranges(MAX_RANGE_BYTES) {
                let range = PutRangeRequest {
                    upload: id,
                    segment: 0,
                    offset: start,
                    bytes: vec![0; usize::try_from(end - start).expect("a range")],
                };
                segments.put_range(Request::new(range)).await?;
            }
            let commit = CommitRequest { upload: id };
            segments.commit(Request::new(commit)).await.map(drop)
        })
    });
    uploaded.expect("uploaded");
    let missing = runtime.block_on(store.find_missing(&[digest]));
    assert!(missing.expect("find").is_empty(), "not held");
    let _ = std::fs::remove_dir_all(&dir);
    peak
}

/// Catches a remote upload held whole in the server until its commit (the segment's
/// buffer): a 2 GiB record uploaded through the `Segments` handlers onto the write tier
/// is held with no more than [`BOUND`] live at once (the ranges in flight, written
/// through to the tier as they arrive).
#[test]
fn a_2_gib_remote_upload_is_written_through_to_the_tier_in_bounded_memory() {
    let peak = upload_one_large_record("remote-2g", 2 << 30);
    println!("a 2 GiB remote upload: {peak} bytes live at once at most");
    assert!(peak < BOUND, "{peak} bytes live at once");
}
