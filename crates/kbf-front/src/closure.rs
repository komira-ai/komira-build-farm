//! What an `ActionResult` needs of the store besides itself (its closure), worked out in
//! two steps so the cache (which reads the trees from itself) and the routed backend
//! (which reads them from the owners) share the rules: [`direct`] names the blobs the
//! result mentions and the `Tree`s to read, and [`add_tree`] adds the files of one tree
//! read.

use bytes::Bytes;
use kbf_meta::Closure;
use kbf_proto::reapi;
use kbf_types::Digest;
use prost::Message as _;

use crate::cache::{CacheError, is_empty_blob};
use crate::wire;

/// The blobs `result` names itself, and the digests of its output directories' `Tree`s
/// (which are in the closure too, and are read for what they name: [`add_tree`]).
///
/// The empty blob is left out of the closure: it is always present. An output directory's
/// `root_directory_digest`, when set, is required as well, so a hit never names a root
/// `Directory` a client cannot fetch. REAPI does not make the server hold that blob; here
/// the writer (the daemon) must upload it with the tree, or the entry is refused as absent.
///
/// # Errors
/// [`CacheError::Invalid`] for a digest of the result that is malformed.
pub(crate) fn direct(result: &reapi::ActionResult) -> Result<(Closure, Vec<Digest>), CacheError> {
    let mut closure = Closure::new();
    for file in &result.output_files {
        insert(&mut closure, proto_digest(file.digest.as_ref())?);
    }
    for d in [&result.stdout_digest, &result.stderr_digest]
        .into_iter()
        .flatten()
    {
        insert(&mut closure, proto_digest(Some(d))?);
    }
    let mut trees = Vec::new();
    for dir in &result.output_directories {
        let tree = proto_digest(dir.tree_digest.as_ref())?;
        insert(&mut closure, tree);
        if let Some(root) = &dir.root_directory_digest {
            insert(&mut closure, proto_digest(Some(root))?);
        }
        trees.push(tree);
    }
    Ok((closure, trees))
}

/// Adds the files of the `Tree` `bytes`, which is the blob `tree`, to `closure`.
///
/// # Errors
/// [`CacheError::Invalid`] if `bytes` is not a `Tree` or names a malformed digest.
pub(crate) fn add_tree(
    closure: &mut Closure,
    tree: &Digest,
    bytes: Bytes,
) -> Result<(), CacheError> {
    let decoded = reapi::Tree::decode(bytes)
        .map_err(|e| CacheError::Invalid(format!("output tree {tree} does not decode: {e}")))?;
    for directory in decoded.root.iter().chain(&decoded.children) {
        for file in &directory.files {
            insert(closure, proto_digest(file.digest.as_ref())?);
        }
    }
    Ok(())
}

fn insert(closure: &mut Closure, digest: Digest) {
    if !is_empty_blob(&digest) {
        closure.insert(digest);
    }
}

fn proto_digest(d: Option<&reapi::Digest>) -> Result<Digest, CacheError> {
    wire::digest(d).map_err(|s| CacheError::Invalid(s.message().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(bytes: &[u8]) -> Digest {
        kbf_segments::sha256(bytes)
    }

    fn proto(bytes: &[u8]) -> reapi::Digest {
        wire::digest_to_proto(&d(bytes))
    }

    fn file(bytes: &[u8]) -> reapi::OutputFile {
        reapi::OutputFile {
            path: "f".to_owned(),
            digest: Some(proto(bytes)),
            ..reapi::OutputFile::default()
        }
    }

    /// Catches a closure that leaves a blob out (the check then cannot see it go missing)
    /// or puts the empty blob in (always present, but a node would be asked for it): the
    /// output files, stdout, stderr, each tree and its root are in; the empty blob is not.
    #[test]
    fn the_result_names_its_files_streams_trees_and_roots() {
        let result = reapi::ActionResult {
            output_files: vec![file(b"a"), file(b"")],
            stdout_digest: Some(proto(b"out")),
            stderr_digest: Some(proto(b"")),
            output_directories: vec![reapi::OutputDirectory {
                path: "dir".to_owned(),
                tree_digest: Some(proto(b"tree")),
                root_directory_digest: Some(proto(b"root")),
                ..reapi::OutputDirectory::default()
            }],
            ..reapi::ActionResult::default()
        };
        let (closure, trees) = direct(&result).expect("a closure");
        let want: Closure = [d(b"a"), d(b"out"), d(b"tree"), d(b"root")]
            .into_iter()
            .collect();
        assert_eq!(closure, want);
        assert_eq!(trees, [d(b"tree")]);
    }

    /// Catches a malformed digest taken for the empty blob or skipped.
    #[test]
    fn a_malformed_digest_is_invalid() {
        let result = reapi::ActionResult {
            output_files: vec![reapi::OutputFile {
                digest: Some(reapi::Digest {
                    hash: "nothex".to_owned(),
                    size_bytes: 1,
                }),
                ..reapi::OutputFile::default()
            }],
            ..reapi::ActionResult::default()
        };
        assert!(matches!(direct(&result), Err(CacheError::Invalid(_))));
    }

    /// Catches a tree whose files are not in the closure (the root and every child
    /// directory count), a tree that is not decoded as one, and the empty blob let in.
    #[test]
    fn a_trees_files_are_in_the_closure() {
        let tree = reapi::Tree {
            root: Some(reapi::Directory {
                files: vec![reapi::FileNode {
                    name: "r".to_owned(),
                    digest: Some(proto(b"in root")),
                    ..reapi::FileNode::default()
                }],
                ..reapi::Directory::default()
            }),
            children: vec![reapi::Directory {
                files: vec![
                    reapi::FileNode {
                        name: "c".to_owned(),
                        digest: Some(proto(b"in child")),
                        ..reapi::FileNode::default()
                    },
                    reapi::FileNode {
                        name: "e".to_owned(),
                        digest: Some(proto(b"")),
                        ..reapi::FileNode::default()
                    },
                ],
                ..reapi::Directory::default()
            }],
        };
        let mut closure = Closure::new();
        add_tree(&mut closure, &d(b"tree"), Bytes::from(tree.encode_to_vec())).expect("a tree");
        let want: Closure = [d(b"in root"), d(b"in child")].into_iter().collect();
        assert_eq!(closure, want);

        let junk = add_tree(
            &mut closure,
            &d(b"tree"),
            Bytes::from_static(b"\xff\xff\xff"),
        );
        assert!(matches!(junk, Err(CacheError::Invalid(m)) if m.contains("does not decode")));
    }
}
