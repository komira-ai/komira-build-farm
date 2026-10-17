//! Writes and action-cache calls.

use std::sync::Arc;

use bytes::Bytes;
use kbf_map::{MemberId, MemberState};
use kbf_meta::{ActionRecord, ActionWriteError, Closure, Role};
use kbf_storage::StorageError;

use kbf_types::Digest;

use super::support::{
    Cluster, active, blob_where, blob_written_to, blobs_with_primary, map, member, router, sha,
    unavailable, view,
};

/// Catches a blob sent to every owner (a 1 GbE writer would send it R times) or to the
/// replica: it goes once to the primary, and the answer is its digest.
#[tokio::test]
async fn a_write_goes_to_the_primary_only() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_written_to(&map, &[2, 3]);

    assert_eq!(router.put(Bytes::from(blob)).await.expect("stored"), digest);
    assert_eq!(cluster.node(2).put_log(), [digest]);
    assert!(cluster.node(1).put_log().is_empty());
    assert!(cluster.node(3).put_log().is_empty());
    assert_eq!(cluster.dialled(), [(2, 1)]);
}

/// Catches a write sent to a primary the map says is down, and a joiner made the primary:
/// the write goes to the next owner, which is not down; a joiner is never first.
#[tokio::test]
async fn a_write_skips_an_owner_that_is_down() {
    let members = vec![
        member(1, MemberState::Down),
        member(2, MemberState::Active),
        member(3, MemberState::Joining),
    ];
    let map = map(1, 2, members);
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = super::support::blob_where(|d| {
        map.write_owners(d) == [kbf_map::MemberId(1), kbf_map::MemberId(2)]
    });

    router.put(Bytes::from(blob)).await.expect("stored");
    assert!(cluster.node(1).put_log().is_empty());
    assert_eq!(cluster.node(2).put_log(), [digest]);
}

/// Catches a write that goes nowhere without saying so, and a node called for it: every
/// owner down is UNAVAILABLE and nobody is called.
#[tokio::test]
async fn a_write_with_every_owner_down_is_unavailable() {
    let all_down = vec![member(1, MemberState::Down), member(2, MemberState::Down)];
    let map = map(1, 2, all_down);
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let error = router
        .put(Bytes::from_static(b"x"))
        .await
        .expect_err("down");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(cluster.dialled().is_empty());
}

/// Catches a node's failure swallowed, or an answer for another blob passed on as success:
/// the primary's error is the caller's, and a node that says it stored a different digest
/// is an internal error.
#[tokio::test]
async fn a_write_returns_the_nodes_failure_and_checks_its_answer() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    cluster.node(1).fail_next(unavailable());
    let error = router
        .put(Bytes::from_static(b"x"))
        .await
        .expect_err("fails");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");

    cluster.node(1).answer_puts_with(sha(b"other"));
    let error = router
        .put(Bytes::from_static(b"x"))
        .await
        .expect_err("wrong digest");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
}

/// A failure of a node that could not be reached, naming `who`.
fn gone(who: &str) -> StorageError {
    StorageError::Unavailable(Arc::new(std::io::Error::other(format!("{who} is gone"))))
}

/// Catches a write failed for the whole member lease (15 s) by an owner that is dead but
/// not yet marked down: the primary of the blob cannot be reached, the map still has it
/// active, and the write goes to the next owner and is answered, once there. The first
/// owner is tried first (and only dialled for the failed attempt), the next is dialled
/// with the same generation. Mutant: `at_a_write_owner` ends at the first owner's failure.
#[tokio::test]
async fn a_write_goes_on_to_the_next_owner_when_the_first_cannot_be_reached() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_written_to(&map, &[2, 3]);
    cluster.node(2).set_down(true);

    assert_eq!(router.put(Bytes::from(blob)).await.expect("stored"), digest);
    assert!(cluster.node(2).put_log().is_empty());
    assert_eq!(cluster.node(3).put_log(), [digest]);
    assert!(cluster.node(1).put_log().is_empty());
    assert_eq!(cluster.dialled(), [(2, 1), (3, 1)]);
}

