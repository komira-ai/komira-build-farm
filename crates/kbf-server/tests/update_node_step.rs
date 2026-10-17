//! The node step, steps 1 to 5 (`docs/design/split-mode-updates.md` section 2.6, #898):
//! claim, cordon, leases, planned-down and the gate. One test group per row of its table:
//! what leaves a step, and every cell of "If it fails". Steps 6 to 10 and the walk are in
//! `update_node_rejoin.rs`, the holds, the capacity floor and the Macs in
//! `update_node_hold.rs`, the picker in `update_pick.rs`.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use kbf_map::{MemberId, MemberState};
use kbf_server::update::{
    Alert, Command, DaemonLoad, Facts, GateFail, GiveBack, HoldReason, HostPhase, Policy, Severity,
    SlotOp,
};
use support::{
    BOOT, IDLE, NEXT_ORDER, NOW, activate, busy, gave_back, host_mut, id, linux, linux_fleet,
    member_mut, policy, run, step_for, storage_fleet, t,
};

const MIN: u64 = 60;

// Step 1: claim.

/// Catches: a cordon ordered before the claim is visible (two controllers both cordon),
/// or a claim that does not move on once the slot is ours.
#[test]
fn selected_cordons_once_the_claim_is_visible_and_the_gate_holds() {
    let mut f = storage_fleet();
    host_mut(&mut f, "s1").cordoned = Some(false);
    let moved = run(&f, "s1", &HostPhase::Selected, 0);
    assert_eq!(moved.next, HostPhase::Cordoned);
    assert_eq!(moved.commands, vec![Command::Cordon]);
}

/// Catches: a host that goes on after another host won the slot (two hosts in a step).
/// "If it fails: nothing changed": no command, and no uncordon of a cordon that is not ours.
#[test]
fn a_lost_claim_changes_nothing() {
    let mut f = storage_fleet();
    f.slot = Some(id("s2"));
    let moved = run(&f, "s1", &HostPhase::Selected, 0);
    assert_eq!(moved.next, HostPhase::Pending);
    assert_eq!(moved.commands, Vec::new());
}

/// Catches: a claim not yet visible read as lost (the host gives up on a CAS that
/// succeeded), or as won (the cordon goes out without the slot).
#[test]
fn a_claim_not_yet_visible_is_claimed_again() {
    let mut f = storage_fleet();
    f.slot = None;
    let moved = run(&f, "s1", &HostPhase::Selected, 0);
    assert_eq!(moved.next, HostPhase::Selected);
    assert_eq!(moved.commands, vec![Command::CasSlot(SlotOp::Claim)]);
}

/// Catches: no re-check of G_S at the claim ("Re-check G_S"): the picker saw the fleet
/// healthy, a debt appeared before the claim.
#[test]
fn the_claim_rechecks_the_storage_gate_and_gives_back_untouched() {
    let mut f = storage_fleet();
    host_mut(&mut f, "s1").cordoned = Some(false);
    member_mut(&mut f, 2).repair_debt = Some(3);
    let moved = run(&f, "s1", &HostPhase::Selected, 0);
    let why = GiveBack::GateClosed(GateFail::Debt {
        member: MemberId(2),
        blobs: 3,
    });
    assert_eq!(moved.next, HostPhase::Pending);
    // Nothing was cordoned yet, so there is nothing to uncordon.
    let want = vec![
        Command::CasSlot(SlotOp::Release),
        Command::Alert(Alert::GaveBack(why)),
    ];
    assert_eq!(moved.commands, want);
}

/// Catches: a storage-and-daemon host whose storage half is not linked going into a step
/// (its gate cannot be evaluated, so it must be shut).
#[test]
fn an_unlinked_storage_host_is_given_back_at_the_claim() {
    let mut f = storage_fleet();
    host_mut(&mut f, "s1").cordoned = Some(false);
    host_mut(&mut f, "s1").member = None;
    let moved = run(&f, "s1", &HostPhase::Selected, 0);
    assert_eq!(moved.next, HostPhase::Pending);
    let why = GiveBack::GateClosed(GateFail::Unlinked);
    assert!(
        moved
            .commands
            .contains(&Command::Alert(Alert::GaveBack(why)))
    );
}

