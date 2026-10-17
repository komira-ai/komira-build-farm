//! Shared read-only trees as leases use them (`docs/design/fast-sandbox.md#4-shared-trees`,
//! step 1): which Directory nodes of an input root become mounts, that a lease's setup
//! work no longer grows with a tree's file count, one build for concurrent leases,
//! eviction that spares what a lease holds, and what the tree on disk is.
//!
//! What this file cannot show: that a running container's write into a mount fails
//! `EROFS` (needs a mount namespace); `kbf-driver-container`'s `crun_cache.rs` does,
//! on real rootless `crun`. Here the other barrier is checked: the tree's files are not
//! writable by anyone, and are the cache's inodes.
#![cfg(target_os = "linux")]

mod support;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_daemon::cache::Cache;
use kbf_daemon::cas::{CasError, digest_of};
use kbf_daemon::trees::{Inputs, TreeConfig, Trees};
use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode, SymlinkNode};
use prost::Message as _;
use support::cache_kit::{CountingCas, FakeClock, Gates, config, lease_dir};
use support::scratch;

const GIB: u64 = 1 << 30;

/// A node's cache, tree store and CAS in a scratch directory; `links` counts every hard
/// link the cache makes (tree builds and leases alike).
struct Kit {
    dir: PathBuf,
    cache: Arc<Cache>,
    cas: Arc<CountingCas>,
    trees: Trees,
    links: Arc<AtomicU64>,
}

fn kit(name: &str, tune: impl FnOnce(&mut TreeConfig)) -> Kit {
    let dir = scratch(name);
    let links = Arc::new(AtomicU64::new(0));
    let mut cache_config = config(&dir, GIB, Gates::running(), &FakeClock::at(1000));
    let counted = Arc::clone(&links);
    cache_config.link = Arc::new(move |from: &Path, to: &Path| {
        counted.fetch_add(1, Ordering::SeqCst);
        std::fs::hard_link(from, to)
    });
    let cache = Cache::open(cache_config).expect("open the cache");
    let mut trees = TreeConfig::new(dir.join("trees"), GIB);
    tune(&mut trees);
    Kit {
        trees: Trees::open(trees).expect("open the store"),
        cas: Arc::new(CountingCas::default()),
        cache,
        links,
        dir,
    }
}

impl Kit {
    async fn lease(&self, root: &Digest, name: &str) -> (PathBuf, Inputs) {
        self.lease_with(root, name, &[], "").await
    }

    async fn lease_with(
        &self,
        root: &Digest,
        name: &str,
        outputs: &[&str],
        working_directory: &str,
    ) -> (PathBuf, Inputs) {
        let dir = lease_dir(&self.dir, name);
        let outputs: Vec<String> = outputs.iter().map(|s| (*s).to_owned()).collect();
        let inputs = self
            .trees
            .materialize(
                &*self.cas,
                &self.cache,
                root,
                &dir,
                &outputs,
                working_directory,
            )
            .await
            .expect("materialize");
        (dir, inputs)
    }
}

/// `count` files under `prefix`, in a few subdirectories; `tag` makes their bytes (and
/// so the tree's digest) its own.
fn files(prefix: &str, count: usize, tag: &str) -> Vec<(String, Vec<u8>, bool)> {
    (0..count)
        .map(|i| {
            (
                format!("{prefix}/d{}/f{i}", i % 7),
                format!("{tag} file {i}").into_bytes(),
                i % 20 == 0,
            )
        })
        .collect()
}

fn small() -> Vec<(String, Vec<u8>, bool)> {
    ["src/a.c", "src/b.c", "BUILD", "pkg/x.h", "pkg/y.h"]
        .iter()
        .map(|p| ((*p).to_owned(), format!("small {p}").into_bytes(), false))
        .collect()
}

fn paths(inputs: &Inputs) -> Vec<&str> {
    inputs.mounts.iter().map(|m| m.path.as_str()).collect()
}

/// The digest of the directory tree at `path`, computed from its bytes: what a Directory
/// message of it would be.
fn digest_of_dir(path: &Path) -> Digest {
    let mut names: Vec<_> = std::fs::read_dir(path)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    names.sort();
    let mut directory = Directory::default();
    for name in names {
        let at = path.join(&name);
        let name = name.to_string_lossy().into_owned();
        let meta = std::fs::symlink_metadata(&at).expect("meta");
        if meta.file_type().is_symlink() {
            directory.symlinks.push(SymlinkNode {
                name,
                target: std::fs::read_link(&at)
                    .expect("link")
                    .to_string_lossy()
                    .into_owned(),
                ..SymlinkNode::default()
            });
        } else if meta.is_dir() {
            directory.directories.push(DirectoryNode {
                name,
                digest: Some(digest_of_dir(&at)),
            });
        } else {
            directory.files.push(FileNode {
                name,
                digest: Some(digest_of(&std::fs::read(&at).expect("read"))),
                is_executable: meta.mode() & 0o111 != 0,
                ..FileNode::default()
            });
        }
    }
    digest_of(&directory.encode_to_vec())
}

