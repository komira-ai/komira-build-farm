//! Callers of one blob at once share one read.

use std::time::Duration;

use kbf_storage::StorageError;

use super::support::{Cluster, active, blob_read_from, map, router};

/// Catches a read repeated for each caller (the red test of the work plan, "flight dedup
/// across two owners"): two callers ask for one blob whose first owner lacks it and whose
/// second holds it. The first owner is asked once and the second once, both callers get
/// the bytes, and nothing is left in flight. Catches a flight left in the table after it
/// landed.
#[tokio::test]
async fn callers_of_one_blob_share_one_read_across_owners() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold(&blob);
    let gate = cluster.node(1).gate();

    // `join!` polls its futures in order: both callers are waiting on the first owner
    // before this opens the gate.
    let release = async { gate.add_permits(100) };
    let (first, second, ()) = tokio::join!(
        router.read_blob(&digest),
        router.read_blob(&digest),
        release
    );
    assert_eq!(first.expect("the blob"), blob);
    assert_eq!(second.expect("the blob"), blob);
    assert_eq!(cluster.node(1).read_count(), 1);
    assert_eq!(cluster.node(2).read_count(), 1);
    assert_eq!(router.reads_in_flight(), 0);

    // A caller after it asks again: the answer is not kept.
    router.read_blob(&digest).await.expect("the blob");
    assert_eq!(cluster.node(2).read_count(), 2);
}

/// Catches a read kept alive for nobody: the only caller leaves while the first owner is
/// still answering, nothing is left in flight, and the next caller starts a read of its
/// own instead of finding a half-made one.
#[tokio::test]
async fn a_read_nobody_waits_for_is_dropped() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let blob = b"nobody waits".to_vec();
    let digest = cluster.node(1).hold(&blob);
    let gate = cluster.node(1).gate();

    let gone = tokio::time::timeout(Duration::from_millis(50), router.read_blob(&digest)).await;
    assert!(gone.is_err(), "the owner was holding the read");
    assert_eq!(cluster.node(1).read_count(), 1);
    assert_eq!(router.reads_in_flight(), 0);

    gate.add_permits(100);
    assert_eq!(router.read_blob(&digest).await.expect("the blob"), blob);
    assert_eq!(cluster.node(1).read_count(), 2);
}

/// Catches a failure kept for later callers, or not shared with the ones waiting: two
/// callers get the same failure from one read, and a caller after the farm is back gets
/// the blob.
#[tokio::test]
async fn a_failure_is_shared_and_not_kept() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(1).hold(&blob);
    cluster.node(2).hold(&blob);
    cluster.node(1).set_down(true);
    cluster.node(2).set_down(true);
    let gate = cluster.node(1).gate();
    let release = async { gate.add_permits(100) };
    let (first, second, ()) = tokio::join!(
        router.read_blob(&digest),
        router.read_blob(&digest),
        release
    );
    for failure in [first, second] {
        let error = failure.expect_err("both are down");
        assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    }
    assert_eq!(cluster.node(1).read_count(), 1);

    cluster.node(1).set_down(false);
    cluster.node(2).set_down(false);
    assert_eq!(router.read_blob(&digest).await.expect("the blob"), blob);
}