/// Catches: the storage gate applied to a daemon-only host, which has no storage half.
#[test]
fn a_daemon_only_host_has_no_storage_gate() {
    let mut f = linux_fleet();
    host_mut(&mut f, "d1").cordoned = Some(false);
    member_mut(&mut f, 1).repair_debt = Some(9);
    let moved = run(&f, "d1", &HostPhase::Selected, 0);
    assert_eq!(moved.next, HostPhase::Cordoned);
}

/// Catches: a host whose stream closed after the claim going on to a cordon it can never
/// finish: it is given back with the slot freed, and nothing uncordoned (it was not
/// cordoned yet).
#[test]
fn a_host_that_disconnects_before_the_cordon_is_given_back_untouched() {
    let mut f = storage_fleet();
    host_mut(&mut f, "s1").cordoned = Some(false);
    host_mut(&mut f, "s1").connected = false;
    let moved = run(&f, "s1", &HostPhase::Selected, 0);
    assert_eq!(moved.next, HostPhase::Pending);
    let want = vec![
        Command::CasSlot(SlotOp::Release),
        Command::Alert(Alert::GaveBack(GiveBack::Disconnected)),
    ];
    assert_eq!(moved.commands, want);
}

/// Catches: a claim that never shows holding every other host back for ever (the CAS keeps
/// failing, or the controller lost it): it is claimed again at each look for the health
/// limit (120 s), then given back with the slot freed and nothing uncordoned.
#[test]
fn a_claim_that_never_shows_is_given_back_at_the_health_limit() {
    let mut f = storage_fleet();
    f.slot = None;
    let before = run(&f, "s1", &HostPhase::Selected, 119);
    assert_eq!(before.next, HostPhase::Selected);
    assert_eq!(before.commands, vec![Command::CasSlot(SlotOp::Claim)]);

    let at = run(&f, "s1", &HostPhase::Selected, 120);
    assert_eq!(at.next, HostPhase::Pending);
    let want = vec![
        Command::CasSlot(SlotOp::Release),
        Command::Alert(Alert::GaveBack(GiveBack::ClaimNotVisible)),
    ];
    assert_eq!(at.commands, want);
}

/// Catches: the capacity floor (section 2.8: "a node is cordoned only if the remaining
/// nodes meet `min_serving`") not kept at the claim: a host that would leave too few hosts
/// of its platform serving is given back untouched; one more serving host and it goes on.
/// A host that is cordoned, disconnected or of another platform does not count as serving.
#[test]
fn a_host_that_would_leave_too_few_serving_is_given_back_at_the_claim() {
    let floor = Policy {
        min_serving: 2,
        ..policy()
    };
    let mut f = linux_fleet();
    host_mut(&mut f, "d1").cordoned = Some(false);
    let one = step_for(&f, "d1", &HostPhase::Selected, 0, &floor);
    assert_eq!(one.next, HostPhase::Pending);
    let why = GiveBack::BelowMinServing { serving: 1 };
    let want = vec![
        Command::CasSlot(SlotOp::Release),
        Command::Alert(Alert::GaveBack(why)),
    ];
    assert_eq!(one.commands, want);

    // A third host serving meets the floor.
    f.hosts.push(linux("d3"));
    let two = step_for(&f, "d1", &HostPhase::Selected, 0, &floor);
    assert_eq!(two.next, HostPhase::Cordoned);

    // Not serving: cordoned, disconnected, placement unknown, or a Mac.
    for change in 0..4 {
        let mut f = f.clone();
        let d3 = host_mut(&mut f, "d3");
        match change {
            0 => d3.cordoned = Some(true),
            1 => d3.connected = false,
            2 => d3.cordoned = None,
            _ => d3.platform = support::mac("m", "arm64").platform,
        }
        let moved = step_for(&f, "d1", &HostPhase::Selected, 0, &floor);
        assert_eq!(moved.next, HostPhase::Pending, "change {change}");
    }

    // The default policy has no floor.
    let mut alone = linux_fleet();
    alone.hosts.truncate(1);
    host_mut(&mut alone, "d1").cordoned = Some(false);
    let moved = run(&alone, "d1", &HostPhase::Selected, 0);
    assert_eq!(moved.next, HostPhase::Cordoned);
}

