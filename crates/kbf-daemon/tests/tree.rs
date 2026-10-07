//! Writing input roots and reading outputs: the rules that keep a client's tree, and
//! what an action leaves behind, from reaching outside the lease's directory.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use kbf_daemon::CasError;
use kbf_daemon::cas::digest_of;
use kbf_daemon::tree::{TreeError, collect, materialize, output_paths, real_dirs};
use kbf_proto::reapi::{
    ActionResult, Command, Directory, DirectoryNode, FileNode, SymlinkNode, Tree,
};
use prost::Message;
use support::memory::MemoryCas;
use support::scratch;

fn file(name: &str, digest: Option<kbf_proto::reapi::Digest>) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest,
        ..FileNode::default()
    }
}

async fn write(cas: &MemoryCas, directory: &Directory, name: &str) -> Result<(), TreeError> {
    let root = cas.message(directory);
    materialize(cas, &root, &scratch(name)).await
}

fn invalid(outcome: &Result<impl std::fmt::Debug, TreeError>, says: &str) -> bool {
    matches!(outcome, Err(TreeError::Invalid(why)) if why.contains(says))
}

/// Catches an entry name that would put a file outside the lease's directory (`..`,
/// a slash, an absolute path) or nowhere (empty, `.`, NUL).
#[tokio::test]
async fn names_that_are_not_one_component_are_refused() {
    let cas = MemoryCas::default();
    let blob = Some(cas.insert(b"x".to_vec()));
    for (i, name) in ["..", ".", "", "a/b", "/etc", "nul\0"]
        .into_iter()
        .enumerate()
    {
        let tree = Directory {
            files: vec![file(name, blob.clone())],
            ..Directory::default()
        };
        let outcome = write(&cas, &tree, &format!("name-{i}")).await;
        assert!(
            invalid(&outcome, "single path component"),
            "{name:?}: {outcome:?}"
        );
    }
    let escaping_dir = Directory {
        directories: vec![DirectoryNode {
            name: "..".to_owned(),
            digest: Some(cas.message(&Directory::default())),
        }],
        ..Directory::default()
    };
    let outcome = write(&cas, &escaping_dir, "name-dir").await;
    assert!(invalid(&outcome, "single path component"), "{outcome:?}");
}

/// Catches a name used twice in one directory (a symlink and a file named alike would
/// let the second write follow the first).
#[tokio::test]
async fn a_name_used_twice_is_refused() {
    let cas = MemoryCas::default();
    let blob = Some(cas.insert(b"x".to_vec()));
    let tree = Directory {
        files: vec![file("a", blob)],
        symlinks: vec![SymlinkNode {
            name: "a".to_owned(),
            target: "/etc/passwd".to_owned(),
            ..SymlinkNode::default()
        }],
        ..Directory::default()
    };
    let outcome = write(&cas, &tree, "twice").await;
    assert!(invalid(&outcome, "twice"), "{outcome:?}");
}

/// Catches entries without digests, blobs that do not decode, and blobs the CAS lacks
/// or returns altered, each being written as something.
#[tokio::test]
async fn broken_trees_are_refused() {
    let cas = MemoryCas::default();
    let no_file_digest = Directory {
        files: vec![file("f", None)],
        ..Directory::default()
    };
    let outcome = write(&cas, &no_file_digest, "no-file-digest").await;
    assert!(invalid(&outcome, "has no digest"), "{outcome:?}");
    let no_dir_digest = Directory {
        directories: vec![DirectoryNode {
            name: "d".to_owned(),
            digest: None,
        }],
        ..Directory::default()
    };
    let outcome = write(&cas, &no_dir_digest, "no-dir-digest").await;
    assert!(invalid(&outcome, "has no digest"), "{outcome:?}");

    let garbage = cas.insert(vec![0xff, 0xff, 0xff]);
    let outcome = materialize(&cas, &garbage, &scratch("tree-garbage")).await;
    assert!(invalid(&outcome, "does not decode"), "{outcome:?}");

    let absent = Directory {
        files: vec![file("f", Some(digest_of(b"never stored")))],
        ..Directory::default()
    };
    let outcome = write(&cas, &absent, "absent").await;
    assert!(
        matches!(outcome, Err(TreeError::Cas(CasError::Missing(_)))),
        "{outcome:?}"
    );

    let blob = cas.insert(b"good".to_vec());
    cas.corrupt(&blob, b"evil".to_vec());
    let corrupt = Directory {
        files: vec![file("f", Some(blob))],
        ..Directory::default()
    };
    let outcome = write(&cas, &corrupt, "corrupt").await;
    assert!(
        matches!(&outcome, Err(TreeError::Cas(e @ CasError::Corrupt(..))) if e.to_string().contains("failed verification")),
        "{outcome:?}"
    );
}

