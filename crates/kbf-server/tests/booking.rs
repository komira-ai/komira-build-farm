//! Sized bookings end to end: `kbf-book-cpus` and `kbf-book-mem-gib` on an action, the
//! booking in the daemon's `Start`, and placement that books it on the node.

mod support;

use support::{Job, done, output, ran};

const GIB: u64 = 1 << 30;

/// Catches: a sized booking that reaches the scheduler but not the daemon's `Start`
/// (the native driver sets its memory kill from `Start.memory_bytes`, so a large link
/// step would still die at 2 GiB); a booking the scheduler does not hold on the node,
/// so a second large action is placed beside the first in memory the node does not
/// have; and a booking kept after its lease ends, so the queued action never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sized_booking_reaches_the_daemon_and_holds_the_node() {
    let cell = support::Cell::start().await;
    let mut node = cell.daemon("node-a", 8, 16).await;
    let big = [("kbf-book-cpus", "4"), ("kbf-book-mem-gib", "12")];
    let first = Job::new("link one", &big);
    let second = Job::new("link two", &big);
    let small = Job::new("compile", &[]);
    for job in [&first, &second, &small] {
        cell.upload(&job.blobs()).await;
    }

    let mut first_ops = cell.execute(&first.action).await;
    let first_start = node.start().await;
    assert_eq!(
        first_start.action_digest.as_ref(),
        Some(&first.action.proto)
    );
    assert_eq!(
        (first_start.millicpus, first_start.memory_bytes),
        (4_000, 12 * GIB)
    );

    // 4 GiB are left: the second 12 GiB booking waits, a 1 GiB action does not.
    let mut second_ops = cell.execute(&second.action).await;
    node.no_work().await;
    let mut small_ops = cell.execute(&small.action).await;
    let small_start = node.start().await;
    assert_eq!(
        small_start.action_digest.as_ref(),
        Some(&small.action.proto)
    );
    assert_eq!(
        (small_start.millicpus, small_start.memory_bytes),
        (1_000, GIB)
    );
    let compiled = output(&cell, "compiled", 0).await;
    assert!(
        node.report(ran(small_start.lease_id, &compiled))
            .await
            .accepted
    );
    done(&mut small_ops).await;

    let linked = output(&cell, "linked once", 0).await;
    assert!(
        node.report(ran(first_start.lease_id, &linked))
            .await
            .accepted
    );
    done(&mut first_ops).await;
    let second_start = node.start().await;
    assert_eq!(
        second_start.action_digest.as_ref(),
        Some(&second.action.proto)
    );
    assert_eq!(second_start.memory_bytes, 12 * GIB);
    let linked = output(&cell, "linked twice", 0).await;
    assert!(
        node.report(ran(second_start.lease_id, &linked))
            .await
            .accepted
    );
    done(&mut second_ops).await;
}
