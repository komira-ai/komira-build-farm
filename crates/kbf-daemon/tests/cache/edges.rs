//! The input cache's refusals and failures: directories it will not own, what start-up
//! moves aside, files under `blobs/` that are not entries, and I/O errors that must
//! never fail a lease.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::support::cache_kit::{
    CountingCas, FakeClock, Gates, config, digest, entries, entry_path, file, key, lease,
    on_disk_sum, stop, tmp_files,
};
use crate::support::scratch;
use kbf_daemon::cache::{Cache, Disabled, Point};

const GIB: u64 = 1 << 30;

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn refused(dir: &Path) -> Option<Disabled> {
    Cache::open(config(dir, GIB, Gates::new(), &FakeClock::at(1000))).err()
}

/// Catches a cache directory a lease sweep would delete accepted, and a directory the
/// cache cannot own taken as an empty cache: a missing directory, a `v1` that is a
/// file, a `tmp/` that cannot be cleared, an `index.snap.tmp` that cannot be removed.
/// Each is refused with its reason, and the daemon then runs uncached.
#[test]
fn directories_the_cache_cannot_own_are_refused() {
    let base = scratch("cache-refused");
    for name in ["kbf-lease-1-2", "lease-7"] {
        let dir = base.join(name).join("x");
        let mut config = config(&dir, GIB, Gates::new(), &FakeClock::at(1000));
        config.dir = base.join(name);
        assert_eq!(
            Cache::open(config).err(),
            Some(Disabled::LeaseName),
            "{name}"
        );
    }
    let mut missing = config(&base.join("m"), GIB, Gates::new(), &FakeClock::at(1000));
    missing.dir = base.join("absent");
    let reason = Cache::open(missing).err().map(|e| e.reason());
    assert_eq!(reason, Some("unusable"));
    let file = base.join("file");
    std::fs::create_dir_all(file.join("cache")).expect("dir");
    std::fs::write(file.join("cache/v1"), b"").expect("v1 file");
    assert!(matches!(refused(&file), Some(Disabled::Unusable(why)) if why.contains("v1")));
    let subdir = base.join("subdir");
    std::fs::create_dir_all(subdir.join("cache/v1/tmp/left")).expect("dirs");
    assert!(matches!(refused(&subdir), Some(Disabled::Unusable(why)) if why.contains("tmp")));
    let snap = base.join("snap");
    std::fs::create_dir_all(snap.join("cache/v1/index.snap.tmp")).expect("dirs");
    assert!(
        matches!(refused(&snap), Some(Disabled::Unusable(why)) if why.contains("index.snap.tmp"))
    );
    let blobs = base.join("blobs");
    std::fs::create_dir_all(blobs.join("cache/v1/blobs")).expect("dirs");
    chmod(&blobs.join("cache/v1/blobs"), 0o500);
    let outcome = refused(&blobs);
    chmod(&blobs.join("cache/v1/blobs"), 0o700);
    assert!(matches!(outcome, Some(Disabled::Unusable(why)) if why.contains("blobs")));
    let readonly = base.join("readonly");
    std::fs::create_dir_all(readonly.join("cache")).expect("dirs");
    std::fs::write(readonly.join("cache/v9"), b"").expect("v9");
    chmod(&readonly.join("cache"), 0o500);
    let outcome = refused(&readonly);
    chmod(&readonly.join("cache"), 0o700);
    assert_eq!(outcome.map(|e| e.reason()), Some("unusable"));
    for (disabled, word) in [
        (Disabled::Locked, "locked"),
        (Disabled::NoBudget, "no-budget"),
        (Disabled::NativeSameUid, "native-same-uid"),
        (Disabled::GraphDriver("fuse".into()), "graph-driver"),
    ] {
        assert_eq!(disabled.reason(), word);
        assert!(disabled.to_string().starts_with(word), "{disabled}");
    }
}

