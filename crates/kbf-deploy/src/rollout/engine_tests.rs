//! The rollout against the simulator ([`super::super::fleet_tests`]): each test plants
//! one fault, or none, and reads the outcome and the fleet's log.

use std::cell::RefCell;
use std::time::Duration;

use super::*;
use crate::rollout::fleet_tests::{
    Bad, Call, Event, Fault, Fleet, commit, most_storage_nodes_out, run_id,
};
use crate::rollout::inventory::Inventory;
use crate::rollout::transport::TransportError;

/// Listed out of rollout order on purpose: the plan must come from the roles.
const INVENTORY: &str = "\
m1 daemon-mac
s1 storage-and-daemon
front frontend
d1 daemon
s2 storage-and-daemon
m2 daemon-mac
s3 storage-and-daemon
";

fn setup() -> (Plan, Fleet) {
    let inventory = Inventory::parse(INVENTORY).expect("the test inventory");
    let plan = Plan::new(&inventory, run_id("run-1"), commit('b'));
    (plan, Fleet::new(&inventory, &commit('a')))
}

fn go_with(plan: &Plan, fleet: &Fleet, limits: &Limits) -> (Result<Outcome, RolloutError>, String) {
    let mut out = Vec::new();
    let result = run(plan, &mut fleet.clone(), limits, &|| false, &mut out);
    (result, String::from_utf8(out).expect("utf-8 output"))
}

fn go(plan: &Plan, fleet: &Fleet) -> Outcome {
    go_with(plan, fleet, &Limits::default())
        .0
        .expect("the rollout starts")
}

