//! The input cache as leases use it (`docs/design/worker-cache.md#10-test-plan`): one
//! fetch per blob however many leases want it, links of one inode per key per lease,
//! the fallbacks to a private copy, the checks on every hit, eviction by budget and by
//! free space, and the one-owner lock; restarts (`restart`), crashes (`crash`) and
//! refusals and I/O errors (`edges`); the node's one cache with local storage and fills
//! that stream (`one_cache`); fills that end without an entry leave nothing in `tmp/`
//! (`tmp_files`); measurements at 1M entries, off the gate (`measure`). One test
//! binary, so each part of the cache is measured in one place.
#![cfg(target_os = "linux")]

#[path = "cache/crash.rs"]
mod crash;
#[path = "cache/edges.rs"]
mod edges;
#[path = "cache/measure.rs"]
mod measure;
#[path = "cache/one_cache.rs"]
mod one_cache;
#[path = "cache/restart.rs"]
mod restart;
mod support;
#[path = "cache/tmp_files.rs"]
mod tmp_files;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::Arc;

use kbf_daemon::cache::{Cache, Disabled, FIXED_MTIME, LinkMode, Point};
use kbf_daemon::cas::CasError;
use kbf_daemon::tree::{
    TreeError, materialize_cached, materialize_cached_phased, materialize_phased,
};
use support::cache_kit::{
    CountingCas, FakeClock, Gates, config, digest, entries, entry_path, file, key, lease,
    lease_dir, on_disk_sum, stop, tmp_files, toolchain,
};
use support::scratch;

const GIB: u64 = 1 << 30;

fn meta(path: &Path) -> std::fs::Metadata {
    std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// C-4. Catches fills not merged per key (the "remove the fill map" mutant: every miss
/// fetches): 32 leases at once over one tree, through a CAS that yields on every read
/// so their misses overlap, read each file blob exactly once, and each lease's files
/// are right.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_leases_fetch_each_blob_once() {
    let dir = scratch("cache-c4");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = Arc::new(CountingCas::default());
    let tree: Vec<_> = (0..64)
        .map(|i| {
            file(
                &format!("d{}/f{i}", i % 4),
                format!("blob {}", i % 48).as_bytes(),
                false,
            )
        })
        .collect();
    let root = cas.tree(&tree);
    let leases: Vec<_> = (0..32)
        .map(|n| {
            let (cas, cache, root, dir) = (
                Arc::clone(&cas),
                Arc::clone(&cache),
                root.clone(),
                dir.clone(),
            );
            tokio::spawn(async move { lease(&cas, &cache, &root, &dir, &format!("l{n}")).await })
        })
        .collect();
    let mut waits = 0;
    for done in leases {
        let (at, stats) = done.await.expect("joined").expect("written");
        waits += stats.fill_waits;
        for (path, bytes, _) in &tree {
            assert_eq!(
                std::fs::read(at.join(path)).expect("read"),
                *bytes,
                "{path}"
            );
        }
    }
    assert_eq!(cas.most_reads_of_one_file(), 1, "a blob was fetched twice");
    assert_eq!(cas.file_reads(), 48);
    assert!(
        waits > 0,
        "no lease waited on another's fill: the leases did not overlap"
    );
    stop(cache, &gates);
}

/// C-7. Catches a hit served without the stat check (the "skip the `fstatat`" mutant):
/// an entry chmodded to 0644 after it was filled is evicted at its next use, fetched
/// again, and back at 0444; the failure is counted.
#[tokio::test]
async fn an_entry_whose_mode_changed_is_evicted_at_its_next_use() {
    let dir = scratch("cache-c7");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    let root = cas.tree(&[file("a", b"a's bytes", false)]);
    lease(&cas, &cache, &root, &dir, "one")
        .await
        .expect("first");
    let entry = entry_path(&dir, &key(b"a's bytes", false));
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let (second, stats) = lease(&cas, &cache, &root, &dir, "two")
        .await
        .expect("second");
    assert_eq!(
        cas.reads_of(&digest(b"a's bytes")),
        2,
        "the changed entry was served"
    );
    assert_eq!(stats.cache_misses, 1);
    assert_eq!(meta(&entry).permissions().mode() & 0o7777, 0o444);
    assert_eq!(std::fs::read(second.join("a")).expect("read"), b"a's bytes");
    assert_eq!(cache.stats().verify_failures, 1);
    stop(cache, &gates);
}