/// Pins that the router keeps no memory of a failed owner (the map's member lease is the
/// only authority on who is down): the owner that failed is tried first by the next write,
/// and answers it when it is back. Fails too without the retry, at the first write.
#[tokio::test]
async fn the_owner_that_failed_is_tried_first_by_the_next_write() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_written_to(&map, &[2, 3]);
    cluster.node(2).fail_next(unavailable());

    router.put(Bytes::from(blob.clone())).await.expect("stored");
    assert_eq!(cluster.node(3).put_log(), [digest]);
    router.put(Bytes::from(blob)).await.expect("stored");
    assert_eq!(cluster.node(2).put_log(), [digest]);
    assert_eq!(cluster.node(3).put_log(), [digest]);
}

/// Catches an answer from a node that was reached passed over for the next owner: a node
/// that declines the write (the request is invalid, or it is broken) is the answer and the
/// next owner is not dialled; so is STALE_MAP, which is the map's rule (fetched, once)
/// rather than the next owner's. Mutant: every failure moves on.
#[tokio::test]
async fn a_write_an_owner_answered_is_not_tried_at_the_next() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, cell) = router(&map, &cluster);
    let (blob, _) = blob_written_to(&map, &[2, 3]);

    for refusal in [
        StorageError::Invalid("a blob over the largest".to_owned()),
        StorageError::Internal("a disk".to_owned()),
        StorageError::ActionWrite(ActionWriteError::NotDaemon),
    ] {
        let said = format!("{refusal:?}");
        cluster.node(2).fail_next(refusal);
        let error = router
            .put(Bytes::from(blob.clone()))
            .await
            .expect_err("refused");
        assert_eq!(format!("{error:?}"), said);
    }
    cluster
        .node(2)
        .fail_next(StorageError::StaleMap { current: 7 });
    let error = router
        .put(Bytes::from(blob))
        .await
        .expect_err("no newer map");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert_eq!(cell.fetched(), [7]);
    assert!(cluster.node(3).put_log().is_empty());
    assert!(cluster.dialled().iter().all(|(member, _)| *member == 2));
}

/// Catches a write failed with the last owner's failure, or with a failure that is not the
/// owners': when no owner can be reached the caller is told what the first one said, and
/// both were tried. Mutant: the last failure returned.
#[tokio::test]
async fn a_write_no_owner_can_take_fails_with_the_first_owners_failure() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, _) = blob_written_to(&map, &[2, 3]);
    cluster.node(2).fail_next(gone("node two"));
    cluster.node(3).fail_next(gone("node three"));

    let error = router.put(Bytes::from(blob)).await.expect_err("both gone");
    assert!(
        matches!(&error, StorageError::Unavailable(why) if why.to_string() == "node two is gone"),
        "{error:?}"
    );
    assert_eq!(cluster.dialled(), [(2, 1), (3, 1)]);
}

/// Catches a write that goes on to a member that does not own the blob: the owner a write
/// goes on to is the other one the map names, never a member outside the write owners.
/// With a farm of three, R = 2 and both owners failing, the third node is never called.
#[tokio::test]
async fn a_write_goes_on_to_the_blobs_other_owner_only() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, _) = blob_written_to(&map, &[3, 2]);
    cluster.node(3).set_down(true);
    cluster.node(2).set_down(true);

    router
        .put(Bytes::from(blob))
        .await
        .expect_err("both owners gone");
    assert_eq!(cluster.dialled(), [(3, 1), (2, 1)]);
    assert!(cluster.node(1).put_log().is_empty());
}

/// Catches a member whose address is not one stopping the write: the member cannot be
/// dialled (the test cluster has no node for it), as a read passes it over, so does a
/// write. Mutant: the dial error returned.
#[tokio::test]
async fn a_member_that_cannot_be_dialled_is_passed_over_by_a_write() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Arc::new(Cluster::default());
    cluster.add(2);
    cluster.add(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_written_to(&map, &[1, 2]);

    router.put(Bytes::from(blob)).await.expect("stored");
    assert_eq!(cluster.node(2).put_log(), [digest]);
}