// Step 2: cordon.

/// Catches: leaving the cordon step before placement shows it.
#[test]
fn a_cordon_that_placement_shows_moves_on_to_the_leases() {
    let f = storage_fleet();
    let moved = run(&f, "s1", &HostPhase::Cordoned, 1);
    assert_eq!(moved.next, HostPhase::WaitingLeases);
    assert_eq!(moved.commands, Vec::new());
}

/// Catches: no re-assert of a cordon that placement does not show (a frontend that
/// restarted lost it), which would leave the host taking leases while it updates.
#[test]
fn a_cordon_placement_does_not_show_is_ordered_again() {
    let mut f = storage_fleet();
    host_mut(&mut f, "s1").cordoned = Some(false);
    let moved = run(&f, "s1", &HostPhase::Cordoned, 5);
    assert_eq!(moved.next, HostPhase::Cordoned);
    assert_eq!(moved.commands, vec![Command::Cordon]);
}

/// Catches: an unknown placement read as cordoned (go on) or as lost (a command every
/// look, and `Farm::place` logs each).
#[test]
fn an_unknown_placement_waits_without_a_command() {
    let mut f = storage_fleet();
    host_mut(&mut f, "s1").cordoned = None;
    let moved = run(&f, "s1", &HostPhase::Cordoned, 5);
    assert_eq!(moved.next, HostPhase::Cordoned);
    assert_eq!(moved.commands, Vec::new());
}

/// Catches: a cordon that never shows holding the slot for ever; the health limit (120 s)
/// gives the host back. A placement that is never reported is the same: only an observed
/// cordon leaves the step.
#[test]
fn a_cordon_that_never_shows_gives_the_host_back_at_the_health_limit() {
    for shown in [Some(false), None] {
        let mut f = storage_fleet();
        host_mut(&mut f, "s1").cordoned = shown;
        let before = run(&f, "s1", &HostPhase::Cordoned, 119);
        assert_eq!(before.next, HostPhase::Cordoned, "{shown:?}");
        let at = run(&f, "s1", &HostPhase::Cordoned, 120);
        assert_eq!(at.next, HostPhase::Pending, "{shown:?}");
        let want = gave_back(false, GiveBack::CordonNotTaken);
        assert_eq!(at.commands, want, "{shown:?}");
    }
}

// Step 3: leases.

/// Catches: a storage host skipping the planned-down step, or a daemon-only host taking
/// it (steps 4, 5, 7 and 8 are storage's).
#[test]
fn zero_leases_move_a_storage_host_to_planned_down_and_a_daemon_only_host_to_activate() {
    let f = storage_fleet();
    let moved = run(&f, "s1", &HostPhase::WaitingLeases, 30);
    assert_eq!(moved.next, HostPhase::PlannedDown);
    assert_eq!(moved.commands, vec![Command::MarkDown]);
    // The move records the boot counter the member has before the restart, and mints no order.
    assert_eq!(moved.boot, Some(BOOT));
    assert_eq!(moved.order, None);

    let f = linux_fleet();
    let moved = run(&f, "d1", &HostPhase::WaitingLeases, 30);
    assert_eq!(moved.next, HostPhase::Activating);
    assert_eq!(moved.commands, vec![activate()]);
    // The order is the fleet's next id, named so the row records it; no boot counter.
    assert_eq!(moved.order, Some(NEXT_ORDER));
    assert_eq!(moved.boot, None);
}