/// C-8. Catches eviction of a key a lease is writing (the "ignore pins" mutant) and a
/// byte total that drifts from what is on disk (an entry counted twice). The budget
/// holds three 100-byte entries. `old` is the least recently used when a lease that
/// needs `c`, `d` and `old`, one key at a time and in that order, runs: its inserts of
/// `c` and `d` evict over the budget, but not `old`, which the lease pins before it
/// starts, so `old` is a hit and never fetched again. A lease's link outlives the
/// entry's name.
#[tokio::test]
async fn the_budget_evicts_least_recently_used_but_never_a_pinned_key() {
    let dir = scratch("cache-c8");
    let gates = Gates::new();
    let clock = FakeClock::at(1000);
    let mut serial = config(&dir, 300, Arc::clone(&gates), &clock);
    serial.link_concurrency = 1;
    let cache = Cache::open(serial).expect("open");
    let cas = CountingCas::default();
    let blob = |c: u8| vec![c; 100];
    let one = |name: &str, c: u8| cas.tree(&[file(name, &blob(c), false)]);
    let (first, _) = lease(&cas, &cache, &one("old", b'o'), &dir, "old")
        .await
        .expect("old");
    for (n, c) in [b'a', b'b'].into_iter().enumerate() {
        clock.advance(60);
        lease(&cas, &cache, &one("x", c), &dir, &format!("l{n}"))
            .await
            .expect("lease");
    }
    clock.advance(60);
    let both = cas.tree(&[
        file("c", &blob(b'c'), false),
        file("d", &blob(b'd'), false),
        file("old", &blob(b'o'), false),
    ]);
    let (_, stats) = lease(&cas, &cache, &both, &dir, "pinned")
        .await
        .expect("pinned lease");
    assert_eq!(
        cas.reads_of(&digest(&blob(b'o'))),
        1,
        "the pinned key was evicted and fetched again"
    );
    assert_eq!(stats.cache_hits, 1);
    assert!(cache.holds(&key(&blob(b'o'), false)));
    assert!(
        !cache.holds(&key(&blob(b'a'), false)),
        "the least recently used key was kept"
    );
    assert!(!cache.holds(&key(&blob(b'b'), false)));
    assert_eq!(cache.stats().bytes, 300);
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    // Three later inserts evict `c`, `d` and then `old` (unpinned now), and the first
    // lease's link to `old` still reads.
    for (n, c) in [b'e', b'f', b'g'].into_iter().enumerate() {
        clock.advance(60);
        lease(&cas, &cache, &one("e", c), &dir, &format!("later{n}"))
            .await
            .expect("later");
    }
    assert!(!cache.holds(&key(&blob(b'o'), false)));
    assert_eq!(std::fs::read(first.join("old")).expect("read"), blob(b'o'));
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    assert_eq!(cache.stats().evictions, 5);
    stop(cache, &gates);
}

