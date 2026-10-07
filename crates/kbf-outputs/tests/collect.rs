//! `collect` against trees on the real filesystem: what is recorded, what is left out,
//! what is never followed, and where the limits stop it.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use kbf_outputs::{
    CHUNK_BYTES, Exceeded, OutputLimits, OutputsError, Store, StoreError, collect, digest_of,
};
use kbf_proto::reapi::{ActionResult, Digest, Directory, Tree};
use prost::Message;

/// Blobs in memory. `put_file` reads the file through and checks its digest, as the
/// daemon's client relies on the front to; `refuse` makes every put fail.
#[derive(Default)]
struct MemoryStore {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    files: Mutex<usize>,
    refuse: bool,
}

impl MemoryStore {
    fn get(&self, digest: &Digest) -> Vec<u8> {
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)[&digest.hash].clone()
    }

    fn keep(&self, bytes: Vec<u8>) -> Result<Digest, StoreError> {
        if self.refuse {
            return Err("the store is down".into());
        }
        let digest = digest_of(&bytes);
        self.blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(digest.hash.clone(), bytes);
        Ok(digest)
    }
}

impl Store for MemoryStore {
    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, StoreError> {
        self.keep(bytes)
    }

    async fn put_file(
        &self,
        mut file: std::fs::File,
        digest: Digest,
    ) -> Result<Digest, StoreError> {
        *self.files.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let stored = self.keep(bytes)?;
        if stored != digest {
            return Err("the file does not hash to its digest".into());
        }
        Ok(stored)
    }
}