/// Catches: a node marked down on a gate last read at the claim, up to 15 minutes before
/// (the design's order allows it): a debt, a catching-up peer or another planned-down
/// member that appeared while the leases ended shuts the gate, and the host is given back
/// before the mark is written, so there is nothing to undo.
#[test]
fn a_gate_that_shut_while_the_leases_ended_stops_the_mark() {
    let mut debt = storage_fleet();
    member_mut(&mut debt, 2).repair_debt = Some(3);
    let mut catching = storage_fleet();
    member_mut(&mut catching, 3).state = MemberState::CatchingUp;
    let mut down = storage_fleet();
    member_mut(&mut down, 3).state = MemberState::Down;
    let wants = [
        (
            debt,
            GateFail::Debt {
                member: MemberId(2),
                blobs: 3,
            },
        ),
        (
            catching,
            GateFail::CatchingUp {
                member: MemberId(3),
            },
        ),
        (
            down,
            GateFail::NotActive {
                member: MemberId(3),
                state: MemberState::Down,
            },
        ),
    ];
    for (f, fail) in wants {
        let moved = run(&f, "s1", &HostPhase::WaitingLeases, 14 * MIN);
        assert_eq!(moved.next, HostPhase::Pending, "{fail}");
        let want = gave_back(false, GiveBack::GateClosed(fail.clone()));
        assert_eq!(moved.commands, want, "{fail}");
        assert!(!moved.commands.contains(&Command::MarkDown), "{fail}");
    }
}

/// Catches: a node marked down whose boot counter is not reported: its return could not be
/// told from the old process still running (step 7), so the mark is never written.
#[test]
fn a_member_with_no_boot_counter_is_given_back_before_the_mark() {
    let mut f = storage_fleet();
    member_mut(&mut f, 1).boot = None;
    let moved = run(&f, "s1", &HostPhase::WaitingLeases, 30);
    assert_eq!(moved.next, HostPhase::Pending);
    assert_eq!(moved.commands, gave_back(false, GiveBack::BootUnknown));
}

/// Catches: an idle test that looks at leases alone: a claim granted and not yet
/// delivered, or a whole-machine hold, is not idle (section 2.7).
#[test]
fn any_lease_claim_or_hold_keeps_the_host_waiting() {
    let loads = [
        busy(1),
        DaemonLoad {
            undelivered: 1,
            ..IDLE
        },
        DaemonLoad {
            whole_machine_hold: true,
            ..IDLE
        },
    ];
    for load in loads {
        let mut f = linux_fleet();
        host_mut(&mut f, "d1").load = Some(load);
        let moved = run(&f, "d1", &HostPhase::WaitingLeases, 30);
        assert_eq!(moved.next, HostPhase::WaitingLeases, "{load:?}");
        assert_eq!(moved.commands, Vec::new(), "{load:?}");
    }
}

/// Catches: an absent lease count read as zero: a host whose load is unknown would be
/// activated under running builds.
#[test]
fn an_absent_lease_count_never_reads_as_zero() {
    let mut f = linux_fleet();
    host_mut(&mut f, "d1").load = None;
    let moved = run(&f, "d1", &HostPhase::WaitingLeases, 30);
    assert_eq!(moved.next, HostPhase::WaitingLeases);
    assert_eq!(moved.commands, Vec::new());
}

/// Catches: the idle test passing while the cordon is lost or unknown (a lease can arrive
/// between "idle" and "activate"); the cordon is ordered again and nothing proceeds.
#[test]
fn nothing_proceeds_without_the_cordon_shown() {
    let mut f = linux_fleet();
    host_mut(&mut f, "d1").cordoned = Some(false);
    let moved = run(&f, "d1", &HostPhase::WaitingLeases, 30);
    assert_eq!(moved.next, HostPhase::WaitingLeases);
    assert_eq!(moved.commands, vec![Command::Cordon]);

    host_mut(&mut f, "d1").cordoned = None;
    let moved = run(&f, "d1", &HostPhase::WaitingLeases, 30);
    assert_eq!(moved.next, HostPhase::WaitingLeases);
    assert_eq!(moved.commands, Vec::new());
}

/// Catches: a lease wait without a limit, or with the wrong one: 15 minutes on Linux
/// ("after the limit: uncordon, `Pending`"). A host that is idle at the limit goes on.
#[test]
fn leases_that_outlast_the_limit_give_the_host_back_and_idle_at_the_limit_goes_on() {
    let mut f = storage_fleet();
    host_mut(&mut f, "s1").load = Some(busy(2));
    let before = run(&f, "s1", &HostPhase::WaitingLeases, 15 * MIN - 1);
    assert_eq!(before.next, HostPhase::WaitingLeases);
    let at = run(&f, "s1", &HostPhase::WaitingLeases, 15 * MIN);
    assert_eq!(at.next, HostPhase::Pending);
    assert_eq!(at.commands, gave_back(false, GiveBack::LeasesNotEnded));

    host_mut(&mut f, "s1").load = Some(IDLE);
    let idle = run(&f, "s1", &HostPhase::WaitingLeases, 15 * MIN);
    assert_eq!(idle.next, HostPhase::PlannedDown);
}