/// C-8, two leases. Catches a pin count dropped when the first of two leases that pin
/// one key finishes (the "remove the pin on any release" mutant): the budget holds one
/// 100-byte entry; of two pins on `old`, the first is released, and the other keeps
/// `old` through an insert that puts the cache over the budget, so the round after the
/// insert's lease evicts the new key instead. Once both pins are released, the next
/// insert evicts `old`.
#[tokio::test]
async fn a_key_two_leases_pin_stays_until_both_release_it() {
    let dir = scratch("cache-c8-two");
    let gates = Gates::new();
    let clock = FakeClock::at(1000);
    let cache = Cache::open(config(&dir, 100, Arc::clone(&gates), &clock)).expect("open");
    let cas = CountingCas::default();
    let one = |c: u8| cas.tree(&[file("f", &[c; 100], false)]);
    let old = key(&[b'o'; 100], false);
    lease(&cas, &cache, &one(b'o'), &dir, "old")
        .await
        .expect("old");
    let first = cache.pin(vec![old]);
    let second = cache.pin(vec![old]);
    drop(first);
    clock.advance(60);
    lease(&cas, &cache, &one(b'n'), &dir, "new")
        .await
        .expect("new");
    cache.evict();
    assert!(
        cache.holds(&old),
        "a key still pinned by one lease was evicted when the other released it"
    );
    assert!(
        !cache.holds(&key(&[b'n'; 100], false)),
        "the unpinned key was kept"
    );
    drop(second);
    clock.advance(60);
    lease(&cas, &cache, &one(b't'), &dir, "third")
        .await
        .expect("third");
    assert!(!cache.holds(&old), "a key both leases released was kept");
    assert!(cache.holds(&key(&[b't'; 100], false)));
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    stop(cache, &gates);
}

/// C-10. Catches a key without the executable bit (the "drop `.x`" mutant): one blob
/// named plain twice and executable once is two entries, 0444 and 0555, the plain names
/// one inode and the executable name the other; the second key is a local copy of the
/// first, not a second fetch.
#[tokio::test]
async fn one_blob_plain_and_executable_is_two_entries() {
    let dir = scratch("cache-c10");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    let bytes = b"#!/bin/true\n";
    let root = cas.tree(&[
        file("a", bytes, false),
        file("b", bytes, true),
        file("sub/c", bytes, false),
    ]);
    let (at, _) = lease(&cas, &cache, &root, &dir, "one")
        .await
        .expect("written");
    let (plain, exec) = (
        meta(&entry_path(&dir, &key(bytes, false))),
        meta(&entry_path(&dir, &key(bytes, true))),
    );
    assert_eq!(plain.permissions().mode() & 0o7777, 0o444);
    assert_eq!(exec.permissions().mode() & 0o7777, 0o555);
    assert_eq!(meta(&at.join("a")).ino(), plain.ino());
    assert_eq!(meta(&at.join("sub/c")).ino(), plain.ino());
    assert_eq!(meta(&at.join("b")).ino(), exec.ino());
    assert_eq!(
        cas.reads_of(&digest(bytes)),
        1,
        "the second key was fetched, not copied"
    );
    stop(cache, &gates);
}

/// C-13. Catches a cache on another filesystem than the scratch directory failing
/// leases instead of copying (the "remove the fallback" mutant: no copy mode). The
/// scratch directory here is `/proc`'s filesystem; the lease's files are copies, one
/// inode per key, and still fetched once.
#[tokio::test]
async fn a_cache_on_another_filesystem_copies() {
    let dir = scratch("cache-c13");
    let gates = Gates::new();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    config.scratch = "/proc/self".into();
    let cache = Cache::open(config).expect("open");
    assert_eq!(cache.link_mode(), LinkMode::Copy);
    let cas = CountingCas::default();
    let root = cas.tree(&[file("a", b"bytes", false), file("b", b"bytes", false)]);
    for name in ["one", "two"] {
        let (at, stats) = lease(&cas, &cache, &root, &dir, name)
            .await
            .expect("written");
        let entry = meta(&entry_path(&dir, &key(b"bytes", false)));
        assert_ne!(
            meta(&at.join("a")).ino(),
            entry.ino(),
            "a link in copy mode"
        );
        assert_eq!(
            meta(&at.join("a")).ino(),
            meta(&at.join("b")).ino(),
            "two inodes for one key"
        );
        assert_eq!(meta(&at.join("a")).mtime(), FIXED_MTIME);
        assert_eq!(std::fs::read(at.join("b")).expect("read"), b"bytes");
        assert_eq!(stats.link_fallbacks, 1);
    }
    assert_eq!(cas.reads_of(&digest(b"bytes")), 1);
    stop(cache, &gates);
}