/// Catches the tree not being a mount (the classifier never selecting, or the lease
/// still linking the tree's files), the mount point missing from the input root, the
/// small files not linked as before, and a tree whose contents are not the Directory it
/// is named for: the digest of the built tree, recomputed from its bytes, is the
/// node's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_directory_is_a_mount_whose_content_is_its_digest() {
    let kit = kit("trees-digest", |_| {});
    let mut input = files("toolchain", 300, "zig");
    input.extend(small());
    let root = kit.cas.tree(&input);
    let (dir, inputs) = kit.lease(&root, "lease-1").await;

    assert_eq!(paths(&inputs), ["toolchain"]);
    assert_eq!(inputs.tree_files, 300);
    assert_eq!(inputs.trees_built, 1);
    let tree = &inputs.mounts[0].source;
    assert!(tree.starts_with(kit.dir.join("trees")), "{tree:?}");
    // The mount point: an empty directory in the input root, and the small files linked.
    assert_eq!(std::fs::read_dir(dir.join("toolchain")).unwrap().count(), 0);
    assert_eq!(
        std::fs::read(dir.join("src/a.c")).unwrap(),
        b"small src/a.c"
    );
    assert_eq!(inputs.stats.files, 5, "{}", inputs.stats);
    // The tree is the Directory it is named for (recomputed from its bytes), and its
    // files are the cache's inodes.
    let mut expected = files("", 300, "zig");
    for (path, _, _) in &mut expected {
        *path = path.trim_start_matches('/').to_owned();
    }
    let node = kit.cas.tree(&expected);
    assert_eq!(digest_of_dir(tree), node, "the tree is not its digest");
    let (file, other) = (tree.join("d1/f1"), tree.join("d2/f2"));
    assert_eq!(std::fs::read(&file).unwrap(), b"zig file 1");
    assert_ne!(
        std::fs::metadata(&file).unwrap().ino(),
        std::fs::metadata(&other).unwrap().ino()
    );
    assert!(
        std::fs::metadata(&file).unwrap().nlink() >= 2,
        "a cache link"
    );
    // What a log line or a failed assertion shows.
    let shown = format!("{:?} {:?}", kit.trees, inputs.trees[0]);
    assert!(
        shown.contains("Trees") && shown.contains("TreeLease"),
        "{shown}"
    );
}

