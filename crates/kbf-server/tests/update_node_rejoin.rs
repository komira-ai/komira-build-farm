//! The node step, steps 6 to 10 (`docs/design/split-mode-updates.md` section 2.6, #898):
//! activate, rejoin storage, repaired, confirm, rejoin work; the cordon through a hold; the
//! phases that do not move by themselves; and the walk through all ten steps, forward and
//! back. Steps 1 to 5 are in `update_node_step.rs`, the holds, the capacity floor and the
//! Macs in `update_node_hold.rs`.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::time::Duration;

use kbf_map::{MemberId, MemberState};
use kbf_server::members::MembershipConfig;
use kbf_server::update::{
    Alert, Command, Facts, HelperStatus, HoldReason, HostPhase, HostRecord, OrderId, Policy,
    PolicyError, Release, Severity, SlotOp, Transition, Work,
};
use support::{
    BOOT, NEW, NEXT_ORDER, NOW, OLD, ORDER, SERIAL, activate, confirm, facts, host_mut, id, linux,
    linux_fleet, mac, member_mut, members, policy, record_move, report, report_of, run, run_row,
    step_for, storage, storage_fleet, t, waiting_in,
};

const MIN: u64 = 60;

// Step 6: activate.

fn activating_fleet() -> Facts {
    waiting_in(&HostPhase::Activating)
}

/// Catches: leaving the activate step on the order alone (the helper may be refusing it),
/// or never leaving it when the helper reports the flip.
#[test]
fn the_helpers_report_moves_a_storage_host_on_to_rejoin() {
    let mut f = activating_fleet();
    let waiting = run(&f, "s1", &HostPhase::Activating, 5);
    assert_eq!(waiting.next, HostPhase::Activating);
    assert_eq!(waiting.commands, Vec::new());

    host_mut(&mut f, "s1").helper = report(HelperStatus::ActivatedUnconfirmed);
    let moved = run(&f, "s1", &HostPhase::Activating, 5);
    assert_eq!(moved.next, HostPhase::RejoiningStorage);
    assert_eq!(moved.commands, Vec::new());
    // No new order: the activation order of the row stands for the whole update.
    assert_eq!(moved.order, None);
}

/// Catches: a helper that reverted or refused being waited on for ever. "The helper
/// reverts itself": the controller issues no revert, no uncordon, and holds.
#[test]
fn a_reverted_or_refused_helper_holds_the_run_and_touches_nothing() {
    for (status, reason) in [
        (HelperStatus::Reverted, HoldReason::Reverted),
        (HelperStatus::Refused, HoldReason::RevertRefused),
    ] {
        let mut f = activating_fleet();
        host_mut(&mut f, "s1").helper = report(status);
        let moved = run(&f, "s1", &HostPhase::Activating, 5);
        assert_eq!(moved.next, HostPhase::Held { reason });
        assert_eq!(moved.commands, vec![Command::Alert(Alert::Held(reason))]);
    }
    assert_eq!(HoldReason::RevertRefused.severity(), Severity::Page);
}

/// Catches: a status left over from an earlier update read as the answer to this one. A
/// host is updated many times: a `CONFIRMED` of the last release would carry a fresh host
/// through the activate and the confirm without the helper having done anything, and a
/// `REVERTED` of the last would hold a healthy run. A status counts only if it names the
/// row's own activation order; an older id and an id the row does not know are both no
/// report, so the phase waits as if nothing had come.
#[test]
fn a_status_that_names_another_order_is_no_report() {
    let phases = [
        HostPhase::Activating,
        HostPhase::RejoiningStorage,
        HostPhase::Repairing,
        HostPhase::Confirming,
    ];
    let statuses = [
        HelperStatus::ActivatedUnconfirmed,
        HelperStatus::Confirmed,
        HelperStatus::Reverted,
        HelperStatus::Refused,
    ];
    for order in [OrderId(ORDER.0 - 1), OrderId(0), OrderId(ORDER.0 + 1)] {
        for status in statuses {
            for phase in &phases {
                let mut f = waiting_in(phase);
                host_mut(&mut f, "s1").helper = report_of(order, status);
                let moved = run(&f, "s1", phase, 10);
                let why = format!("{phase:?} {status:?} of order {}", order.0);
                assert_eq!(moved.next, *phase, "{why}");
                assert_eq!(moved.commands, Vec::new(), "{why}");
            }
        }
    }
    // A daemon-only host back at the target does not confirm on a stale flip either.
    let mut f = linux_fleet();
    host_mut(&mut f, "d1").release = Some(Release::new(NEW));
    host_mut(&mut f, "d1").helper = report_of(OrderId(ORDER.0 - 1), HelperStatus::Confirmed);
    let moved = run(&f, "d1", &HostPhase::Activating, 10);
    assert_eq!(moved.next, HostPhase::Activating);
    // ... and the same status naming the row's order does.
    host_mut(&mut f, "d1").helper = report(HelperStatus::ActivatedUnconfirmed);
    let moved = run(&f, "d1", &HostPhase::Activating, 10);
    assert_eq!(moved.next, HostPhase::Confirming);
}

