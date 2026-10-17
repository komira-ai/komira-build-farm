//! FindMissing: split by primary, merged in the order asked.

use kbf_map::MemberState;
use kbf_storage::StorageError;
use kbf_types::Digest;

use super::support::{
    Cluster, active, blob_where, blobs_with_primary, map, member, router, sha, unavailable,
};

/// The digests of `count` blobs whose primary is `id`.
fn primary_is(map: &kbf_map::Map, id: u64, count: usize) -> Vec<Digest> {
    blobs_with_primary(map, id, count)
        .into_iter()
        .map(|(_, d)| d)
        .collect()
}

/// `digests` in digest order: a node is asked each digest once, in this order.
fn sorted(mut digests: Vec<Digest>) -> Vec<Digest> {
    digests.sort();
    digests
}

/// Catches digests asked of every owner, asked in one call of everything, or answered in
/// the nodes' order: three digests each of two primaries are asked as one call to each
/// primary naming exactly its own (each once, though one was asked twice), the replicas are never asked, and the answer is the
/// missing ones in the order asked, a repeated digest answered twice.
#[tokio::test]
async fn digests_are_asked_of_their_primaries_and_answered_in_order() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (of_1, of_2) = (primary_is(&map, 1, 3), primary_is(&map, 2, 3));
    // Interleaved, with one repeated; node 1 and node 2 hold the first of their own.
    let asked = vec![
        of_2[0], of_1[0], of_2[1], of_1[1], of_2[2], of_1[2], of_1[1],
    ];
    cluster.node(1).hold_as(of_1[0], b"x");
    cluster.node(2).hold_as(of_2[0], b"y");

    let missing = router.find_missing(&asked).await.expect("an answer");
    assert_eq!(missing, [of_2[1], of_1[1], of_2[2], of_1[2], of_1[1]]);
    assert_eq!(cluster.node(1).find_log(), [sorted(of_1.clone())]);
    assert_eq!(cluster.node(2).find_log(), [sorted(of_2.clone())]);
    assert!(cluster.node(3).find_log().is_empty());
    assert_eq!(cluster.dialled(), [(1, 1), (2, 1)]);
}

/// Catches an empty question calling anybody.
#[tokio::test]
async fn nothing_asked_calls_nobody() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    assert_eq!(router.find_missing(&[]).await.expect("an answer"), []);
    assert!(cluster.dialled().is_empty());
}

/// Catches a primary that is down in the map still asked (the call would fail or wait for
/// it) and a joiner that is the primary: the down primary is skipped and the next owner is
/// asked in its place.
#[tokio::test]
async fn a_down_primary_is_skipped() {
    let members = vec![
        member(1, MemberState::Down),
        member(2, MemberState::Active),
        member(3, MemberState::Active),
    ];
    let map = map(1, 2, members);
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (_, digest) =
        blob_where(|d| map.write_owners(d) == [kbf_map::MemberId(1), kbf_map::MemberId(2)]);

    assert_eq!(
        router.find_missing(&[digest]).await.expect("an answer"),
        [digest]
    );
    assert!(cluster.node(1).find_log().is_empty());
    assert_eq!(cluster.node(2).find_log(), [vec![digest]]);
}

/// Catches a primary's failure failing the whole call, or its digests left unanswered: the
/// primary of the first three digests fails, so those go to their next owner and the call
/// still answers every digest; the other primary's digests are not asked twice.
#[tokio::test]
async fn a_primary_that_fails_hands_its_digests_to_the_next_owner() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (of_1, of_2) = (primary_is(&map, 1, 2), primary_is(&map, 2, 2));
    cluster.node(1).fail_next(unavailable());
    cluster.node(2).hold_as(of_1[0], b"replica copy");

    let asked = vec![of_1[0], of_2[0], of_1[1], of_2[1]];
    let missing = router.find_missing(&asked).await.expect("an answer");
    assert_eq!(missing, [of_2[0], of_1[1], of_2[1]]);
    let log = cluster.node(2).find_log();
    assert_eq!(log, [sorted(of_2.clone()), sorted(of_1.clone())]);
}

/// Catches a partial answer when nobody could answer for some digest: the primary and the
/// replica both fail and the call fails, naming the failure; with every owner of a digest
/// down it says so.
#[tokio::test]
async fn a_digest_nobody_can_answer_for_fails_the_call() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let digest = primary_is(&map, 1, 1)[0];
    cluster.node(1).set_down(true);
    cluster.node(2).set_down(true);
    let error = router
        .find_missing(&[digest])
        .await
        .expect_err("nobody answers");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");

    let all_down = vec![member(1, MemberState::Down), member(2, MemberState::Down)];
    let map = super::support::map(1, 2, all_down);
    let (router, _) = super::support::router(&map, &cluster);
    let error = router
        .find_missing(&[sha(b"x")])
        .await
        .expect_err("all down");
    assert!(error.to_string().contains("is down"), "{error}");
}

/// Catches a long list asked of a node in one call (a node refuses a call of more than
/// `MAX_FIND_MISSING_DIGESTS` digests as invalid, and the whole call would fail): 70 000
/// digests for one primary go as a call of the most a node takes and a call of the rest, and
/// the answer is every digest that node lacks.
#[tokio::test]
async fn a_long_list_is_asked_in_calls_a_node_takes() {
    use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let digests: Vec<Digest> = (0_u64..70_000).map(|i| sha(&i.to_le_bytes())).collect();
    let held = digests[0];
    cluster.node(1).hold_as(held, b"held");

    let missing = router.find_missing(&digests).await.expect("an answer");
    assert_eq!(missing.len(), digests.len() - 1);
    assert!(!missing.contains(&held));
    let calls: Vec<usize> = cluster.node(1).find_log().iter().map(Vec::len).collect();
    assert_eq!(
        calls,
        [MAX_FIND_MISSING_DIGESTS, 70_000 - MAX_FIND_MISSING_DIGESTS]
    );
}