/// Catches the tree's files being writable (the barrier under the read-only mount: the
/// files are the cache's, 0444 or 0555, not group or other writable), executables
/// losing their bit, and directories the container's root (an "other" user) cannot read
/// or traverse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_trees_files_are_not_writable_and_directories_are_readable() {
    let kit = kit("trees-modes", |_| {});
    let root = kit.cas.tree(&files("toolchain", 300, "modes"));
    let (_, inputs) = kit.lease(&root, "lease-1").await;
    let tree = inputs.mounts[0].source.clone();
    let mut stack = vec![tree.clone()];
    let mut seen = 0;
    while let Some(dir) = stack.pop() {
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "{dir:?}");
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            let mode = meta.permissions().mode() & 0o7777;
            assert!(mode == 0o444 || mode == 0o555, "{path:?} is {mode:o}");
            seen += 1;
        }
    }
    assert_eq!(seen, 300);
    // As the file's number says: f20 is executable, f21 is not.
    let exec = std::fs::metadata(tree.join("d6/f20"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(exec & 0o777, 0o555);
    if rustix::process::geteuid().is_root() {
        return;
    }
    let write = std::fs::OpenOptions::new()
        .append(true)
        .open(tree.join("d1/f1"));
    assert_eq!(
        write.expect_err("a tree file opened for writing").kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

/// The measure: what a lease costs to set up no longer scales with the tree. Catches the
/// tree being linked per lease (the "no classifier" mutant): the second lease over a
/// 1,500-file tree makes the same number of link calls as over a 300-file tree, and that
/// number is the small files; the control without trees makes one per file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leases_setup_work_does_not_grow_with_the_tree() {
    let mut warm = Vec::new();
    for count in [300, 1500] {
        let kit = kit(&format!("trees-links-{count}"), |_| {});
        let mut input = files("toolchain", count, "scale");
        input.extend(small());
        let root = kit.cas.tree(&input);
        let (_, first) = kit.lease(&root, "lease-1").await;
        let cold = kit.links.load(Ordering::SeqCst);
        assert!(
            cold >= count as u64,
            "the build links every file once: {cold}"
        );
        let (_, second) = kit.lease(&root, "lease-2").await;
        assert_eq!(second.trees_built, 0, "the tree is there");
        warm.push(kit.links.load(Ordering::SeqCst) - cold);
        drop((first, second));
    }
    assert_eq!(
        warm,
        [5, 5],
        "link calls of a warm lease at 300 and 1500 files"
    );

    // The control: the same trees through the cache without shared trees.
    let mut control = Vec::new();
    for count in [300, 1500] {
        let kit = kit(&format!("trees-control-{count}"), |c| {
            c.min_files = u64::MAX
        });
        let mut input = files("toolchain", count, "scale");
        input.extend(small());
        let root = kit.cas.tree(&input);
        kit.lease(&root, "lease-1").await;
        let cold = kit.links.load(Ordering::SeqCst);
        let (_, second) = kit.lease(&root, "lease-2").await;
        assert!(second.mounts.is_empty());
        control.push(kit.links.load(Ordering::SeqCst) - cold);
    }
    assert_eq!(
        control,
        [305, 1505],
        "without trees a lease links every file"
    );
}

/// A warm node does not read the Directory messages below a tree again: the walk stops
/// at the tree. Catches the summary not being kept (every lease re-walking the
/// toolchain, a CAS round trip per directory).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_warm_lease_does_not_read_the_directories_below_a_tree() {
    let kit = kit("trees-warm-walk", |_| {});
    let mut input = files("toolchain", 300, "walk");
    input.extend(small());
    let root = kit.cas.tree(&input);
    kit.lease(&root, "lease-1").await;
    let first = kit.cas.most_reads_of_one_file();
    assert_eq!(first, 1, "each blob read once for the first lease");
    // Root, `src`, `pkg`, `toolchain` and its 7 subdirectories, each read once: the tree's
    // build uses the messages the lease's walk read.
    assert_eq!(kit.cas.directory_reads(), 11);
    let before = kit.cas.directory_reads();
    let (_, second) = kit.lease(&root, "lease-2").await;
    // The second lease reads the root, `src` and `pkg`: nothing below `toolchain`, and
    // no file blob (the small files are in the cache).
    assert_eq!(
        kit.cas.directory_reads() - before,
        3,
        "reads of the second lease"
    );
    assert_eq!(
        kit.cas.file_reads(),
        305,
        "file blobs are read by the first lease only"
    );
    drop(second);
}

/// The lease's own count of what its inputs cost (`Inputs::phases`, the container
/// driver's `inputs phases:` line) agrees with the CAS and the files. Catches
/// `dir_reads` counting only the walk below the classifier (a warm lease's walk reads
/// nothing: it would say 0, and a cold one 0 too), counting a tree build's reads or the
/// file blobs (more than 11 cold), `links` counting private copies too (6, not 5), and
/// `dirs_made` counting the tree's own directories (more than 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leases_phases_count_its_reads_directories_links_and_copies() {
    let kit = kit("trees-phases", |_| {});
    let mut input = files("toolchain", 300, "phases");
    input.extend(small());
    input.push(("src/o.txt".to_owned(), b"output input".to_vec(), false));
    let root = kit.cas.tree(&input);
    let (_, cold) = kit.lease_with(&root, "cold", &["src/o.txt"], "").await;
    assert_eq!(paths(&cold), ["toolchain"]);
    // As `a_warm_lease_does_not_read_the_directories_below_a_tree`: root, `src`, `pkg`,
    // `toolchain` and its 7 subdirectories, read by the classifier; the tree's build and
    // the walk read none again.
    assert_eq!(kit.cas.directory_reads(), 11);
    assert_eq!(cold.phases.dir_reads, 11, "{:?}", cold.phases);
    let before = kit.cas.directory_reads();
    let (_, warm) = kit.lease_with(&root, "warm", &["src/o.txt"], "").await;
    assert_eq!(kit.cas.directory_reads() - before, 3);
    assert_eq!(warm.phases.dir_reads, 3, "{:?}", warm.phases);
    for (lease, inputs) in [("cold", &cold), ("warm", &warm)] {
        let p = inputs.phases;
        // The files outside the tree: 5 small ones linked, the output-named one copied.
        assert_eq!(inputs.stats.files, 6, "{lease}: {p:?}");
        assert_eq!((p.links, p.copies), (5, 1), "{lease}: {p:?}");
        // `src`, `pkg` and the mount point `toolchain`; nothing inside the tree.
        assert_eq!(p.dirs_made, 3, "{lease}: {p:?}");
    }
    assert_eq!((cold.trees_built, warm.trees_built), (1, 0));
}

