//! Reads: rank order, previous generation, failures, ranges, streaming and resuming.

use std::sync::atomic::Ordering;

use futures::StreamExt as _;
use kbf_map::MemberState;
use kbf_route::{MAX_RESUMES, MapView, Router};
use kbf_storage::StorageError;

use super::support::{
    Cell, Cluster, active, blob_read_from, blob_where, collect, map, member, router, sha,
    unavailable, view,
};

/// Catches owners tried in another order than rank (a read would go to the replica
/// first), and a read that goes on asking after an owner answered: the first owner holds
/// the blob and the second is never asked; the first lacks it and the second is asked
/// once and answers.
#[tokio::test]
async fn owners_are_tried_in_rank_order_and_the_first_hit_ends_it() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);

    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(1).hold(&blob);
    let (bytes, _) = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, blob);
    assert_eq!(cluster.node(1).read_count(), 1);
    assert_eq!(cluster.node(2).read_count(), 0);

    let (blob, digest) = blob_read_from(&map, &[2, 3]);
    cluster.node(3).hold(&blob);
    let (bytes, _) = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, blob);
    assert_eq!(cluster.node(2).read_count(), 1);
    assert_eq!(cluster.node(3).read_count(), 1);
    assert_eq!(
        cluster.node(1).read_count(),
        1,
        "a node that does not own the blob is not asked"
    );
}

/// Catches an absence taken for an outage and an outage taken for an absence: a blob
/// every owner lacks is NOT_FOUND; one that an owner could not answer for is UNAVAILABLE
/// (never "absent": it may be there), even when the other owner says it is not; and a
/// held blob that cannot be read is a failure, not an absence.
#[tokio::test]
async fn not_found_needs_every_owner_to_say_so() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (_, digest) = blob_read_from(&map, &[1, 2]);

    let error = router
        .read_range(&digest, 0, 0)
        .await
        .err()
        .expect("absent");
    assert!(
        matches!(error, StorageError::NotFound(d) if d == digest),
        "{error:?}"
    );

    cluster.node(1).set_down(true);
    let error = router
        .read_range(&digest, 0, 0)
        .await
        .err()
        .expect("not known");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert_eq!(
        cluster.node(2).read_count(),
        2,
        "the second owner was still asked"
    );

    cluster.node(1).set_down(false);
    cluster.node(1).fail_read(StorageError::Unreachable(digest));
    let error = router
        .read_range(&digest, 0, 0)
        .await
        .err()
        .expect("not known");
    assert!(
        matches!(error, StorageError::Unreachable(d) if d == digest),
        "{error:?}"
    );
}

/// Catches a request that no owner would answer differently walked on regardless: an
/// offset past the end and an invalid request end the read at the first owner, and the
/// second is never asked.
#[tokio::test]
async fn a_bad_request_ends_the_walk() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(1).hold(&blob);
    cluster.node(2).hold(&blob);

    let past = blob.len() as u64 + 1;
    let error = router
        .read_range(&digest, past, 0)
        .await
        .err()
        .expect("past the end");
    assert!(
        matches!(error, StorageError::OutOfRange { .. }),
        "{error:?}"
    );
    cluster
        .node(1)
        .fail_read(StorageError::Invalid("bad".to_owned()));
    let error = router
        .read_range(&digest, 0, 0)
        .await
        .err()
        .expect("invalid");
    assert!(matches!(error, StorageError::Invalid(_)), "{error:?}");
    assert_eq!(cluster.node(2).read_count(), 0);
}