/// C-13. Catches `EMLINK` or `EXDEV` from a link failing the lease (the "remove the
/// fallback" mutant), through a link operation that fails as a full link count or
/// another filesystem would: the lease gets one private copy per key. A link refused
/// for another reason fails the lease, naming the path.
#[tokio::test]
async fn a_link_refused_for_the_link_count_or_filesystem_falls_back_to_a_copy() {
    for (errno, name) in [(libc::EMLINK, "emlink"), (libc::EXDEV, "exdev")] {
        let dir = scratch(&format!("cache-c13-{name}"));
        let gates = Gates::new();
        let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
        config.link =
            Arc::new(move |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(errno)));
        let cache = Cache::open(config).expect("open");
        let cas = CountingCas::default();
        let root = cas.tree(&[file("a", b"bytes", true), file("b", b"bytes", true)]);
        let (at, stats) = lease(&cas, &cache, &root, &dir, "one")
            .await
            .expect("written");
        assert_eq!(stats.link_fallbacks, 1, "{name}");
        assert_eq!(
            meta(&at.join("a")).ino(),
            meta(&at.join("b")).ino(),
            "{name}"
        );
        assert_eq!(
            meta(&at.join("a")).permissions().mode() & 0o7777,
            0o555,
            "{name}"
        );
        stop(cache, &gates);
    }
    let dir = scratch("cache-c13-eperm");
    let gates = Gates::new();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    config.link =
        Arc::new(|_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(libc::EPERM)));
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    let root = cas.tree(&[file("a", b"bytes", false)]);
    let outcome = lease(&cas, &cache, &root, &dir, "one").await;
    assert!(
        matches!(&outcome, Err(TreeError::Io { path, .. }) if path.ends_with("a")),
        "{outcome:?}"
    );
    stop(cache, &gates);
}

/// C-17. Catches the cache not consulted on the hot path (the "`materialize` ignoring
/// the cache" mutant): a second lease of a toolchain-shaped tree of 20,000 files
/// fetches no byte, and every file but the empty ones is a hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_lease_of_a_toolchain_tree_fetches_nothing() {
    let dir = scratch("cache-c17");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    let tree = toolchain(20_000);
    let root = cas.tree(&tree);
    let (_, first) = lease(&cas, &cache, &root, &dir, "one")
        .await
        .expect("first");
    let fetched = cas.file_bytes();
    let started = std::time::Instant::now();
    let (at, second) = lease(&cas, &cache, &root, &dir, "two")
        .await
        .expect("second");
    let link_ms = started.elapsed().as_millis();
    eprintln!("cache: toolchain lease 1 {first}; lease 2 {second}; lease 2 took {link_ms} ms");
    assert_eq!(cas.file_bytes(), fetched, "the second lease fetched bytes");
    assert_eq!(second.files, 20_000);
    assert_eq!(second.empty_files, 400);
    assert_eq!(second.cache_hits, second.files - second.empty_files);
    assert_eq!(second.fetched_bytes, 0);
    assert_eq!(first.cache_misses, first.files - first.empty_files);
    for (path, bytes, _) in tree.iter().step_by(997) {
        assert_eq!(
            std::fs::read(at.join(path)).expect("read"),
            *bytes,
            "{path}"
        );
    }
    stop(cache, &gates);
}

/// C-18 (the link half; `kbf-driver-container`'s real-`crun` test runs `stat` and `tar` in a
/// container). Catches a fallback per name instead of per key per lease: two files of
/// one blob are one inode on a run that links, and on a run where linking the second
/// name fails with `EMLINK`, with the same mode, mtime and bytes both times.
#[tokio::test]
async fn identical_files_are_one_inode_even_when_a_link_fails() {
    let dir = scratch("cache-c18");
    let gates = Gates::new();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    config.link = Arc::new(|from: &Path, to: &Path| {
        if to.parent().is_some_and(|p| p.ends_with("emlink")) && to.ends_with("b") {
            return Err(std::io::Error::from_raw_os_error(libc::EMLINK));
        }
        std::fs::hard_link(from, to)
    });
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    let root = cas.tree(&[file("a", b"same", false), file("b", b"same", false)]);
    let mut seen = Vec::new();
    for name in ["links", "emlink"] {
        let (at, stats) = lease(&cas, &cache, &root, &dir, name)
            .await
            .expect("written");
        let (a, b) = (meta(&at.join("a")), meta(&at.join("b")));
        assert_eq!(a.ino(), b.ino(), "{name}: two inodes for one key");
        assert_eq!(stats.link_fallbacks, u64::from(name == "emlink"), "{name}");
        seen.push((
            a.permissions().mode(),
            a.mtime(),
            a.nlink(),
            std::fs::read(at.join("b")).expect("read"),
        ));
    }
    assert_eq!(seen[0].0, seen[1].0);
    assert_eq!(seen[0].1, seen[1].1);
    assert_eq!(seen[0].3, seen[1].3);
    assert_eq!(seen[1].2, 2, "the private copy has the lease's two names");
    stop(cache, &gates);
}

