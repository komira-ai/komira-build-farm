//! Every blob a directory output names is in the CAS when the driver returns its
//! result, against the stand-in `crun` (`support/fake.rs`).
//!
//! A module of `fake_crun.rs`, not a test binary of its own, for the reason
//! `fake_memory` gives: the coverage job counts the driver's lines per test binary.

use crate::support::fake::{Fake, image};
use crate::support::{Spec, tree};

/// Catches a directory output whose root Directory, or a Directory below it, is not
/// in the CAS: the farm's action cache refuses a result that names a blob it does
/// not hold ("an OK result whose outputs are not stored"), and `root_directory_digest`
/// and each DirectoryNode name one. The tree has nested files and an empty directory;
/// the empty Directory is the empty blob, which the cache always holds.
#[tokio::test]
async fn every_directory_a_directory_output_names_is_stored() {
    let fake = Fake::new("directory-blobs");
    let mut spec = Spec::new(&image(), "unused by the fake");
    spec.outputs = vec!["out".to_owned()];
    let steps = [
        "mkdir",
        "$UPPER/out/a/b",
        ";",
        "mkdir",
        "$UPPER/out/empty",
        ";",
        "write-lines",
        "$UPPER/out/top",
        "top",
        ";",
        "write-lines",
        "$UPPER/out/a/mid",
        "mid",
        ";",
        "write-lines",
        "$UPPER/out/a/b/leaf",
        "leaf",
    ];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    let out = &result.output_directories[0];
    let tree = tree(&fake.cas, out.tree_digest.as_ref());
    assert_eq!(tree.children.len(), 3, "a, a/b and empty: {tree:?}");
    let mut named = vec![out.root_directory_digest.clone().expect("root digest")];
    for directory in tree.root.iter().chain(&tree.children) {
        named.extend(directory.files.iter().filter_map(|f| f.digest.clone()));
        named.extend(
            directory
                .directories
                .iter()
                .filter_map(|d| d.digest.clone()),
        );
    }
    named.retain(|d| d.size_bytes > 0);
    assert_eq!(named.len(), 6, "root, a, a/b and three files: {named:?}");
    let absent: Vec<_> = named
        .iter()
        .filter(|d| fake.cas.blob(d).is_none())
        .collect();
    assert!(absent.is_empty(), "named but not stored: {absent:?}");
    fake.assert_clean(1);
}
