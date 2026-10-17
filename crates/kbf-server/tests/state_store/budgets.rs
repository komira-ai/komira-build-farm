//! What an operation's fault budgets leave in the state store, and what a farm that takes
//! the store over (after a handover, or after the old process died) holds because of
//! them: the farm runs it has used, the nodes placement skips for it, and the leases
//! given up as not started. Each test has a handover variant (the old farm is fenced) and
//! a crash variant (it is dropped with no fence).

use std::collections::BTreeSet;

use kbf_proto::worker::Lost;
use kbf_sched::{FARM_RERUNS, NOT_STARTED_RERUNS};
use kbf_server::farm::NodeAction;

use super::*;

/// How the old process ends.
#[derive(Clone, Copy, Debug)]
enum End {
    Handover,
    Crash,
}

/// The old farm ends as `end` says; returns the successor over the same store.
fn take_over(disk: &mut Disk, old: TestFarm, end: End) -> TestFarm {
    match end {
        End::Handover => old.fence(),
        End::Crash => disk.crash(old),
    }
    disk.generation_farm()
}

/// Registers `name` with room for four actions.
fn register_as(farm: &TestFarm, name: &str) -> (StreamId, Inbox) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(4_000, 8 * GIB), caps);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let stream = farm.register(
        &WorkerId::new(name),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        reported,
        outbound,
        ServerMessage::default(),
    );
    (stream, inbox)
}

/// A result of `lease` the farm lost: the node fenced the run.
fn lost(lease: LeaseId) -> worker::Result {
    worker::Result {
        status: Some(rpc::Status {
            code: Code::Unavailable as i32,
            message: "fenced".to_owned(),
            details: Vec::new(),
        }),
        lost: Lost::Fenced as i32,
        ..invalid(lease)
    }
}

/// The one lease waiting in `inbox`.
fn one_lease(inbox: &mut Inbox) -> LeaseId {
    let [lease] = starts(inbox)[..] else {
        panic!("one lease expected");
    };
    lease
}

/// An operation whose farm runs `FARM_RERUNS` have ended as the farm's fault on
/// `node-a`, queued behind a cordon for the rerun the budget still allows.
async fn farm_runs_used(farm: &TestFarm, inbox: &mut Inbox) {
    submit(farm, request(1)).expect("submitted");
    for _ in 1..FARM_RERUNS {
        farm.tick().await;
        let lease = one_lease(inbox);
        let ack = farm.report(&node(), lost(lease)).await;
        assert!(ack.expect("an acknowledgement").accepted);
    }
    farm.tick().await;
    let lease = one_lease(inbox);
    farm.place(&node(), NodeAction::Cordon).expect("cordoned");
    let ack = farm.report(&node(), lost(lease)).await;
    assert!(ack.expect("an acknowledgement").accepted);
    assert!(starts(inbox).is_empty(), "a cordoned node was granted");
}

/// The farm runs are spent by the old process, so the successor's next fault is the
/// last: the operation ends with no further run.
async fn farm_runs_are_not_given_back(end: End) {
    let mut disk = Disk::new();
    let old = disk.generation_farm();
    let (_, mut inbox) = register_as(&old, "node-a");
    farm_runs_used(&old, &mut inbox).await;
    assert_eq!((disk.rows("op/"), disk.rows("queue/")), (1, 1));

    let successor = take_over(&mut disk, old, end);
    let (_, mut inbox) = register_as(&successor, "node-a");
    successor
        .place(&node(), NodeAction::Uncordon)
        .expect("uncordoned");
    successor.tick().await;
    let lease = one_lease(&mut inbox);
    let ack = successor.report(&node(), lost(lease)).await;
    assert!(ack.expect("an acknowledgement").accepted);
    assert_eq!(disk.rows("op/"), 0, "the operation was not finished");
    assert!(
        starts(&mut inbox).is_empty(),
        "the successor ran it a further time: the budget of {FARM_RERUNS} reruns restarted"
    );
}

/// Catches: farm runs that live only in the old process. An operation whose run was lost
/// `FARM_RERUNS` times is, at the successor, given a further rerun at its next fault (one
/// more than the cap, at every handover). Mutant: the fate row not written for a fault.
#[tokio::test(start_paused = true)]
async fn farm_runs_survive_a_handover() {
    farm_runs_are_not_given_back(End::Handover).await;
}

/// Catches: the same, when the old process dies with no fence.
#[tokio::test(start_paused = true)]
async fn farm_runs_survive_a_crash() {
    farm_runs_are_not_given_back(End::Crash).await;
}