/// C-20. Catches a free-space floor that empties the cache without freeing space (the
/// "ignore `st_nlink`" mutant): with a floor no eviction can reach, only entries no
/// lease links are evicted; those a lease still links survive, and the round counts
/// `floor_unreachable`.
#[tokio::test]
async fn the_free_space_floor_evicts_only_entries_no_lease_links() {
    let dir = scratch("cache-c20");
    let gates = Gates::new();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    config.min_free_bytes = u64::MAX;
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    let kept = cas.tree(&[file("k1", b"kept 1", false), file("k2", b"kept 2", false)]);
    let gone = cas.tree(&[file("g1", b"gone 1", false), file("g2", b"gone 2", false)]);
    lease(&cas, &cache, &kept, &dir, "running")
        .await
        .expect("running lease");
    let (done, _) = lease(&cas, &cache, &gone, &dir, "done")
        .await
        .expect("finished lease");
    std::fs::remove_dir_all(&done).expect("the finished lease is removed");
    let unreachable = cache.stats().floor_unreachable;
    cache.evict();
    for (bytes, held) in [
        (&b"kept 1"[..], true),
        (b"kept 2", true),
        (b"gone 1", false),
        (b"gone 2", false),
    ] {
        assert_eq!(
            cache.holds(&key(bytes, false)),
            held,
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
    assert!(cache.stats().floor_unreachable > unreachable);
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    stop(cache, &gates);
}

/// Catches an output-named input linked to the cache (the action could then reach the
/// entry through the overlay's upper directory), an empty file looked up or linked,
/// and wrong modes: an input at or under a private path is a writable copy of its own,
/// an empty file is created directly, and a shared name of the same blob is the entry.
#[tokio::test]
async fn private_and_empty_files_are_never_the_entry() {
    let dir = scratch("cache-private");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    let tree = [
        file("out/a", b"blob", false),
        file("out/sub/b", b"blob", true),
        file("in/c", b"blob", false),
        file("in/empty", b"", true),
    ];
    let root = cas.tree(&tree);
    let at = lease_dir(&dir, "one");
    let stats = materialize_cached(&cas, &cache, &root, &at, &["out".to_owned()])
        .await
        .expect("written");
    let entry = meta(&entry_path(&dir, &key(b"blob", false)));
    assert_eq!(meta(&at.join("in/c")).ino(), entry.ino());
    for (path, mode) in [("out/a", 0o644), ("out/sub/b", 0o755)] {
        let private = meta(&at.join(path));
        assert_ne!(private.ino(), entry.ino(), "{path}");
        assert_eq!(private.nlink(), 1, "{path}");
        assert_eq!(private.permissions().mode() & 0o777, mode, "{path}");
    }
    let empty = meta(&at.join("in/empty"));
    assert_eq!(
        (
            empty.len(),
            empty.permissions().mode() & 0o777,
            empty.nlink()
        ),
        (0, 0o755, 1)
    );
    assert_eq!(
        (stats.files, stats.empty_files, stats.cache_misses),
        (4, 1, 3)
    );
    assert!(
        !entries(&dir)
            .keys()
            .any(|name| name.starts_with(&hex::encode([0xe3, 0xb0]))),
        "the empty blob was cached"
    );
    stop(cache, &gates);
}

/// The phases of a write without shared trees (the container driver's `inputs
/// phases:` line), through the cache and without it, over one tree: the root, `out`,
/// `out/sub` and `in`. Catches `dir_reads` or `dirs_made` miscounted, the cache's
/// `links` and `copies` not passed on (0 and 0) or swapped, private copies counted as
/// links, and the plain write's files not all counted as copies.
#[tokio::test]
async fn a_write_without_trees_counts_its_reads_directories_links_and_copies() {
    let dir = scratch("cache-phases");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    let tree = [
        file("out/a", b"blob", false),
        file("out/sub/b", b"blob", true),
        file("in/c", b"blob", false),
        file("in/d", b"other", false),
        file("in/empty", b"", true),
    ];
    let root = cas.tree(&tree);
    let (stats, cached) = materialize_cached_phased(
        &cas,
        &cache,
        &root,
        &lease_dir(&dir, "one"),
        &["out".to_owned()],
    )
    .await
    .expect("written");
    assert_eq!((cached.dir_reads, cached.dirs_made), (4, 3), "{cached:?}");
    // `in/c` and `in/d` are the entries' inodes; `out/a` and `out/sub/b` are private.
    assert_eq!((cached.links, cached.copies), (2, 2), "{cached:?}");
    assert_eq!((stats.links, stats.copies, stats.empty_files), (2, 2, 1));
    assert_eq!(cached.trees_ms, 0);
    let plain = materialize_phased(&cas, &root, &lease_dir(&dir, "two"))
        .await
        .expect("written");
    // Without the cache every name is a file of its own, the empty one included.
    assert_eq!(
        (plain.dir_reads, plain.dirs_made, plain.links, plain.copies),
        (4, 3, 0, 5),
        "{plain:?}"
    );
    stop(cache, &gates);
}

/// Catches a failed fill reported to its waiters as anything but its own error (a
/// missing blob must stay `Missing` for every lease that wanted it) or retried by each
/// of them; and a digest that names no SHA-256 blob passing as a key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_fill_fails_every_waiter_with_its_error() {
    let dir = scratch("cache-missing");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = Arc::new(CountingCas::default());
    let root = cas.tree(&[file("a", b"not stored", false)]);
    cas.blobs.remove(&digest(b"not stored"));
    cas.stall();
    let leases: Vec<_> = (0..8)
        .map(|n| {
            let (cas, cache, root, dir) = (
                Arc::clone(&cas),
                Arc::clone(&cache),
                root.clone(),
                dir.clone(),
            );
            tokio::spawn(async move { lease(&cas, &cache, &root, &dir, &format!("l{n}")).await })
        })
        .collect();
    // The first fill is stalled in its read; the others reach its slot meanwhile.
    while cas.reads_of(&digest(b"not stored")) == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cas.unstall();
    let mut waits = 0;
    for done in leases {
        let outcome = done.await.expect("joined");
        assert!(
            matches!(&outcome, Err(TreeError::Cas(CasError::Missing(_)))),
            "{outcome:?}"
        );
        waits += 1;
    }
    assert_eq!(waits, 8);
    assert_eq!(
        cas.reads_of(&digest(b"not stored")),
        1,
        "a waiter fetched again"
    );
    let mut upper = kbf_proto::reapi::Directory::default();
    upper.files.push(kbf_proto::reapi::FileNode {
        name: "f".to_owned(),
        digest: Some(kbf_proto::reapi::Digest {
            hash: "AB".repeat(32),
            size_bytes: 1,
        }),
        ..Default::default()
    });
    let root = cas.blobs.message(&upper);
    let outcome = materialize_cached(&*cas, &cache, &root, &lease_dir(&dir, "upper"), &[]).await;
    assert!(
        matches!(&outcome, Err(TreeError::Cas(CasError::Missing(_)))),
        "{outcome:?}"
    );
    stop(cache, &gates);
}

/// C-16. Catches two owners of one cache directory and a start-up that cleans before it
/// holds the lock (the "shared lock", "clear `tmp/` before the lock" and "recreate the
/// lock file" mutants): while the first cache holds a fill open in `tmp/`, a second
/// open is refused as `locked`, and the lock file (same inode), the entries and the
/// fill are untouched; the fill then completes, and a third open is still refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_owner_is_refused_and_changes_nothing() {
    let dir = scratch("cache-c16");
    let gates = Gates::new();
    let config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    let cache = Cache::open(config.clone()).expect("open");
    let cas = Arc::new(CountingCas::default());
    lease(
        &cas,
        &cache,
        &cas.tree(&[file("a", b"first", false)]),
        &dir,
        "first",
    )
    .await
    .expect("first");
    let lock_inode = meta(&dir.join("cache/lock")).ino();
    let before = entries(&dir);
    let filling = key(b"second", false);
    let held = gates.hold(move |p| *p == Point::Filled(&filling));
    let root = cas.tree(&[file("b", b"second", false)]);
    let writing = {
        let (cas, cache, dir) = (Arc::clone(&cas), Arc::clone(&cache), dir.clone());
        tokio::spawn(async move { lease(&cas, &cache, &root, &dir, "second").await })
    };
    tokio::task::spawn_blocking({
        let gates = Arc::clone(&gates);
        move || gates.wait_reached(held)
    })
    .await
    .expect("reached");
    assert_eq!(tmp_files(&dir), 1);
    assert_eq!(Cache::open(config.clone()).err(), Some(Disabled::Locked));
    assert_eq!(
        meta(&dir.join("cache/lock")).ino(),
        lock_inode,
        "the lock file was replaced"
    );
    assert_eq!(tmp_files(&dir), 1, "the owner's fill was cleared");
    assert_eq!(entries(&dir), before);
    gates.release(held);
    writing.await.expect("joined").expect("the fill completed");
    assert!(cache.holds(&key(b"second", false)));
    assert_eq!(Cache::open(config).err(), Some(Disabled::Locked));
    stop(cache, &gates);
}