/// Catches a build per lease: two leases that want one tree at once share one build
/// (the "no single flight" mutant builds twice and reads every blob twice). The CAS is
/// stalled so the second lease is certain to find the first one building.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_leases_share_one_build() {
    let kit = Arc::new(kit("trees-single-flight", |_| {}));
    let root = kit.cas.tree(&files("toolchain", 300, "flight"));
    kit.cas.stall();
    let spawn = |name: &'static str| {
        let (kit, root) = (Arc::clone(&kit), root.clone());
        tokio::spawn(async move { kit.lease(&root, name).await })
    };
    let first = spawn("lease-1");
    let second = spawn("lease-2");
    // Both are in `acquire`: one builds, blocked on its first blob, the other waits.
    wait_for(|| kit.trees.store().stats().waits >= 1).await;
    assert_eq!(kit.trees.store().stats().builds, 0);
    kit.cas.unstall();
    let done = tokio::time::timeout(Duration::from_secs(30), async {
        (first.await.unwrap(), second.await.unwrap())
    });
    let ((_, a), (_, b)) = done.await.expect("the leases never finished");
    let stats = kit.trees.store().stats();
    assert_eq!((stats.builds, stats.trees), (1, 1), "{stats:?}");
    assert_eq!(a.mounts[0].source, b.mounts[0].source);
    assert_eq!(a.trees_built + b.trees_built, 1);
    assert_eq!(kit.cas.most_reads_of_one_file(), 1, "a blob read twice");
}

async fn wait_for(mut done: impl FnMut() -> bool) {
    for _ in 0..3000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the condition never held");
}

/// Catches a builder that dies (its lease cancelled) leaving the digest `Building`
/// forever: the waiter is woken, builds in its place, and the dead builder leaves no
/// `.tmp-*` directory behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_build_hands_over_to_a_waiter() {
    let kit = Arc::new(kit("trees-cancel", |_| {}));
    let root = kit.cas.tree(&files("toolchain", 300, "cancel"));
    kit.cas.stall();
    let spawn = |name: &'static str| {
        let (kit, root) = (Arc::clone(&kit), root.clone());
        tokio::spawn(async move { kit.lease(&root, name).await })
    };
    let leader = spawn("lease-1");
    wait_for(|| kit.cas.file_reads() > 0).await;
    let waiter = spawn("lease-2");
    wait_for(|| kit.trees.store().stats().waits >= 1).await;
    leader.abort();
    assert!(leader.await.unwrap_err().is_cancelled());
    kit.cas.unstall();
    let (_, inputs) = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .expect("the waiter never took over a dead build")
        .unwrap();
    assert_eq!(inputs.trees_built, 1, "the waiter built it");
    assert_eq!(kit.trees.store().stats().trees, 1);
    wait_for(|| hidden(&kit.dir.join("trees")).is_empty()).await;
}

/// The `.tmp-*` and `.dead-*` names in `dir`.
fn hidden(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect()
}

/// Nodes with an output or the working directory in them are not trees (the "ignore
/// outputs" mutant mounts a tree the action must write to), but the nodes below and
/// beside them still are when they are clean; a node under an output directory is not
/// either; a working directory beside a tree leaves it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_output_or_the_working_directory_disqualifies_a_node() {
    let kit = kit("trees-reserved", |_| {});
    let mut input = files("tc/a", 300, "a");
    input.extend(files("tc/b", 300, "b"));
    input.extend(small());
    let root = kit.cas.tree(&input);

    // Nothing reserved: the highest node, `tc`, is the one tree.
    let (_, none) = kit.lease(&root, "none").await;
    assert_eq!(paths(&none), ["tc"]);
    // An output file in `tc/a`: `tc` and `tc/a` are out, `tc/b` is a tree. The output's
    // input (not here) would be a private copy; `tc/a`'s files are linked per action.
    let (dir, out) = kit.lease_with(&root, "out", &["tc/a/d1/o.bin"], "").await;
    assert_eq!(paths(&out), ["tc/b"]);
    assert!(
        dir.join("tc/a/d1/f1").exists(),
        "tc/a is linked file by file"
    );
    // The working directory inside `tc/b` takes `tc/b` out as well: `tc` and `tc/a`.
    let (_, wd) = kit.lease_with(&root, "wd", &["o.bin"], "tc/b/d2").await;
    assert_eq!(paths(&wd), ["tc/a"]);
    // An output *directory* above a node: nothing below it is a tree.
    let (_, above) = kit.lease_with(&root, "above", &["tc"], "").await;
    assert!(above.mounts.is_empty(), "{:?}", above.mounts);
    // A working directory beside the trees, and an output elsewhere, leave `tc` alone.
    let (_, beside) = kit.lease_with(&root, "beside", &["src/o"], "pkg").await;
    assert_eq!(paths(&beside), ["tc"]);
}

/// An output that is also an input, outside any tree, is a private copy on the tree
/// path too (nlink 1): a write to it must not change the cache's entry. Its sibling
/// stays a cache link. Catches the private list being dropped in `Trees::materialize`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_output_named_input_beside_a_tree_is_a_private_copy() {
    let kit = kit("trees-private", |_| {});
    let mut input = files("tc", 300, "p");
    input.extend(small());
    input.push(("src/o.txt".to_owned(), b"output input".to_vec(), false));
    let root = kit.cas.tree(&input);
    let (dir, inputs) = kit.lease_with(&root, "private", &["src/o.txt"], "").await;
    assert_eq!(paths(&inputs), ["tc"]);
    let own = std::fs::metadata(dir.join("src/o.txt")).expect("o.txt");
    assert_eq!(
        own.nlink(),
        1,
        "the output-named input is not the cache inode"
    );
    assert_eq!(
        std::fs::read(dir.join("src/o.txt")).expect("read"),
        b"output input"
    );
    let sibling = std::fs::metadata(dir.join("src/a.c")).expect("a.c");
    assert!(sibling.nlink() >= 2, "a sibling is a cache link");
}