// Step 4: planned-down.

/// Catches: leaving step 4 before the mark is visible and the writes are zero: the router
/// would still choose the node as a write primary while it stops.
#[test]
fn the_mark_seen_and_no_writes_move_on_to_the_gate() {
    let mut f = storage_fleet();
    member_mut(&mut f, 1).planned_down = true;
    let moved = run(&f, "s1", &HostPhase::PlannedDown, 3);
    assert_eq!(moved.next, HostPhase::Gated);
    assert_eq!(moved.commands, Vec::new());
}

/// Catches: an open write session, a routed write in flight, or either count absent read
/// as zero (the stop would fail a write buck2 retries for only about 1.5 s).
#[test]
fn open_or_unknown_writes_keep_the_host_in_planned_down() {
    let cases = [
        (Some(1), Some(0)),
        (Some(0), Some(1)),
        (None, Some(0)),
        (Some(0), None),
    ];
    for (open, routed) in cases {
        let mut f = storage_fleet();
        member_mut(&mut f, 1).planned_down = true;
        let h = host_mut(&mut f, "s1");
        h.open_writes = open;
        h.routed_writes = routed;
        let moved = run(&f, "s1", &HostPhase::PlannedDown, 3);
        assert_eq!(moved.next, HostPhase::PlannedDown, "{open:?} {routed:?}");
        assert_eq!(moved.commands, Vec::new(), "{open:?} {routed:?}");
    }
}

/// Catches: zero writes counted as the mark being in force: a mark not yet visible is
/// ordered again, not assumed.
#[test]
fn a_mark_that_is_not_visible_is_ordered_again() {
    let f = storage_fleet();
    let moved = run(&f, "s1", &HostPhase::PlannedDown, 3);
    assert_eq!(moved.next, HostPhase::PlannedDown);
    assert_eq!(moved.commands, vec![Command::MarkDown]);
}

/// Catches: a stuck upload holding the node out for ever (C31): at the write limit
/// (60 seconds) the mark is undone, the node uncordoned and the slot freed.
#[test]
fn writes_that_never_stop_undo_the_mark_and_give_the_host_back() {
    let mut f = storage_fleet();
    member_mut(&mut f, 1).planned_down = true;
    host_mut(&mut f, "s1").open_writes = Some(4);
    let before = run(&f, "s1", &HostPhase::PlannedDown, MIN - 1);
    assert_eq!(before.next, HostPhase::PlannedDown);
    let at = run(&f, "s1", &HostPhase::PlannedDown, MIN);
    assert_eq!(at.next, HostPhase::Pending);
    assert_eq!(at.commands, gave_back(true, GiveBack::WritesNotQuiet));
}

/// Catches: carrying on with a node out while another member is down (C24): blobs owned
/// by both would answer `UNAVAILABLE`. The node is put back first, then the run holds.
#[test]
fn a_second_member_down_while_planned_down_puts_the_node_back_and_holds() {
    let mut f = storage_fleet();
    member_mut(&mut f, 1).planned_down = true;
    member_mut(&mut f, 3).state = MemberState::Down;
    let moved = run(&f, "s1", &HostPhase::PlannedDown, 3);
    let reason = HoldReason::SecondMemberDown;
    assert_eq!(moved.next, HostPhase::Held { reason });
    let want = vec![
        Command::Undown,
        Command::Uncordon,
        Command::Alert(Alert::Held(reason)),
    ];
    assert_eq!(moved.commands, want);
    assert_eq!(reason.severity(), Severity::Page);
}

// Step 5: gate.

/// A fleet whose host `s1` is in step 5: marked, writes quiet, cordoned.
fn gated_fleet() -> Facts {
    let mut f = storage_fleet();
    member_mut(&mut f, 1).planned_down = true;
    f
}