/// Catches a plain commit that fails over: the caller put the entry's blobs at the
/// action's first owner, the next owner holds none of them and would refuse the entry as
/// `Absent`, so when the first cannot be reached the caller must see UNAVAILABLE (which it
/// can retry), and the next owner must be asked nothing.
/// Mutant: `commit_in` goes through `at_a_write_owner`.
#[tokio::test]
async fn a_plain_commit_does_not_go_to_an_owner_that_lacks_the_blobs() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(1).check_closure();
    let (_, action) = blob_written_to(&map, &[3, 1]);
    let record = ActionRecord {
        result: sha(b"the result blob"),
        closure: Closure::new(),
    };
    cluster.node(3).set_down(true);

    let error = router
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect_err("the action's owner is down");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(cluster.node(1).commits.lock().expect("lock").is_empty());
}

/// Catches a commit with a closure that ends at the action's owner when it cannot be
/// reached: the blobs the next owner lacks are copied to it (and only the ones it lacks),
/// and the entry is committed there. Mutant: `commit_with_closure` ends at the first owner.
#[tokio::test]
async fn a_commit_with_closure_goes_on_to_the_next_owner_and_copies_there() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(1).check_closure();
    let e = entry(&map);
    cluster.node(1).hold(&e.output.0);
    cluster.node(2).hold(&e.result.0);
    cluster.node(3).set_down(true);

    router
        .commit_with_closure(Role::Daemon, e.action, e.record)
        .await
        .expect("committed at the next owner");
    assert_eq!(cluster.node(1).put_log(), [e.result.1]);
    assert_eq!(
        cluster.node(1).commits.lock().expect("lock").clone(),
        [e.action]
    );
    assert!(cluster.node(3).commits.lock().expect("lock").is_empty());
    let found = router.action_result(&e.action).await.expect("an answer");
    assert_eq!(found.map(|(d, _)| d), Some(e.result.1));
}

/// Catches a commit that ends when the action's owner answers FindMissing and then fails
/// to take a copy: the failure is the owner's, so the next owner is asked what it lacks,
/// is copied that, and commits. Mutant: the copy's `put` failure counted as not the
/// owner's (`Tried::Elsewhere`).
#[tokio::test]
async fn a_commit_goes_on_when_the_owner_fails_to_take_a_copy() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(1).check_closure();
    let e = entry(&map);
    cluster.node(1).hold(&e.output.0);
    cluster.node(2).hold(&e.result.0);
    cluster.node(2).hold(&e.output.0);
    cluster.node(3).fail_next_put(unavailable());

    router
        .commit_with_closure(Role::Daemon, e.action, e.record)
        .await
        .expect("committed at the next owner");
    // The first owner was reached (it answered FindMissing) and failed at the copy.
    assert!(!cluster.node(3).find_log().is_empty());
    assert!(cluster.node(3).commits.lock().expect("lock").is_empty());
    assert_eq!(cluster.node(1).put_log(), [e.result.1]);
    assert_eq!(
        cluster.node(1).commits.lock().expect("lock").clone(),
        [e.action]
    );
}

/// Catches a commit that ends when the action's owner holds every blob of the entry (so
/// there is nothing to copy) and then fails the commit itself: the next owner is copied
/// what it lacks and commits. Mutant: the commit's failure counted as not the owner's
/// (`Tried::Elsewhere`).
#[tokio::test]
async fn a_commit_goes_on_when_the_owner_fails_the_commit_itself() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(1).check_closure();
    let e = entry(&map);
    cluster.node(1).hold(&e.output.0);
    cluster.node(2).hold(&e.result.0);
    cluster.node(3).hold(&e.result.0);
    cluster.node(3).hold(&e.output.0);
    cluster.node(3).fail_next_commit(unavailable());

    router
        .commit_with_closure(Role::Daemon, e.action, e.record)
        .await
        .expect("committed at the next owner");
    assert!(cluster.node(3).put_log().is_empty());
    assert!(cluster.node(3).commits.lock().expect("lock").is_empty());
    assert_eq!(cluster.node(1).put_log(), [e.result.1]);
    assert_eq!(
        cluster.node(1).commits.lock().expect("lock").clone(),
        [e.action]
    );
}