/// A hit refreshes a tree's recency: with `a` and `b` built and released, `a` used
/// again, and a third tree over the budget, `b` goes and `a` stays. Catches LRU
/// degrading to first-in-first-out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hit_makes_a_tree_the_most_recent() {
    const ONE: u64 = 2890;
    let kit = kit("trees-recency", |c| c.max_bytes = ONE * 2 + 500);
    let roots: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|tag| kit.cas.tree(&files("tc", 300, tag)))
        .collect();
    let (_, a) = kit.lease(&roots[0], "a").await;
    let (_, b) = kit.lease(&roots[1], "b").await;
    let (a_tree, b_tree) = (a.mounts[0].source.clone(), b.mounts[0].source.clone());
    drop(a);
    drop(b);
    let (_, hit) = kit.lease(&roots[0], "a-hit").await;
    assert_eq!(hit.trees_built, 0, "a is a hit");
    drop(hit);
    let (_, c) = kit.lease(&roots[2], "c").await;
    wait_for(|| !b_tree.exists()).await;
    assert!(a_tree.is_dir(), "the tree just used is kept");
    assert!(c.mounts[0].source.is_dir());
    assert_eq!(kit.trees.store().stats().evictions, 1);
}

/// The size rule is `>=` and counts files and symlinks, not directories: 255 files are
/// linked, 256 are a tree (the off-by-one mutants of the comparison fail one side).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_threshold_is_files_at_or_over_the_flag() {
    let kit = kit("trees-threshold", |_| {});
    let root255 = kit.cas.tree(&files("tc", 255, "t"));
    let (_, below) = kit.lease(&root255, "below").await;
    assert!(below.mounts.is_empty());
    let root256 = kit.cas.tree(&files("tc", 256, "t"));
    let (_, at) = kit.lease(&root256, "at").await;
    assert_eq!(paths(&at), ["tc"]);
    let kit = self::kit("trees-threshold-flag", |c| c.min_files = 10);
    let (_, low) = kit.lease(&kit.cas.tree(&files("tc", 10, "t")), "low").await;
    assert_eq!(paths(&low), ["tc"]);
}

/// At most `max_mounts` trees, the largest first; the others are linked. Catches the
/// cap missing, and the small ones taken instead of the large.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_most_max_mounts_trees_the_largest_first() {
    let kit = kit("trees-max-mounts", |c| c.max_mounts = 2);
    let mut input = files("one", 300, "1");
    input.extend(files("two", 500, "2"));
    input.extend(files("three", 400, "3"));
    let root = kit.cas.tree(&input);
    let (dir, inputs) = kit.lease(&root, "lease-1").await;
    assert_eq!(paths(&inputs), ["two", "three"]);
    assert!(
        dir.join("one/d1/f1").is_file(),
        "the smallest is linked per action"
    );
    assert_eq!(inputs.tree_files, 900);
}

/// A tree larger than the store's budget is never built (a store that cannot hold it
/// would build and evict it for every lease); a path with ':' or ','
/// in it is linked, not mounted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_the_store_cannot_hold_or_name_is_linked() {
    let kit = kit("trees-too-big", |c| c.max_bytes = 1000);
    let root = kit.cas.tree(&files("tc", 300, "big"));
    let (dir, inputs) = kit.lease(&root, "lease-1").await;
    assert!(inputs.mounts.is_empty());
    assert!(dir.join("tc/d1/f1").is_file());

    let kit = self::kit("trees-names", |_| {});
    let mut input = files("a:b", 300, "colon");
    input.extend(files("c,d", 300, "comma"));
    input.extend(files("fine", 300, "fine"));
    let (_, inputs) = kit.lease(&kit.cas.tree(&input), "lease-1").await;
    assert_eq!(paths(&inputs), ["fine"]);
}