/// Catches: a helper that never reports hanging the run: the activate limit holds it.
#[test]
fn a_silent_helper_holds_the_run_at_the_activate_limit() {
    let f = activating_fleet();
    let before = run(&f, "s1", &HostPhase::Activating, 5 * MIN - 1);
    assert_eq!(before.next, HostPhase::Activating);
    let at = run(&f, "s1", &HostPhase::Activating, 5 * MIN);
    let reason = HoldReason::ActivateStalled;
    assert_eq!(at.next, HostPhase::Held { reason });
}

/// Catches: waiting for the dwell to decide. The restart deadline is the dwell less two
/// time-to-lives (300 - 30 = 270 s) from the mark; the run holds, with a page, before the
/// dwell drops the member and starts re-replication.
#[test]
fn a_node_not_back_by_dwell_minus_two_ttl_holds_the_run() {
    let mut f = activating_fleet();
    member_mut(&mut f, 1).down_since = Some(t(NOW - 269));
    let before = run(&f, "s1", &HostPhase::Activating, 60);
    assert_eq!(before.next, HostPhase::Activating);

    member_mut(&mut f, 1).down_since = Some(t(NOW - 270));
    let at = run(&f, "s1", &HostPhase::Activating, 60);
    let reason = HoldReason::RestartDeadline;
    assert_eq!(at.next, HostPhase::Held { reason });
    assert_eq!(at.commands, vec![Command::Alert(Alert::Held(reason))]);
    assert_eq!(reason.severity(), Severity::Page);
}

/// Catches: a daemon-only host confirmed before its daemon is back at the target and
/// connected (section 2.10: "register at the commit, confirm"). It stands in for step 7.
#[test]
fn a_daemon_only_host_is_confirmed_only_once_its_daemon_is_back_at_the_target() {
    let mut f = linux_fleet();
    host_mut(&mut f, "d1").helper = report(HelperStatus::ActivatedUnconfirmed);
    let old = run(&f, "d1", &HostPhase::Activating, 30);
    assert_eq!(old.next, HostPhase::Activating);

    host_mut(&mut f, "d1").release = Some(Release::new(NEW));
    host_mut(&mut f, "d1").connected = false;
    let away = run(&f, "d1", &HostPhase::Activating, 30);
    assert_eq!(away.next, HostPhase::Activating);

    host_mut(&mut f, "d1").connected = true;
    let back = run(&f, "d1", &HostPhase::Activating, 30);
    assert_eq!(back.next, HostPhase::Confirming);
    // The confirm names the activation of the row.
    assert_eq!(back.commands, vec![confirm()]);
}

// Step 7: rejoin storage.

fn rejoining_fleet() -> Facts {
    waiting_in(&HostPhase::RejoiningStorage)
}

/// The world after the new process registered: the mark deleted, the boot counter larger.
fn registered(f: &mut Facts) {
    member_mut(f, 1).planned_down = false;
    member_mut(f, 1).down_since = None;
    member_mut(f, 1).boot = Some(BOOT + 1);
}

/// Catches: leaving step 7 on anything but the new process's own registration. The old
/// process's `Active` row (the mark still there), the old process again after the mark was
/// cleared some other way (an undo, an operator, a lapse: the counter has not moved), a
/// counter that is not reported, and a row that never recorded the counter before the mark
/// must all wait. Only the mark gone and a larger counter is the new process; the step
/// would otherwise treat a node as back while the old binary still runs.
#[test]
fn only_a_larger_boot_counter_with_the_mark_gone_is_the_new_process() {
    let mut f = rejoining_fleet();
    let old = run(&f, "s1", &HostPhase::RejoiningStorage, 10);
    assert_eq!(old.next, HostPhase::RejoiningStorage);

    // The mark cleared by something else; the same process is still running.
    member_mut(&mut f, 1).planned_down = false;
    member_mut(&mut f, 1).down_since = None;
    let cleared = run(&f, "s1", &HostPhase::RejoiningStorage, 10);
    assert_eq!(cleared.next, HostPhase::RejoiningStorage);
    assert_eq!(cleared.commands, Vec::new());

    // The counter is not reported.
    member_mut(&mut f, 1).boot = None;
    let unreported = run(&f, "s1", &HostPhase::RejoiningStorage, 10);
    assert_eq!(unreported.next, HostPhase::RejoiningStorage);

    // The counter is larger but the mark is still there (the map lags the registration).
    member_mut(&mut f, 1).boot = Some(BOOT + 1);
    member_mut(&mut f, 1).planned_down = true;
    let lagging = run(&f, "s1", &HostPhase::RejoiningStorage, 10);
    assert_eq!(lagging.next, HostPhase::RejoiningStorage);

    // The row never recorded the counter: nothing can show the restart.
    registered(&mut f);
    host_mut(&mut f, "s1").record.boot_before = None;
    let unrecorded = run(&f, "s1", &HostPhase::RejoiningStorage, 10);
    assert_eq!(unrecorded.next, HostPhase::RejoiningStorage);

    host_mut(&mut f, "s1").record.boot_before = Some(BOOT);
    let moved = run(&f, "s1", &HostPhase::RejoiningStorage, 10);
    assert_eq!(moved.next, HostPhase::Repairing);
    assert_eq!(moved.commands, Vec::new());
}