/// Catches a write through a path that already exists (files are created exclusively,
/// so an entry can never overwrite or follow what is there), and a failure to create
/// a symlink or a directory being ignored.
#[tokio::test]
async fn existing_paths_are_never_written_through() {
    let cas = MemoryCas::default();
    let blob = Some(cas.insert(b"x".to_vec()));
    for (i, tree) in [
        Directory {
            files: vec![file("f", blob)],
            ..Directory::default()
        },
        Directory {
            symlinks: vec![SymlinkNode {
                name: "f".to_owned(),
                target: "elsewhere".to_owned(),
                ..SymlinkNode::default()
            }],
            ..Directory::default()
        },
        Directory {
            directories: vec![DirectoryNode {
                name: "f".to_owned(),
                digest: Some(cas.message(&Directory::default())),
            }],
            ..Directory::default()
        },
    ]
    .iter()
    .enumerate()
    {
        let root = cas.message(tree);
        let dir = scratch(&format!("tree-exclusive-{i}"));
        std::fs::write(dir.join("f"), b"already here").expect("plant");
        let outcome = materialize(&cas, &root, &dir).await;
        let error = outcome.expect_err("refused");
        assert!(matches!(error, TreeError::Io { .. }), "{error:?}");
        assert!(error.to_string().contains("exists"), "{error}");
        assert_eq!(std::fs::read(dir.join("f")).expect("kept"), b"already here");
    }
}

/// Catches a tree written wrongly: a file's bytes, its executable bit, a nested
/// directory or a symlink's target.
#[tokio::test]
async fn a_tree_is_written_as_described() {
    let cas = MemoryCas::default();
    let leaf = Directory {
        files: vec![FileNode {
            is_executable: true,
            ..file("run", Some(cas.insert(b"#!/bin/sh\n".to_vec())))
        }],
        ..Directory::default()
    };
    let root = Directory {
        files: vec![file("data", Some(cas.insert(b"bytes".to_vec())))],
        directories: vec![DirectoryNode {
            name: "bin".to_owned(),
            digest: Some(cas.message(&leaf)),
        }],
        symlinks: vec![SymlinkNode {
            name: "link".to_owned(),
            target: "data".to_owned(),
            ..SymlinkNode::default()
        }],
        ..Directory::default()
    };
    let dir = scratch("tree-written");
    materialize(&cas, &cas.message(&root), &dir)
        .await
        .expect("written");
    assert_eq!(std::fs::read(dir.join("data")).expect("data"), b"bytes");
    let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&dir.join("data")), 0o644);
    assert_eq!(mode(&dir.join("bin/run")), 0o755);
    assert_eq!(
        std::fs::read_link(dir.join("link")).expect("link"),
        Path::new("data")
    );
}

/// Catches older clients' outputs being ignored (REAPI before 2.1 lists files and
/// directories separately), and an empty or escaping output path being accepted.
#[test]
#[allow(deprecated)]
fn output_paths_old_and_new() {
    let new = Command {
        output_paths: vec!["a".to_owned(), "b/c".to_owned()],
        output_files: vec!["ignored".to_owned()],
        ..Command::default()
    };
    assert_eq!(output_paths(&new).expect("paths"), ["a", "b/c"]);
    let old = Command {
        output_files: vec!["f".to_owned()],
        output_directories: vec!["d".to_owned()],
        ..Command::default()
    };
    assert_eq!(output_paths(&old).expect("paths"), ["f", "d"]);
    for bad in ["", "../up", "/abs", "a/./b", "a//b", "nul\0"] {
        let command = Command {
            output_paths: vec![bad.to_owned()],
            ..Command::default()
        };
        let outcome = output_paths(&command);
        assert!(
            matches!(outcome, Err(TreeError::Invalid(_))),
            "{bad:?}: {outcome:?}"
        );
    }
}