/// Catches another layout version read as this one, or anything but a layout version
/// moved: `v2` and `v10` are moved aside, `v1`, `vx` and other names are left.
#[test]
fn other_layout_versions_are_moved_aside() {
    let dir = scratch("cache-aside");
    for name in ["v1", "v2", "v10", "vx", "v", "notes"] {
        std::fs::create_dir_all(dir.join("cache").join(name)).expect("dir");
    }
    std::fs::write(dir.join("cache/v1/kept"), b"").expect("a file in v1");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1234))).expect("open");
    let mut names: Vec<String> = std::fs::read_dir(dir.join("cache"))
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "lock",
            "notes",
            "v",
            "v1",
            "v10.moved-1234",
            "v2.moved-1234",
            "vx"
        ]
    );
    assert!(
        dir.join("cache/v1/kept").exists(),
        "this layout was moved aside"
    );
    stop(cache, &gates);
}

/// Catches a cache on the scratch directory's filesystem put in copy mode because the
/// scratch directory does not exist yet (it is made with the first lease).
#[test]
fn an_absent_scratch_directory_is_judged_by_its_parent() {
    let dir = scratch("cache-scratch");
    let gates = Gates::new();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    config.scratch = dir.join("leases/not/made/yet");
    let cache = Cache::open(config.clone()).expect("open");
    assert_eq!(cache.link_mode().to_string(), "hardlink");
    assert!(cache.link_max() > 1000);
    stop(cache, &gates);
    // One that cannot be looked at is taken as another filesystem's.
    let shut = dir.join("shut");
    std::fs::create_dir(&shut).expect("dir");
    chmod(&shut, 0o000);
    config.scratch = shut.join("leases");
    let cache = Cache::open(config).expect("open");
    chmod(&shut, 0o700);
    assert_eq!(cache.link_mode().to_string(), "copy");
    stop(cache, &gates);
}

/// Catches files under `blobs/` that are not entries counted, adopted, or removed: a
/// name that is not a key, a key's name in another prefix's directory, a directory and
/// a file of the wrong size under a key's name, and a prefix directory that cannot be
/// listed. They are counted as foreign, left in place, and kept out of the total.
#[test]
fn foreign_files_are_counted_and_left_alone() {
    let dir = scratch("cache-foreign");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let blobs = dir.join("cache/v1/blobs");
    let k = key(b"some bytes", false);
    std::fs::write(blobs.join("00/junk"), b"junk").expect("junk");
    let other = if k.hash[0] == 0 { "01" } else { "00" };
    std::fs::write(blobs.join(other).join(k.name()), b"some bytes").expect("wrong prefix");
    std::fs::create_dir(entry_path(&dir, &k)).expect("a directory");
    let sized = key(b"sized", true);
    std::fs::write(entry_path(&dir, &sized), b"not five").expect("wrong size");
    let unreadable = blobs.join("ff");
    chmod(&unreadable, 0o000);
    cache.adopt_pass();
    chmod(&unreadable, 0o700);
    let stats = cache.stats();
    assert_eq!(
        (stats.foreign_files, stats.adopted_entries, stats.bytes),
        (4, 0, 0)
    );
    assert!(blobs.join("00/junk").exists() && entry_path(&dir, &sized).exists());
    stop(cache, &gates);
}

/// Catches an I/O error in the cache failing a lease that would run without it: a fill
/// that cannot write into `tmp/` hands the lease the bytes (not indexed, nothing left
/// in `tmp/`), and an eviction that cannot unlink leaves the file for the pass and is
/// still counted.
#[tokio::test]
async fn cache_io_errors_never_fail_a_lease() {
    let dir = scratch("cache-io");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, 100, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    let tmp = dir.join("cache/v1/tmp");
    chmod(&tmp, 0o500);
    let outcome = lease(
        &cas,
        &cache,
        &cas.tree(&[file("a", b"no room", false), file("b", b"no room", false)]),
        &dir,
        "full",
    )
    .await;
    chmod(&tmp, 0o700);
    let (at, stats) = outcome.expect("the lease ran without the cache");
    assert_eq!(std::fs::read(at.join("b")).expect("read"), b"no room");
    assert_eq!(
        std::fs::metadata(at.join("a")).expect("a").ino(),
        std::fs::metadata(at.join("b")).expect("b").ino()
    );
    assert_eq!(stats.link_fallbacks, 1);
    assert!(!cache.holds(&key(b"no room", false)));
    assert_eq!(tmp_files(&dir), 0);
    let first = key(&[1; 100], false);
    lease(
        &cas,
        &cache,
        &cas.tree(&[file("f", &[1; 100], false)]),
        &dir,
        "one",
    )
    .await
    .expect("one");
    let prefix = dir.join("cache/v1/blobs").join(first.prefix());
    chmod(&prefix, 0o500);
    let outcome = lease(
        &cas,
        &cache,
        &cas.tree(&[file("f", &[2; 100], false)]),
        &dir,
        "two",
    )
    .await;
    chmod(&prefix, 0o700);
    outcome.expect("the lease ran");
    assert!(!cache.holds(&first) && entry_path(&dir, &first).exists());
    assert_eq!(cache.stats().evictions, 1);
    stop(cache, &gates);
}