/// R-9 (a). Catches the evictor removing a key without its slot (the "evict without
/// the slot" mutant: a fill would adopt the file the evictor then unlinks, leaving an
/// entry with no file): the evictor is held between the index removal and the unlink
/// of K while a lease needs K; the lease waits on the slot, then fetches and inserts K.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_fill_waits_for_the_evictor_holding_its_key() {
    let dir = scratch("cache-r9a");
    let cas = Arc::new(CountingCas::default());
    let gates = Gates::new();
    let clock = FakeClock::at(1000);
    let cache = Cache::open(config(&dir, 200, Arc::clone(&gates), &clock)).expect("open");
    let one = |c: u8| cas.tree(&[file("f", &[c; 100], false)]);
    let k = key(&[b'k'; 100], false);
    lease(&cas, &cache, &one(b'k'), &dir, "k").await.expect("k");
    clock.advance(60);
    lease(&cas, &cache, &one(b'l'), &dir, "l").await.expect("l");
    let held = gates.hold(move |p| *p == Point::Evicting(&k));
    let evicting = {
        let (cas, cache, root, dir) =
            (Arc::clone(&cas), Arc::clone(&cache), one(b'm'), dir.clone());
        tokio::spawn(async move { lease(&cas, &cache, &root, &dir, "m").await })
    };
    let waiting = Arc::clone(&gates);
    tokio::task::spawn_blocking(move || waiting.wait_reached(held))
        .await
        .expect("held");
    let needing = {
        let (cas, cache, root, dir) =
            (Arc::clone(&cas), Arc::clone(&cache), one(b'k'), dir.clone());
        tokio::spawn(async move { lease(&cas, &cache, &root, &dir, "k-again").await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        cas.reads_of(&digest(&[b'k'; 100])),
        1,
        "K was fetched while the evictor held it"
    );
    gates.release(held);
    evicting.await.expect("joined").expect("m");
    let (at, _) = needing.await.expect("joined").expect("k again");
    assert_eq!(std::fs::read(at.join("f")).expect("read"), [b'k'; 100]);
    assert!(cache.holds(&k));
    assert!(entry_path(&dir, &k).exists(), "K is indexed with no file");
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    stop(cache, &gates);
}