/// A fresh scratch directory for one test, beside the test binary.
fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-outputs-tests")
        .join(name);
    if dir.exists() {
        kbf_outputs::remove_tree(&dir).expect("clear old scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

async fn run(
    store: &MemoryStore,
    root: &Path,
    wd: &str,
    paths: &[&str],
    limits: OutputLimits,
) -> Result<ActionResult, OutputsError> {
    let paths: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
    let mut result = ActionResult::default();
    collect(store, root, wd, &paths, limits, &mut result).await?;
    Ok(result)
}

fn tree(store: &MemoryStore, result: &ActionResult, i: usize) -> Tree {
    let digest = result.output_directories[i]
        .tree_digest
        .as_ref()
        .expect("tree digest");
    Tree::decode(store.get(digest).as_slice()).expect("a Tree")
}

/// Catches a file, directory or symlink output recorded under the wrong path, with the
/// wrong bytes or executable bit, a Directory listed out of name order, or a Tree whose
/// root digest does not match its root.
#[tokio::test]
async fn files_directories_and_symlinks_are_recorded() {
    let root = scratch("recorded");
    let wd = root.join("w");
    std::fs::create_dir_all(wd.join("out/sub")).expect("mkdir");
    std::fs::write(wd.join("a.txt"), b"alpha").expect("write");
    std::fs::write(wd.join("tool"), b"#!/bin/sh\n").expect("write");
    std::fs::set_permissions(wd.join("tool"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    std::fs::write(wd.join("out/z"), b"zed").expect("write");
    std::fs::write(wd.join("out/b"), b"bee").expect("write");
    std::fs::write(wd.join("out/sub/c"), b"see").expect("write");
    symlink("b", wd.join("out/link")).expect("symlink");
    symlink("../a.txt", wd.join("ln")).expect("symlink");
    let store = MemoryStore::default();
    let result = run(
        &store,
        &root,
        "w",
        &["a.txt", "tool", "out", "ln", "absent"],
        OutputLimits::DEFAULT,
    )
    .await
    .expect("collect");

    let files: Vec<(&str, bool)> = result
        .output_files
        .iter()
        .map(|f| (f.path.as_str(), f.is_executable))
        .collect();
    assert_eq!(files, [("a.txt", false), ("tool", true)]);
    assert_eq!(
        store.get(result.output_files[0].digest.as_ref().expect("d")),
        b"alpha"
    );
    assert_eq!(result.output_symlinks.len(), 1);
    assert_eq!(result.output_symlinks[0].path, "ln");
    assert_eq!(result.output_symlinks[0].target, "../a.txt");

    assert_eq!(result.output_directories.len(), 1);
    let out = &result.output_directories[0];
    assert_eq!(out.path, "out");
    let tree = tree(&store, &result, 0);
    let top = tree.root.clone().expect("root");
    assert_eq!(
        out.root_directory_digest,
        Some(digest_of(&top.encode_to_vec()))
    );
    let names: Vec<&str> = top.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["b", "z"], "sorted by name");
    assert_eq!(top.symlinks.len(), 1);
    assert_eq!(
        (
            top.symlinks[0].name.as_str(),
            top.symlinks[0].target.as_str()
        ),
        ("link", "b")
    );
    assert_eq!(top.directories.len(), 1);
    assert_eq!(tree.children.len(), 1);
    let sub: &Directory = &tree.children[0];
    assert_eq!(
        top.directories[0].digest,
        Some(digest_of(&sub.encode_to_vec()))
    );
    assert_eq!(sub.files[0].name, "c");
}

/// Catches a symlink followed at any level: a working directory, a directory on an
/// output's path, or a directory inside an output replaced by a symlink to a host
/// directory must not bring that directory's files into the result. A symlink output
/// is recorded with its target, and the file it points at is never stored.
#[tokio::test]
async fn no_symlink_is_followed_at_any_level() {
    let base = scratch("no-follow");
    let host = base.join("host");
    std::fs::create_dir_all(host.join("deeper")).expect("mkdir");
    std::fs::write(host.join("key.pem"), b"host secret").expect("write");
    std::fs::write(host.join("deeper/key.pem"), b"host secret").expect("write");
    let root = base.join("root");
    std::fs::create_dir_all(root.join("ok/out")).expect("mkdir");
    // The working directory itself is a symlink to the host directory.
    symlink(&host, root.join("w")).expect("symlink");
    // A directory on the way to an output is a symlink.
    symlink(&host, root.join("ok/d")).expect("symlink");
    // Inside an output directory: a symlink to a host directory and to a host file.
    symlink(&host, root.join("ok/out/dir")).expect("symlink");
    symlink(host.join("key.pem"), root.join("ok/out/file")).expect("symlink");
    // An output that is itself a symlink to a host file.
    symlink(host.join("key.pem"), root.join("ok/key.pem")).expect("symlink");
    let store = MemoryStore::default();

    let result = run(
        &store,
        &root,
        "w",
        &["key.pem", "deeper"],
        OutputLimits::DEFAULT,
    )
    .await
    .expect("collect");
    assert_eq!(
        result,
        ActionResult::default(),
        "nothing under a symlinked working directory"
    );

    let result = run(
        &store,
        &root,
        "ok",
        &["d/key.pem", "d/deeper", "out", "key.pem"],
        OutputLimits::DEFAULT,
    )
    .await
    .expect("collect");
    assert!(result.output_files.is_empty(), "{result:?}");
    assert_eq!(result.output_symlinks.len(), 1);
    assert_eq!(
        result.output_symlinks[0].target,
        host.join("key.pem").to_str().expect("utf8")
    );
    let tree = tree(&store, &result, 0);
    let top = tree.root.expect("root");
    assert!(
        top.files.is_empty() && top.directories.is_empty(),
        "{top:?}"
    );
    let links: Vec<&str> = top.symlinks.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(links, ["dir", "file"]);
    let stored = store.blobs.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(
        !stored.values().any(|bytes| bytes == b"host secret"),
        "a host file was stored"
    );
}

/// Catches an output under a path that is a file, or an entry that is neither file,
/// directory nor symlink (a FIFO), being recorded or failing the action.
#[tokio::test]
async fn files_on_the_way_and_special_files_are_left_out() {
    let root = scratch("left-out");
    std::fs::write(root.join("file"), b"x").expect("write");
    std::fs::create_dir(root.join("dir")).expect("mkdir");
    let fifo = |at: &Path| {
        let made = std::process::Command::new("mkfifo")
            .arg(at)
            .status()
            .expect("mkfifo");
        assert!(made.success());
    };
    fifo(&root.join("pipe"));
    fifo(&root.join("dir/pipe"));
    let store = MemoryStore::default();
    let result = run(
        &store,
        &root,
        "",
        &["file/under", "pipe", "dir"],
        OutputLimits::DEFAULT,
    )
    .await
    .expect("collect");
    assert!(result.output_files.is_empty());
    assert!(result.output_symlinks.is_empty());
    let top = tree(&store, &result, 0).root.expect("root");
    assert_eq!(top, Directory::default(), "the FIFO inside is left out");
}

/// Catches each limit being off by one or not applied: depth, entries and bytes each
/// pass exactly at the limit and fail one past it, naming the limit.
#[tokio::test]
async fn each_limit_holds_exactly() {
    let root = scratch("limits");
    std::fs::create_dir_all(root.join("deep/1/2/3")).expect("mkdir");
    std::fs::write(root.join("f1"), vec![0u8; 10]).expect("write");
    std::fs::write(root.join("f2"), vec![1u8; 10]).expect("write");
    let store = MemoryStore::default();
    let with = |max_depth, max_entries, max_bytes| OutputLimits {
        max_depth,
        max_entries,
        max_bytes,
        max_stdio_bytes: u64::MAX,
    };
    let big = u64::MAX;

    // `deep` holds 1, 1/2, 1/2/3: three levels, four entries with `deep` itself.
    run(&store, &root, "", &["deep"], with(3, big, big))
        .await
        .expect("depth 3");
    let why = run(&store, &root, "", &["deep"], with(2, big, big))
        .await
        .expect_err("depth 2");
    assert!(
        matches!(
            why,
            OutputsError::Limit {
                what: Exceeded::Depth,
                limit: 2,
                ..
            }
        ),
        "{why}"
    );
    run(&store, &root, "", &["deep"], with(512, 4, big))
        .await
        .expect("4 entries");
    let why = run(&store, &root, "", &["deep"], with(512, 3, big))
        .await
        .expect_err("3 entries");
    assert!(
        matches!(
            why,
            OutputsError::Limit {
                what: Exceeded::Entries,
                ..
            }
        ),
        "{why}"
    );

    run(&store, &root, "", &["f1", "f2"], with(512, big, 20))
        .await
        .expect("20 bytes");
    let why = run(&store, &root, "", &["f1", "f2"], with(512, big, 19))
        .await
        .expect_err("19");
    assert!(
        matches!(
            why,
            OutputsError::Limit {
                what: Exceeded::Bytes,
                limit: 19,
                ..
            }
        ),
        "{why}"
    );
    assert!(why.to_string().contains("--output-max-bytes"), "{why}");
    // Inside a directory too.
    std::fs::create_dir(root.join("both")).expect("mkdir");
    std::fs::write(root.join("both/f"), vec![2u8; 10]).expect("write");
    let why = run(&store, &root, "", &["f1", "both"], with(512, big, 19))
        .await
        .expect_err("dir");
    assert!(
        matches!(
            why,
            OutputsError::Limit {
                what: Exceeded::Bytes,
                ..
            }
        ),
        "{why}"
    );
}

/// Catches a recursive walk (a deep tree would overflow the stack) and a walk that
/// holds a descriptor per level (a deep tree would run out of them): a tree thousands
/// of levels deep, deeper than any path the kernel accepts, is read when the limit
/// allows it.
#[tokio::test]
async fn a_tree_deeper_than_a_path_is_read_iteratively() {
    let root = scratch("deep");
    let levels = 3000;
    let mut here = rustix::fs::openat(
        rustix::fs::CWD,
        &root,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )
    .expect("open");
    for _ in 0..levels {
        rustix::fs::mkdirat(&here, "d", rustix::fs::Mode::RWXU).expect("mkdir");
        here = rustix::fs::openat(
            &here,
            "d",
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .expect("open");
    }
    drop(here);
    let store = MemoryStore::default();
    let limits = OutputLimits {
        max_depth: levels,
        ..OutputLimits::DEFAULT
    };
    let result = run(&store, &root, "", &["d"], limits)
        .await
        .expect("collect");
    assert_eq!(tree(&store, &result, 0).children.len(), levels - 1);
    kbf_outputs::remove_tree(&root).expect("remove");
    assert!(!root.exists());
}

/// Catches a large file held whole in memory instead of handed to `put_file`, and a
/// large file stored with the wrong bytes.
#[tokio::test]
async fn a_large_file_is_streamed_to_the_store() {
    let root = scratch("large");
    let large: Vec<u8> = (0..(CHUNK_BYTES * 3 + 1))
        .map(|i| (i % 249) as u8)
        .collect();
    std::fs::create_dir(root.join("dir")).expect("mkdir");
    std::fs::write(root.join("big"), &large).expect("write");
    std::fs::write(root.join("dir/big"), &large).expect("write");
    let store = MemoryStore::default();
    let result = run(&store, &root, "", &["big", "dir"], OutputLimits::DEFAULT)
        .await
        .expect("collect");
    assert_eq!(
        *store.files.lock().unwrap_or_else(PoisonError::into_inner),
        2
    );
    assert_eq!(result.output_files[0].digest, Some(digest_of(&large)));
    let top = tree(&store, &result, 0).root.expect("root");
    assert_eq!(top.files[0].digest, Some(digest_of(&large)));
}

/// Catches a store failure swallowed, or reported without the path it was storing.
#[tokio::test]
async fn a_store_failure_fails_the_collection() {
    let root = scratch("store-down");
    std::fs::write(root.join("f"), b"x").expect("write");
    let store = MemoryStore {
        refuse: true,
        ..MemoryStore::default()
    };
    let why = run(&store, &root, "", &["f"], OutputLimits::DEFAULT)
        .await
        .expect_err("down");
    assert!(matches!(why, OutputsError::Store { .. }), "{why}");
    assert!(why.to_string().contains("the store is down"), "{why}");
    assert!(why.to_string().contains("/f: store"), "{why}");
}

/// Catches bad output paths and working directories being walked instead of refused.
#[tokio::test]
async fn bad_paths_are_the_actions_error() {
    let root = scratch("bad-paths");
    let store = MemoryStore::default();
    for (wd, path) in [("", ""), ("", "../x"), ("..", "x"), ("", "/etc/passwd")] {
        let why = run(&store, &root, wd, &[path], OutputLimits::DEFAULT)
            .await
            .expect_err(path);
        assert!(
            matches!(why, OutputsError::Invalid(_)),
            "{wd:?} {path:?}: {why}"
        );
    }
    let why = run(
        &store,
        &root.join("absent"),
        "",
        &["x"],
        OutputLimits::DEFAULT,
    )
    .await
    .expect_err("no root");
    assert!(matches!(why, OutputsError::Io { .. }), "{why}");
}

/// Catches a name that is not UTF-8 decoded lossily (two names could become one)
/// instead of failing the action. macOS filesystems refuse such names outright.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_name_that_is_not_utf8_is_refused() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let root = scratch("not-utf8");
    std::fs::create_dir(root.join("dir")).expect("mkdir");
    std::fs::write(root.join("dir").join(OsStr::from_bytes(b"\xff")), b"x").expect("write");
    symlink(OsStr::from_bytes(b"\xfe"), root.join("link")).expect("symlink");
    let store = MemoryStore::default();
    for output in ["dir", "link"] {
        let why = run(&store, &root, "", &[output], OutputLimits::DEFAULT)
            .await
            .expect_err(output);
        assert!(
            matches!(&why, OutputsError::Invalid(m) if m.contains("not UTF-8")),
            "{why}"
        );
    }
}

/// Catches captured output stored with the wrong bytes, a large one held whole, and a
/// missing file or a path naming no file taken for an empty blob.
#[tokio::test]
async fn store_file_stores_small_and_large_files() {
    let dir = scratch("store-file");
    let large: Vec<u8> = (0..(CHUNK_BYTES + 5)).map(|i| (i % 241) as u8).collect();
    std::fs::write(dir.join("small"), b"out").expect("write");
    std::fs::write(dir.join("large"), &large).expect("write");
    let store = MemoryStore::default();
    let small = kbf_outputs::store_file(&store, &dir.join("small"), 3)
        .await
        .expect("small");
    assert_eq!(store.get(&small), b"out");
    let big = kbf_outputs::store_file(&store, &dir.join("large"), u64::MAX)
        .await
        .expect("large");
    assert_eq!(big, digest_of(&large));
    assert_eq!(
        *store.files.lock().unwrap_or_else(PoisonError::into_inner),
        1
    );
    let why = kbf_outputs::store_file(&store, &dir.join("small"), 2)
        .await
        .expect_err("past the limit");
    assert!(
        matches!(
            why,
            OutputsError::Limit {
                what: Exceeded::Stdio,
                limit: 2,
                ..
            }
        ),
        "{why}"
    );
    let missing = kbf_outputs::store_file(&store, &dir.join("absent"), u64::MAX).await;
    assert!(
        matches!(missing, Err(OutputsError::Io { .. })),
        "{missing:?}"
    );
    let root = kbf_outputs::store_file(&store, Path::new("/"), u64::MAX).await;
    assert!(matches!(root, Err(OutputsError::Invalid(_))), "{root:?}");
}

/// Catches a directory the walk may not enter taken for an absent output and left
/// out without a word: one without permissions on an output's path, and one that
/// may be read but not searched, both fail the collection.
#[tokio::test]
async fn a_directory_the_walk_may_not_enter_fails() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("no-entry");
    std::fs::create_dir_all(root.join("locked/sub")).expect("mkdir");
    std::fs::create_dir(root.join("blind")).expect("mkdir");
    std::fs::write(root.join("blind/f"), b"x").expect("write");
    let chmod = |dir: &str, mode| {
        std::fs::set_permissions(root.join(dir), std::fs::Permissions::from_mode(mode))
            .expect("chmod");
    };
    chmod("locked", 0o000);
    chmod("blind", 0o400);
    let store = MemoryStore::default();
    for path in ["locked/sub/f", "blind/f"] {
        let why = run(&store, &root, "", &[path], OutputLimits::DEFAULT)
            .await
            .expect_err(path);
        assert!(matches!(why, OutputsError::Io { .. }), "{path}: {why}");
    }
    chmod("locked", 0o755);
    chmod("blind", 0o755);
}
