//! Read repair: when the hook is told, what it is told, and how often.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use kbf_map::MemberId;
use kbf_route::{MapView, RepairLimit, Router};
use kbf_storage::StorageError;

use super::support::{
    Cell, Cluster, Hook, active, blob_read_from, blob_where, blobs_read_from, collect, map, view,
};

fn limit(burst: u32, per_second: u32) -> RepairLimit {
    RepairLimit {
        burst: NonZeroU32::new(burst).expect("non-zero"),
        per_second: NonZeroU32::new(per_second).expect("non-zero"),
    }
}

fn repairing(cell: Arc<Cell>, cluster: &Arc<Cluster>, limit: RepairLimit) -> (Router, Arc<Hook>) {
    let hook = Arc::new(Hook::default());
    let router = Router::new(cell, cluster.clone()).with_read_repair(hook.clone(), limit);
    (router, hook)
}

async fn read(router: &Router, digest: &kbf_types::Digest) {
    collect(router.read_range(digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
}

/// Catches a non-first hit that does not reach the repair side, or reaches it wrong: the
/// first owner lacks the blob and the second answers, and the hook is told the digest,
/// who answered and who lacked it. Catches a hook told of a first hit.
#[tokio::test]
async fn a_hit_after_a_miss_tells_the_hook_who_lacked_the_blob() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, hook) = repairing(Cell::with(view(&map)), &cluster, limit(10, 1));
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold(&blob);
    read(&router, &digest).await;
    assert_eq!(hook.told(), [(digest, 2, vec![1])]);

    let (blob, digest) = blob_read_from(&map, &[2, 3]);
    cluster.node(2).hold(&blob);
    read(&router, &digest).await;
    assert_eq!(hook.told().len(), 1, "a first hit repairs nothing");
}

/// Catches an owner that was down reported as one that lacks the blob (the repair side
/// would copy a blob over one it never saw): the first owner is down, the second answers,
/// and the hook is not called.
#[tokio::test]
async fn an_owner_that_failed_is_not_reported_as_lacking_the_blob() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, hook) = repairing(Cell::with(view(&map)), &cluster, limit(10, 1));
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold(&blob);
    cluster.node(1).set_down(true);

    read(&router, &digest).await;
    assert!(hook.told().is_empty(), "{:?}", hook.told());
}

/// Catches a held-but-unreadable copy left out of the report (it needs repair as much as a
/// missing one), and a former owner that said it lacked the blob named as an owner that
/// should have it: the owners of this generation lack it (one holds it unreadably), a former
/// owner lacks it too, a second former owner answers; only this generation's owners are
/// named.
#[tokio::test]
async fn only_owners_of_this_generation_are_reported() {
    let before = map(1, 2, active(&[3, 4, 5]));
    let now = map(2, 2, active(&[1, 2]));
    let cluster = Cluster::of(5);
    let cell = Cell::with(MapView {
        map: now.clone(),
        previous: Some(before.clone()),
    });
    let (router, hook) = repairing(cell, &cluster, limit(10, 1));
    let (blob, digest) = blob_where(|d| {
        now.read_order(d) == [MemberId(1), MemberId(2)]
            && before.read_order(d) == [MemberId(3), MemberId(4)]
    });
    cluster.node(4).hold(&blob);
    cluster.node(1).fail_read(StorageError::Unreachable(digest));

    read(&router, &digest).await;
    assert_eq!(hook.told(), [(digest, 4, vec![1, 2])]);
}

/// Catches a hook called as often as reads miss (a cold owner during a join would turn
/// every read into a repair) and a limit that never gives anything back or gives back more
/// than its burst: a burst of 2 lets two of five misses through, a second later one more,
/// and an idle hour no more than the burst again.
#[tokio::test(start_paused = true)]
async fn the_hook_is_called_at_the_rate_the_limit_gives() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, hook) = repairing(Cell::with(view(&map)), &cluster, limit(2, 1));
    let blobs = blobs_read_from(&map, &[1, 2], 12);
    for (bytes, _) in &blobs {
        cluster.node(2).hold(bytes);
    }
    for (_, digest) in &blobs[..5] {
        read(&router, digest).await;
    }
    assert_eq!(hook.told().len(), 2);

    tokio::time::advance(Duration::from_secs(1)).await;
    for (_, digest) in &blobs[5..7] {
        read(&router, digest).await;
    }
    assert_eq!(hook.told().len(), 3);

    tokio::time::advance(Duration::from_secs(3600)).await;
    for (_, digest) in &blobs[7..12] {
        read(&router, digest).await;
    }
    assert_eq!(hook.told().len(), 5);
}

/// Catches a limit that does not accrue in fractions: at one a second with a burst of
/// one, half a second twice is one more call, and half a second once is none.
#[tokio::test(start_paused = true)]
async fn the_limit_gives_back_fractions_of_a_second() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, hook) = repairing(Cell::with(view(&map)), &cluster, limit(1, 1));
    let blobs = blobs_read_from(&map, &[1, 2], 4);
    for (bytes, _) in &blobs {
        cluster.node(2).hold(bytes);
    }
    read(&router, &blobs[0].1).await;
    read(&router, &blobs[1].1).await;
    assert_eq!(hook.told().len(), 1);
    tokio::time::advance(Duration::from_millis(500)).await;
    read(&router, &blobs[2].1).await;
    assert_eq!(hook.told().len(), 1);
    tokio::time::advance(Duration::from_millis(500)).await;
    read(&router, &blobs[3].1).await;
    assert_eq!(hook.told().len(), 2);
}
