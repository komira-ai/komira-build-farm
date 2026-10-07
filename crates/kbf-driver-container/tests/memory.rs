//! How much memory the driver holds while it stores large files: a counting global
//! allocator records the most bytes live at once while each test runs. The files are
//! sparse, so a 2 GiB output costs the test machine no disk.
//!
//! The tests share the counters, so each runs alone ([`peak_during`]).

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use kbf_daemon::RuntimeError;
use kbf_driver_container::tree::{Exceeded, OutputLimits, TreeError, collect};
use kbf_driver_container::{CHUNK, FileBlob, MemoryCas};
use kbf_proto::reapi::ActionResult;
use support::Spec;
use support::fake::{Fake, image};

/// 2 GiB, the size of every large file here.
const LARGE: u64 = 2 << 30;
/// The most memory a test may hold beyond what was live when it started: a few
/// chunks, and room for the runtime's own allocations. A file read whole, or up to a
/// 1 GiB limit, is far past it.
const BOUND: usize = 8 * CHUNK;

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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// Makes `path` a sparse file of `size` bytes.
fn sparse(path: &std::path::Path, size: u64) {
    std::fs::File::create(path)
        .and_then(|file| file.set_len(size))
        .expect("sparse file");
}

/// Catches a file hashed or sent whole: a 2 GiB file is hashed, then read back in
/// chunks of at most [`CHUNK`] bytes that add up to it, with no more than [`BOUND`]
/// live at once (seen red with the hash pass reading the file into one buffer, and
/// with `next_chunk` returning the rest of the file at once).
#[test]
fn a_2_gib_file_is_hashed_and_read_back_in_bounded_memory() {
    let dir = support::scratch("memory-blob");
    let path = dir.join("large");
    sparse(&path, LARGE);
    let runtime = runtime();
    let ((size, total, largest), peak) = peak_during(|| {
        let file = std::fs::File::open(&path).expect("open");
        let mut blob = FileBlob::hash(file, u64::MAX).expect("read").expect("fits");
        let (mut total, mut largest) = (0u64, 0usize);
        while let Some(chunk) = runtime.block_on(blob.next_chunk()).expect("chunk") {
            total += chunk.len() as u64;
            largest = largest.max(chunk.len());
        }
        (blob.size(), total, largest)
    });
    assert_eq!((size, total), (LARGE, LARGE));
    assert!(largest <= CHUNK, "a chunk of {largest} bytes");
    assert!(peak < BOUND, "{peak} bytes live at once");
    support::force_remove(&dir);
}

/// Catches an output past the byte limit read up to the limit before it is refused:
/// a 2 GiB output against a 1 GiB `--output-max-bytes` fails the collection with no
/// more than [`BOUND`] live at once (seen red with the output read into one buffer
/// up to `limit + 1` bytes, as before).
#[test]
fn an_output_past_the_byte_limit_is_refused_in_bounded_memory() {
    let upper = support::scratch("memory-output");
    std::fs::create_dir(upper.join("out")).expect("mkdir");
    sparse(&upper.join("out/large"), LARGE);
    let limits = OutputLimits {
        max_bytes: 1 << 30,
        ..OutputLimits::DEFAULT
    };
    let runtime = runtime();
    let (outcome, peak) = peak_during(|| {
        let cas = MemoryCas::new();
        let mut result = ActionResult::default();
        let outputs = ["out".to_owned()];
        runtime.block_on(collect(&cas, &upper, "", &outputs, limits, &mut result))
    });
    assert!(
        matches!(
            outcome,
            Err(TreeError::Limit {
                what: Exceeded::Bytes,
                ..
            })
        ),
        "{outcome:?}"
    );
    assert!(peak < BOUND, "{peak} bytes live at once");
    support::force_remove(&upper);
}

/// Catches stdout read whole: an action whose stdout is 2 GiB fails against the
/// default 1 GiB `--output-max-stdio-bytes` with no more than [`BOUND`] live at once
/// in the daemon (seen red with the log read whole, as before), and the lease is
/// cleaned.
#[test]
fn a_stdout_past_its_limit_is_refused_in_bounded_memory() {
    let fake = Fake::new("memory-stdout");
    let spec = Spec::new(&image(), "unused");
    let script = format!(r#"truncate -s {LARGE} "$(dirname "$ROOT")/stdout""#);
    let runtime = runtime();
    let (outcome, peak) = peak_during(|| runtime.block_on(fake.run(1, &spec, &script)));
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why))
            if why.contains("/stdout: ") && why.contains("--output-max-stdio-bytes")),
        "{outcome:?}"
    );
    assert!(peak < BOUND, "{peak} bytes live at once");
    fake.assert_clean(1);
}
