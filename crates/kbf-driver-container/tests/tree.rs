//! Writing input roots and reading outputs: the rules that keep a client's tree from
//! reaching outside the lease's directory.

mod support;

use kbf_driver_container::cas::digest_of;
use kbf_driver_container::tree::{TreeError, materialize, output_paths};
use kbf_driver_container::{CasError, MemoryCas};
use kbf_proto::reapi::{Command, Directory, DirectoryNode, FileNode, SymlinkNode};
use prost::Message;
use support::exists;

fn file(name: &str, digest: Option<kbf_proto::reapi::Digest>) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest,
        ..FileNode::default()
    }
}

async fn write(cas: &MemoryCas, directory: &Directory, name: &str) -> Result<(), TreeError> {
    let root = cas.insert(directory.encode_to_vec());
    let dir = support::scratch(&format!("tree-{name}"));
    materialize(cas, &root, &dir).await
}

/// Catches an entry name that would put a file outside the lease's directory (`..`,
/// a slash, an absolute path) or nowhere (empty, `.`, NUL).
#[tokio::test]
async fn names_that_are_not_one_component_are_refused() {
    let cas = MemoryCas::new();
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
            matches!(outcome, Err(TreeError::Invalid(_))),
            "{name:?}: {outcome:?}"
        );
    }
    let escaping_dir = Directory {
        directories: vec![DirectoryNode {
            name: "..".to_owned(),
            digest: Some(cas.insert(Directory::default().encode_to_vec())),
        }],
        ..Directory::default()
    };
    let outcome = write(&cas, &escaping_dir, "name-dir").await;
    assert!(matches!(outcome, Err(TreeError::Invalid(_))), "{outcome:?}");
}

/// Catches a name used twice in one directory (a symlink and a file named alike would
/// let the second write follow the first).
#[tokio::test]
async fn a_name_used_twice_is_refused() {
    let cas = MemoryCas::new();
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
    assert!(
        matches!(outcome, Err(TreeError::Invalid(ref why)) if why.contains("twice")),
        "{outcome:?}"
    );
}

/// Catches entries without digests, blobs that do not decode, and blobs the CAS lacks
/// or returns altered, each being written as something.
#[tokio::test]
async fn broken_trees_are_refused() {
    let cas = MemoryCas::new();
    let no_file_digest = Directory {
        files: vec![file("f", None)],
        ..Directory::default()
    };
    assert!(matches!(
        write(&cas, &no_file_digest, "no-file-digest").await,
        Err(TreeError::Invalid(_))
    ));
    let no_dir_digest = Directory {
        directories: vec![DirectoryNode {
            name: "d".to_owned(),
            digest: None,
        }],
        ..Directory::default()
    };
    assert!(matches!(
        write(&cas, &no_dir_digest, "no-dir-digest").await,
        Err(TreeError::Invalid(_))
    ));

    let garbage = cas.insert(vec![0xff, 0xff, 0xff]);
    let dir = support::scratch("tree-garbage");
    let outcome = materialize(&cas, &garbage, &dir).await;
    assert!(
        matches!(outcome, Err(TreeError::Invalid(ref why)) if why.contains("does not decode")),
        "{outcome:?}"
    );

    let absent = Directory {
        files: vec![file("f", Some(digest_of(b"never stored")))],
        ..Directory::default()
    };
    assert!(matches!(
        write(&cas, &absent, "absent").await,
        Err(TreeError::Cas(CasError::Missing(_)))
    ));

    let blob = cas.insert(b"good".to_vec());
    cas.corrupt(&blob, b"evil".to_vec());
    let corrupt = Directory {
        files: vec![file("f", Some(blob))],
        ..Directory::default()
    };
    assert!(matches!(
        write(&cas, &corrupt, "corrupt").await,
        Err(TreeError::Cas(CasError::Corrupt(..)))
    ));
}

/// Catches a write through a path that already exists: files are created exclusively,
/// so a second entry can never overwrite or follow the first.
#[tokio::test]
async fn files_are_created_exclusively() {
    let cas = MemoryCas::new();
    let blob = Some(cas.insert(b"x".to_vec()));
    let tree = Directory {
        files: vec![file("f", blob)],
        ..Directory::default()
    };
    let root = cas.insert(tree.encode_to_vec());
    let dir = support::scratch("tree-exclusive");
    std::fs::write(dir.join("f"), b"already here").expect("plant");
    let outcome = materialize(&cas, &root, &dir).await;
    assert!(matches!(outcome, Err(TreeError::Io { .. })), "{outcome:?}");
    assert!(outcome.expect_err("io").to_string().contains("exists"));
    assert!(exists(&dir.join("f")));
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
    for bad in ["", "../up", "/abs", "a/./b"] {
        let command = Command {
            output_paths: vec![bad.to_owned()],
            ..Command::default()
        };
        assert!(
            matches!(output_paths(&command), Err(TreeError::Invalid(_))),
            "{bad:?}"
        );
    }
}