/// Catches a file another process made at a key's name during its fill being linked
/// or indexed unhashed (the "on EEXIST, link it without hashing" mutant): the rename
/// refuses, the lease gets the fetched bytes, the key is recorded, the next pass adopts
/// the file unverified, and its first use re-hashes it, finds it wrong, and fetches the
/// blob again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_made_outside_during_a_fill_is_never_served_unhashed() {
    let dir = scratch("cache-eexist");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = Arc::new(CountingCas::default());
    let k = key(b"the right one", false);
    let held = gates.hold(move |p| *p == Point::Filled(&k));
    let root = cas.tree(&[file("f", b"the right one", false)]);
    let writing = {
        let (cas, cache, root, dir) = (
            Arc::clone(&cas),
            Arc::clone(&cache),
            root.clone(),
            dir.clone(),
        );
        tokio::spawn(async move { lease(&cas, &cache, &root, &dir, "one").await })
    };
    let waiting = Arc::clone(&gates);
    tokio::task::spawn_blocking(move || waiting.wait_reached(held))
        .await
        .expect("held");
    let outside = entry_path(&dir, &k);
    std::fs::write(&outside, b"the wrong one").expect("outside");
    chmod(&outside, 0o444);
    gates.release(held);
    let (at, _) = writing.await.expect("joined").expect("one");
    assert_eq!(std::fs::read(at.join("f")).expect("read"), b"the right one");
    assert_ne!(
        std::fs::metadata(at.join("f")).expect("f").ino(),
        std::fs::metadata(&outside).expect("e").ino()
    );
    assert!(!cache.holds(&k));
    cache.adopt_pass();
    assert!(cache.holds(&k), "the recorded key was not adopted");
    let (at, _) = lease(&cas, &cache, &root, &dir, "two").await.expect("two");
    assert_eq!(std::fs::read(at.join("f")).expect("read"), b"the right one");
    assert_eq!(cas.reads_of(&digest(b"the right one")), 2);
    assert_eq!(cache.stats().verify_failures, 1);
    stop(cache, &gates);
}

/// Catches a failed snapshot write losing the previous snapshot or going uncounted:
/// with `index.snap.tmp` made a directory, the write fails, and the previous snapshot
/// is what the next start loads.
#[tokio::test]
async fn a_failed_snapshot_write_keeps_the_previous_one() {
    let dir = scratch("cache-snapfail");
    let gates = Gates::running();
    // The background thread's snapshots are held off: they would race the test's.
    let held = gates.hold(|p| *p == Point::PassStarts);
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    gates.wait_reached(held);
    let cas = CountingCas::default();
    lease(
        &cas,
        &cache,
        &cas.tree(&[file("a", b"saved", false)]),
        &dir,
        "a",
    )
    .await
    .expect("a");
    cache.write_snapshot().expect("saved");
    lease(
        &cas,
        &cache,
        &cas.tree(&[file("b", b"not saved", false)]),
        &dir,
        "b",
    )
    .await
    .expect("b");
    std::fs::create_dir(dir.join("cache/v1/index.snap.tmp")).expect("in the way");
    assert!(cache.write_snapshot().is_err());
    assert!(cache.stats().snapshot_failures >= 1);
    stop(cache, &gates);
    std::fs::remove_dir(dir.join("cache/v1/index.snap.tmp")).expect("out of the way");
    let gates = Gates::running();
    let pass = gates.hold(|p| *p == Point::PassStarts);
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000))).expect("open");
    gates.wait_reached(pass);
    assert!(cache.holds(&key(b"saved", false)));
    assert!(!cache.holds(&key(b"not saved", false)));
    gates.release(pass);
    stop(cache, &gates);
}

