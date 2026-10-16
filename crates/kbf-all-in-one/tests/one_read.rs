//! Each output file is read once on the in-process plane: the container driver's output
//! collection (`kbf_driver_container::tree::collect`, the call a lease makes) over a
//! lease of [`LocalDataPlane`], over a server started as `kbf-server` starts one. The
//! test counts, with inotify, how many times each output file is opened for reading
//! while its outputs are collected and stored, and reads every blob back.
//!
//! The opens are counted outside the code under test: an inotify watch on each
//! directory of the outputs reports every open of a file in it (`IN_OPEN`), by name. A
//! pass that hashes a file and another that reads it again to store it are two opens;
//! the range reads of a second pass open it once per range, so a large file counts
//! more.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use clap::Parser as _;
use kbf_all_in_one::LocalDataPlane;
use kbf_daemon::cpu_pool::CpuPool;
use kbf_driver_container::tree::{OutputLimits, collect};
use kbf_proto::reapi::{ActionResult, Digest, Tree};
use kbf_transfer::{Cas, Record, Source, fetch};
use kbf_types::LeaseId;
use prost::Message as _;
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};

/// Servers started by this binary so far: each directory is one server's alone.
static STARTED: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty directory under Cargo's temporary directory for this target.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-all-in-one-one-read")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// The plane over a server started in this runtime on the stores under `dir`, with a
/// write tier (`tier`) or without one.
async fn plane(dir: &Path, tier: bool) -> Arc<LocalDataPlane> {
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
    if tier {
        argv.push(format!(
            "--hot-store=file://{}",
            dir.join(format!("hot-{n}")).display()
        ));
    } else {
        argv.push(format!(
            "--meta-dir={}",
            dir.join(format!("meta-{n}")).display()
        ));
    }
    let args = kbf_server::Args::try_parse_from(argv).expect("flags");
    let never = || Ok(std::future::pending::<()>());
    let server = kbf_server::start(&args, never).await.expect("start");
    let plane = LocalDataPlane::new(server.local(), tokio::runtime::Handle::current());
    tokio::spawn(server.serve());
    Arc::new(plane)
}

/// `len` bytes no other file here holds: a counter's bytes after `seed`.
fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i % 251) as u8).wrapping_mul(7))
        .collect()
}

/// Watches the directories `dirs` for opens of what is in them.
fn watch(dirs: &[&Path]) -> OwnedFd {
    let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).expect("inotify");
    for dir in dirs {
        inotify::add_watch(&fd, *dir, WatchFlags::OPEN).expect("watch");
    }
    fd
}

/// How many times each file (not directory) in the watched directories was opened
/// since the watch began, by name.
fn opens(fd: &OwnedFd) -> BTreeMap<String, u32> {
    let mut buffer = [MaybeUninit::uninit(); 64 << 10];
    let mut reader = inotify::Reader::new(fd, &mut buffer);
    let mut opens = BTreeMap::new();
    loop {
        let event = match reader.next() {
            Ok(event) => event,
            Err(rustix::io::Errno::AGAIN) => break,
            Err(e) => panic!("reading inotify events: {e}"),
        };
        if event.events().contains(ReadFlags::ISDIR) {
            continue;
        }
        if let Some(name) = event.file_name() {
            *opens
                .entry(name.to_string_lossy().into_owned())
                .or_insert(0) += 1;
        }
    }
    opens
}

/// What the plane holds under `digest`.
async fn read(plane: &impl Cas, digest: Option<&Digest>) -> Vec<u8> {
    fetch(plane, digest.expect("a digest"))
        .await
        .expect("read back")
}

