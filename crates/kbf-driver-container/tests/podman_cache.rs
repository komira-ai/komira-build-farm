//! The driver with the node's input cache, against real rootless `crun`
//! (`docs/design/worker-cache.md#51-container-driver-linux-rootless-podman-hard-links-into-the-overlay-lower`,
//! tests C-1, C-2, C-12 and C-18). Run by `cargo xtask podman-tests`, as the other
//! real-`crun` tests are: each is `#[ignore]` with that reason.
//!
//! A cached input is a hard link to an entry owned by the daemon's user, which the
//! container's id mapping (no id of the daemon user) maps to nothing in the container:
//! the overlay never writes its lower layer, and the container's root may not open the
//! file for writing or change its mode. The driver's `__chown-dirs` and `__unpack-layers`
//! helpers are this test binary.

mod support;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_daemon::cache::{CacheConfig, Key};
use kbf_daemon::cas::digest_of;
use kbf_driver_container::{CacheUse, PodmanConfig};
use support::real::{Cell, sh};

/// An empty cache beside `config`'s scratch directory, linking with `link`. Like the
/// scratch directory, it is emptied first: CI restores `target` from an earlier run.
fn with_cache(config: &mut PodmanConfig, link: Option<Arc<kbf_daemon::cache::LinkFn>>) {
    let dir = PathBuf::from(format!("{}-cache", config.scratch.display()));
    support::force_remove(&dir);
    std::fs::create_dir_all(&dir).expect("cache dir");
    let mut cache = CacheConfig::new(dir, config.scratch.clone(), 1 << 30);
    if let Some(link) = link {
        cache.link = link;
    }
    config.cache = Some(CacheUse {
        config: cache,
        helper: std::env::current_exe().expect("this test binary"),
        trees: None,
    });
}

/// The entry of `bytes` (plain) in `cell`'s cache.
fn entry(cell: &Cell, bytes: &[u8]) -> PathBuf {
    let key = Key::of(&digest_of(bytes), false).expect("a key");
    let cache = cell.config.cache.as_ref().expect("a cache");
    (cache.config.dir)
        .join("v1/blobs")
        .join(key.prefix())
        .join(key.name())
}

/// Asserts the entry at `path` still holds `bytes`, read-only and the daemon's.
fn unchanged(path: &Path, bytes: &[u8]) {
    let meta = std::fs::metadata(path).expect("the entry");
    assert_eq!(
        std::fs::read(path).expect("read"),
        bytes,
        "the cached inode was written"
    );
    assert_eq!(
        meta.permissions().mode() & 0o7777,
        0o444,
        "its mode changed"
    );
    assert_eq!(
        meta.uid(),
        rustix::process::geteuid().as_raw(),
        "it was re-owned"
    );
    assert_eq!(
        digest_of(&std::fs::read(path).expect("read")),
        digest_of(bytes),
        "re-hash"
    );
}

/// C-1. Catches a cached inode a container can write or re-own (the "`chown -hR 1:1`
/// over `root`, files included" mutant): an action appends to, truncates, chmods,
/// opens for read and write, and links-then-appends a cached input. Each write is
/// refused (in place, the action gets EACCES), the entry is unchanged, and a second
/// lease reads its bytes. The graph driver and the overlay's mount options are
/// printed, which is what this test proved the behaviour on.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask podman-tests"]
async fn a_container_cannot_write_or_re_own_a_cached_input() {
    let cell = Cell::with("cache-c1", |config| with_cache(config, None));
    // The overlay is the kernel's, mounted by `crun` inside the container's mount
    // namespace; the mountinfo line the action prints below shows its options.
    let mut spec = sh("grep ' /kbf/root ' /proc/self/mountinfo; \
         for try in 'echo x >> in' 'truncate -s0 in' 'chmod u+w in' 'exec 3<>in' 'ln in in2 && echo y >> in2'; do \
           if sh -c \"$try\" 2>/dev/null; then echo \"WROTE: $try\"; else echo \"refused: $try\"; fi; \
         done");
    spec.inputs = vec![("in", b"cached bytes", false)];
    let result = cell.run(1, &spec).await.expect("ran");
    let said = cell.stdout(&result);
    eprintln!("{said}");
    assert!(!said.contains("WROTE"), "{said}");
    assert_eq!(said.matches("refused: ").count(), 5, "{said}");
    unchanged(&entry(&cell, b"cached bytes"), b"cached bytes");
    let mut second = sh("cat in");
    second.inputs = spec.inputs.clone();
    let result = cell.run(2, &second).await.expect("read");
    assert_eq!(cell.stdout(&result), "cached bytes");
    cell.assert_clean(2);
}