/// Unpinned trees are evicted by recency once over budget, never one a lease holds
/// (the "evict pinned" mutant removes a mounted tree), and the evicted tree is gone from
/// disk. Each tree is 300 files of 2,890 bytes together; the budget holds two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eviction_takes_the_least_recent_unpinned_tree_and_never_a_pinned_one() {
    const ONE: u64 = 2890;
    let kit = kit("trees-evict", |c| c.max_bytes = ONE * 2 + 500);
    let roots: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|tag| kit.cas.tree(&files("tc", 300, tag)))
        .collect();
    let (_, a) = kit.lease(&roots[0], "a").await;
    let (_, b) = kit.lease(&roots[1], "b").await;
    assert_eq!(kit.trees.store().stats().bytes, ONE * 2);
    let (a_tree, b_tree) = (a.mounts[0].source.clone(), b.mounts[0].source.clone());
    // Both pinned, room for two: a third is built and everything is kept (over budget
    // by a pinned tree, which nothing may evict).
    let (_, c) = kit.lease(&roots[2], "c").await;
    let c_tree = c.mounts[0].source.clone();
    let stats = kit.trees.store().stats();
    assert_eq!((stats.trees, stats.evictions), (3, 0), "{stats:?}");
    assert!(a_tree.is_dir() && b_tree.is_dir() && c_tree.is_dir());
    // `a` is released: it is the least recently used unpinned tree, and the store is
    // over budget, so it goes. `b` and `c` are still pinned.
    drop(a);
    wait_for(|| !a_tree.exists()).await;
    assert!(b_tree.is_dir() && c_tree.is_dir());
    assert_eq!(kit.trees.store().stats().evictions, 1);
    // Released too, `b` is not evicted: the store is within budget now.
    drop(b);
    drop(c);
    assert!(b_tree.is_dir() && c_tree.is_dir());
    assert_eq!(kit.trees.store().stats().evictions, 1);
    // A lease for the evicted tree builds it again.
    let (_, again) = kit.lease(&roots[0], "a-again").await;
    assert_eq!(again.trees_built, 1);
    // It evicted the older of the two unpinned trees, `b`, not the one it holds.
    assert_eq!(kit.trees.store().stats().evictions, 2);
    wait_for(|| !b_tree.exists()).await;
    assert!(c_tree.is_dir() && again.mounts[0].source.is_dir());
    wait_for(|| hidden(&kit.dir.join("trees")).is_empty()).await;
}

/// A symlink in a tree stays a symlink (its target is not followed on the host or
/// rewritten), counts as a file, and a hard-linked pair (two names of one blob) is one
/// inode: so `link(2)` of a tree file into the input root is a link across the mount in
/// the container (`EXDEV`; documented in `trees`, shown on real `crun`), and two names
/// in a tree share their inode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symlinks_stay_symlinks_and_equal_files_share_an_inode() {
    let kit = kit("trees-symlinks", |_| {});
    let cas = &kit.cas.blobs;
    let blob = cas.insert(&b"same bytes"[..]);
    let mut tool = Directory::default();
    for i in 0..300 {
        tool.files.push(FileNode {
            name: format!("f{i:03}"),
            digest: Some(cas.insert(format!("unique {i}").into_bytes())),
            ..FileNode::default()
        });
    }
    for name in ["same1", "same2"] {
        tool.files.push(FileNode {
            name: name.to_owned(),
            digest: Some(blob.clone()),
            ..FileNode::default()
        });
    }
    tool.files.sort_by(|a, b| a.name.cmp(&b.name));
    tool.symlinks = vec![
        SymlinkNode {
            name: "abs".to_owned(),
            target: "/etc/hostname".to_owned(),
            ..SymlinkNode::default()
        },
        SymlinkNode {
            name: "rel".to_owned(),
            target: "f000".to_owned(),
            ..SymlinkNode::default()
        },
        SymlinkNode {
            name: "up".to_owned(),
            target: "../outside".to_owned(),
            ..SymlinkNode::default()
        },
    ];
    let tool = cas.message(&tool);
    let outside = FileNode {
        name: "outside".to_owned(),
        digest: Some(cas.insert(&b"out"[..])),
        ..FileNode::default()
    };
    let link_to_tree = SymlinkNode {
        name: "into".to_owned(),
        target: "tool/f000".to_owned(),
        ..SymlinkNode::default()
    };
    let root = cas.message(&Directory {
        files: vec![outside],
        directories: vec![DirectoryNode {
            name: "tool".to_owned(),
            digest: Some(tool),
        }],
        symlinks: vec![link_to_tree],
        ..Directory::default()
    });
    let (dir, inputs) = kit.lease(&root, "lease-1").await;
    assert_eq!(paths(&inputs), ["tool"]);
    assert_eq!(inputs.tree_files, 305, "300 + 2 + 3 symlinks");
    let tree = &inputs.mounts[0].source;
    for (name, target) in [
        ("abs", "/etc/hostname"),
        ("rel", "f000"),
        ("up", "../outside"),
    ] {
        let link = tree.join(name);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_link(&link).unwrap().to_str(), Some(target));
    }
    assert_eq!(
        std::fs::metadata(tree.join("same1")).unwrap().ino(),
        std::fs::metadata(tree.join("same2")).unwrap().ino()
    );
    // The per-action part: the symlink into the mount is the client's, untouched.
    assert_eq!(
        std::fs::read_link(dir.join("into")).unwrap().to_str(),
        Some("tool/f000")
    );
}