/// Catches a working directory or output parent made through a symlink or a file the
/// input tree planted (the action's files would land wherever the link points), and
/// an existing directory refused.
#[tokio::test]
async fn real_dirs_never_pass_through_a_link() {
    let dir = scratch("real-dirs");
    std::fs::create_dir(dir.join("kept")).expect("dir");
    std::os::unix::fs::symlink("/tmp", dir.join("link")).expect("link");
    std::fs::write(dir.join("file"), b"").expect("file");

    let made = real_dirs(&dir, "kept/new/deeper").await.expect("made");
    assert_eq!(made, dir.join("kept/new/deeper"));
    assert!(made.is_dir());
    assert_eq!(real_dirs(&dir, "").await.expect("itself"), dir);
    for bad in ["link", "link/below", "file/below"] {
        let outcome = real_dirs(&dir, bad).await;
        assert!(
            invalid(&outcome, "other than a directory"),
            "{bad}: {outcome:?}"
        );
    }
    // Not a refusal but an I/O failure: a parent the daemon cannot write.
    std::fs::create_dir(dir.join("locked")).expect("dir");
    std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o555))
        .expect("chmod");
    let outcome = real_dirs(&dir, "locked/below").await;
    assert!(matches!(outcome, Err(TreeError::Io { .. })), "{outcome:?}");
    std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod back");
}

/// Catches outputs read wrongly: a file's bytes or executable bit, a directory's
/// tree (sorted, nested, with its symlinks), a symlink output's target; and outputs
/// the action never made, or made as a FIFO, being reported. The tree holds 22 files
/// written in an unsorted order, so a missing sort cannot pass by the directory
/// happening to list them sorted (ext4's hash order, tmpfs's newest-first order).
#[tokio::test]
async fn outputs_are_read_back_as_the_action_left_them() {
    let cas = MemoryCas::default();
    let dir = scratch("collect");
    std::fs::write(dir.join("file"), b"out").expect("file");
    std::fs::set_permissions(dir.join("file"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    std::fs::create_dir_all(dir.join("tree/sub")).expect("tree");
    std::fs::write(dir.join("tree/b"), b"b").expect("b");
    std::fs::write(dir.join("tree/a"), b"a").expect("a");
    // n00..n19 in the order 0, 7, 14, 1, 8, ...: neither sorted nor reversed.
    for i in 0..20 {
        let n = i * 7 % 20;
        std::fs::write(dir.join(format!("tree/n{n:02}")), b"n").expect("n");
    }
    std::fs::write(dir.join("tree/sub/c"), b"c").expect("c");
    std::os::unix::fs::symlink("a", dir.join("tree/l")).expect("link in tree");
    std::os::unix::fs::symlink("file", dir.join("link")).expect("link");
    // FIFOs: neither a file, a directory nor a symlink.
    let made = std::process::Command::new("mkfifo")
        .arg(dir.join("tree/s"))
        .arg(dir.join("fifo"))
        .status()
        .expect("run mkfifo");
    assert!(made.success());

    let paths: Vec<String> = ["file", "tree", "link", "fifo", "never", "never/below"]
        .map(str::to_owned)
        .to_vec();
    let mut result = ActionResult::default();
    collect(&cas, &dir, "", &paths, &mut result)
        .await
        .expect("collected");

    assert_eq!(result.output_files.len(), 1);
    let out = &result.output_files[0];
    assert_eq!(out.path, "file");
    assert!(out.is_executable);
    assert_eq!(
        cas.blob(out.digest.as_ref().expect("digest"))
            .expect("stored"),
        b"out"
    );

    assert_eq!(result.output_symlinks.len(), 1);
    assert_eq!(result.output_symlinks[0].path, "link");
    assert_eq!(result.output_symlinks[0].target, "file");

    assert_eq!(result.output_directories.len(), 1);
    let od = &result.output_directories[0];
    assert_eq!(od.path, "tree");
    let tree = Tree::decode(
        cas.blob(od.tree_digest.as_ref().expect("tree digest"))
            .expect("stored")
            .as_slice(),
    )
    .expect("a Tree");
    let root = tree.root.expect("root");
    assert_eq!(
        od.root_directory_digest,
        Some(digest_of(&root.encode_to_vec()))
    );
    let names: Vec<&str> = root.files.iter().map(|f| f.name.as_str()).collect();
    let mut sorted = vec!["a".to_owned(), "b".to_owned()];
    sorted.extend((0..20).map(|n| format!("n{n:02}")));
    assert_eq!(names, sorted, "sorted, FIFO left out");
    assert!(!root.files[0].is_executable);
    assert_eq!(root.directories.len(), 1);
    assert_eq!(root.directories[0].name, "sub");
    assert_eq!(root.symlinks.len(), 1);
    assert_eq!(
        (
            root.symlinks[0].name.as_str(),
            root.symlinks[0].target.as_str()
        ),
        ("l", "a")
    );
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].files[0].name, "c");
    assert_eq!(
        root.directories[0].digest,
        Some(digest_of(&tree.children[0].encode_to_vec()))
    );
}