/// Catches: no activate order once G_S holds with the node out.
#[test]
fn an_open_gate_activates() {
    let f = gated_fleet();
    let moved = run(&f, "s1", &HostPhase::Gated, 5);
    assert_eq!(moved.next, HostPhase::Activating);
    assert_eq!(moved.commands, vec![activate()]);
    assert_eq!(moved.order, Some(NEXT_ORDER));
}

/// Catches: a gate answered from the reports of before the node went out (they say the
/// fleet is healthy; the second copy may have gone since): a report older than the mark
/// waits for the next heartbeat, and does not give the host back.
#[test]
fn a_report_older_than_the_mark_waits_for_the_next_heartbeat() {
    let mut f = gated_fleet();
    // The step was entered 10 s ago; member 3 last reported 12 s ago: heard within a
    // time to live (15 s), but before the node went out.
    member_mut(&mut f, 3).heard_at = Some(t(NOW - 12));
    let moved = run(&f, "s1", &HostPhase::Gated, 10);
    assert_eq!(moved.next, HostPhase::Gated);
    assert_eq!(moved.commands, Vec::new());
}

/// Catches: a silent member counted as present. Past the time to live it is down (G_S
/// clause 1), so the run holds and the node is put back.
#[test]
fn a_silent_member_is_a_second_member_down() {
    let reason = HoldReason::SecondMemberDown;
    let mut f = gated_fleet();
    member_mut(&mut f, 3).heard_at = Some(t(NOW - 16));
    let moved = run(&f, "s1", &HostPhase::Gated, 20);
    assert_eq!(moved.next, HostPhase::Held { reason });
    assert!(
        moved
            .commands
            .starts_with(&[Command::Undown, Command::Uncordon])
    );

    let mut f = gated_fleet();
    member_mut(&mut f, 3).heard_at = None;
    let moved = run(&f, "s1", &HostPhase::Gated, 20);
    assert_eq!(moved.next, HostPhase::Held { reason });
}

/// Catches: "Held if a second member is Down" turned into a retry (`Pending`), and a
/// failure on a lower member hiding a down member: Down outranks a debt.
#[test]
fn a_down_member_outranks_a_debt_on_a_lower_member() {
    let mut f = gated_fleet();
    member_mut(&mut f, 2).repair_debt = Some(1);
    member_mut(&mut f, 3).state = MemberState::Down;
    let moved = run(&f, "s1", &HostPhase::Gated, 5);
    let reason = HoldReason::SecondMemberDown;
    assert_eq!(moved.next, HostPhase::Held { reason });
}

/// Catches: G_S without its clause 2 (repair debt zero) or clause 3 (nobody catching up
/// or planned down), and a debt read as zero when the member does not report it. Each
/// shuts the gate; the node is put back and `Pending`, not held.
#[test]
fn a_debt_an_unknown_debt_a_rebuild_or_a_mark_shut_the_gate() {
    let mut open_debt = gated_fleet();
    member_mut(&mut open_debt, 2).repair_debt = Some(7);
    let mut unknown_debt = gated_fleet();
    member_mut(&mut unknown_debt, 2).repair_debt = None;
    let mut own_debt = gated_fleet();
    member_mut(&mut own_debt, 1).repair_debt = Some(2);
    let mut catching = gated_fleet();
    member_mut(&mut catching, 3).state = MemberState::CatchingUp;
    let mut marked = gated_fleet();
    member_mut(&mut marked, 3).planned_down = true;
    let mut joining = gated_fleet();
    member_mut(&mut joining, 3).state = MemberState::Joining;
    let mut draining = gated_fleet();
    member_mut(&mut draining, 2).state = MemberState::Draining;
    let wants = [
        (
            open_debt,
            GateFail::Debt {
                member: MemberId(2),
                blobs: 7,
            },
        ),
        (
            unknown_debt,
            GateFail::DebtUnknown {
                member: MemberId(2),
            },
        ),
        (
            own_debt,
            GateFail::Debt {
                member: MemberId(1),
                blobs: 2,
            },
        ),
        (
            catching,
            GateFail::CatchingUp {
                member: MemberId(3),
            },
        ),
        (
            marked,
            GateFail::PlannedDown {
                member: MemberId(3),
            },
        ),
        (
            joining,
            GateFail::NotActive {
                member: MemberId(3),
                state: MemberState::Joining,
            },
        ),
        (
            draining,
            GateFail::NotActive {
                member: MemberId(2),
                state: MemberState::Draining,
            },
        ),
    ];
    for (f, fail) in wants {
        let moved = run(&f, "s1", &HostPhase::Gated, 5);
        assert_eq!(moved.next, HostPhase::Pending, "{fail}");
        let want = gave_back(true, GiveBack::GateClosed(fail.clone()));
        assert_eq!(moved.commands, want, "{fail}");
    }
}