/// A store reopened (a daemon restart) keeps the finished trees and does not rebuild
/// them, sweeps what an interrupted build or eviction left, and leaves a name that is
/// not a tree alone. Catches an adopt that counts no bytes (the budget would then never
/// evict) and a sweep that misses `.tmp-*`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_store_adopts_finished_trees_and_sweeps_the_rest() {
    let kit = kit("trees-reopen", |_| {});
    let root = kit.cas.tree(&files("tc", 300, "reopen"));
    let (_, inputs) = kit.lease(&root, "lease-1").await;
    let (source, bytes) = (
        inputs.mounts[0].source.clone(),
        kit.trees.store().stats().bytes,
    );
    drop(inputs);
    let store = kit.dir.join("trees");
    std::fs::create_dir_all(store.join(".tmp-9/half")).unwrap();
    std::fs::write(store.join(".tmp-9/half/f"), b"x").unwrap();
    std::fs::create_dir_all(store.join(".dead-3")).unwrap();
    std::fs::write(store.join("stranger"), b"mine").unwrap();
    let reopened = Trees::open(TreeConfig::new(store.clone(), GIB)).unwrap();
    let stats = reopened.store().stats();
    assert_eq!((stats.trees, stats.bytes), (1, bytes), "{stats:?}");
    assert_eq!(hidden(&store), Vec::<String>::new());
    assert!(store.join("stranger").exists());
    let dir = lease_dir(&kit.dir, "lease-2");
    let again = reopened
        .materialize(&*kit.cas, &kit.cache, &root, &dir, &[], "")
        .await
        .unwrap();
    assert_eq!(again.mounts[0].source, source);
    assert_eq!(again.trees_built, 0, "adopted, not rebuilt");
    // A store smaller than what it adopted evicts at open.
    drop(again);
    let tiny = Trees::open(TreeConfig::new(store.clone(), 10)).unwrap();
    assert_eq!(tiny.store().stats().trees, 0);
    assert!(!source.exists());
}

/// The store directory is the way to every tree: a rootless runtime's container root is
/// a subordinate id (an "other" to the owner), so it must traverse the directory to bind
/// a tree, and must not list it or write in it. Catches a store made 0700 (every lease
/// that mounted a tree failed `cannot stat ...: Permission denied` on a build host), a
/// store made listable (0755) or group or other writable, and a store left 0700 by an
/// older daemon when it is reopened: a fresh store is 0711 whatever the umask, and an
/// existing 0700 one is repaired at open without losing its trees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_store_directory_is_searchable_by_others_but_not_listable_or_writable() {
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    let kit = kit("trees-store-mode", |_| {});
    let store = kit.dir.join("trees");
    assert_eq!(mode(&store), 0o711, "a fresh store");
    let root = kit.cas.tree(&files("tc", 300, "store-mode"));
    let (_, inputs) = kit.lease(&root, "lease-1").await;
    let source = inputs.mounts[0].source.clone();
    assert_eq!(source.parent(), Some(store.as_path()));
    drop(inputs);

    // What an older daemon left behind.
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700)).unwrap();
    let reopened = Trees::open(TreeConfig::new(store.clone(), GIB)).unwrap();
    assert_eq!(mode(&store), 0o711, "a 0700 store, reopened");
    assert_eq!(reopened.store().stats().trees, 1, "the tree is adopted");
    assert_eq!(mode(&source), 0o755, "the tree is not touched");

    // A store somebody opened up is closed again: not listable, not writable.
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o777)).unwrap();
    let _ = Trees::open(TreeConfig::new(store.clone(), GIB)).unwrap();
    assert_eq!(mode(&store), 0o711, "a 0777 store, reopened");

    // A directory above the store that others cannot search is the operator's: the
    // store opens (it is only warned about) and is still 0711.
    let closed = kit.dir.join("closed");
    std::fs::create_dir(&closed).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
    let below = Trees::open(TreeConfig::new(closed.join("trees"), GIB)).unwrap();
    assert_eq!(
        mode(&closed.join("trees")),
        0o711,
        "a store below a closed directory"
    );
    assert_eq!(
        mode(&closed),
        0o700,
        "the operator's directory is not touched"
    );
    drop(below);
}

/// A build that fails (a blob the CAS does not have) names no tree and leaves no
/// `.tmp-*` directory; the lease fails with the CAS's error, and the next lease with the
/// blob present builds the tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_build_leaves_nothing_and_the_next_lease_builds() {
    let kit = kit("trees-failed", |_| {});
    let input = files("tc", 300, "missing");
    let root = kit.cas.tree(&input);
    let lost = digest_of(&input[7].1);
    let saved = kit.cas.blobs.blob(&lost).unwrap();
    kit.cas.blobs.remove(&lost);
    let dir = lease_dir(&kit.dir, "lease-1");
    let failed = kit
        .trees
        .materialize(&*kit.cas, &kit.cache, &root, &dir, &[], "")
        .await
        .expect_err("a missing blob");
    assert!(
        matches!(
            failed,
            kbf_daemon::tree::TreeError::Cas(CasError::Missing(_))
        ),
        "{failed}"
    );
    wait_for(|| hidden(&kit.dir.join("trees")).is_empty()).await;
    assert_eq!(kit.trees.store().stats().trees, 0);
    kit.cas.blobs.insert(saved);
    let (_, inputs) = kit.lease(&root, "lease-2").await;
    assert_eq!(inputs.trees_built, 1);
}