/// Catches a commit retried for a failure that is not the owner's: when a blob of the entry
/// cannot be read from the farm (its holders cannot be reached), the next owner of the
/// action would read it no better, so the commit ends with that read's failure and the next
/// owner is not asked a thing. Mutant: a failed read counted as the owner's failure.
#[tokio::test]
async fn a_closure_blob_that_cannot_be_read_does_not_move_the_commit_on() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(3).check_closure();
    let e = entry(&map);
    cluster.node(2).hold(&e.result.0);
    // The output's owners are 1 and 2: 1 is gone, and 2 does not hold it.
    cluster.node(1).set_down(true);

    let error = router
        .commit_with_closure(Role::Daemon, e.action, e.record)
        .await
        .expect_err("the output cannot be read");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(cluster.node(3).commits.lock().expect("lock").is_empty());
    assert!(cluster.node(1).find_log().is_empty());
}

/// Catches a commit sent anywhere but the action's primary, or the entry then not read
/// back from the owners: a commit lands on the primary of the action digest, and a read of
/// the action finds it there.
#[tokio::test]
async fn an_action_entry_is_committed_at_the_primary_and_read_from_the_owners() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (_, action) = blob_written_to(&map, &[3, 1]);
    let result = sha(b"the result blob");
    let record = ActionRecord {
        result,
        closure: Closure::new(),
    };

    router
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect("committed");
    assert_eq!(
        cluster.node(3).commits.lock().expect("lock").clone(),
        [action]
    );
    assert!(cluster.node(1).commits.lock().expect("lock").is_empty());

    let (found, _) = router
        .action_result(&action)
        .await
        .expect("an answer")
        .expect("an entry");
    assert_eq!(found, result);
}

/// Catches an entry lost behind an owner that lacks it or cannot answer: the first owner
/// has none and the second has it (found, and the first was asked); an action nobody has
/// is a miss, and so is one where some owner could not answer and the rest have none (a
/// cache is only ever missed); every owner failing is the failure.
#[tokio::test]
async fn an_action_is_looked_up_at_each_owner_until_one_has_it() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (_, action) = super::support::blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold_action(action, sha(b"r"));

    let found = router.action_result(&action).await.expect("an answer");
    assert!(found.is_some());
    assert_eq!(
        cluster
            .node(1)
            .action_gets
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        cluster
            .node(2)
            .action_gets
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    let other = super::support::blobs_read_from(&map, &[1, 2], 2)
        .remove(1)
        .1;
    assert!(
        router
            .action_result(&other)
            .await
            .expect("an answer")
            .is_none()
    );

    cluster.node(1).set_down(true);
    assert!(
        router
            .action_result(&other)
            .await
            .expect("a miss")
            .is_none()
    );

    cluster.node(2).set_down(true);
    let error = router
        .action_result(&other)
        .await
        .expect_err("nobody answers");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
}

/// An entry for `action`, owned by node 3, whose result blob and one output are owned by
/// the other nodes: the shape that a commit alone cannot take.
struct Entry {
    action: Digest,
    record: ActionRecord,
    result: (Vec<u8>, Digest),
    output: (Vec<u8>, Digest),
}

fn entry(map: &Arc<kbf_map::Map>) -> Entry {
    let (_, action) = blob_written_to(map, &[3, 1]);
    let output = blobs_with_primary(map, 1, 1).remove(0);
    let result = blobs_with_primary(map, 2, 1).remove(0);
    Entry {
        action,
        record: ActionRecord {
            result: result.1,
            closure: [output.1].into_iter().collect(),
        },
        result,
        output,
    }
}