/// Catches an output read through a directory the action replaced with a symlink (the
/// daemon would upload a file from wherever the link points) or with a file.
#[tokio::test]
async fn outputs_under_a_link_or_a_file_are_left_out() {
    let cas = MemoryCas::default();
    let outside = scratch("collect-outside");
    std::fs::write(outside.join("secret"), b"not the action's").expect("secret");
    let dir = scratch("collect-link");
    std::os::unix::fs::symlink(&outside, dir.join("d")).expect("link");
    std::fs::write(dir.join("f"), b"").expect("file");
    let paths = ["d/secret", "f/below"].map(str::to_owned).to_vec();
    let mut result = ActionResult::default();
    collect(&cas, &dir, "", &paths, &mut result)
        .await
        .expect("collected");
    assert_eq!(result, ActionResult::default());
}

/// Catches outputs read through a working directory, or a directory above it, that
/// the action replaced with a symlink: only the output path's own components were
/// checked, so `cd .. && mv work w && ln -s /host/dir work` uploaded host files. Also
/// catches the working directory left out of the output's path (the control: a real
/// working directory's output is read, and reported relative to it).
#[tokio::test]
async fn outputs_under_a_replaced_working_directory_are_left_out() {
    let cas = MemoryCas::default();
    let outside = scratch("collect-wd-outside");
    std::fs::write(outside.join("secret"), b"HOST SECRET").expect("secret");
    let dir = scratch("collect-wd");
    std::os::unix::fs::symlink(&outside, dir.join("work")).expect("work is a link");
    std::fs::create_dir(dir.join("up")).expect("up");
    std::os::unix::fs::symlink(&outside, dir.join("up/work")).expect("up/work is a link");
    std::os::unix::fs::symlink(&outside, dir.join("above")).expect("above is a link");
    std::fs::create_dir_all(dir.join("real/work")).expect("real");
    std::fs::write(dir.join("real/work/secret"), b"the action's").expect("output");
    let secret = ["secret".to_owned()];
    for working_directory in ["work", "up/work", "above/work"] {
        let mut result = ActionResult::default();
        collect(&cas, &dir, working_directory, &secret, &mut result)
            .await
            .expect("collected");
        assert_eq!(result, ActionResult::default(), "{working_directory}");
    }
    let mut result = ActionResult::default();
    collect(&cas, &dir, "real/work", &secret, &mut result)
        .await
        .expect("collected");
    let [out] = result.output_files.as_slice() else {
        panic!("one output: {result:?}");
    };
    assert_eq!(out.path, "secret");
    assert_eq!(
        cas.blob(out.digest.as_ref().expect("digest")),
        Some(b"the action's".to_vec())
    );
}

/// Catches an output the daemon cannot read being left out silently instead of
/// failing the lease: an output that exists but is unreadable is not an absent one.
#[tokio::test]
async fn an_unreadable_output_fails() {
    let cas = MemoryCas::default();
    let dir = scratch("collect-unreadable");
    std::fs::create_dir_all(dir.join("closed/inner")).expect("dirs");
    std::fs::create_dir(dir.join("shut")).expect("dir");
    std::fs::create_dir(dir.join("tree")).expect("dir");
    std::fs::write(dir.join("tree/f"), b"").expect("file");
    for (path, mode) in [("closed", 0o000), ("shut", 0o000), ("tree/f", 0o000)] {
        std::fs::set_permissions(dir.join(path), std::fs::Permissions::from_mode(mode))
            .expect("chmod");
    }
    // A parent that cannot be searched, a leaf in one, and a file in a tree that
    // cannot be read.
    for path in ["closed/inner/x", "shut/x", "tree"] {
        let mut result = ActionResult::default();
        let outcome = collect(&cas, &dir, "", &[path.to_owned()], &mut result).await;
        assert!(
            matches!(outcome, Err(TreeError::Io { .. })),
            "{path}: {outcome:?}"
        );
    }
    // So the scratch directory can be removed.
    for path in ["closed", "shut", "tree/f"] {
        std::fs::set_permissions(dir.join(path), std::fs::Permissions::from_mode(0o755))
            .expect("chmod back");
    }
}