fn failure(outcome: Outcome) -> Failure {
    match outcome {
        Outcome::Failed(f) => f,
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// The hosts whose first state-changing call is a drain, in the order they got one.
fn drained_order(fleet: &Fleet) -> Vec<String> {
    fleet
        .changes()
        .into_iter()
        .filter(|(_, c)| *c == Call::Drain)
        .map(|(h, _)| h)
        .collect()
}

fn untouched(fleet: &Fleet, hosts: &[&str]) {
    for h in hosts {
        assert_eq!(fleet.calls_to(h), vec![], "{h} must not be touched");
    }
}

fn on_new(fleet: &Fleet, hosts: &[&str]) {
    for h in hosts {
        assert_eq!(fleet.current(h), commit('b'), "{h} is on the new commit");
        assert!(!fleet.is_drained(h), "{h} takes work again");
    }
}

fn on_old(fleet: &Fleet, hosts: &[&str]) {
    for h in hosts {
        assert_eq!(fleet.current(h), commit('a'), "{h} is on the old commit");
    }
}

// The order and the happy path.

/// Catches an order that follows the file, not the roles, and a host skipped or visited
/// twice: storage nodes, then the frontend, then the Linux daemon, then the Macs.
#[test]
fn a_clean_rollout_visits_each_host_once_in_role_order() {
    let (plan, fleet) = setup();
    assert_eq!(go(&plan, &fleet), Outcome::Done { hosts: 7 });
    assert_eq!(
        drained_order(&fleet),
        ["s1", "s2", "s3", "front", "d1", "m1", "m2"]
    );
    on_new(&fleet, &["s1", "s2", "s3", "front", "d1", "m1", "m2"]);
    assert_eq!(fleet.lock_holder(), None);
}

/// Catches a verb out of order or skipped: the state-changing calls on one host.
#[test]
fn a_host_goes_through_stage_drain_swap_confirm_undrain() {
    let (plan, fleet) = setup();
    go(&plan, &fleet);
    assert_eq!(
        fleet.calls_to("s1"),
        vec![
            Call::Stage(commit('b')),
            Call::Drain,
            Call::InFlight,
            Call::Swap {
                commit: commit('b'),
                in_flight: 0,
                under_replicated: 0
            },
            Call::Check(commit('b')),
            Call::Confirm,
            Call::Undrain,
        ]
    );
}

/// Catches a plan and a run that disagree: the lines an apply prints are the lines a dry
/// run prints.
#[test]
fn an_apply_announces_exactly_the_steps_the_plan_lists() {
    let (plan, fleet) = setup();
    let (result, out) = go_with(&plan, &fleet, &Limits::default());
    assert_eq!(result.expect("starts"), Outcome::Done { hosts: 7 });
    let planned: Vec<String> = plan.steps().iter().map(|s| format!("+ {s}")).collect();
    let announced: Vec<&str> = out.lines().collect();
    assert_eq!(announced, planned);
}

/// Catches a Mac visited before a Linux daemon, whatever the inventory's order.
#[test]
fn macs_are_strictly_after_the_linux_daemons() {
    let (plan, fleet) = setup();
    go(&plan, &fleet);
    let changes = fleet.changes();
    let at = |host: &str, call: Call| {
        changes
            .iter()
            .position(|(h, c)| h == host && *c == call)
            .expect("the call was made")
    };
    let last_linux_undrain = ["s1", "s2", "s3", "front", "d1"]
        .iter()
        .map(|h| at(h, Call::Undrain))
        .max()
        .expect("hosts");
    for mac in ["m1", "m2"] {
        assert!(at(mac, Call::Drain) > last_linux_undrain, "{mac}");
    }
}

/// Catches two storage nodes out at once, on a clean run and on a run that rolls one back.
#[test]
fn never_two_storage_nodes_are_drained_at_the_same_time() {
    let (plan, fleet) = setup();
    go(&plan, &fleet);
    assert_eq!(most_storage_nodes_out(&fleet.events(), &fleet.roles()), 1);

    let (plan, fleet) = setup();
    fleet.bad("s2", Bad::NeverRegisters);
    failure(go(&plan, &fleet));
    assert_eq!(most_storage_nodes_out(&fleet.events(), &fleet.roles()), 1);
}

/// Catches the assertion above passing because it cannot fail: two drains with no
/// undrain between them count as two.
#[test]
fn the_overlap_check_counts_two_drained_storage_nodes() {
    let (_, fleet) = setup();
    let ev = |host: &str, call| Event {
        host: HostName::new(host).expect("name"),
        call,
    };
    let overlap = [
        ev("s1", Call::Drain),
        ev("s2", Call::Drain),
        ev("s1", Call::Undrain),
    ];
    assert_eq!(most_storage_nodes_out(&overlap, &fleet.roles()), 2);
    // A drained frontend does not count as a storage node.
    let front = [ev("s1", Call::Drain), ev("front", Call::Drain)];
    assert_eq!(most_storage_nodes_out(&front, &fleet.roles()), 1);
}

// Health: a host that goes wrong is rolled back, and nothing after it is touched.

fn host(name: &str) -> HostName {
    HostName::new(name).expect("a test host name")
}

/// Catches a rollout that goes on past a host that never became healthy, one that
/// leaves that host on the bad binary or drained, and one that gives up before the
/// health window has passed. The three ways a binary can be bad.
#[test]
fn a_host_that_never_goes_healthy_is_rolled_back_and_the_rest_are_untouched() {
    for bad in [Bad::NeverRegisters, Bad::CrashLoops, Bad::NotReady] {
        let (plan, fleet) = setup();
        fleet.bad("s2", bad);
        let limits = Limits::default();
        let (result, _) = go_with(&plan, &fleet, &limits);
        let f = failure(result.expect("starts"));
        assert_eq!(f.host, host("s2"), "{bad:?}");
        assert_eq!(f.recovery, Recovery::RolledBack, "{bad:?}");
        on_new(&fleet, &["s1"]);
        on_old(&fleet, &["s2"]);
        assert!(!fleet.is_drained("s2"), "{bad:?}");
        untouched(&fleet, &["s3", "front", "d1", "m1", "m2"]);
        assert!(
            fleet.elapsed() >= limits.health,
            "{bad:?}: waited the whole window"
        );
        assert!(!fleet.calls_to("s2").contains(&Call::Confirm), "{bad:?}");
        assert_eq!(fleet.lock_holder(), None, "{bad:?}: the lock is released");
    }
}

/// Catches a failed health gate that is not told apart from a pass: the reason names
/// what was missing.
#[test]
fn the_failure_says_what_was_not_healthy() {
    let (plan, fleet) = setup();
    fleet.bad("s1", Bad::NeverRegisters);
    let f = failure(go(&plan, &fleet));
    assert!(
        f.why.contains("not registered active at the commit"),
        "{}",
        f.why
    );
    assert!(f.why.contains("health: not done within 120 s"), "{}", f.why);

    let (plan, fleet) = setup();
    fleet.bad("s1", Bad::CrashLoops);
    let f = failure(go(&plan, &fleet));
    assert!(f.why.contains("unit not active, not ready"), "{}", f.why);
}

/// Catches a host whose first looks are unhealthy being failed at once: it is given
/// its window, goes on when it comes up inside it, and is rolled back when it comes up
/// only after.
#[test]
fn a_host_that_comes_up_late_is_waited_for() {
    let (plan, fleet) = setup();
    fleet.slow_to_register("s1", Duration::from_secs(100));
    assert_eq!(go(&plan, &fleet), Outcome::Done { hosts: 7 });
    assert!(fleet.elapsed() >= Duration::from_secs(100));

    let (plan, fleet) = setup();
    fleet.slow_to_register("s1", Duration::from_secs(300));
    let f = failure(go(&plan, &fleet));
    assert_eq!(f.recovery, Recovery::RolledBack);
    on_old(&fleet, &["s1"]);
}

// Replication: a storage node's blobs must be back on all their copies.

/// Catches a stuck replication count not stopping the rollout: the node that left blobs
/// under-replicated is rolled back after the repair window, and the next node is never
/// touched.
#[test]
fn blobs_that_stay_under_replicated_stop_the_rollout() {
    let (plan, fleet) = setup();
    fleet.degrades_on_swap("s1", 3, None);
    let limits = Limits::default();
    let (result, _) = go_with(&plan, &fleet, &limits);
    let f = failure(result.expect("starts"));
    assert_eq!(f.host, host("s1"));
    assert_eq!(f.recovery, Recovery::RolledBack);
    assert!(
        f.why.contains("replication: not done within 300 s"),
        "{}",
        f.why
    );
    assert!(f.why.contains("3 blobs under-replicated"), "{}", f.why);
    assert!(fleet.elapsed() >= limits.repair);
    untouched(&fleet, &["s2", "s3", "front", "d1", "m1", "m2"]);
    on_old(&fleet, &["s1"]);
}

/// Catches a node swapped while the cluster still has blobs on one copy: every swap
/// of a rollout whose nodes each leave blobs to heal sees none under-replicated.
#[test]
fn a_swap_never_starts_while_blobs_are_under_replicated() {
    let (plan, fleet) = setup();
    for s in ["s1", "s2", "s3"] {
        fleet.degrades_on_swap(s, 4, Some(Duration::from_secs(60)));
    }
    assert_eq!(go(&plan, &fleet), Outcome::Done { hosts: 7 });
    let swaps: Vec<u64> = fleet
        .events()
        .into_iter()
        .filter_map(|e| match e.call {
            Call::Swap {
                under_replicated, ..
            } => Some(under_replicated),
            _ => None,
        })
        .collect();
    assert_eq!(swaps, [0; 7]);
}

/// Catches a rollout that starts on a cluster already missing copies: nothing is
/// touched, not even staged.
#[test]
fn a_cluster_that_starts_under_replicated_is_refused_untouched() {
    let (plan, fleet) = setup();
    fleet.start_degraded(2);
    let f = failure(go(&plan, &fleet));
    assert_eq!(f.host, host("s1"));
    assert_eq!(f.recovery, Recovery::Untouched);
    assert!(f.why.contains("2 blobs are under-replicated"), "{}", f.why);
    untouched(&fleet, &["s1", "s2", "s3", "front", "d1", "m1", "m2"]);
    assert_eq!(fleet.lock_holder(), None);
}

/// Catches an unanswered replication count read as zero.
#[test]
fn a_replication_count_that_cannot_be_read_stops_before_the_host() {
    let (plan, fleet) = setup();
    fleet.fault("front", Fault::UnderReplicated);
    let f = failure(go(&plan, &fleet));
    assert_eq!(f.host, host("s1"));
    assert_eq!(f.recovery, Recovery::Untouched);
    untouched(&fleet, &["s1", "s2"]);
}

// The frontend: never swapped while an Execute stream is open.

/// Catches a swap with Execute streams still open: the frontend's drain waits for them,
/// and the swap sees none.
#[test]
fn the_frontend_waits_for_open_executions_and_never_swaps_early() {
    let (plan, fleet) = setup();
    fleet.busy_until("front", Duration::from_secs(1800));
    assert_eq!(go(&plan, &fleet), Outcome::Done { hosts: 7 });
    let swap = fleet
        .calls_to("front")
        .into_iter()
        .find_map(|c| match c {
            Call::Swap { in_flight, .. } => Some(in_flight),
            _ => None,
        })
        .expect("the frontend was swapped");
    assert_eq!(swap, 0);
    assert!(fleet.elapsed() >= Duration::from_secs(1800));
}

/// Catches a frontend that is restarted under open builds when the wait runs out: it
/// is undrained as it was, the run is pending (not failed), and the hosts that were
/// done stay done.
#[test]
fn a_frontend_that_never_goes_idle_is_left_alone_and_the_run_is_pending() {
    let (plan, fleet) = setup();
    fleet.busy_forever("front");
    let limits = Limits {
        frontend_idle: Duration::from_secs(60),
        ..Limits::default()
    };
    let (result, _) = go_with(&plan, &fleet, &limits);
    match result.expect("starts") {
        Outcome::Pending { host: h, why } => {
            assert_eq!(h, host("front"));
            assert!(why.contains("3 still in flight"), "{why}");
        }
        other => panic!("expected pending, got {other:?}"),
    }
    assert!(
        !fleet
            .calls_to("front")
            .iter()
            .any(|c| matches!(c, Call::Swap { .. })),
        "never swapped"
    );
    on_old(&fleet, &["front"]);
    assert!(!fleet.is_drained("front"));
    on_new(&fleet, &["s1", "s2", "s3"]);
    untouched(&fleet, &["d1", "m1", "m2"]);
    assert_eq!(fleet.lock_holder(), None);
}

/// Catches one drain limit for every host: a Mac gets an hour for its leases, a Linux
/// daemon 15 minutes, and neither is killed when the time is up.
#[test]
fn a_mac_may_take_an_hour_to_drain_and_a_linux_daemon_a_quarter_of_one() {
    let (plan, fleet) = setup();
    fleet.busy_until("m1", Duration::from_secs(30 * 60));
    assert_eq!(go(&plan, &fleet), Outcome::Done { hosts: 7 });

    let (plan, fleet) = setup();
    fleet.busy_until("d1", Duration::from_secs(30 * 60));
    match go(&plan, &fleet) {
        Outcome::Pending { host: h, .. } => assert_eq!(h, host("d1")),
        other => panic!("expected pending, got {other:?}"),
    }
    untouched(&fleet, &["m1", "m2"]);
    on_old(&fleet, &["d1"]);

    let (plan, fleet) = setup();
    fleet.busy_forever("m1");
    match go(&plan, &fleet) {
        Outcome::Pending { host: h, .. } => assert_eq!(h, host("m1")),
        other => panic!("expected pending, got {other:?}"),
    }
    assert!(fleet.elapsed() >= Duration::from_secs(60 * 60));
    untouched(&fleet, &["m2"]);
}

// Transport errors.

/// Catches a host left on whatever a lost swap reply left it on: a swap that failed
/// after it was done, and one that failed before, are both answered with a rollback of
/// that host, and nothing after it is touched.
#[test]
fn a_transport_error_in_the_swap_rolls_that_host_back() {
    for fault in [Fault::SwapLostReply, Fault::SwapNotApplied] {
        let (plan, fleet) = setup();
        fleet.fault("s2", fault);
        let f = failure(go(&plan, &fleet));
        assert_eq!(f.host, host("s2"), "{fault:?}");
        assert_eq!(f.recovery, Recovery::RolledBack, "{fault:?}");
        assert!(f.why.contains("unreachable"), "{}", f.why);
        on_old(&fleet, &["s2"]);
        on_new(&fleet, &["s1"]);
        assert!(!fleet.is_drained("s2"));
        untouched(&fleet, &["s3", "front", "d1", "m1", "m2"]);
        let calls = fleet.calls_to("s2");
        let swap = calls
            .iter()
            .position(|c| matches!(c, Call::Swap { .. }))
            .expect("swap attempted");
        assert_eq!(
            calls[swap + 1],
            Call::Rollback,
            "{fault:?}: rolled back at once"
        );
        assert_eq!(calls[swap + 2], Call::Undrain, "{fault:?}");
    }
}

/// Catches an unreachable host being waited on forever, or read as healthy: a lost
/// connection in the health wait is a failed gate.
#[test]
fn a_connection_lost_in_the_health_wait_rolls_the_host_back() {
    let (plan, fleet) = setup();
    fleet.fault("s1", Fault::Check);
    let f = failure(go(&plan, &fleet));
    assert_eq!(f.recovery, Recovery::RolledBack);
    on_old(&fleet, &["s1"]);
    untouched(&fleet, &["s2"]);
}

/// Catches a confirm that did not arrive being taken as success: the host may revert
/// by itself, so the orchestrator makes it so.
#[test]
fn a_confirm_that_fails_rolls_the_host_back() {
    let (plan, fleet) = setup();
    fleet.fault("s1", Fault::Confirm);
    let f = failure(go(&plan, &fleet));
    assert_eq!(f.recovery, Recovery::RolledBack);
    on_old(&fleet, &["s1"]);
    untouched(&fleet, &["s2"]);
}

/// Catches a failed rollback being reported as a rollback: the host stays drained,
/// is not undrained, and the run stops.
#[test]
fn a_rollback_that_fails_leaves_the_host_drained_and_stops() {
    let (plan, fleet) = setup();
    fleet
        .bad("s1", Bad::CrashLoops)
        .fault("s1", Fault::Rollback);
    let f = failure(go(&plan, &fleet));
    match &f.recovery {
        Recovery::Cordoned(why) => assert!(why.contains("rollback failed"), "{why}"),
        other => panic!("expected cordoned, got {other:?}"),
    }
    assert!(fleet.is_drained("s1"));
    assert_eq!(fleet.calls_to("s1").last(), Some(&Call::Rollback));
    untouched(&fleet, &["s2", "s3", "front", "d1", "m1", "m2"]);
    assert_eq!(fleet.lock_holder(), None);
}

/// Catches a host rolled back but never returned to service being counted as recovered.
#[test]
fn a_rolled_back_host_that_cannot_be_undrained_stays_cordoned() {
    let (plan, fleet) = setup();
    fleet.bad("s1", Bad::CrashLoops).fault("s1", Fault::Undrain);
    let f = failure(go(&plan, &fleet));
    match &f.recovery {
        Recovery::Cordoned(why) => assert!(why.contains("rolled back, but not undrained"), "{why}"),
        other => panic!("expected cordoned, got {other:?}"),
    }
    on_old(&fleet, &["s1"]);
    assert!(fleet.is_drained("s1"));
}

/// Catches a host that stays drained after a good upgrade being passed over: the run
/// stops, the host keeps the new commit, and a person is told.
#[test]
fn a_host_that_cannot_be_undrained_after_a_good_swap_stops_the_run() {
    let (plan, fleet) = setup();
    fleet.fault("s1", Fault::Undrain);
    let f = failure(go(&plan, &fleet));
    assert!(matches!(f.recovery, Recovery::Cordoned(_)));
    assert_eq!(fleet.current("s1"), commit('b'));
    assert!(fleet.is_drained("s1"));
    untouched(&fleet, &["s2", "s3"]);
}

// Before the swap.

/// Catches a host that fails to stage being drained anyway.
#[test]
fn a_host_that_cannot_stage_is_never_drained() {
    let (plan, fleet) = setup();
    fleet.fault("s1", Fault::Stage);
    let f = failure(go(&plan, &fleet));
    assert_eq!(f.recovery, Recovery::Untouched);
    assert_eq!(fleet.calls_to("s1"), vec![Call::Stage(commit('b'))]);
    untouched(&fleet, &["s2"]);
}

/// Catches a drain that fails and leaves the host half-drained: it is undrained.
#[test]
fn a_drain_that_fails_is_undone() {
    let (plan, fleet) = setup();
    fleet.fault("s1", Fault::Drain);
    let f = failure(go(&plan, &fleet));
    assert_eq!(f.recovery, Recovery::Undrained);
    assert_eq!(
        fleet.calls_to("s1"),
        vec![Call::Stage(commit('b')), Call::Drain, Call::Undrain]
    );
    untouched(&fleet, &["s2"]);
}

/// Catches a drain wait that loses its host: a lost connection while waiting for the
/// work in flight undrains the host and fails the run (not pending).
#[test]
fn a_connection_lost_while_waiting_to_drain_undrains_and_fails() {
    let (plan, fleet) = setup();
    fleet.fault("s1", Fault::InFlight);
    let f = failure(go(&plan, &fleet));
    assert_eq!(f.recovery, Recovery::Undrained);
    assert!(!fleet.is_drained("s1"));
    assert!(
        !fleet
            .calls_to("s1")
            .iter()
            .any(|c| matches!(c, Call::Swap { .. }))
    );
}

/// Catches a host that cannot be undrained after a drain that did not finish being
/// reported as fine, whether the drain failed or only timed out.
#[test]
fn a_host_that_cannot_be_undrained_after_a_failed_drain_is_cordoned() {
    let (plan, fleet) = setup();
    fleet.fault("s1", Fault::Drain).fault("s1", Fault::Undrain);
    let f = failure(go(&plan, &fleet));
    assert!(matches!(f.recovery, Recovery::Cordoned(_)));

    let (plan, fleet) = setup();
    fleet.busy_forever("s1").fault("s1", Fault::Undrain);
    let f = failure(go(&plan, &fleet));
    assert!(matches!(f.recovery, Recovery::Cordoned(_)));
    assert!(
        f.why.contains("work in flight: not done within 900 s"),
        "{}",
        f.why
    );
    assert!(fleet.is_drained("s1"));
    untouched(&fleet, &["s2"]);
}

/// Catches a state file never backed up, or backed up after a host was touched.
#[test]
fn the_state_store_is_backed_up_first() {
    let (plan, fleet) = setup();
    go(&plan, &fleet);
    let changes = fleet.changes();
    assert_eq!(changes[0].1, Call::Lock(run_id("run-1")));
    assert_eq!(changes[1], ("front".to_owned(), Call::Backup));
    assert_eq!(changes[2].1, Call::Stage(commit('b')));
}

/// Catches a rollout that goes on without its backup.
#[test]
fn a_backup_that_fails_stops_before_any_host() {
    let (plan, fleet) = setup();
    fleet.fault("front", Fault::Backup);
    let f = failure(go(&plan, &fleet));
    assert_eq!(f.host, host("front"));
    assert_eq!(f.recovery, Recovery::Untouched);
    untouched(&fleet, &["s1", "s2", "s3", "d1", "m1", "m2"]);
    assert_eq!(fleet.lock_holder(), None);
}

// The lock.

/// Catches a second rollout that does not notice the first: it is refused, with the
/// name of the holder, and does nothing at all to the fleet.
#[test]
fn a_rollout_is_refused_while_another_holds_the_lock() {
    let (plan, fleet) = setup();
    fleet.hold_lock("other-run");
    let (result, _) = go_with(&plan, &fleet, &Limits::default());
    match result {
        Err(RolloutError::Locked { by }) => assert_eq!(by, run_id("other-run")),
        other => panic!("expected the lock to refuse, got {other:?}"),
    }
    assert_eq!(fleet.changes().len(), 1, "only the lock call was made");
    assert_eq!(
        fleet.lock_holder(),
        Some(run_id("other-run")),
        "the holder keeps it"
    );
}

/// Catches two rollouts really running at once: the second starts in the middle of the
/// first (between its hosts) and is refused, while the first goes on and finishes.
#[test]
fn a_second_rollout_started_in_the_middle_of_the_first_is_refused() {
    let (plan, fleet) = setup();
    let inventory = Inventory::parse(INVENTORY).expect("inventory");
    let second = Plan::new(&inventory, run_id("run-2"), commit('c'));
    let result: RefCell<Option<Result<Outcome, RolloutError>>> = RefCell::new(None);
    let paused = || {
        let first_done = fleet.calls_to("s1").contains(&Call::Undrain);
        if first_done && result.borrow().is_none() {
            let mut out = Vec::new();
            let r = run(
                &second,
                &mut fleet.clone(),
                &Limits::default(),
                &|| false,
                &mut out,
            );
            *result.borrow_mut() = Some(r);
        }
        false
    };
    let mut out = Vec::new();
    let first = run(
        &plan,
        &mut fleet.clone(),
        &Limits::default(),
        &paused,
        &mut out,
    );
    assert_eq!(first.expect("starts"), Outcome::Done { hosts: 7 });
    match result.into_inner() {
        Some(Err(RolloutError::Locked { by })) => assert_eq!(by, run_id("run-1")),
        other => panic!("the second rollout must be refused, got {other:?}"),
    }
    // The second left no mark: every host is on the first rollout's commit.
    on_new(&fleet, &["s1", "s2", "s3", "front", "d1", "m1", "m2"]);
    assert_eq!(fleet.lock_holder(), None);
}

/// Catches a lock that is not released after a failure, and one that is released by
/// someone else's run.
#[test]
fn the_lock_is_released_after_a_failure_and_a_later_rollout_can_run() {
    let (plan, fleet) = setup();
    fleet.fault("s1", Fault::Stage);
    failure(go(&plan, &fleet));
    assert_eq!(fleet.lock_holder(), None);
}

/// Catches a lock that cannot be taken being treated as taken.
#[test]
fn a_lock_call_that_fails_refuses_the_rollout() {
    let (plan, fleet) = setup();
    fleet.fault("front", Fault::Lock);
    let (result, _) = go_with(&plan, &fleet, &Limits::default());
    assert!(matches!(result, Err(RolloutError::Lock(_))));
    assert_eq!(fleet.changes().len(), 1);
}

/// Catches an unlock that fails being hidden: the run's result stands and a line says
/// the lock is still held.
#[test]
fn a_lock_that_cannot_be_released_is_reported() {
    let (plan, fleet) = setup();
    fleet.fault("front", Fault::Unlock);
    let (result, out) = go_with(&plan, &fleet, &Limits::default());
    assert_eq!(result.expect("starts"), Outcome::Done { hosts: 7 });
    assert!(out.contains("! the lock was not released"), "{out}");
}

// The pause control.

/// Catches a paused rollout that still takes the lock or touches a host.
#[test]
fn a_rollout_paused_from_the_start_touches_nothing() {
    let (plan, fleet) = setup();
    let mut out = Vec::new();
    let result = run(
        &plan,
        &mut fleet.clone(),
        &Limits::default(),
        &|| true,
        &mut out,
    );
    assert_eq!(result.expect("starts"), Outcome::Paused { done: 0 });
    assert_eq!(fleet.events(), vec![]);
    assert!(out.is_empty());
}

/// Catches a pause that is not read between hosts, and one that stops in the middle of
/// a host: s1 is finished and undrained, nothing after it is touched.
#[test]
fn a_pause_set_during_the_run_stops_between_hosts() {
    let (plan, fleet) = setup();
    let paused = || fleet.calls_to("s1").contains(&Call::Undrain);
    let mut out = Vec::new();
    let result = run(
        &plan,
        &mut fleet.clone(),
        &Limits::default(),
        &paused,
        &mut out,
    );
    assert_eq!(result.expect("starts"), Outcome::Paused { done: 1 });
    on_new(&fleet, &["s1"]);
    untouched(&fleet, &["s2", "s3", "front", "d1", "m1", "m2"]);
    assert_eq!(fleet.lock_holder(), None);
}

// Texts.

/// Catches a message that lost its words: each ending's text, as the command line
/// prints it.
#[test]
fn the_errors_and_recoveries_read_as_sentences() {
    let host = host("s1");
    let run = run_id("r1");
    assert_eq!(
        RolloutError::Locked { by: run.clone() }.to_string(),
        "the fleet is locked by run r1"
    );
    let lost = TransportError::Unreachable {
        host: host.clone(),
        why: "gone".to_owned(),
    };
    assert_eq!(
        RolloutError::Lock(lost).to_string(),
        "cannot take the fleet lock: s1: unreachable: gone"
    );
    let verb = TransportError::Verb {
        host,
        verb: "stage",
        why: "bad sum".to_owned(),
    };
    assert_eq!(verb.to_string(), "s1: stage failed: bad sum");
    assert_eq!(Recovery::Untouched.to_string(), "the host was not changed");
    assert_eq!(
        Recovery::Undrained.to_string(),
        "the host was undrained, not swapped"
    );
    assert_eq!(
        Recovery::RolledBack.to_string(),
        "the host was rolled back and undrained"
    );
    assert_eq!(
        Recovery::Cordoned("x".to_owned()).to_string(),
        "the host stays drained: x"
    );
}

/// Catches a verb failure that is not a failure: a host verb that ran and failed fails
/// the step like a lost connection does.
#[test]
fn a_verb_failure_fails_the_step_it_names() {
    // The simulator only plants lost connections; a Verb error is the real transport's
    // answer to a refused verb, and arrives through the same `?`.
    let why = Why::from(TransportError::Verb {
        host: host("s1"),
        verb: "swap",
        why: "refused".to_owned(),
    });
    assert_eq!(why.to_string(), "s1: swap failed: refused");
}