/// Catches the refusal the frontend hit on three nodes: a node commits an entry only if it
/// holds the entry's blobs itself, and the action's owner is not where the result or the
/// output are. A plain commit is refused `Absent`; `commit_with_closure` copies the two
/// blobs to the owner (and only the owner), commits, and the entry is then found.
#[tokio::test]
async fn an_entry_is_committed_with_its_blobs_copied_to_the_actions_owner() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(3).check_closure();
    let e = entry(&map);
    cluster.node(1).hold(&e.output.0);
    cluster.node(2).hold(&e.result.0);

    let plain = router
        .commit_action_record(Role::Daemon, e.action, e.record.clone())
        .await
        .expect_err("the owner lacks the blobs");
    assert!(matches!(plain, StorageError::ActionWrite(_)), "{plain:?}");
    assert!(cluster.node(3).commits.lock().expect("lock").is_empty());

    router
        .commit_with_closure(Role::Daemon, e.action, e.record.clone())
        .await
        .expect("committed");
    let mut copied = cluster.node(3).put_log();
    copied.sort();
    let mut expect = vec![e.output.1, e.result.1];
    expect.sort();
    assert_eq!(copied, expect);
    assert_eq!(
        cluster.node(3).commits.lock().expect("lock").clone(),
        [e.action]
    );
    assert!(cluster.node(1).put_log().is_empty());
    assert!(cluster.node(2).put_log().is_empty());
    let found = router.action_result(&e.action).await.expect("an answer");
    assert_eq!(found.map(|(d, _)| d), Some(e.result.1));
}

/// Catches a closure copy made with a plain write: a node in a chain refuses a blob it is
/// not the primary of, and the action's owner is rarely the primary of the entry's blobs, so
/// the commit failed and a repeat of the action ran again. The copy is an unowned write:
/// with the owner refusing plain writes the blobs are stored and the entry is committed.
#[tokio::test]
async fn the_closure_copy_is_taken_by_an_owner_that_refuses_plain_writes() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(3).check_closure();
    cluster.node(3).refuse_plain_puts();
    let e = entry(&map);
    cluster.node(1).hold(&e.output.0);
    cluster.node(2).hold(&e.result.0);

    router
        .commit_with_closure(Role::Daemon, e.action, e.record)
        .await
        .expect("committed");
    assert_eq!(
        cluster.node(3).commits.lock().expect("lock").clone(),
        [e.action]
    );
}

/// Catches a copy of what the owner already has: a blob the owner holds is not read from
/// anyone or written again.
#[tokio::test]
async fn a_blob_the_owner_holds_is_not_copied() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(3).check_closure();
    let e = entry(&map);
    cluster.node(1).hold(&e.output.0);
    cluster.node(2).hold(&e.result.0);
    cluster.node(3).hold(&e.output.0);

    router
        .commit_with_closure(Role::Daemon, e.action, e.record)
        .await
        .expect("committed");
    assert_eq!(cluster.node(3).put_log(), [e.result.1]);
    assert_eq!(cluster.node(1).read_count(), 0);
}

/// Catches a commit with a hole in its closure: a blob that no owner holds fails the
/// commit with that blob's error, and nothing is committed.
#[tokio::test]
async fn a_blob_held_nowhere_is_refused_and_nothing_is_committed() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(3).check_closure();
    let e = entry(&map);
    cluster.node(2).hold(&e.result.0);

    let error = router
        .commit_with_closure(Role::Daemon, e.action, e.record)
        .await
        .expect_err("a blob nobody holds");
    assert!(
        matches!(error, StorageError::NotFound(d) if d == e.output.1),
        "{error:?}"
    );
    assert!(cluster.node(3).commits.lock().expect("lock").is_empty());
}

/// Catches a bad copy spread to the owner: the first node to be asked sends other bytes
/// than the digest names, and the owner is given the next node's copy, never the bad one.
#[tokio::test]
async fn a_corrupt_copy_is_never_copied_to_the_owner() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(3).check_closure();
    let (_, action) = blob_written_to(&map, &[3, 1]);
    let result = blob_where(|d| map.read_order(d)[..2] == [MemberId(1), MemberId(2)]);
    let record = ActionRecord {
        result: result.1,
        closure: Closure::new(),
    };
    cluster.node(1).hold_as(result.1, b"not the result");
    cluster.node(2).hold(&result.0);

    router
        .commit_with_closure(Role::Daemon, action, record)
        .await
        .expect("committed from the good copy");
    assert_eq!(
        cluster.node(3).blob(&result.1).expect("on the owner"),
        result.0
    );
    assert_eq!(cluster.node(3).put_log(), [result.1]);
}