/// Catches: an intact node that stays away being waited on for ever: not `Active` within
/// the health limit (120 s) holds the run; the helper reverts it by its own timer.
#[test]
fn an_intact_node_not_active_in_the_health_limit_holds_the_run() {
    let mut f = rejoining_fleet();
    member_mut(&mut f, 1).state = MemberState::Down;
    member_mut(&mut f, 1).down_since = Some(t(NOW - 100));
    let before = run(&f, "s1", &HostPhase::RejoiningStorage, 119);
    assert_eq!(before.next, HostPhase::RejoiningStorage);
    let at = run(&f, "s1", &HostPhase::RejoiningStorage, 120);
    let reason = HoldReason::HealthLimit;
    assert_eq!(at.next, HostPhase::Held { reason });
    assert_eq!(at.commands, vec![Command::Alert(Alert::Held(reason))]);
}

/// Catches: the restart deadline checked only while activating. A node that has not
/// registered by dwell minus two time-to-lives (270 s from the mark) must hold the run,
/// with a page, from step 7 too, before the health limit has run out: the dwell would drop
/// the member and start re-replication.
#[test]
fn the_restart_deadline_holds_a_node_that_is_still_down_while_it_rejoins() {
    let mut f = rejoining_fleet();
    member_mut(&mut f, 1).state = MemberState::Down;
    member_mut(&mut f, 1).down_since = Some(t(NOW - 269));
    let before = run(&f, "s1", &HostPhase::RejoiningStorage, 30);
    assert_eq!(before.next, HostPhase::RejoiningStorage);

    member_mut(&mut f, 1).down_since = Some(t(NOW - 270));
    let at = run(&f, "s1", &HostPhase::RejoiningStorage, 30);
    let reason = HoldReason::RestartDeadline;
    assert_eq!(at.next, HostPhase::Held { reason });
    assert_eq!(at.commands, vec![Command::Alert(Alert::Held(reason))]);
}

/// Catches: the health limit applied to a rebuilding node ("Rebuilding node: wait while it
/// makes progress, never revert"); and, when the policy names a rebuild limit, no limit.
/// A rebuild that outlasts the health limit alerts, level-triggered, so a stuck rebuild
/// does not wait in silence; it does not before, and a node that has not restarted is not
/// a rebuilding one.
#[test]
fn a_rebuilding_node_waits_past_the_health_limit_and_holds_only_at_a_rebuild_limit() {
    let mut f = rejoining_fleet();
    registered(&mut f);
    member_mut(&mut f, 1).state = MemberState::CatchingUp;
    let phase = HostPhase::RejoiningStorage;
    let early = run(&f, "s1", &phase, 119);
    assert_eq!(early.next, phase);
    assert_eq!(early.commands, Vec::new());

    let alert = vec![Command::Alert(Alert::RebuildWaiting)];
    for waited in [120, 10 * 60 * MIN] {
        let long = run(&f, "s1", &phase, waited);
        assert_eq!(long.next, phase, "{waited}");
        assert_eq!(long.commands, alert, "{waited}");
    }
    assert_eq!(Alert::RebuildWaiting.severity(), Severity::Warn);

    let limited = Policy {
        rebuild_limit: Some(Duration::from_secs(30 * MIN)),
        ..policy()
    };
    let before = step_for(&f, "s1", &phase, 30 * MIN - 1, &limited);
    assert_eq!(before.next, phase);
    let at = step_for(&f, "s1", &phase, 30 * MIN, &limited);
    let reason = HoldReason::RebuildLimit;
    assert_eq!(at.next, HostPhase::Held { reason });

    // `CatchingUp` with the old counter is not the new process rebuilding: the old one is
    // still up, and the health limit holds the run.
    member_mut(&mut f, 1).boot = Some(BOOT);
    let old = run(&f, "s1", &phase, 120);
    let reason = HoldReason::HealthLimit;
    assert_eq!(old.next, HostPhase::Held { reason });
}

/// Catches: a member that vanished from the map (the dwell dropped it) treated as one
/// still to come.
#[test]
fn a_member_gone_from_the_map_holds_the_run() {
    let mut f = rejoining_fleet();
    f.members.retain(|m| m.id != MemberId(1));
    let moved = run(&f, "s1", &HostPhase::RejoiningStorage, 10);
    let reason = HoldReason::MemberGone;
    assert_eq!(moved.next, HostPhase::Held { reason });
    assert_eq!(reason.severity(), Severity::Page);
}