/// C-2. Catches an input an output names linked to the cache (the "keep
/// `prepare_outputs`' rename of a link, with the upper directory's chown" mutant: the
/// chown would give the cached inode to the container's root): the action appends to
/// it, the output has the new bytes, and the entry has the old.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask podman-tests"]
async fn an_output_that_is_an_input_is_a_private_copy() {
    let cell = Cell::with("cache-c2", |config| with_cache(config, None));
    let mut spec = sh("printf ' more' >> out/o.txt && cat same.txt");
    spec.inputs = vec![("out/o.txt", b"orig", false), ("same.txt", b"orig", false)];
    spec.outputs = vec!["out/o.txt".to_owned()];
    let result = cell.run(1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", cell.stderr(&result));
    assert_eq!(cell.stdout(&result), "orig");
    let output = support::blob(&cell.cas, result.output_files[0].digest.as_ref());
    assert_eq!(output, b"orig more");
    unchanged(&entry(&cell, b"orig"), b"orig");
    cell.assert_clean(1);
}

/// C-12 (C1's half: no subtree is reused yet). Catches one lease's changes reaching the
/// next: lease 1 creates and deletes files in a tree and tries to rewrite one (keeping
/// its size, so the stat check alone would not catch it) and to rename another; lease
/// 2 with the same input root sees it pristine. In place, the rewrite gets `EACCES`
/// and the rename `EOVERFLOW`: overlayfs refuses to copy up a file whose owner the
/// container does not map, which a rename needs (`worker-cache.md`, "C1 as built").
/// Mutant: the input root mounted without the overlay and its files given to the
/// container's root, so the rewrite lands in the cache's inode.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask podman-tests"]
async fn a_lease_sees_none_of_the_last_ones_changes() {
    let cell = Cell::with("cache-c12", |config| with_cache(config, None));
    let inputs = vec![
        ("lib/a", &b"a bytes"[..], false),
        ("lib/b", b"b bytes", false),
    ];
    let mut first = sh("echo new > lib/new; \
         printf 'A bytes' 2>/dev/null > lib/a || echo 'rewrite refused'; \
         mv lib/b lib/c 2>/dev/null || echo 'rename refused'; \
         rm lib/a; ls lib");
    first.inputs = inputs.clone();
    let result = cell.run(1, &first).await.expect("lease 1");
    let said = cell.stdout(&result);
    eprintln!("lease 1: {said}");
    assert_eq!(said, "rewrite refused\nrename refused\nb\nnew\n");
    let mut second = sh("ls lib; cat lib/a lib/b");
    second.inputs = inputs;
    let result = cell.run(2, &second).await.expect("lease 2");
    assert_eq!(cell.stdout(&result), "a\nb\na bytesb bytes");
    unchanged(&entry(&cell, b"a bytes"), b"a bytes");
    cell.assert_clean(2);
}

/// C-18. Catches a link fallback per name instead of per key per lease: two inputs
/// with one blob are one inode in the container (`stat`) on a run that links them and
/// on one where linking the second name fails with `EMLINK`, and a `tar` of them is
/// the same bytes both times (it records the hard link, the modes and the mtimes).
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask podman-tests"]
async fn identical_inputs_are_one_inode_and_tar_alike_on_every_run() {
    let refuse = Arc::new(AtomicBool::new(false));
    let refusing = Arc::clone(&refuse);
    let link: Arc<kbf_daemon::cache::LinkFn> = Arc::new(move |from: &Path, to: &Path| {
        if refusing.load(Ordering::SeqCst) && to.ends_with("b") {
            return Err(rustix::io::Errno::MLINK.into());
        }
        std::fs::hard_link(from, to)
    });
    let cell = Cell::with("cache-c18", |config| with_cache(config, Some(link)));
    let mut spec = sh("stat -c %i a b | uniq | wc -l; tar -cf - a b | sha256sum");
    spec.inputs = vec![("a", b"same", false), ("b", b"same", false)];
    let mut seen = Vec::new();
    for (seq, emlink) in [(1, false), (2, true)] {
        refuse.store(emlink, Ordering::SeqCst);
        let result = cell.run(seq, &spec).await.expect("ran");
        let said = cell.stdout(&result);
        let (inodes, tar) = said.split_once('\n').expect("two lines");
        assert_eq!(
            inodes.trim(),
            "1",
            "EMLINK {emlink}: two inodes for one key: {said}"
        );
        let inputs = cell.runtime.last_inputs().expect("counted");
        assert_eq!(inputs.link_fallbacks, u64::from(emlink), "{inputs}");
        seen.push(tar.to_owned());
    }
    assert_eq!(
        seen[0], seen[1],
        "the tar of the inputs changed with the fallback"
    );
    cell.assert_clean(2);
}

// Shared read-only trees (`docs/design/fast-sandbox.md#44-correctness-an-action-cannot-modify-a-shared-tree`).
// The daemon-side tests (`kbf-daemon/tests/trees.rs`) show which directories become mounts and
// what a tree is on disk; the stand-in `crun` shows the bind reaches the bundle.
// Only a kernel shows the rest, below.

/// A cache and an empty tree store beside `config`'s scratch directory. A store left by
/// an earlier run (CI restores `target`, `CARGO_TARGET_TMPDIR` included) would answer
/// the first lease's tree as a hit instead of a build.
fn with_trees(config: &mut PodmanConfig) {
    with_cache(config, None);
    let trees = PathBuf::from(format!("{}-trees", config.scratch.display()));
    support::force_remove(&trees);
    let mut trees = kbf_daemon::trees::TreeConfig::new(trees, 1 << 30);
    trees.min_files = 20;
    config.cache.as_mut().expect("a cache").trees = Some(trees);
}

fn toolchain() -> Vec<support::Input> {
    let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    (0..30)
        .map(|i| {
            let path = leak(format!("tc/lib/f{i}"));
            let bytes: &'static [u8] = leak(format!("tool file {i}")).as_bytes();
            (path, bytes, i == 0)
        })
        .collect()
}

/// Catches a shared tree the container can change or lift the read-only flag from: the
/// action tries to create a file, append to and truncate a file, chmod, make a
/// directory, rename a file inside the tree and out of it, hard-link a tree file into
/// the input root (`EXDEV`: another mount), and remount the tree read-write as the
/// container's root with its own mount namespace. Every attempt is refused and the next
/// lease reads the tree whole. The mountinfo line of the tree is printed: it shows
/// `ro`, which is what this test proved the behaviour on.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask podman-tests"]
async fn a_container_cannot_change_a_shared_tree_or_lift_its_read_only_flag() {
    let cell = Cell::with("trees-ro", with_trees);
    let mut spec = sh("grep ' /kbf/root/tc ' /proc/self/mountinfo; \
         for try in 'echo x > tc/new' 'echo x >> tc/lib/f1' 'truncate -s0 tc/lib/f1' 'chmod u+w tc/lib/f1' \
                    'mkdir tc/d' 'mv tc/lib/f1 tc/lib/g' 'mv tc/lib/f2 f2' 'ln tc/lib/f3 f3' \
                    'rm tc/lib/f4' 'mount -o remount,rw /kbf/root/tc'; do \
           if sh -c \"$try\" 2>/dev/null; then echo \"WROTE: $try\"; else echo \"refused: $try\"; fi; \
         done; cat tc/lib/f1; echo; ls tc/lib | wc -l");
    spec.inputs = toolchain();
    spec.inputs.push(("src/a", b"small", false));
    let result = cell.run(1, &spec).await.expect("ran");
    let said = cell.stdout(&result);
    eprintln!("{said}");
    assert!(!said.contains("WROTE"), "{said}");
    assert_eq!(said.matches("refused: ").count(), 10, "{said}");
    assert!(
        said.lines().next().is_some_and(|l| l.contains(" ro")),
        "{said}"
    );
    assert!(said.ends_with("tool file 1\n30\n"), "{said}");
    let mut second = sh("cat tc/lib/f1; echo; ls tc/lib | wc -l; test -x tc/lib/f0 && echo exec");
    second.inputs = spec.inputs.clone();
    let result = cell.run(2, &second).await.expect("lease 2");
    assert_eq!(cell.stdout(&result), "tool file 1\n30\nexec\n");
    let stats = cell.runtime.tree_stats().expect("a store");
    assert_eq!((stats.builds, stats.hits), (1, 1), "{stats:?}");
    cell.assert_clean(2);
}

/// Catches a cache or tree store that an earlier run of a test leaves for the next (the
/// "do not empty them in `with_cache`/`with_trees`" mutant: `Cell` removes only its
/// scratch directory, and CI keeps `target` between runs). The same cell runs twice in
/// one process, each time from `Cell::with`; the second run's first lease builds the
/// tree again and finds none of its inputs in the cache, as the first run's did.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask podman-tests"]
async fn a_cache_and_tree_store_start_empty_whatever_an_earlier_run_left() {
    let mut spec = sh("cat src/a; echo; ls tc/lib | wc -l");
    spec.inputs = toolchain();
    spec.inputs.push(("src/a", b"small", false));
    for run in 1..=2 {
        let cell = Cell::with("trees-again", with_trees);
        let result = cell.run(1, &spec).await.expect("ran");
        assert_eq!(cell.stdout(&result), "small\n30\n", "run {run}");
        let stats = cell.runtime.tree_stats().expect("a store");
        assert_eq!((stats.builds, stats.hits), (1, 0), "run {run}: {stats:?}");
        let inputs = cell.runtime.last_inputs().expect("counted");
        assert_eq!(inputs.cache_hits, 0, "run {run}: {inputs}");
        cell.assert_clean(1);
    }
}