/// Catches the previous generation ignored, or asked before this one's owners: during a
/// change a blob written before it is found where it was, after this generation's
/// owners; every call carries the generation the router routed by.
#[tokio::test]
async fn the_previous_generation_is_asked_after_this_ones() {
    let before = map(1, 2, active(&[1, 2, 3]));
    let now = map(2, 2, active(&[1, 2, 4]));
    let cluster = Cluster::of(4);
    let (router, cell) = router(&now, &cluster);
    cell.set(MapView {
        map: now.clone(),
        previous: Some(before.clone()),
    });
    // Owned by 1 and 4 now, and by 1 and 3 before: the old copy is at 3.
    let (blob, digest) = blob_where(|d| {
        now.read_order(d) == [kbf_map::MemberId(1), kbf_map::MemberId(4)]
            && before.read_order(d) == [kbf_map::MemberId(1), kbf_map::MemberId(3)]
    });
    cluster.node(3).hold(&blob);

    let (bytes, _) = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, blob);
    assert_eq!(cluster.dialled(), [(1, 2), (4, 2), (3, 2)]);
    for id in [1, 3, 4] {
        assert_eq!(cluster.node(id).read_count(), 1, "{id}");
    }
    assert_eq!(cluster.node(2).read_count(), 0);
}

/// Catches a farm whose owners are all down read as "absent" (the caller would upload the
/// blob again, or fail the action for a missing input), a node called when there is
/// nobody to call, and a router with no map routing somewhere.
#[tokio::test]
async fn nobody_to_ask_is_unavailable() {
    let down = vec![member(1, MemberState::Down), member(2, MemberState::Down)];
    let map = map(1, 2, down);
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let digest = sha(b"anything");
    let error = router.read_range(&digest, 0, 0).await.err().expect("down");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(error.to_string().contains("is down"), "{error}");
    assert!(cluster.dialled().is_empty());

    let nothing = Router::new(Cell::empty(), cluster.clone());
    let error = nothing
        .read_range(&digest, 0, 0)
        .await
        .err()
        .expect("no map");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(error.to_string().contains("no storage map"), "{error}");
}

/// Catches an owner that cannot be dialled (an address that is not one) ending the read
/// or being taken for an absence: it is skipped, the next owner answers, and when nobody
/// does the failure is the answer, not NOT_FOUND.
#[tokio::test]
async fn an_owner_that_cannot_be_dialled_is_skipped() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = std::sync::Arc::new(Cluster::default());
    let node_2 = cluster.add(2);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    node_2.hold(&blob);
    let (bytes, _) = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, blob);

    let (_, absent) = super::support::blobs_read_from(&map, &[1, 2], 2).remove(1);
    let error = router
        .read_range(&absent, 0, 0)
        .await
        .err()
        .expect("not known");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
}

/// Catches an offset or a limit changed on the way to the node: they are passed as
/// given, and the bytes are the range, in the node's chunks.
#[tokio::test]
async fn a_range_is_passed_to_the_owner_as_asked() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let blob = b"0123456789abcdefghij".to_vec();
    let digest = cluster.node(1).hold(&blob);

    let (bytes, chunks) = collect(router.read_range(&digest, 5, 8).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, &blob[5..13]);
    assert_eq!(chunks, 2);
    assert_eq!(cluster.node(1).read_log(), [(5, 8)]);
}

/// Catches a router that collects the blob it reads: a blob of a thousand chunks is
/// opened and three chunks are taken, and the node has produced no more than the next
/// one; collecting would have produced them all.
#[tokio::test]
async fn a_read_is_passed_on_chunk_by_chunk() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let (digest, produced) = cluster.node(1).hold_generated(1000);

    let mut stream = router.read_range(&digest, 0, 0).await.expect("a read");
    for _ in 0..3 {
        stream.next().await.expect("a chunk").expect("bytes");
    }
    let made = produced.load(Ordering::SeqCst);
    assert!(made <= 4, "{made} chunks produced for 3 taken");
}