// Step 8: repaired.

fn repairing_fleet() -> Facts {
    let mut f = rejoining_fleet();
    registered(&mut f);
    host_mut(&mut f, "s1").release = Some(Release::new(NEW));
    f
}

/// Catches: confirming a node before the debts of its downtime are settled, or never
/// confirming once they are.
#[test]
fn settled_debts_and_a_daemon_back_at_the_target_confirm() {
    let f = repairing_fleet();
    let moved = run(&f, "s1", &HostPhase::Repairing, 20);
    assert_eq!(moved.next, HostPhase::Confirming);
    assert_eq!(moved.commands, vec![confirm()]);
    assert_eq!(moved.order, None);
}

/// Catches: G_S with the node in missing a clause: a debt on any member, an unknown debt,
/// a member still rebuilding, joining or draining, a member not heard, or the node's own
/// member not `Active`.
#[test]
fn an_open_or_unknown_debt_a_rebuild_or_an_unsettled_node_keep_the_host_repairing() {
    let mut peer_debt = repairing_fleet();
    member_mut(&mut peer_debt, 2).repair_debt = Some(1);
    let mut unknown = repairing_fleet();
    member_mut(&mut unknown, 3).repair_debt = None;
    let mut own_debt = repairing_fleet();
    member_mut(&mut own_debt, 1).repair_debt = Some(5);
    let mut own_unknown = repairing_fleet();
    member_mut(&mut own_unknown, 1).repair_debt = None;
    let mut own_catching = repairing_fleet();
    member_mut(&mut own_catching, 1).state = MemberState::CatchingUp;
    let mut stale = repairing_fleet();
    member_mut(&mut stale, 2).heard_at = Some(t(NOW - 16));
    let mut joining = repairing_fleet();
    member_mut(&mut joining, 2).state = MemberState::Joining;
    let mut draining = repairing_fleet();
    member_mut(&mut draining, 3).state = MemberState::Draining;
    let mut peer_catching = repairing_fleet();
    member_mut(&mut peer_catching, 3).state = MemberState::CatchingUp;
    for f in [
        peer_debt,
        unknown,
        own_debt,
        own_unknown,
        own_catching,
        stale,
        joining,
        draining,
        peer_catching,
    ] {
        let moved = run(&f, "s1", &HostPhase::Repairing, 20);
        assert_eq!(moved.next, HostPhase::Repairing);
        assert_eq!(moved.commands, Vec::new());
    }
}

/// Catches: the repair wait (5 minutes) leaving a healthy binary to the helper's revert
/// timer. The binary is healthy, so it is confirmed (or the timer would revert it), the
/// node stays cordoned and the run holds. Never an uncordon, never an undo. The confirm
/// names the row's activation order.
#[test]
fn debts_that_do_not_settle_hold_the_run_with_the_healthy_binary_confirmed() {
    let mut f = repairing_fleet();
    member_mut(&mut f, 2).repair_debt = Some(12);
    let before = run(&f, "s1", &HostPhase::Repairing, 5 * MIN - 1);
    assert_eq!(before.next, HostPhase::Repairing);
    let at = run(&f, "s1", &HostPhase::Repairing, 5 * MIN);
    let reason = HoldReason::RepairWait;
    assert_eq!(at.next, HostPhase::Held { reason });
    let want = vec![confirm(), Command::Alert(Alert::Held(reason))];
    assert_eq!(at.commands, want);
}

/// Catches: confirming a binary whose daemon half did not come back at the target: at the
/// repair wait it is held unconfirmed, and the helper reverts it.
#[test]
fn a_daemon_half_that_is_not_back_at_the_repair_wait_is_not_confirmed() {
    let mut f = repairing_fleet();
    host_mut(&mut f, "s1").release = Some(Release::new(OLD));
    let at = run(&f, "s1", &HostPhase::Repairing, 5 * MIN);
    let reason = HoldReason::HealthLimit;
    assert_eq!(at.next, HostPhase::Held { reason });
    assert_eq!(at.commands, vec![Command::Alert(Alert::Held(reason))]);
}

// Step 9: confirm.

/// Catches: the uncordon before `CONFIRMED`, and no uncordon after it.
#[test]
fn confirmed_uncordons_and_not_before() {
    let mut f = repairing_fleet();
    host_mut(&mut f, "s1").helper = report(HelperStatus::ActivatedUnconfirmed);
    let waiting = run(&f, "s1", &HostPhase::Confirming, 10);
    assert_eq!(waiting.next, HostPhase::Confirming);
    assert_eq!(waiting.commands, Vec::new());

    host_mut(&mut f, "s1").helper = report(HelperStatus::Confirmed);
    let moved = run(&f, "s1", &HostPhase::Confirming, 10);
    assert_eq!(moved.next, HostPhase::RejoiningWork);
    assert_eq!(moved.commands, vec![Command::Uncordon]);
}

