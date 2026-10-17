//! Who holds a digest, for a caller that talks to the nodes itself.

use kbf_map::{MemberId, MemberState};
use kbf_route::{MapView, Router};
use kbf_storage::StorageError;

use super::support::{Cell, Cluster, active, blob_where, map, member, router, sha};

fn router_for(map: &std::sync::Arc<kbf_map::Map>) -> (Router, std::sync::Arc<Cell>) {
    router(map, &Cluster::of(3))
}

/// Catches a placement that is not the map's own answer: the generation, the read order
/// (this generation's owners, then the previous generation's) and the member a write goes
/// to, which is the first owner that is not down.
#[tokio::test]
async fn a_placement_is_what_the_map_says() {
    let before = map(1, 2, active(&[1, 2, 3]));
    let members = vec![
        member(1, MemberState::Down),
        member(2, MemberState::Active),
        member(3, MemberState::Active),
        member(4, MemberState::Active),
    ];
    let now = map(2, 2, members);
    let cell = Cell::with(MapView {
        map: now.clone(),
        previous: Some(before.clone()),
    });
    let router = Router::new(cell, Cluster::of(4));
    let (_, digest) =
        blob_where(|d| now.write_owners(d)[0] == MemberId(1) && now.write_owners(d).len() == 2);

    let placement = router.place(&digest).expect("a placement");
    assert_eq!(placement.generation, 2);
    let want: Vec<_> = now
        .read_order_with_previous(Some(&before), &digest)
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(placement.read, want);
    let second = now.write_owners(&digest)[1];
    assert_eq!(placement.write.map(|m| m.id), Some(second));
}

/// Catches a farm with every owner down placed somewhere, and no map placed at all.
#[tokio::test]
async fn nobody_to_place_on_is_empty_or_an_error() {
    let all_down = vec![member(1, MemberState::Down), member(2, MemberState::Down)];
    let map = map(1, 2, all_down);
    let (router, _) = router(&map, &Cluster::of(2));
    let placement = router.place(&sha(b"x")).expect("a placement");
    assert!(placement.read.is_empty());
    assert!(placement.write.is_none());

    let none = Router::new(Cell::empty(), Cluster::of(1));
    let error = none.place(&sha(b"x")).expect_err("no map");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
}

/// Catches a placement that hides the owners a write falls back to: `writers` is every
/// write owner that is not down, in the order a write tries them, and `write` is the
/// first of them. Down members are left out; with every owner down it is empty.
#[tokio::test]
async fn a_placement_lists_the_writers_in_the_order_a_write_tries_them() {
    let up = map(1, 2, active(&[1, 2, 3]));
    let (router, _) = router(&up, &Cluster::of(3));
    let (_, digest) = blob_where(|d| up.write_owners(d) == [MemberId(2), MemberId(3)]);
    let placement = router.place(&digest).expect("a placement");
    let ids: Vec<_> = placement.writers.iter().map(|m| m.id).collect();
    assert_eq!(ids, [MemberId(2), MemberId(3)]);
    assert_eq!(placement.write, placement.writers.first().cloned());

    let members = vec![
        member(1, MemberState::Active),
        member(2, MemberState::Down),
        member(3, MemberState::Active),
    ];
    let down = map(2, 2, members);
    let (router, _) = router_for(&down);
    let (_, digest) = blob_where(|d| down.write_owners(d) == [MemberId(2), MemberId(3)]);
    let placement = router.place(&digest).expect("a placement");
    let ids: Vec<_> = placement.writers.iter().map(|m| m.id).collect();
    assert_eq!(ids, [MemberId(3)]);

    let all_down = map(
        3,
        2,
        vec![member(1, MemberState::Down), member(2, MemberState::Down)],
    );
    let (router, _) = router_for(&all_down);
    assert!(
        router
            .place(&sha(b"x"))
            .expect("a placement")
            .writers
            .is_empty()
    );
}