/// The node an operation was lost on (`lost_on`) is skipped by the successor's placement
/// while the other could run it. Run for each of two nodes, so one of the runs loses the
/// operation on the node that placement would pick anyway.
async fn avoid_is_kept(end: End, lost_on: &str) {
    let lost_on = WorkerId::new(lost_on);
    let mut disk = Disk::new();
    let old = disk.generation_farm();
    let (_, mut inbox) = register_as(&old, lost_on.as_str());
    submit(&old, request(1)).expect("submitted");
    let lease = one_lease(&mut inbox);
    old.place(&lost_on, NodeAction::Cordon).expect("cordoned");
    let ack = old.report(&lost_on, lost(lease)).await;
    assert!(ack.expect("an acknowledgement").accepted);
    assert!(starts(&mut inbox).is_empty());
    let avoided = old.avoided();
    assert_eq!(
        avoided.values().collect::<Vec<_>>(),
        [&BTreeSet::from([lost_on.clone()])]
    );

    let successor = take_over(&mut disk, old, end);
    let mut offered = BTreeMap::new();
    for name in ["node-a", "node-b"] {
        offered.insert(name, register_as(&successor, name).1);
    }
    successor
        .place(&lost_on, NodeAction::Uncordon)
        .expect("uncordoned");
    assert_eq!(
        successor.avoided(),
        avoided,
        "the successor skips other nodes"
    );
    successor.tick().await;
    for (name, inbox) in &mut offered {
        let granted = starts(inbox).len();
        let expected = usize::from(*name != lost_on.as_str());
        assert_eq!(
            granted, expected,
            "{name} was offered {granted} leases; the run was lost on {lost_on:?}"
        );
    }
}

/// Catches: the nodes placement skips for an operation living only in the old process,
/// so the successor runs it on the node that lost it (placement picks one of the two
/// nodes by its own order, and the test loses the run on each). Mutant: the avoid set not written.
#[tokio::test(start_paused = true)]
async fn the_nodes_to_avoid_survive_a_handover() {
    avoid_is_kept(End::Handover, "node-a").await;
    avoid_is_kept(End::Handover, "node-b").await;
}

/// Catches: the same, across a crash.
#[tokio::test(start_paused = true)]
async fn the_nodes_to_avoid_survive_a_crash() {
    avoid_is_kept(End::Crash, "node-a").await;
    avoid_is_kept(End::Crash, "node-b").await;
}

/// The Starts the node drops are given up as not started `NOT_STARTED_RERUNS` times by the
/// old process; the successor's first such give-up ends the operation.
async fn not_started_is_kept(end: End) {
    let mut disk = Disk::new();
    let old = disk.generation_farm();
    let (stream, mut inbox) = register_as(&old, "node-a");
    submit(&old, request(1)).expect("submitted");
    for seq in 1..=u64::from(NOT_STARTED_RERUNS) {
        old.tick().await;
        one_lease(&mut inbox);
        if seq == u64::from(NOT_STARTED_RERUNS) {
            // Cordoned, the node is not offered the operation the last give-up requeues.
            old.place(&node(), NodeAction::Cordon).expect("cordoned");
        }
        tokio::time::advance(START_GRACE + Duration::from_secs(1)).await;
        assert!(old.heartbeat(&node(), stream, seq, Vec::new()).await);
    }

    let successor = take_over(&mut disk, old, end);
    let (stream, mut inbox) = register_as(&successor, "node-a");
    successor
        .place(&node(), NodeAction::Uncordon)
        .expect("uncordoned");
    successor.tick().await;
    one_lease(&mut inbox);
    tokio::time::advance(START_GRACE + Duration::from_secs(1)).await;
    assert!(successor.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert_eq!(disk.rows("op/"), 0, "the operation was not finished");
    assert!(
        starts(&mut inbox).is_empty(),
        "the successor started it again: the not-started budget of {NOT_STARTED_RERUNS} restarted"
    );
}

/// Catches: the count of give-ups as not started living only in the old process, so a
/// node that drops every `Start` holds the operation for `NOT_STARTED_RERUNS` more at
/// every handover. Mutant: the not-started count not written.
#[tokio::test(start_paused = true)]
async fn the_not_started_budget_survives_a_handover() {
    not_started_is_kept(End::Handover).await;
}

/// Catches: the same, across a crash.
#[tokio::test(start_paused = true)]
async fn the_not_started_budget_survives_a_crash() {
    not_started_is_kept(End::Crash).await;
}