/// Catches snapshots or passes that never come again after the first: with both a few
/// milliseconds apart, several of each run; a stop then ends the thread at once.
#[test]
fn snapshots_and_passes_repeat_on_their_intervals() {
    let dir = scratch("cache-periodic");
    let gates = Gates::running();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    config.snapshot_every = Duration::from_millis(20);
    config.pass_every = Duration::from_millis(30);
    let cache = Cache::open(config).expect("open");
    let deadline = Instant::now() + Duration::from_secs(30);
    while gates.points().iter().filter(|p| *p == "Listed(0)").count() < 3
        || cache.stats().snapshots < 8
    {
        assert!(Instant::now() < deadline, "{:?}", cache.stats());
        std::thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    stop(cache, &gates);
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Catches a stop that waits for a pass's pause between directories, a second stop
/// that hangs or fails, and the pause kept under free-space pressure (the pass would
/// take 256 of these pauses): with a pause of an hour, a stop ends the paused pass at
/// once; with the floor unreachable, a pass runs through without one.
#[tokio::test]
async fn the_pass_pause_yields_to_a_stop_and_to_free_space_pressure() {
    let dir = scratch("cache-pace");
    let gates = Gates::running();
    let mut paced = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    paced.pass_pace = Duration::from_secs(3600);
    let cache = Cache::open(paced.clone()).expect("open");
    let started = Instant::now();
    while !gates.points().iter().any(|p| p == "Listed(0)") {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the first pass never listed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    cache.stop();
    cache.stop();
    // A pass asked of a stopped cache does nothing.
    cache.adopt_pass();
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(format!("{cache:?}").contains("cache"));
    drop(cache);
    let gates = Gates::running();
    paced.hooks = Arc::clone(&gates) as _;
    paced.min_free_bytes = u64::MAX;
    let pass = gates.hold(|p| *p == Point::PassStarts);
    let cache = Cache::open(paced).expect("open");
    gates.wait_reached(pass);
    let cas = CountingCas::default();
    lease(
        &cas,
        &cache,
        &cas.tree(&[file("a", b"pressure", false)]),
        &dir,
        "a",
    )
    .await
    .expect("a");
    assert!(cache.stats().floor_unreachable > 0);
    let started = Instant::now();
    cache.adopt_pass();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the pass paused under pressure"
    );
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    assert_eq!(entries(&dir).len(), 1);
    gates.release(pass);
    stop(cache, &gates);
}

/// Catches a free-space round that never ends once the floor is met, and the pressure
/// flag left set: with a floor the filesystem has, nothing is evicted.
#[tokio::test]
async fn a_floor_the_filesystem_meets_evicts_nothing() {
    let dir = scratch("cache-floor-met");
    let gates = Gates::new();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    config.min_free_bytes = 1;
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    lease(
        &cas,
        &cache,
        &cas.tree(&[file("a", b"kept", false)]),
        &dir,
        "a",
    )
    .await
    .expect("a");
    assert!(cache.holds(&key(b"kept", false)));
    assert_eq!(
        (cache.stats().evictions, cache.stats().floor_unreachable),
        (0, 0)
    );
    stop(cache, &gates);
}

/// Catches a sibling entry copied without its hash checked (the plain key changed on
/// disk would become the executable key's bytes), or its refused copy left in `tmp/`:
/// the executable key is then fetched, and `tmp/` is empty.
#[tokio::test]
async fn a_sibling_that_hashes_wrong_is_not_copied() {
    let dir = scratch("cache-sibling");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    lease(
        &cas,
        &cache,
        &cas.tree(&[file("a", b"tool", false)]),
        &dir,
        "plain",
    )
    .await
    .expect("plain");
    let plain = entry_path(&dir, &key(b"tool", false));
    chmod(&plain, 0o644);
    std::fs::write(&plain, b"fool").expect("change");
    let (at, _) = lease(
        &cas,
        &cache,
        &cas.tree(&[file("x", b"tool", true)]),
        &dir,
        "exec",
    )
    .await
    .expect("exec");
    assert_eq!(std::fs::read(at.join("x")).expect("read"), b"tool");
    assert_eq!(cas.reads_of(&digest(b"tool")), 2);
    assert_eq!(tmp_files(&dir), 0, "the refused copy is left in tmp/");
    stop(cache, &gates);
}

/// Catches a lease that goes on starting keys after one failed, a second failure
/// replacing the first, and a key near the link limit linked anyway: one key at a time,
/// a missing first blob leaves the second unwritten; two missing blobs fail with one of
/// their errors; with a link limit of 1,001, every key gets a private copy.
#[tokio::test]
async fn a_lease_stops_at_its_first_failure_and_respects_the_link_limit() {
    let dir = scratch("cache-failures");
    let gates = Gates::new();
    let mut serial = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    serial.link_concurrency = 1;
    let cache = Cache::open(serial.clone()).expect("open");
    let cas = CountingCas::default();
    let root = cas.tree(&[
        file("a", b"missing a", false),
        file("b", b"present b", false),
    ]);
    cas.blobs.remove(&digest(b"missing a"));
    // The tree's files are written in name order: `a`, then `b`.
    let outcome = lease(&cas, &cache, &root, &dir, "serial").await;
    assert!(outcome.is_err());
    assert_eq!(
        cas.reads_of(&digest(b"present b")),
        0,
        "a key started after the failure"
    );
    stop(cache, &gates);
    let dir = scratch("cache-failures-two");
    let gates = Gates::new();
    let mut limited = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    limited.link_max = Some(1001);
    let cache = Cache::open(limited).expect("open");
    let cas = CountingCas::default();
    let two = cas.tree(&[file("a", b"gone a", false), file("b", b"gone b", false)]);
    cas.blobs.remove(&digest(b"gone a"));
    cas.blobs.remove(&digest(b"gone b"));
    let outcome = lease(&cas, &cache, &two, &dir, "two").await;
    assert!(
        matches!(outcome, Err(kbf_daemon::tree::TreeError::Cas(_))),
        "{outcome:?}"
    );
    let (at, stats) = lease(
        &cas,
        &cache,
        &cas.tree(&[file("c", b"c", false)]),
        &dir,
        "c",
    )
    .await
    .expect("c");
    assert_eq!(stats.link_fallbacks, 1);
    assert_eq!(std::fs::metadata(at.join("c")).expect("c").nlink(), 1);
    stop(cache, &gates);
}

/// Catches a fallback copy that fails when the entry went away under it: the entry is
/// deleted and the link refused as `EMLINK`, the copy finds no entry, and the key is
/// settled again.
#[tokio::test]
async fn a_fallback_that_finds_the_entry_gone_settles_it_again() {
    let dir = scratch("cache-copy-gone");
    let gates = Gates::new();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deleted = Arc::clone(&once);
    config.link = Arc::new(move |from: &Path, to: &Path| {
        if !deleted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            std::fs::remove_file(from)?;
            return Err(std::io::Error::from_raw_os_error(libc::EMLINK));
        }
        std::fs::hard_link(from, to)
    });
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    let (at, _) = lease(
        &cas,
        &cache,
        &cas.tree(&[file("a", b"again", false)]),
        &dir,
        "a",
    )
    .await
    .expect("a");
    assert_eq!(std::fs::read(at.join("a")).expect("read"), b"again");
    assert_eq!(cas.reads_of(&digest(b"again")), 2);
    stop(cache, &gates);
}

/// Catches the bytes of a fill the cache could not keep written to the shared names
/// only: the private names get writable copies of them too, and a key with private
/// names alone is no fallback.
#[tokio::test]
async fn bytes_the_cache_could_not_keep_reach_private_names_too() {
    let dir = scratch("cache-bytes-private");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    let tmp = dir.join("cache/v1/tmp");
    let root = cas.tree(&[
        file("out/a", b"both", false),
        file("in/b", b"both", false),
        file("out/c", b"only out", true),
    ]);
    let at = crate::support::cache_kit::lease_dir(&dir, "private");
    chmod(&tmp, 0o500);
    let outcome =
        kbf_daemon::tree::materialize_cached(&cas, &cache, &root, &at, &["out".to_owned()]).await;
    chmod(&tmp, 0o700);
    let stats = outcome.expect("written");
    assert_eq!(
        stats.link_fallbacks, 1,
        "the private-only key counted as a fallback"
    );
    assert_eq!(std::fs::read(at.join("out/a")).expect("read"), b"both");
    assert_eq!(std::fs::read(at.join("out/c")).expect("read"), b"only out");
    assert_eq!(
        std::fs::metadata(at.join("out/c"))
            .expect("c")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_ne!(
        std::fs::metadata(at.join("out/a")).expect("a").ino(),
        std::fs::metadata(at.join("in/b")).expect("b").ino()
    );
    stop(cache, &gates);
}

/// Catches the files a previous run left fetched again, or served unhashed (the "link
/// an existing name without hashing" mutant): a cache opened again on the same
/// directory, with no index of them, finds each at its key's name; a lease's fills
/// re-hash them, adopt the one that is right, and fetch the one a byte was flipped in.
#[tokio::test]
async fn files_a_previous_run_left_are_adopted_after_a_re_hash() {
    let dir = scratch("cache-reopen");
    let cas = CountingCas::default();
    let root = cas.tree(&[file("a", b"kept a", false), file("b", b"kept b", true)]);
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    lease(&cas, &cache, &root, &dir, "one").await.expect("one");
    stop(cache, &gates);
    // No index of what is on disk, as after a crash; one file's bytes changed.
    let _ = std::fs::remove_file(dir.join("cache/v1/index.snap"));
    let flipped = entry_path(&dir, &key(b"kept b", true));
    chmod(&flipped, 0o755);
    std::fs::write(&flipped, b"kept B").expect("flip");
    chmod(&flipped, 0o555);
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000))).expect("open");
    let (at, stats) = lease(&cas, &cache, &root, &dir, "two").await.expect("two");
    assert_eq!(
        std::fs::read(at.join("b")).expect("read"),
        b"kept b",
        "served unhashed"
    );
    assert_eq!(
        cas.reads_of(&digest(b"kept a")),
        1,
        "a left file was fetched again"
    );
    assert_eq!(cas.reads_of(&digest(b"kept b")), 2);
    assert_eq!((stats.cache_misses, stats.verify_hashes), (2, 1));
    assert!(cache.holds(&key(b"kept a", false)) && cache.holds(&key(b"kept b", true)));
    stop(cache, &gates);
}