/// Collects `out/small`, `out/large` (several of the plane's 4 MiB ranges) and
/// `out/dir` (holding `nested`) through a lease of the plane, and checks each file was
/// opened once and each blob reads back as the file.
async fn each_output_file_is_read_once(tier: bool) {
    let dir = scratch(if tier { "tier" } else { "no-tier" });
    let plane = plane(&dir.join("server"), tier).await;
    let upper = dir.join("upper");
    let out = upper.join("out");
    let nested_dir = out.join("dir");
    std::fs::create_dir_all(&nested_dir).expect("mkdir");
    let small = b"a small output\n".to_vec();
    let large = pattern(3, (13 << 20) + 5);
    let nested = pattern(11, 70_000);
    std::fs::write(out.join("small"), &small).expect("write");
    std::fs::write(out.join("large"), &large).expect("write");
    std::fs::write(nested_dir.join("nested"), &nested).expect("write");
    let watched = watch(&[&out, &nested_dir]);

    let lease = plane.lease(&LeaseId::new(1, 1));
    let mut result = ActionResult::default();
    let paths = [
        "out/small".to_owned(),
        "out/large".to_owned(),
        "out/dir".to_owned(),
    ];
    collect(
        &lease,
        &upper,
        &CpuPool::new(4),
        "",
        &paths,
        OutputLimits::DEFAULT,
        &mut result,
    )
    .await
    .expect("collect");

    let opens = opens(&watched);
    for name in ["small", "large", "nested"] {
        let reads = opens.get(name).copied().unwrap_or(0);
        assert_eq!(
            reads, 1,
            "output file {name} was opened {reads} times (reads={reads}), not once: {opens:?}"
        );
    }
    let [small_out, large_out] = &result.output_files[..] else {
        panic!("two output files: {result:?}");
    };
    assert_eq!(read(&lease, small_out.digest.as_ref()).await, small);
    assert_eq!(read(&lease, large_out.digest.as_ref()).await, large);
    let [tree] = &result.output_directories[..] else {
        panic!("one output directory: {result:?}");
    };
    let tree =
        Tree::decode(read(&lease, tree.tree_digest.as_ref()).await.as_slice()).expect("a Tree");
    let root = tree.root.expect("a root");
    assert_eq!(root.files.len(), 1, "{root:?}");
    assert_eq!(read(&lease, root.files[0].digest.as_ref()).await, nested);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches an output file read twice on the in-process plane (once by the walk that
/// hashes it, again by the plane that stores it), with a write tier, where the server
/// trusts the daemon's digests at the commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_a_tier_each_output_file_is_read_once() {
    each_output_file_is_read_once(true).await;
}

/// The same without a write tier, where the server hashes each record as it arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_tier_each_output_file_is_read_once() {
    each_output_file_is_read_once(false).await;
}

/// One sparse output over 1 GiB (the size above which a file used not to be taken),
/// collected through a lease of the plane: it must be opened once, and be held after,
/// streamed onto the write tier or, without one, to the store (never held in memory).
async fn a_file_over_1_gib_is_read_once(tier: bool) {
    let dir = scratch(if tier { "huge-tier" } else { "huge-no-tier" });
    let plane = plane(&dir.join("server"), tier).await;
    let upper = dir.join("upper");
    let out = upper.join("out");
    std::fs::create_dir_all(&out).expect("mkdir");
    std::fs::File::create(out.join("huge"))
        .and_then(|file| file.set_len((1 << 30) + (1 << 20)))
        .expect("sparse file");
    let watched = watch(&[&out]);
    let lease = plane.lease(&LeaseId::new(1, 1));
    let mut result = ActionResult::default();
    let collected = collect(
        &lease,
        &upper,
        &CpuPool::new(4),
        "",
        &["out/huge".to_owned()],
        OutputLimits::DEFAULT,
        &mut result,
    )
    .await;
    let opens = opens(&watched);
    let reads = opens.get("huge").copied().unwrap_or(0);
    assert_eq!(
        reads, 1,
        "a file over 1 GiB was opened {reads} times (reads={reads}), not once: {opens:?}"
    );
    // Held either way: streamed onto the tier, or straight to the store without one.
    collected.expect("collect");
    let digest = result.output_files[0].digest.clone().expect("a digest");
    assert!(lease.missing(&[digest]).await.is_empty(), "not held");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches a file over 1 GiB not taken by the plane (read again, or refused): with a
/// write tier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_a_tier_a_file_over_1_gib_is_read_once() {
    a_file_over_1_gib_is_read_once(true).await;
}

/// Catches a file over 16 MiB refused without a write tier (mutant: the refusal
/// restored), or read again to be stored: it streams to the store from its one read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_tier_a_file_over_1_gib_is_read_once() {
    a_file_over_1_gib_is_read_once(false).await;
}

/// Catches a largest blob on the local plane (mutant: the 5 GiB cap restored): a
/// record over 5 GiB, S3's single-PUT limit, is taken like any other with a write tier
/// (a range of its own reserved for it) and its bytes are read; here its source has
/// none, so the read, not its size, fails it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_over_5_gib_is_not_refused_for_its_size() {
    let dir = scratch("over-5-gib");
    let plane = plane(&dir.join("server"), true).await;
    let digest = kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [7; 32], (5 << 30) + 1);
    let record = Record {
        digest,
        crc32c: 0,
        source: Source::Bytes(bytes::Bytes::new()),
    };
    let why = plane
        .put_records(vec![record])
        .await
        .expect_err("a source with no bytes");
    let why = why.to_string();
    assert!(why.contains("could not be read"), "{why}");
    assert!(!why.contains("largest blob"), "{why}");
    let _ = std::fs::remove_dir_all(&dir);
}