/// Catches a commit that ignores the staleness rule or loops on it: one STALE_MAP from the
/// owner refetches the map once and the commit then succeeds against the new generation;
/// a second one is UNAVAILABLE.
#[tokio::test]
async fn a_commit_with_closure_follows_the_stale_map_rule() {
    let one = map(1, 1, active(&[1]));
    let two = map(2, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, cell) = router(&one, &cluster);
    let record = ActionRecord {
        result: cluster.node(1).hold(b"r"),
        closure: Closure::new(),
    };
    let action = sha(b"action");
    cell.answer_fetch(Ok(view(&two)));
    cluster
        .node(1)
        .fail_next(StorageError::StaleMap { current: 2 });
    router
        .commit_with_closure(Role::Daemon, action, record.clone())
        .await
        .expect("committed after one refetch");
    assert_eq!(cell.fetched(), [2]);
    assert_eq!(cluster.dialled(), [(1, 1), (1, 2)]);

    let three = map(3, 1, active(&[1]));
    cell.answer_fetch(Ok(view(&three)));
    cluster
        .node(1)
        .fail_next(StorageError::StaleMap { current: 3 });
    cluster
        .node(1)
        .fail_next(StorageError::StaleMap { current: 4 });
    let error = router
        .commit_with_closure(Role::Daemon, action, record)
        .await
        .expect_err("two stale answers");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert_eq!(cell.fetched(), [2, 3]);
}

/// Catches an entry whose result blob is empty (an `ActionResult` with nothing set encodes
/// to no bytes) committed without the blob ever stored on the owner: a node reports the
/// empty blob held, but commits against its index, so the commit would be refused `Absent`.
/// The empty blob is written to the owner, and read from nobody.
#[tokio::test]
async fn an_empty_result_blob_is_stored_on_the_owner() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    cluster.node(3).check_closure();
    let (_, action) = blob_written_to(&map, &[3, 1]);
    let record = ActionRecord {
        result: sha(b""),
        closure: Closure::new(),
    };

    router
        .commit_with_closure(Role::Daemon, action, record)
        .await
        .expect("committed");
    assert_eq!(cluster.node(3).put_log(), [sha(b"")]);
    assert_eq!(
        cluster.node(3).commits.lock().expect("lock").clone(),
        [action]
    );
    assert_eq!(
        (1..=3).map(|n| cluster.node(n).read_count()).sum::<usize>(),
        0
    );
}

/// Catches a commit with a closure that goes nowhere or trusts a copy that was not made:
/// with every owner of the action down it is UNAVAILABLE and no node is called; and a node
/// that answers a copy with another digest is an internal error, with nothing committed.
#[tokio::test]
async fn a_commit_with_closure_needs_an_owner_and_checks_each_copy() {
    let all_down = vec![member(1, MemberState::Down), member(2, MemberState::Down)];
    let down = map(1, 2, all_down);
    let cluster = Cluster::of(2);
    let (nowhere, _) = router(&down, &cluster);
    let record = ActionRecord {
        result: sha(b"r"),
        closure: Closure::new(),
    };
    let error = nowhere
        .commit_with_closure(Role::Daemon, sha(b"action"), record.clone())
        .await
        .expect_err("every owner is down");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(cluster.dialled().is_empty());
    // A plain commit, which never goes past the first owner, needs one as much.
    let error = nowhere
        .commit_action_record(Role::Daemon, sha(b"action"), record.clone())
        .await
        .expect_err("every owner is down");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(cluster.dialled().is_empty());

    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (_, action) = blob_written_to(&map, &[3, 1]);
    cluster.node(1).hold(b"r");
    cluster.node(2).hold(b"r");
    cluster.node(3).answer_puts_with(sha(b"another blob"));
    let error = router
        .commit_with_closure(Role::Daemon, action, record)
        .await
        .expect_err("the copy was stored as another blob");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
    assert!(cluster.node(3).commits.lock().expect("lock").is_empty());
}