/// An eviction that cannot rename its victim (the tree's directory was moved behind the
/// store's back) keeps it in the books and evicts nothing, instead of dropping a tree
/// that is still on disk from the count or looping.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_eviction_that_cannot_rename_keeps_the_tree_in_the_books() {
    const ONE: u64 = 2890;
    let kit = kit("trees-rename-fails", |c| c.max_bytes = ONE + 500);
    let (_, a) = kit.lease(&kit.cas.tree(&files("tc", 300, "a")), "a").await;
    std::fs::rename(&a.mounts[0].source, kit.dir.join("moved-away")).unwrap();
    let (_, b) = kit.lease(&kit.cas.tree(&files("tc", 300, "b")), "b").await;
    drop(a);
    let stats = kit.trees.store().stats();
    assert_eq!(
        (stats.trees, stats.evictions, stats.bytes),
        (2, 0, 2 * ONE),
        "{stats:?}"
    );
    drop(b);
}

/// A tree whose removal fails after its eviction (a directory in it the daemon's user
/// cannot empty) is left as `.dead-<n>`, outside the books, with a warning; the lease
/// that dropped its pin on a thread with no runtime removes inline; the next start
/// refuses to open on it until it can be removed, then sweeps it. Catches the eviction
/// waiting on the removal, and a start that ignores a leftover it cannot remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_that_fails_leaves_the_dead_tree_for_the_next_start() {
    const ONE: u64 = 2890;
    let kit = kit("trees-remove-fails", |c| c.max_bytes = ONE + 500);
    let (_, a) = kit.lease(&kit.cas.tree(&files("tc", 300, "a")), "a").await;
    let (_, b) = kit.lease(&kit.cas.tree(&files("tc", 300, "b")), "b").await;
    let a_tree = a.mounts[0].source.clone();
    if rustix::process::geteuid().is_root() {
        return;
    }
    let locked = a_tree.join("d1");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Dropped on a thread of its own: no runtime there, so the removal runs inline.
    let trees = a.trees;
    std::thread::spawn(move || drop(trees)).join().unwrap();
    let stats = kit.trees.store().stats();
    assert_eq!((stats.trees, stats.evictions), (1, 1), "{stats:?}");
    assert!(!a_tree.exists());
    let store = kit.dir.join("trees");
    let dead = hidden(&store);
    assert!(dead.len() == 1 && dead[0].starts_with(".dead-"), "{dead:?}");
    let refused = Trees::open(TreeConfig::new(store.clone(), GIB)).expect_err("cannot sweep");
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
    std::fs::set_permissions(
        store.join(&dead[0]).join("d1"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let reopened = Trees::open(TreeConfig::new(store.clone(), GIB)).expect("sweeps");
    assert_eq!(hidden(&store), Vec::<String>::new());
    assert_eq!(reopened.store().stats().trees, 1, "b is still a tree");
    drop(b);
}

/// A name that looks like a tree but is a file is left alone and not counted; a tree
/// named behind the store's back (between the build and its rename) is kept, not
/// replaced, and the spare build is removed. Catches a plain rename over the name (a
/// tree a lease has mounted would be swapped), and an adopt that takes any name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_taken_behind_the_stores_back_is_kept() {
    let kit = kit("trees-taken", |_| {});
    let root = kit.cas.tree(&files("tc", 300, "taken"));
    let (_, first) = kit.lease(&root, "lease-1").await;
    let name = first.mounts[0].source.file_name().unwrap().to_owned();
    drop(first);

    let second = kit.dir.join("second-store");
    std::fs::create_dir_all(&second).unwrap();
    let lookalike = format!("{}-5", "a".repeat(64));
    std::fs::write(second.join(&lookalike), b"a file").unwrap();
    let trees = Trees::open(TreeConfig::new(second.clone(), GIB)).unwrap();
    assert_eq!(trees.store().stats().trees, 0);
    assert!(second.join(&lookalike).is_file());
    std::fs::create_dir_all(second.join(&name)).unwrap();
    std::fs::write(second.join(&name).join("marker"), b"theirs").unwrap();
    let dir = lease_dir(&kit.dir, "lease-2");
    let inputs = trees
        .materialize(&*kit.cas, &kit.cache, &root, &dir, &[], "")
        .await
        .unwrap();
    assert_eq!(inputs.trees_built, 1);
    assert_eq!(
        std::fs::read(second.join(&name).join("marker")).unwrap(),
        b"theirs"
    );
    wait_for(|| hidden(&second).is_empty()).await;
    assert_eq!(trees.store().stats().trees, 1);
}