/// Catches: "Confirm lost: the timer fires; node returns to N; Held": a reverted helper
/// walked past, and a confirm that never comes waited on for ever.
#[test]
fn a_lost_confirm_holds_the_run() {
    let mut f = repairing_fleet();
    host_mut(&mut f, "s1").helper = report(HelperStatus::Reverted);
    let moved = run(&f, "s1", &HostPhase::Confirming, 10);
    let reverted = HoldReason::Reverted;
    assert_eq!(moved.next, HostPhase::Held { reason: reverted });

    host_mut(&mut f, "s1").helper = report(HelperStatus::ActivatedUnconfirmed);
    let stalled = run(&f, "s1", &HostPhase::Confirming, 120);
    let reason = HoldReason::ConfirmStalled;
    assert_eq!(stalled.next, HostPhase::Held { reason });
}

// Step 10: rejoin work.

/// Catches: finishing before the node is given work or the queue shown empty; an unknown
/// answer read as finished. A first lease proves the uncordon whatever placement still
/// shows.
#[test]
fn the_first_lease_finishes_and_frees_the_slot() {
    for cordoned in [Some(false), None, Some(true)] {
        let mut f = storage_fleet();
        host_mut(&mut f, "s1").cordoned = cordoned;
        host_mut(&mut f, "s1").work = Some(Work::Leased);
        let moved = run(&f, "s1", &HostPhase::RejoiningWork, 3);
        assert_eq!(moved.next, HostPhase::Done, "{cordoned:?}");
        assert_eq!(moved.commands, vec![Command::CasSlot(SlotOp::Release)]);
    }
    for work in [Some(Work::Waiting), None] {
        let mut f = storage_fleet();
        host_mut(&mut f, "s1").cordoned = Some(false);
        host_mut(&mut f, "s1").work = work;
        let moved = run(&f, "s1", &HostPhase::RejoiningWork, 3);
        assert_eq!(moved.next, HostPhase::RejoiningWork, "{work:?}");
        assert_eq!(moved.commands, Vec::new());
    }
}

/// Catches: "one heartbeat with nothing waiting for it" finishing a node that is still
/// cordoned. An empty queue does not prove the uncordon took effect (the queue is empty
/// for a cordoned node too), so the slot is freed only with the cordon shown gone; with it
/// shown still there the uncordon is ordered again, and with it unknown the step waits.
#[test]
fn an_empty_queue_finishes_a_node_only_once_the_cordon_is_shown_gone() {
    let phase = HostPhase::RejoiningWork;
    let mut f = storage_fleet();
    host_mut(&mut f, "s1").work = Some(Work::QueueEmpty);

    host_mut(&mut f, "s1").cordoned = Some(false);
    let done = run(&f, "s1", &phase, 3);
    assert_eq!(done.next, HostPhase::Done);
    assert_eq!(done.commands, vec![Command::CasSlot(SlotOp::Release)]);

    host_mut(&mut f, "s1").cordoned = Some(true);
    let cordoned = run(&f, "s1", &phase, 3);
    assert_eq!(cordoned.next, phase);
    assert_eq!(cordoned.commands, vec![Command::Uncordon]);

    host_mut(&mut f, "s1").cordoned = None;
    let unknown = run(&f, "s1", &phase, 3);
    assert_eq!(unknown.next, phase);
    assert_eq!(unknown.commands, Vec::new());
}

/// Catches: "node stays cordoned, alert": an uncordon that does not show is ordered again
/// until the health limit, then the run holds with the node as it is.
#[test]
fn a_node_that_stays_cordoned_is_uncordoned_again_and_then_held() {
    let f = storage_fleet();
    let again = run(&f, "s1", &HostPhase::RejoiningWork, 30);
    assert_eq!(again.next, HostPhase::RejoiningWork);
    assert_eq!(again.commands, vec![Command::Uncordon]);

    let held = run(&f, "s1", &HostPhase::RejoiningWork, 120);
    let reason = HoldReason::StillCordoned;
    assert_eq!(held.next, HostPhase::Held { reason });
    assert_eq!(held.commands, vec![Command::Alert(Alert::Held(reason))]);
}

// The cordon from step 2 to step 9, and through a hold.