/// Catches a closure check that asks the action's owner about every blob in one call (a
/// node refuses a call of more than `MAX_FIND_MISSING_DIGESTS` digests as invalid, and the
/// commit, with the action-cache write behind it, would be lost): an entry of one more
/// blob than that, with the result, is asked about in a call of the most a node takes and
/// a call of the rest, and the one blob the owner lacks is copied to it.
#[tokio::test]
async fn a_long_closure_is_checked_in_calls_a_node_takes() {
    use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (_, action) = blob_written_to(&map, &[1, 2]);
    let last = sha(b"the one blob the owner lacks");
    cluster
        .node(2)
        .hold_as(last, b"the one blob the owner lacks");
    let held: Vec<Digest> = (0_u64..MAX_FIND_MISSING_DIGESTS as u64)
        .map(|i| sha(&i.to_le_bytes()))
        .collect();
    for digest in &held {
        cluster.node(1).hold_as(*digest, b"held");
    }
    let record = ActionRecord {
        result: cluster.node(1).hold(b"the result"),
        closure: held.iter().copied().chain([last]).collect(),
    };
    let asked = 1 + record.closure.len();
    assert!(asked > MAX_FIND_MISSING_DIGESTS);

    router
        .commit_with_closure(Role::Daemon, action, record)
        .await
        .expect("committed");
    let calls: Vec<usize> = cluster.node(1).find_log().iter().map(Vec::len).collect();
    assert_eq!(
        calls,
        [MAX_FIND_MISSING_DIGESTS, asked - MAX_FIND_MISSING_DIGESTS]
    );
    assert_eq!(cluster.node(1).put_log(), [last]);
    assert_eq!(
        cluster.node(1).commits.lock().expect("lock").clone(),
        [action]
    );
}

/// An entry for an action owned by node 1 whose `count` output blobs of `size` bytes are
/// held by node 2 alone, with the result held by node 1.
fn heavy_entry(
    cluster: &Cluster,
    map: &kbf_map::Map,
    count: usize,
    size: usize,
) -> (Digest, ActionRecord) {
    let (_, action) = blob_written_to(map, &[1, 2]);
    let outputs: Vec<Digest> = (0..count)
        .map(|i| {
            let mut bytes = format!("heavy output {i} ").into_bytes();
            bytes.resize(size, b'.');
            cluster.node(2).hold(&bytes)
        })
        .collect();
    let record = ActionRecord {
        result: cluster.node(1).hold(b"the result"),
        closure: outputs.into_iter().collect(),
    };
    (action, record)
}

/// Catches copies that hold more in memory than the router's budget: with room for two of
/// the four blobs an entry lacks (1 KiB each, 2 KiB of budget), only two are read at once
/// however many commits are wanted, and when the two are done the other two are read and
/// all four reach the owner. The reads are held at the source node, so the count of reads
/// in flight is what the budget let through.
#[tokio::test]
async fn copies_wait_for_room_in_the_budget() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let router = router.with_copy_budget(2048);
    let (action, record) = heavy_entry(&cluster, &map, 4, 1000);
    let gate = cluster.node(2).gate();

    let commit = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .commit_with_closure(Role::Daemon, action, record)
                .await
        }
    });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(router.reads_in_flight(), 2);

    gate.add_permits(100);
    commit.await.expect("joined").expect("committed");
    assert_eq!(cluster.node(1).put_log().len(), 4);
    assert_eq!(router.reads_in_flight(), 0);
}

/// Catches a budget that a large blob can never fit: a blob of 5 000 bytes against a
/// budget of 1 KiB is copied (alone), where waiting for room that the budget cannot give
/// would never end.
#[tokio::test]
async fn a_blob_larger_than_the_budget_is_copied_alone() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let router = router.with_copy_budget(1024);
    let (action, record) = heavy_entry(&cluster, &map, 2, 5000);

    let commit = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .commit_with_closure(Role::Daemon, action, record)
                .await
        }
    });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(commit.is_finished(), "the copies are waiting for room");
    commit.await.expect("joined").expect("committed");
    assert_eq!(cluster.node(1).put_log().len(), 2);
}
