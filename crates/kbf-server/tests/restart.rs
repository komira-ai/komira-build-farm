//! Lease ids across a server restart, and the action a `Result` names (issue #137).
//!
//! A single-node server keeps its leases in its process. A daemon can still hold a
//! lease of the process before a restart, as a run or as an unacknowledged `Result`,
//! when it registers with the next process. If the new process granted the same lease
//! id, the old run's `Result` would be taken as another operation's, answer its callers
//! and be written to the action cache under its action's digest.

mod support;

use support::{Cell, Job, done, done_within_quiet, output, ran, response};
use tonic::Code;

/// Catches issue #137: a restarted server that grants lease ids its predecessor
/// granted (every process numbered leases from `(1, 0)`). The daemon holds the old
/// process's lease, unacknowledged; the new process grants the same node its first
/// lease, for another action; the daemon sends the old run's `Result`. It must be
/// refused, answer nobody and stay out of the action cache, and the new lease's own
/// `Result` must still be accepted. Also catches a `Welcome` that does not name the
/// term the process grants leases under, or names the predecessor's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_server_refuses_its_predecessor_s_results() {
    let before = Cell::start().await;
    let mut old = before.daemon("node-a", 4, 8).await;
    let x = Job::new("before the restart", &[]);
    before.upload(&x.blobs()).await;
    let _x_ops = before.execute(&x.action).await;
    let old_lease = old.start().await.lease_id.expect("a lease id");
    assert_eq!(old_lease.term, old.epoch, "Welcome names the lease term");
    let old_result = output(&before, "from the old process's run", 0).await;

    let after = before.restart().await;
    let mut daemon = after.daemon("node-a", 4, 8).await;
    assert_ne!(daemon.epoch, old.epoch, "a restart kept the lease epoch");
    let y = Job::new("after the restart", &[]);
    after.upload(&y.blobs()).await;
    let mut y_ops = after.execute(&y.action).await;
    let start = daemon.start().await;
    let lease = start.lease_id.expect("a lease id");
    assert_eq!(lease.term, daemon.epoch, "Welcome names the lease term");

    let ack = daemon.report(ran(Some(old_lease), &old_result)).await;
    assert!(
        !ack.accepted,
        "the old process's Result of {old_lease:?} was accepted as {lease:?}'s"
    );
    assert!(
        !done_within_quiet(&mut y_ops).await,
        "the old run answered the new action"
    );
    assert_eq!(after.cached(&y.action).await, Err(Code::NotFound));

    let result = output(&after, "from the new process's run", 0).await;
    assert!(daemon.report(ran(Some(lease), &result)).await.accepted);
    assert_eq!(response(&done(&mut y_ops).await).result, Some(result));
}

/// Catches a server that accepts a `Result` on its lease id alone: one that names
/// another action than the lease's `Start` did is refused, answers nobody and is not
/// cached; one that names the `Start`'s action is accepted. (One that names no action,
/// as a daemon that predates the field sends it, is what every other test sends.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_that_names_another_action_is_refused() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("its own action", &[]);
    let other = Job::new("another action", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    assert_eq!(start.action_digest.as_ref(), Some(&job.action.proto));
    let result = output(&cell, "ran", 0).await;

    let mut wrong = ran(start.lease_id, &result);
    wrong.action_digest = Some(other.action.proto.clone());
    assert!(
        !daemon.report(wrong).await.accepted,
        "a Result for another action was accepted"
    );
    assert!(!done_within_quiet(&mut ops).await, "it answered the caller");
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
    assert_eq!(cell.cached(&other.action).await, Err(Code::NotFound));

    let mut right = ran(start.lease_id, &result);
    right.action_digest = start.action_digest.clone();
    assert!(daemon.report(right).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    assert_eq!(cell.cached(&job.action).await, Ok(result));
}