/// Catches: a cordon lost after the activate order (a frontend that restarted before
/// cordons are durable, an operator) left lost: the node would take leases while it
/// restarts, rejoins, repairs or waits for its confirm. In each of steps 6 to 9 placement
/// showing no cordon orders it again, first and alone, in the same phase; an unknown
/// placement orders nothing; a shown cordon orders nothing. A hold on the same look keeps
/// the order ahead of its alert.
#[test]
fn a_cordon_that_stops_showing_is_ordered_again_from_the_activate_order_to_the_confirm() {
    let steps = [
        HostPhase::Activating,
        HostPhase::RejoiningStorage,
        HostPhase::Repairing,
        HostPhase::Confirming,
    ];
    for phase in steps {
        let mut f = waiting_in(&phase);
        host_mut(&mut f, "s1").cordoned = Some(false);
        let lost = run(&f, "s1", &phase, 10);
        assert_eq!(lost.next, phase, "{phase:?}");
        assert_eq!(lost.commands, vec![Command::Cordon], "{phase:?}");

        host_mut(&mut f, "s1").cordoned = None;
        let unknown = run(&f, "s1", &phase, 10);
        assert_eq!(unknown.commands, Vec::new(), "{phase:?}");

        host_mut(&mut f, "s1").cordoned = Some(true);
        let shown = run(&f, "s1", &phase, 10);
        assert_eq!(shown.commands, Vec::new(), "{phase:?}");
    }
    let mut f = waiting_in(&HostPhase::Activating);
    host_mut(&mut f, "s1").cordoned = Some(false);
    host_mut(&mut f, "s1").helper = report(HelperStatus::Reverted);
    let held = run(&f, "s1", &HostPhase::Activating, 10);
    let reason = HoldReason::Reverted;
    assert_eq!(held.next, HostPhase::Held { reason });
    let want = vec![Command::Cordon, Command::Alert(Alert::Held(reason))];
    assert_eq!(held.commands, want);
}

/// Catches: "Held, cordoned" (section 5.2) not kept: a node held after its restart began
/// (unrepaired, unconfirmed, reverted, refused) whose cordon placement stops showing would
/// take work. Its cordon is ordered again from the held phase, and only for the holds that
/// keep it: not after a second member went down (the node was put back first), not when the
/// node is confirmed and it is the uncordon that did not take. An unknown or shown
/// placement orders nothing.
#[test]
fn a_held_node_keeps_its_cordon_except_when_the_hold_put_it_back() {
    let reasons = [
        HoldReason::SecondMemberDown,
        HoldReason::RestartDeadline,
        HoldReason::MemberGone,
        HoldReason::ActivateStalled,
        HoldReason::HealthLimit,
        HoldReason::RebuildLimit,
        HoldReason::RepairWait,
        HoldReason::ConfirmStalled,
        HoldReason::Reverted,
        HoldReason::RevertRefused,
        HoldReason::StillCordoned,
    ];
    for reason in reasons {
        let phase = HostPhase::Held { reason };
        let mut f = storage_fleet();
        host_mut(&mut f, "s1").cordoned = Some(false);
        let lost = run(&f, "s1", &phase, 1_000);
        assert_eq!(lost.next, phase, "{reason:?}");
        let keeps = !matches!(
            reason,
            HoldReason::SecondMemberDown | HoldReason::StillCordoned
        );
        let want = if keeps {
            vec![Command::Cordon]
        } else {
            Vec::new()
        };
        assert_eq!(lost.commands, want, "{reason:?}");
        assert_eq!(reason.keeps_cordon(), keeps, "{reason:?}");

        for cordoned in [None, Some(true)] {
            host_mut(&mut f, "s1").cordoned = cordoned;
            let quiet = run(&f, "s1", &phase, 1_000);
            assert_eq!(quiet.commands, Vec::new(), "{reason:?} {cordoned:?}");
        }
    }
}

// The policy.

/// Catches: defaults that drift from the numbers of the design (section 2.6 to 2.10) or from
/// the membership the controller reads (the time to live and the dwell are
/// `MembershipConfig`'s), a Mac forced by default, a capacity floor nobody asked for, and
/// waits before the restart that outlast the planned-down mark (it lives for the dwell
/// whatever the node does).
#[test]
fn the_default_policy_is_the_designs_and_fits_the_membership_it_reads() {
    let p = policy();
    let m = MembershipConfig::default();
    assert_eq!(p.ttl, m.ttl);
    assert_eq!(p.dwell, m.dwell);
    let secs = Duration::from_secs;
    assert_eq!(p.restart_deadline(), secs(270));
    assert_eq!(p.idle_wait, secs(2 * MIN));
    assert_eq!(p.lease_limit, secs(15 * MIN));
    assert_eq!(p.mac_lease_limit, secs(60 * MIN));
    assert_eq!(p.health_limit, secs(2 * MIN));
    assert_eq!(p.repair_wait, secs(5 * MIN));
    assert_eq!(p.min_serving, 0);
    assert_eq!(p.mac_force_after, None);
    assert_eq!(p.rebuild_limit, None);
    assert!(p.write_limit * 2 + p.ttl < p.restart_deadline());
    assert_eq!(p.validate(), Ok(()));
}

/// Catches: a policy whose waits cannot be kept. Steps 4 and 5 may each wait a write
/// limit while the mark lives only to the restart deadline; a dwell of two time-to-lives
/// or less leaves no deadline at all. Both are refused, and the boundary is exact.
#[test]
fn a_policy_whose_waits_outlast_the_restart_deadline_is_refused() {
    let secs = Duration::from_secs;
    let short = Policy {
        dwell: secs(30),
        ..policy()
    };
    let error = PolicyError::DwellTooShort {
        dwell: secs(30),
        ttl: secs(15),
    };
    assert_eq!(short.validate(), Err(error));

    let at = Policy {
        write_limit: secs(135),
        ..policy()
    };
    let error = PolicyError::WritesOutlastRestartDeadline {
        waits: secs(270),
        deadline: secs(270),
    };
    assert_eq!(at.validate(), Err(error));
    let under = Policy {
        write_limit: secs(134),
        ..policy()
    };
    assert_eq!(under.validate(), Ok(()));
}

