//! A blob read whole is checked against its digest, whoever sent it.

use std::sync::Arc;

use kbf_route::Router;
use kbf_storage::StorageError;
use kbf_types::{Digest, DigestFunction};

use super::support::{Cell, Cluster, Hook, active, blob_read_from, map, sha, view};

/// A router that tells a hook what a read found where it should not be.
fn repairing(map: &Arc<kbf_map::Map>, cluster: &Arc<Cluster>) -> (Router, Arc<Hook>) {
    let hook = Arc::new(Hook::default());
    let limit = kbf_route::RepairLimit {
        burst: std::num::NonZeroU32::MIN,
        per_second: std::num::NonZeroU32::MIN,
    };
    let router =
        Router::new(Cell::with(view(map)), cluster.clone()).with_read_repair(hook.clone(), limit);
    (router, hook)
}

/// The bytes of `blob` with the last one changed: as long as it, and not it.
fn same_length_lie(blob: &[u8]) -> Vec<u8> {
    let mut lie = blob.to_vec();
    let last = lie.last_mut().expect("a blob that is not empty");
    *last = last.wrapping_add(1);
    lie
}

/// Catches a blob handed on because it is as long as its digest says (the length is all
/// the router used to check): the first owner sends the bytes of another blob of the same
/// length, the second owner has the blob. The caller gets the blob, both owners were
/// asked once, and the read-repair hook is told that the first owner's copy is bad.
#[tokio::test]
async fn an_owner_that_sends_other_bytes_of_the_right_length_is_not_believed() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, hook) = repairing(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(1).hold_as(digest, &same_length_lie(&blob));
    cluster.node(2).hold(&blob);

    assert_eq!(router.read_blob(&digest).await.expect("the blob"), blob);
    assert_eq!(cluster.node(1).read_count(), 1);
    assert_eq!(cluster.node(2).read_count(), 1);
    assert_eq!(hook.told(), [(digest, 2, vec![1])]);
}

/// Catches the lie returned when no owner has the truth: every owner sends the same-length
/// wrong bytes, and the read is `Corrupt`, naming the digest of what came back, never the
/// bytes.
#[tokio::test]
async fn a_blob_every_owner_lies_about_is_corrupt() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, hook) = repairing(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    let lie = same_length_lie(&blob);
    cluster.node(1).hold_as(digest, &lie);
    cluster.node(2).hold_as(digest, &lie);

    let error = router.read_blob(&digest).await.expect_err("only lies");
    match error {
        StorageError::Corrupt {
            digest: asked,
            actual,
        } => {
            assert_eq!((asked, actual), (digest, sha(&lie)));
        }
        other => panic!("{other:?}"),
    }
    assert!(hook.told().is_empty(), "nobody had the blob to repair from");
}

/// Catches a short or a long blob taken for the blob: the bytes are fewer than the digest
/// names, or more, and the read is `Corrupt` either way. For a long one `actual` is the
/// digest of the bytes up to one past the blob's size, which is as many as are read.
#[tokio::test]
async fn a_blob_of_the_wrong_length_is_corrupt() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = repairing(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1]);

    cluster.node(1).hold_as(digest, &blob[..blob.len() - 1]);
    let error = router.read_blob(&digest).await.expect_err("short");
    let StorageError::Corrupt { actual, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(actual, sha(&blob[..blob.len() - 1]));

    let long = [blob.as_slice(), b"tail"].concat();
    cluster.node(1).hold_as(digest, &long);
    let error = router.read_blob(&digest).await.expect_err("long");
    let StorageError::Corrupt { actual, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(actual, sha(&long[..=blob.len()]));
}

/// Catches a read that takes whatever a node sends: a node sends chunk after chunk for a
/// blob of one chunk, and the router stops reading at the first byte past the size
/// instead of collecting it all.
#[tokio::test]
async fn a_node_that_never_stops_is_not_read_to_its_end() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = repairing(&map, &cluster);
    let digest = Digest::new(DigestFunction::Sha256, [4; 32], 1024);
    let produced = cluster.node(1).hold_generated_as(digest, 1000);

    let error = router.read_blob(&digest).await.expect_err("too long");
    assert!(matches!(error, StorageError::Corrupt { .. }), "{error:?}");
    let produced = produced.load(std::sync::atomic::Ordering::SeqCst);
    assert!(produced <= 2, "{produced} chunks were read");
}