/// Catches: a one-member fleet passing G_S vacuously: with nobody else, taking the node
/// out leaves no copy at all.
#[test]
fn a_node_with_no_other_member_cannot_go_out() {
    let mut f = gated_fleet();
    f.members.retain(|m| m.id == MemberId(1));
    let moved = run(&f, "s1", &HostPhase::Gated, 5);
    assert_eq!(moved.next, HostPhase::Pending);
    let want = gave_back(true, GiveBack::GateClosed(GateFail::NoPeer));
    assert_eq!(moved.commands, want);
}

/// Catches: an activate order sent while the mark is gone, a write session reopened or
/// the cordon lost. None of those is a reason to give the host back; the step orders the
/// missing thing again, or waits.
#[test]
fn the_activate_order_waits_for_the_mark_the_writes_and_the_cordon() {
    let mut f = gated_fleet();
    member_mut(&mut f, 1).planned_down = false;
    let moved = run(&f, "s1", &HostPhase::Gated, 5);
    assert_eq!(moved.next, HostPhase::Gated);
    assert_eq!(moved.commands, vec![Command::MarkDown]);

    let mut f = gated_fleet();
    host_mut(&mut f, "s1").routed_writes = Some(1);
    let moved = run(&f, "s1", &HostPhase::Gated, 5);
    assert_eq!(moved.next, HostPhase::Gated);
    assert_eq!(moved.commands, Vec::new());

    let mut f = gated_fleet();
    host_mut(&mut f, "s1").cordoned = Some(false);
    let moved = run(&f, "s1", &HostPhase::Gated, 5);
    assert_eq!(moved.next, HostPhase::Gated);
    assert_eq!(moved.commands, vec![Command::Cordon]);
}

/// Catches: a lost host (stream closed) kept in a step it can no longer be told to
/// finish: it is given back, with the mark undone when it was written.
#[test]
fn a_host_that_disconnects_before_the_activate_order_is_given_back() {
    let mut f = gated_fleet();
    host_mut(&mut f, "s1").connected = false;
    let moved = run(&f, "s1", &HostPhase::Gated, 5);
    assert_eq!(moved.next, HostPhase::Pending);
    assert_eq!(moved.commands, gave_back(true, GiveBack::Disconnected));

    let mut f = linux_fleet();
    host_mut(&mut f, "d1").connected = false;
    let moved = run(&f, "d1", &HostPhase::WaitingLeases, 5);
    assert_eq!(moved.next, HostPhase::Pending);
    assert_eq!(moved.commands, gave_back(false, GiveBack::Disconnected));
}

/// Catches: the write limit (60 s) not applying at the gate: a mark that has gone, or a
/// write that reopened, is ordered again or waited for until the limit, and then the host
/// is given back with the mark undone, not held for ever.
#[test]
fn a_mark_that_is_gone_or_a_reopened_write_gives_the_host_back_at_the_write_limit() {
    let mut gone = gated_fleet();
    member_mut(&mut gone, 1).planned_down = false;
    let mut reopened = gated_fleet();
    host_mut(&mut reopened, "s1").routed_writes = Some(1);
    for (f, again) in [(gone, vec![Command::MarkDown]), (reopened, Vec::new())] {
        let before = run(&f, "s1", &HostPhase::Gated, MIN - 1);
        assert_eq!(before.next, HostPhase::Gated);
        assert_eq!(before.commands, again);
        let at = run(&f, "s1", &HostPhase::Gated, MIN);
        assert_eq!(at.next, HostPhase::Pending);
        assert_eq!(at.commands, gave_back(true, GiveBack::WritesNotQuiet));
    }
}