// Phases that do not move by themselves.

/// Catches: a final or waiting phase producing commands: `Done` and `Held` are a person's
/// or the picker's, and a controller would run whatever they return.
#[test]
fn pending_deferred_done_and_held_do_not_move() {
    let f = storage_fleet();
    let held = HostPhase::Held {
        reason: HoldReason::Reverted,
    };
    for phase in [
        HostPhase::Pending,
        HostPhase::Deferred,
        HostPhase::Done,
        held,
    ] {
        let moved = run(&f, "s1", &phase, 1_000);
        assert_eq!(moved.next, phase);
        assert_eq!(moved.commands, Vec::new());
        assert_eq!((moved.order, moved.boot), (None, None));
    }
}

// The walk.

/// What the world does about each command, at once: the effects the controller's
/// commands would have, applied to the facts.
fn apply(f: &mut Facts, name: &str, commands: &[Command]) {
    let linked = host_mut(f, name).member;
    let target = f.target.clone();
    for command in commands {
        match command {
            Command::CasSlot(SlotOp::Claim) => f.slot = Some(id(name)),
            Command::CasSlot(SlotOp::Release) => f.slot = None,
            Command::Cordon => host_mut(f, name).cordoned = Some(true),
            Command::Uncordon => {
                host_mut(f, name).cordoned = Some(false);
                host_mut(f, name).work = Some(Work::Leased);
            }
            Command::MarkDown => {
                let n = linked.expect("a storage host").0;
                member_mut(f, n).planned_down = true;
                member_mut(f, n).down_since = Some(t(NOW));
            }
            Command::Undown => panic!("a clean walk undoes nothing"),
            Command::Activate { order } => {
                let host = host_mut(f, name);
                host.helper = report_of(*order, HelperStatus::ActivatedUnconfirmed);
                host.release = Some(target.clone());
                if let Some(n) = linked {
                    let member = member_mut(f, n.0);
                    member.planned_down = false;
                    member.down_since = None;
                    member.boot = member.boot.map(|boot| boot + 1);
                }
            }
            Command::Confirm { order } => {
                host_mut(f, name).helper = report_of(*order, HelperStatus::Confirmed);
            }
            Command::Alert(alert) => panic!("a clean walk alerts nothing: {alert:?}"),
        }
    }
}

/// A host walked from the pick to `Done`.
struct Walk {
    /// The phases entered, in order.
    entered: Vec<HostPhase>,
    /// The commands issued, in order.
    issued: Vec<Command>,
    /// The orders the moves named.
    orders: Vec<OrderId>,
    /// The boot counters the moves named.
    boots: Vec<u64>,
}

/// Walks `name` from the pick to `Done`, recording each move in its row and applying each
/// command's effect.
fn walk(f: &mut Facts, name: &str) -> Walk {
    let mut walk = Walk {
        entered: Vec::new(),
        issued: Vec::new(),
        orders: Vec::new(),
        boots: Vec::new(),
    };
    let mut moved = Transition::select();
    for _ in 0..40 {
        apply(f, name, &moved.commands);
        record_move(f, name, &moved, t(NOW));
        walk.orders.extend(moved.order);
        walk.boots.extend(moved.boot);
        walk.issued.extend(moved.commands);
        let phase = f.host(&id(name)).expect("the host").record.phase.clone();
        if walk.entered.last() != Some(&phase) {
            walk.entered.push(phase.clone());
        }
        if phase == HostPhase::Done {
            return walk;
        }
        moved = run_row(f, name);
    }
    panic!("the walk did not finish: {:?}", walk.entered);
}

fn ten_steps() -> Vec<HostPhase> {
    vec![
        HostPhase::Selected,
        HostPhase::Cordoned,
        HostPhase::WaitingLeases,
        HostPhase::PlannedDown,
        HostPhase::Gated,
        HostPhase::Activating,
        HostPhase::RejoiningStorage,
        HostPhase::Repairing,
        HostPhase::Confirming,
        HostPhase::RejoiningWork,
        HostPhase::Done,
    ]
}