/// Catches a read that cannot go on after its owner broke, or goes on from the wrong
/// place: the first owner passes two chunks and breaks, then answers nothing; the read
/// goes on at the second owner from byte 8 (and with a limit, with the bytes still to
/// come as the limit), and the caller gets the blob once, in order.
#[tokio::test]
async fn a_broken_read_goes_on_from_the_bytes_passed() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(1).hold(&blob);
    cluster.node(2).hold(&blob);
    cluster.node(1).break_read(2, unavailable());
    cluster.node(1).fail_read(unavailable());

    let (bytes, _) = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, blob);
    assert_eq!(cluster.node(2).read_log(), [(8, 0)]);

    let want = blob.len().min(15);
    cluster.node(1).break_read(2, unavailable());
    cluster.node(1).fail_read(unavailable());
    let (bytes, _) = collect(
        router
            .read_range(&digest, 0, want as u64)
            .await
            .expect("a read"),
    )
    .await
    .expect("bytes");
    assert_eq!(bytes, &blob[..want]);
    assert_eq!(cluster.node(2).read_log(), [(8, 0), (8, want as u64 - 8)]);
}

/// Catches a read with a limit that goes on after its last byte: the owner passes the two
/// chunks that make up the limit and then breaks, which is no failure of this read, and
/// must not be resumed (a resume would ask for the rest of the blob, with no limit).
#[tokio::test]
async fn a_limited_read_is_over_at_its_limit() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let blob = b"0123456789abcdefghij".to_vec();
    let digest = cluster.node(1).hold(&blob);
    cluster.node(1).break_read(2, unavailable());

    let (bytes, _) = collect(router.read_range(&digest, 0, 8).await.expect("a read"))
        .await
        .expect("the limit was read");
    assert_eq!(bytes, &blob[..8]);
    assert_eq!(cluster.node(1).read_count(), 1);
}

/// Catches a read that never gives up (an owner breaks every time) or gives up early: the
/// caller gets the failure after exactly `MAX_RESUMES` resumes, with the bytes passed
/// before it, and the stream is over then.
#[tokio::test]
async fn a_read_that_keeps_breaking_ends_with_the_failure() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let blob = b"0123456789abcdef0123456789abcdef".to_vec();
    let digest = cluster.node(1).hold(&blob);
    for _ in 0..=MAX_RESUMES {
        cluster.node(1).break_read(1, unavailable());
    }

    let mut stream = router.read_range(&digest, 0, 0).await.expect("a read");
    let mut got = Vec::new();
    let failure = loop {
        match stream.next().await.expect("an item") {
            Ok(chunk) => got.extend_from_slice(&chunk),
            Err(e) => break e,
        }
    };
    assert!(
        matches!(failure, StorageError::Unavailable(_)),
        "{failure:?}"
    );
    let resumes = MAX_RESUMES as usize;
    assert_eq!(got, &blob[..4 * (resumes + 1)]);
    assert_eq!(cluster.node(1).read_count(), resumes + 1);
    assert!(stream.next().await.is_none(), "a failure ends the stream");
}

/// Catches a resume that loses the reason it could not resume: the blob has gone from
/// every owner meanwhile, and the caller is told so, not that the read ended.
#[tokio::test]
async fn a_read_that_cannot_resume_says_why() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let digest = cluster.node(1).hold(b"0123456789abcdef");
    cluster.node(1).break_read(1, unavailable());
    cluster.node(1).fail_read(StorageError::NotFound(digest));

    let error = collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect_err("it cannot go on");
    assert!(
        matches!(error, StorageError::NotFound(d) if d == digest),
        "{error:?}"
    );
}

/// Catches a router that keeps the map it first saw: a map that arrives between two reads
/// is the one the second is routed by.
#[tokio::test]
async fn each_read_uses_the_map_as_it_is_then() {
    let first = map(1, 1, active(&[1]));
    let second = map(2, 1, active(&[2]));
    let cluster = Cluster::of(2);
    let (router, cell) = router(&first, &cluster);
    let digest = cluster.node(1).hold(b"seen at two generations");
    cluster.node(2).hold(b"seen at two generations");

    collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    cell.set(view(&second));
    collect(router.read_range(&digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(cluster.dialled(), [(1, 1), (2, 2)]);
}