/// Catches a link that finds its entry gone (`ENOENT`, an eviction or a deletion between
/// the hit's check and the link) failing the lease (the "map `ENOENT` at link to a
/// lease error" mutant): the key is settled again and the link retried.
#[tokio::test]
async fn a_link_that_finds_the_entry_gone_refills_it() {
    let dir = scratch("cache-enoent");
    let cas = CountingCas::default();
    let root = cas.tree(&[file("a", b"bytes", false)]);
    let gates = Gates::new();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deleted = Arc::clone(&once);
    config.link = Arc::new(move |from: &Path, to: &Path| {
        if to.ends_with("two/a") && !deleted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            std::fs::remove_file(from)?;
        }
        std::fs::hard_link(from, to)
    });
    let cache = Cache::open(config).expect("open");
    lease(&cas, &cache, &root, &dir, "one").await.expect("one");
    let (at, _) = lease(&cas, &cache, &root, &dir, "two").await.expect("two");
    assert!(once.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(std::fs::read(at.join("a")).expect("read"), b"bytes");
    assert_eq!(cas.reads_of(&digest(b"bytes")), 2);
    assert!(cache.holds(&key(b"bytes", false)) && entry_path(&dir, &key(b"bytes", false)).exists());
    stop(cache, &gates);
}

/// Catches an entry whose file was deleted from outside failing a lease or the
/// evictor: the next lease fetches it again, and an eviction of a name that is gone is
/// no error.
#[tokio::test]
async fn an_entry_deleted_from_outside_is_fetched_again_and_evicted_cleanly() {
    let dir = scratch("cache-deleted");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, 100, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    assert_eq!(cache.dir(), dir.join("cache"));
    let cas = CountingCas::default();
    let one = |c: u8| cas.tree(&[file("f", &[c; 100], false)]);
    lease(&cas, &cache, &one(1), &dir, "one")
        .await
        .expect("one");
    std::fs::remove_file(entry_path(&dir, &key(&[1; 100], false))).expect("deleted");
    lease(&cas, &cache, &one(2), &dir, "two")
        .await
        .expect("two evicts one");
    assert_eq!(cache.stats().evictions, 1);
    let (at, _) = lease(&cas, &cache, &one(1), &dir, "again")
        .await
        .expect("one again");
    assert_eq!(std::fs::read(at.join("f")).expect("read"), [1; 100]);
    assert_eq!(cas.reads_of(&digest(&[1; 100])), 2);
    stop(cache, &gates);
}