/// Catches: a step skipped, repeated or reordered on the way through the ten steps of the
/// table, and a command that is not the table's. The activation order is minted once, at
/// step 6, from the fleet's next id, and both orders carry it; the boot counter is taken
/// once, at step 4.
#[test]
fn a_storage_host_walks_the_ten_steps_in_order() {
    let hosts = vec![storage("s1", 1), storage("s2", 2), storage("s3", 3)];
    let mut f = facts(hosts, members(3));
    let walked = walk(&mut f, "s1");
    assert_eq!(walked.entered, ten_steps());
    let commands = vec![
        Command::CasSlot(SlotOp::Claim),
        Command::Cordon,
        Command::MarkDown,
        activate(),
        Command::Confirm { order: NEXT_ORDER },
        Command::Uncordon,
        Command::CasSlot(SlotOp::Release),
    ];
    assert_eq!(walked.issued, commands);
    assert_eq!(walked.orders, vec![NEXT_ORDER]);
    assert_eq!(walked.boots, vec![BOOT]);
    // The controller counted the order used.
    assert_eq!(f.next_order, NEXT_ORDER.next());
}

/// Catches: a daemon-only host (a Linux group, a Mac) taking a storage step: it skips 4,
/// 5, 7 and 8, so no mark is written or undone for it and no boot counter is taken.
#[test]
fn a_daemon_only_host_skips_the_storage_steps() {
    for name in ["d1", "m1"] {
        let host = if name == "d1" {
            linux(name)
        } else {
            mac(name, "arm64")
        };
        let mut f = facts(vec![host], members(3));
        let walked = walk(&mut f, name);
        let want = vec![
            HostPhase::Selected,
            HostPhase::Cordoned,
            HostPhase::WaitingLeases,
            HostPhase::Activating,
            HostPhase::Confirming,
            HostPhase::RejoiningWork,
            HostPhase::Done,
        ];
        assert_eq!(walked.entered, want, "{name}");
        assert!(!walked.issued.contains(&Command::MarkDown), "{name}");
        assert!(!walked.issued.contains(&Command::Undown), "{name}");
        assert_eq!(walked.orders, vec![NEXT_ORDER], "{name}");
        assert_eq!(walked.boots, Vec::<u64>::new(), "{name}");
    }
}

/// Catches: a rollback that needs a mechanism of its own, or that tests releases by an
/// ordering. The old release is the target; a storage host running the new one is walked
/// through the same ten steps, each comparing the reported release with `==`. A daemon-only
/// host is confirmed only once it reports the target, which here is the older release.
#[test]
fn a_rollback_walks_a_host_back_through_the_same_steps() {
    let hosts = vec![storage("s1", 1), storage("s2", 2), storage("s3", 3)];
    let mut f = facts(hosts, members(3));
    f.target = Release::new(OLD);
    for name in ["s1", "s2", "s3"] {
        host_mut(&mut f, name).release = Some(Release::new(NEW));
    }
    let walked = walk(&mut f, "s1");
    assert_eq!(walked.entered, ten_steps());
    let back = f.host(&id("s1")).and_then(|h| h.release.clone());
    assert_eq!(back, Some(Release::new(OLD)));

    // A daemon-only host reporting the release it is leaving is not yet back at the target.
    let mut f = linux_fleet();
    f.target = Release::new(OLD);
    host_mut(&mut f, "d1").release = Some(Release::new(NEW));
    host_mut(&mut f, "d1").helper = report(HelperStatus::ActivatedUnconfirmed);
    let leaving = run(&f, "d1", &HostPhase::Activating, 30);
    assert_eq!(leaving.next, HostPhase::Activating);
    host_mut(&mut f, "d1").release = Some(Release::new(OLD));
    let back = run(&f, "d1", &HostPhase::Activating, 30);
    assert_eq!(back.next, HostPhase::Confirming);
}

/// Catches: a controller that restarts re-running a step it already took: only entering
/// `Activating` orders the activation, so a resume re-issues it for that phase and for no
/// other (C20: no host activated twice), and with the row's own order id, not a new one.
#[test]
fn entry_commands_name_the_activate_order_for_one_phase_only() {
    let phases = [
        HostPhase::Pending,
        HostPhase::Selected,
        HostPhase::Cordoned,
        HostPhase::WaitingLeases,
        HostPhase::PlannedDown,
        HostPhase::Gated,
        HostPhase::Activating,
        HostPhase::RejoiningStorage,
        HostPhase::Repairing,
        HostPhase::Confirming,
        HostPhase::RejoiningWork,
        HostPhase::Done,
        HostPhase::Held {
            reason: HoldReason::Reverted,
        },
        HostPhase::Deferred,
    ];
    let order = OrderId(77);
    for phase in &phases {
        let record = HostRecord {
            phase: phase.clone(),
            since: t(0),
            serial: SERIAL,
            order,
            boot_before: None,
        };
        let commands = record.entry_commands();
        let activates = commands.contains(&Command::Activate { order });
        assert_eq!(activates, *phase == HostPhase::Activating, "{phase:?}");
        let confirms = commands.contains(&Command::Confirm { order });
        assert_eq!(confirms, *phase == HostPhase::Confirming, "{phase:?}");
        let any = commands
            .iter()
            .any(|c| matches!(c, Command::Activate { .. } | Command::Confirm { .. }));
        assert_eq!(any, activates || confirms, "{phase:?}: another order id");
    }
    assert_eq!(Transition::select().order, None);
}
