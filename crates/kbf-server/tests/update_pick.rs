//! The picker (`docs/design/split-mode-updates.md` sections 2.6, 2.7 and 2.10, #898): who
//! goes next. One host at a time, idle first, D3 after `idle_wait`, G_S failing closed for
//! storage hosts, and the rules for Macs. The seeded property test over random fleets is
//! in `update_pick_props.rs`.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use kbf_map::{MemberId, MemberState};
use kbf_server::update::{
    DaemonLoad, Facts, GateFail, HoldReason, HostPhase, Pick, Platform, PlatformClass, Policy,
    Release, Serial, Skip, WaitReason, pick,
};
use kbf_types::FarmTime;
use support::{
    IDLE, NEW, NOW, OLD, SERIAL, busy, facts, host_mut, id, linux, mac, member_mut, members,
    policy, storage, t,
};

const MIN: u64 = 60;

fn pick_now(f: &Facts) -> Pick {
    pick(f, &policy(), t(NOW))
}

fn go(name: &str, force: bool) -> Pick {
    Pick::Host {
        host: id(name),
        force,
    }
}

fn wait(why: WaitReason) -> Pick {
    Pick::Wait { why }
}

/// The picker has been waiting `secs` seconds for an idle host.
fn waited(f: &mut Facts, secs: u64) {
    f.wait_since = t(NOW - secs);
}

fn phase(f: &mut Facts, name: &str, phase: HostPhase) {
    host_mut(f, name).record.phase = phase;
}

// One host at a time.

/// Catches: a host picked while the slot is held — by another host, or by a controller
/// of the old release.
#[test]
fn nothing_starts_while_the_slot_is_held() {
    let mut f = facts(vec![linux("d1"), linux("d2")], members(3));
    f.slot = Some(id("d1"));
    assert_eq!(pick_now(&f), wait(WaitReason::SlotHeld(id("d1"))));
}

/// Catches: a second host starting while one is in a step, for any of the ten steps. Out
/// comes from the record: here the busy host's member still reads `Active` and fresh, its
/// placement `Some(false)` (a cordon not yet shown), and the slot is not yet visible; only
/// the record says a step has begun.
#[test]
fn nothing_starts_while_a_host_is_in_a_step_whatever_the_member_rows_say() {
    let steps = [
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
    ];
    for step in steps {
        let hosts = vec![
            storage("s1", 1),
            storage("s2", 2),
            storage("s3", 3),
            linux("d1"),
        ];
        let mut f = facts(hosts, members(3));
        phase(&mut f, "s1", step.clone());
        assert_eq!(
            pick_now(&f),
            wait(WaitReason::HostInStep(id("s1"))),
            "{step:?}"
        );
    }
}

/// Catches: the run going on past a held host. A failure holds the run (section 2.1);
/// "not Held" is also true of the host itself, which is never a candidate.
#[test]
fn a_held_host_holds_the_run() {
    let mut f = facts(vec![linux("d1"), linux("d2")], members(3));
    let reason = HoldReason::Reverted;
    phase(&mut f, "d1", HostPhase::Held { reason });
    assert_eq!(pick_now(&f), wait(WaitReason::RunHeld(id("d1"))));
}

/// Catches: a finished host picked again because its heartbeat still reports the old
/// release (facts lag the record): a `Done` host is never a candidate.
#[test]
fn a_done_host_is_not_picked_again_however_late_its_release_reports() {
    let mut f = facts(vec![linux("d1")], members(3));
    phase(&mut f, "d1", HostPhase::Done);
    assert_eq!(pick_now(&f), wait(WaitReason::NothingToUpdate));
}

/// Catches: a host already at the target picked (a wasted cordon), and a run that never
/// ends: with every host at the target there is nothing to update.
#[test]
fn a_host_at_the_target_is_not_a_candidate() {
    let mut f = facts(vec![linux("d1"), linux("d2")], members(3));
    host_mut(&mut f, "d1").release = Some(Release::new(NEW));
    assert_eq!(pick_now(&f), go("d2", false));
    host_mut(&mut f, "d2").release = Some(Release::new(NEW));
    assert_eq!(pick_now(&f), wait(WaitReason::NothingToUpdate));
}

/// Catches: a host that reports no release treated as at the target (the run would finish
/// with it unchecked) or as old (picked blind). It is neither picked nor skipped over:
/// when nothing else is left, the run waits with its name.
#[test]
fn a_host_with_no_reported_release_is_never_picked_and_never_counted_done() {
    let mut f = facts(vec![linux("d1"), linux("d2")], members(3));
    host_mut(&mut f, "d1").release = None;
    assert_eq!(pick_now(&f), go("d2", false));
    host_mut(&mut f, "d2").release = Some(Release::new(NEW));
    assert_eq!(
        pick_now(&f),
        wait(WaitReason::ReleaseUnknown(vec![id("d1")]))
    );
}

/// Catches: a rollback that cannot reach the hosts the forward run finished. A rollback is
/// the old release offered with a higher serial (section 2.11); the forward run left every
/// host `Done`, and those rows are history, not this run's finish. Each host is picked once
/// in the new run (a host `Done` in it is not picked again however late it reports), the
/// run ends when every host is, and a hold from the earlier run still holds this one.
#[test]
fn a_rollback_walks_back_hosts_the_forward_run_finished() {
    let hosts = vec![linux("a"), linux("b"), storage("s", 1)];
    let mut f = facts(hosts, members(3));
    f.target = Release::new(OLD);
    f.serial = Serial(SERIAL.0 + 1);
    for name in ["a", "b", "s"] {
        let host = host_mut(&mut f, name);
        host.release = Some(Release::new(NEW));
        host.record.phase = HostPhase::Done;
        host.record.serial = SERIAL;
    }
    let mut order = Vec::new();
    while let Pick::Host { host, force } = pick_now(&f) {
        assert!(!force);
        let name = host.as_str().to_owned();
        // Done in this run; the heartbeat still reports the release it left.
        let run = f.serial;
        let row = &mut host_mut(&mut f, &name).record;
        row.phase = HostPhase::Done;
        row.serial = run;
        order.push(name);
        assert!(order.len() <= 3, "a host was picked twice: {order:?}");
    }
    assert_eq!(order, ["s", "a", "b"]);
    assert_eq!(pick_now(&f), wait(WaitReason::NothingToUpdate));

    // A hold of the earlier run is a person's to decide, in this run too.
    let reason = HoldReason::Reverted;
    host_mut(&mut f, "a").record.phase = HostPhase::Held { reason };
    host_mut(&mut f, "a").record.serial = SERIAL;
    assert_eq!(pick_now(&f), wait(WaitReason::RunHeld(id("a"))));
}

/// Catches: a `Done` row of the *same* run read as open (the rollback fix applied to
/// every serial): `Done` is final for the run that wrote it.
#[test]
fn a_done_row_is_open_only_to_a_later_run() {
    let mut f = facts(vec![linux("a")], members(3));
    phase(&mut f, "a", HostPhase::Done);
    assert_eq!(pick_now(&f), wait(WaitReason::NothingToUpdate));
    f.serial = Serial(SERIAL.0 - 1);
    assert_eq!(
        pick_now(&f),
        wait(WaitReason::NothingToUpdate),
        "a row from a later serial than the run is not this run's to redo"
    );
    f.serial = Serial(SERIAL.0 + 1);
    assert_eq!(pick_now(&f), go("a", false));
}

// Idle first, then D3.

/// Catches: name order, or any order that skips an idle host for a busier one. `a-busy`
/// sorts first and has a lighter name; the idle host is chosen, unforced, even when the
/// idle wait is long over.
#[test]
fn an_idle_host_is_never_skipped_for_a_busier_one() {
    let mut f = facts(vec![linux("a-busy"), linux("z-idle")], members(3));
    host_mut(&mut f, "a-busy").load = Some(busy(1));
    assert_eq!(pick_now(&f), go("z-idle", false));
    waited(&mut f, 24 * 60 * MIN);
    assert_eq!(pick_now(&f), go("z-idle", false));
}

/// Catches: forcing a drain before `idle_wait` (2 minutes) has passed, forcing the wrong
/// host, or not forcing at all: with nobody idle the picker waits, and then cordons the
/// one with the fewest leases (D3).
#[test]
fn a_farm_that_is_never_idle_waits_then_forces_the_least_loaded_host() {
    let hosts = vec![linux("a"), linux("b"), linux("c")];
    let mut f = facts(hosts, members(3));
    host_mut(&mut f, "a").load = Some(busy(3));
    host_mut(&mut f, "b").load = Some(busy(1));
    host_mut(&mut f, "c").load = Some(busy(2));
    waited(&mut f, 2 * MIN - 1);
    let until = t(NOW - (2 * MIN - 1) + 2 * MIN);
    assert_eq!(pick_now(&f), wait(WaitReason::IdleWait { until }));
    waited(&mut f, 2 * MIN);
    assert_eq!(pick_now(&f), go("b", true));
}

/// Catches: a claim granted and not yet delivered, or a whole-machine hold, not counted in
/// the load D3 ranks by.
#[test]
fn d3_ranks_by_leases_claims_and_holds_together() {
    let hosts = vec![linux("a"), linux("b")];
    let mut f = facts(hosts, members(3));
    host_mut(&mut f, "a").load = Some(DaemonLoad {
        leases: 1,
        undelivered: 2,
        whole_machine_hold: false,
    });
    host_mut(&mut f, "b").load = Some(busy(2));
    waited(&mut f, 2 * MIN);
    assert_eq!(pick_now(&f), go("b", true));
}

/// Catches: an unknown load read as zero: such a host would be called idle, or ranked the
/// lightest. It is neither; it is skipped by name.
#[test]
fn an_unknown_load_is_neither_idle_nor_rankable() {
    let mut f = facts(vec![linux("a"), linux("b")], members(3));
    host_mut(&mut f, "a").load = None;
    host_mut(&mut f, "b").load = Some(busy(5));
    waited(&mut f, 2 * MIN);
    assert_eq!(pick_now(&f), go("b", true));

    host_mut(&mut f, "b").load = None;
    let skipped = vec![(id("a"), Skip::LoadUnknown), (id("b"), Skip::LoadUnknown)];
    assert_eq!(pick_now(&f), wait(WaitReason::NoneEligible(skipped)));
}

/// Catches: storage preferred as a barrier. Among idle hosts storage-and-daemon goes
/// first, then Linux daemon-only, then Mac, then the name (a tiebreak); but an idle
/// daemon-only host does not wait for a busy storage node, and D3 ranks by load first.
#[test]
fn storage_first_is_a_tiebreak_and_not_a_barrier() {
    let hosts = vec![
        mac("a-mac", "arm64"),
        linux("b-linux"),
        storage("c-storage", 1),
        storage("d-storage", 2),
        storage("e-storage", 3),
    ];
    let mut f = facts(hosts, members(3));
    assert_eq!(pick_now(&f), go("c-storage", false));
    phase(&mut f, "c-storage", HostPhase::Done);
    assert_eq!(pick_now(&f), go("d-storage", false));
    phase(&mut f, "d-storage", HostPhase::Done);
    phase(&mut f, "e-storage", HostPhase::Done);
    assert_eq!(pick_now(&f), go("b-linux", false));
    phase(&mut f, "b-linux", HostPhase::Done);
    assert_eq!(pick_now(&f), go("a-mac", false));

    // A busy storage node does not hold back an idle Linux host.
    let mut f = facts(vec![storage("s1", 1), linux("d1")], members(3));
    host_mut(&mut f, "s1").load = Some(busy(4));
    assert_eq!(pick_now(&f), go("d1", false));

    // D3 ranks by load before kind: the lighter Linux host beats a busier storage node.
    host_mut(&mut f, "d1").load = Some(busy(1));
    waited(&mut f, 2 * MIN);
    assert_eq!(pick_now(&f), go("d1", true));
    host_mut(&mut f, "d1").load = Some(busy(4));
    assert_eq!(pick_now(&f), go("s1", true));
}

/// Catches: a disconnected host picked (a drained node may be disconnected, and cannot
/// be handed an order), however idle it looks.
#[test]
fn a_disconnected_host_is_never_picked() {
    let mut f = facts(vec![linux("a"), linux("b")], members(3));
    host_mut(&mut f, "a").connected = false;
    assert_eq!(pick_now(&f), go("b", false));
    host_mut(&mut f, "b").connected = false;
    let skipped = vec![(id("a"), Skip::Disconnected), (id("b"), Skip::Disconnected)];
    assert_eq!(pick_now(&f), wait(WaitReason::NoneEligible(skipped)));
}

/// Catches: a host somebody else cordoned picked, which the step would then uncordon at
/// its end; and an unknown placement taken for "not cordoned".
#[test]
fn a_cordoned_or_unplaced_host_is_not_picked() {
    let mut f = facts(vec![linux("a"), linux("b")], members(3));
    host_mut(&mut f, "a").cordoned = Some(true);
    host_mut(&mut f, "b").cordoned = None;
    let skipped = vec![
        (id("a"), Skip::AlreadyCordoned),
        (id("b"), Skip::PlacementUnknown),
    ];
    assert_eq!(pick_now(&f), wait(WaitReason::NoneEligible(skipped)));
}

// G_S for storage hosts.

/// A fleet of three storage hosts and an idle Linux host; `s1` is the one under test, `s2`
/// and `s3` are done and only supply their members.
fn gate_fleet() -> Facts {
    let hosts = vec![
        storage("s1", 1),
        storage("s2", 2),
        storage("s3", 3),
        linux("z-linux"),
    ];
    let mut f = facts(hosts, members(3));
    for name in ["s2", "s3"] {
        phase(&mut f, name, HostPhase::Done);
    }
    f
}

/// Catches: each clause of G_S missing, each unknown read as fine: a member that is
/// `Down`, silent past the time to live, never heard, in debt, with its debt unreported,
/// catching up, planned down (even if its row still says `Active`), or a member missing
/// from the facts, or no other member at all. In every case no storage node is picked
/// (fail closed) and the run goes on with the host that has no storage half.
#[test]
fn a_shut_storage_gate_picks_no_storage_node_and_the_run_goes_on() {
    type Break = fn(&mut Facts);
    let cases: [(&str, Break, GateFail); 12] = [
        (
            "peer down",
            |f| member_mut(f, 2).state = MemberState::Down,
            GateFail::NotActive {
                member: MemberId(2),
                state: MemberState::Down,
            },
        ),
        (
            "peer silent",
            |f| member_mut(f, 2).heard_at = Some(t(NOW - 16)),
            GateFail::Silent {
                member: MemberId(2),
            },
        ),
        (
            "peer never heard",
            |f| member_mut(f, 2).heard_at = None,
            GateFail::Silent {
                member: MemberId(2),
            },
        ),
        (
            "peer in debt",
            |f| member_mut(f, 3).repair_debt = Some(2),
            GateFail::Debt {
                member: MemberId(3),
                blobs: 2,
            },
        ),
        (
            "peer debt unreported",
            |f| member_mut(f, 3).repair_debt = None,
            GateFail::DebtUnknown {
                member: MemberId(3),
            },
        ),
        (
            "peer catching up",
            |f| member_mut(f, 3).state = MemberState::CatchingUp,
            GateFail::CatchingUp {
                member: MemberId(3),
            },
        ),
        (
            "peer planned down but still Active",
            |f| member_mut(f, 3).planned_down = true,
            GateFail::PlannedDown {
                member: MemberId(3),
            },
        ),
        (
            "own debt unreported",
            |f| member_mut(f, 1).repair_debt = None,
            GateFail::DebtUnknown {
                member: MemberId(1),
            },
        ),
        (
            "own member not Active",
            |f| member_mut(f, 1).state = MemberState::Draining,
            GateFail::NotActive {
                member: MemberId(1),
                state: MemberState::Draining,
            },
        ),
        (
            "own member silent",
            |f| member_mut(f, 1).heard_at = None,
            GateFail::Silent {
                member: MemberId(1),
            },
        ),
        (
            "own member not listed",
            |f| f.members.retain(|m| m.id != MemberId(1)),
            GateFail::Unlinked,
        ),
        (
            "no other member",
            |f| f.members.retain(|m| m.id == MemberId(1)),
            GateFail::NoPeer,
        ),
    ];
    for (name, shut, fail) in cases {
        let mut f = gate_fleet();
        shut(&mut f);
        // `s1` is the only storage candidate, and idle. With an idle Linux host as well
        // the run goes on with it; with none, `s1` is skipped, and the reason is named.
        assert_eq!(pick_now(&f), go("z-linux", false), "{name}");
        phase(&mut f, "z-linux", HostPhase::Done);
        let Pick::Wait {
            why: WaitReason::NoneEligible(skips),
        } = pick_now(&f)
        else {
            panic!("{name}: a storage node was picked or waited for");
        };
        let own = skips.iter().find(|(h, _)| *h == id("s1"));
        assert_eq!(own, Some(&(id("s1"), Skip::Gate(fail))), "{name}");
    }
}

/// Catches: a storage-and-daemon host whose storage half is not linked picked: its gate
/// cannot be asked, so it is never eligible. A daemon-only host needs no link.
#[test]
fn an_unlinked_storage_host_is_never_eligible() {
    let mut f = facts(vec![storage("s1", 1), storage("s2", 2)], members(3));
    host_mut(&mut f, "s1").member = None;
    host_mut(&mut f, "s2").load = Some(busy(1));
    waited(&mut f, 24 * 60 * MIN);
    // Even after the idle wait: `s1` is idle but unlinked, `s2` is busy and linked.
    assert_eq!(pick_now(&f), go("s2", true));
    host_mut(&mut f, "s2").load = None;
    let Pick::Wait {
        why: WaitReason::NoneEligible(skips),
    } = pick_now(&f)
    else {
        panic!("an unlinked host was picked");
    };
    assert!(skips.contains(&(id("s1"), Skip::Gate(GateFail::Unlinked))));
}

/// Catches: the freshness bound off by one. A member heard exactly one time to live ago
/// still counts (its next beat is due); one millisecond more is silence.
#[test]
fn a_member_heard_one_time_to_live_ago_still_counts() {
    let hosts = vec![storage("s1", 1), storage("s2", 2)];
    let mut f = facts(hosts, members(2));
    member_mut(&mut f, 2).heard_at = Some(t(NOW - 15));
    assert_eq!(pick_now(&f), go("s1", false));
    member_mut(&mut f, 2).heard_at = Some(FarmTime::from_millis(NOW * 1000 - 15_001));
    // Each storage node now has a peer that is silent, or is itself.
    let skipped = [id("s1"), id("s2")];
    let Pick::Wait {
        why: WaitReason::NoneEligible(skips),
    } = pick_now(&f)
    else {
        panic!("a member silent past the time to live counted as present");
    };
    let names: Vec<_> = skips.into_iter().map(|(host, _)| host).collect();
    assert_eq!(names, skipped);
}

/// Catches: a heartbeat time later than now read as fresh. Whoever filled the facts had a
/// clock that is wrong, and the row must not look newer than it can be: it counts as not
/// heard, so the gate is shut.
#[test]
fn a_member_heard_in_the_future_does_not_count() {
    let mut f = facts(vec![storage("s1", 1), storage("s2", 2)], members(2));
    member_mut(&mut f, 2).heard_at = Some(t(NOW + 1));
    let Pick::Wait {
        why: WaitReason::NoneEligible(skips),
    } = pick_now(&f)
    else {
        panic!("a member heard in the future counted as present");
    };
    let silent = GateFail::Silent {
        member: MemberId(2),
    };
    assert!(skips.contains(&(id("s1"), Skip::Gate(silent))));
}

// Macs.

/// Catches: a busy Mac forced by default (a Mac lease may last an hour; the controller does
/// not force-drain one), however long the wait. It is skipped by name as busy, and the
/// run continues with the other hosts.
#[test]
fn a_busy_mac_is_deferred_and_never_forced_by_default() {
    let mut f = facts(vec![mac("m1", "arm64"), linux("d1")], members(3));
    host_mut(&mut f, "m1").load = Some(busy(1));
    host_mut(&mut f, "d1").load = Some(busy(2));
    waited(&mut f, 10 * 24 * 60 * MIN);
    assert_eq!(pick_now(&f), go("d1", true));
    phase(&mut f, "d1", HostPhase::Done);
    let skipped = vec![(id("m1"), Skip::MacBusy)];
    assert_eq!(pick_now(&f), wait(WaitReason::NoneEligible(skipped)));
}

/// Catches: a Mac with no leases but a whole-machine hold (a session switch in progress,
/// up to ten minutes) called idle and updated under it.
#[test]
fn a_mac_in_a_whole_machine_hold_is_not_idle() {
    let mut f = facts(vec![mac("m1", "arm64"), mac("m2", "arm64")], members(3));
    host_mut(&mut f, "m1").load = Some(DaemonLoad {
        whole_machine_hold: true,
        ..IDLE
    });
    assert_eq!(pick_now(&f), go("m2", false));
    phase(&mut f, "m2", HostPhase::Done);
    let skipped = vec![(id("m1"), Skip::MacBusy)];
    assert_eq!(pick_now(&f), wait(WaitReason::NoneEligible(skipped)));
}

/// Catches: `mac_force_after` ignored or applied early. Once it has elapsed the
/// least-loaded Mac is cordoned as D3 does a Linux host, a Linux host first if one is
/// eligible, and never before the option's own wait.
#[test]
fn mac_force_after_forces_the_least_loaded_mac_only_once_it_has_elapsed() {
    let policy = Policy {
        mac_force_after: Some(Duration::from_secs(30 * MIN)),
        ..policy()
    };
    let mut f = facts(vec![mac("m1", "arm64"), mac("m2", "arm64")], members(3));
    host_mut(&mut f, "m1").load = Some(busy(3));
    host_mut(&mut f, "m2").load = Some(busy(1));
    waited(&mut f, 30 * MIN - 1);
    assert!(matches!(
        pick(&f, &policy, t(NOW)),
        Pick::Wait {
            why: WaitReason::NoneEligible(_)
        }
    ));
    waited(&mut f, 30 * MIN);
    assert_eq!(pick(&f, &policy, t(NOW)), go("m2", true));

    // A Linux host that could be forced goes before any Mac.
    let mut f = facts(vec![mac("m1", "arm64"), linux("d1")], members(3));
    host_mut(&mut f, "m1").load = Some(busy(1));
    host_mut(&mut f, "d1").load = Some(busy(9));
    waited(&mut f, 30 * MIN);
    assert_eq!(pick(&f, &policy, t(NOW)), go("d1", true));
}

/// Catches: a Mac that missed its lease wait forced again at once, every sixty minutes
/// for ever. A deferred Mac is picked again only when it goes idle.
#[test]
fn a_deferred_mac_is_picked_again_when_idle_and_never_forced() {
    let policy = Policy {
        mac_force_after: Some(Duration::from_secs(MIN)),
        ..policy()
    };
    let mut f = facts(vec![mac("m1", "arm64"), mac("m2", "arm64")], members(3));
    phase(&mut f, "m1", HostPhase::Deferred);
    host_mut(&mut f, "m1").load = Some(busy(1));
    host_mut(&mut f, "m2").record.phase = HostPhase::Done;
    waited(&mut f, 24 * 60 * MIN);
    let skipped = vec![(id("m1"), Skip::Deferred)];
    assert_eq!(
        pick(&f, &policy, t(NOW)),
        wait(WaitReason::NoneEligible(skipped))
    );
    host_mut(&mut f, "m1").load = Some(IDLE);
    assert_eq!(pick(&f, &policy, t(NOW)), go("m1", false));
}

/// Catches: the last Mac of a platform class updated while a queued operation needs the
/// class (its queue would stall for the restart), and an unknown queue read as "needs
/// nothing". Another Mac of the class that is connected and uncordoned means it is not the
/// last; one that is not, or of another class, does not count.
#[test]
fn the_last_mac_of_a_class_waits_for_a_queue_that_does_not_need_it() {
    let needs = |classes: &[&str]| -> Option<BTreeSet<PlatformClass>> {
        Some(classes.iter().map(|c| PlatformClass::new(*c)).collect())
    };
    let mut f = facts(vec![mac("m1", "arm64")], members(3));
    f.queued_classes = needs(&["arm64"]);
    let skipped = vec![(id("m1"), Skip::LastOfClass)];
    assert_eq!(
        pick_now(&f),
        wait(WaitReason::NoneEligible(skipped.clone()))
    );
    f.queued_classes = None;
    assert_eq!(
        pick_now(&f),
        wait(WaitReason::NoneEligible(skipped.clone()))
    );
    f.queued_classes = needs(&["x86"]);
    assert_eq!(pick_now(&f), go("m1", false));
    f.queued_classes = needs(&[]);
    assert_eq!(pick_now(&f), go("m1", false));

    // A second Mac of the class, serving: not the last, so the queue's need does not matter.
    // `m2` is done, so only `m1` is a candidate, and `m2` is what keeps the class served.
    let mut f = facts(vec![mac("m1", "arm64"), mac("m2", "arm64")], members(3));
    f.queued_classes = needs(&["arm64"]);
    phase(&mut f, "m2", HostPhase::Done);
    assert_eq!(pick_now(&f), go("m1", false));
    // ... but not one that is disconnected, cordoned, or of another class.
    host_mut(&mut f, "m2").connected = false;
    assert_eq!(
        pick_now(&f),
        wait(WaitReason::NoneEligible(skipped.clone()))
    );
    host_mut(&mut f, "m2").connected = true;
    host_mut(&mut f, "m2").cordoned = Some(true);
    assert_eq!(
        pick_now(&f),
        wait(WaitReason::NoneEligible(skipped.clone()))
    );
    host_mut(&mut f, "m2").cordoned = Some(false);
    host_mut(&mut f, "m2").platform = Platform::Mac(PlatformClass::new("x86"));
    assert_eq!(pick_now(&f), wait(WaitReason::NoneEligible(skipped)));
}

/// Catches: `mac_force_after` forcing the last Mac of a class out from under a queue that
/// needs it. The force option decides *when a busy Mac may be taken*; the last-of-a-class
/// rule still decides *whether this one may*, however long the wait.
#[test]
fn mac_force_after_does_not_force_the_last_mac_of_a_class_a_queue_needs() {
    let policy = Policy {
        mac_force_after: Some(Duration::from_secs(30 * MIN)),
        ..policy()
    };
    let mut f = facts(vec![mac("m1", "arm64")], members(3));
    host_mut(&mut f, "m1").load = Some(busy(2));
    f.queued_classes = Some(BTreeSet::from([PlatformClass::new("arm64")]));
    waited(&mut f, 24 * 60 * MIN);
    let skipped = vec![(id("m1"), Skip::LastOfClass)];
    assert_eq!(
        pick(&f, &policy, t(NOW)),
        wait(WaitReason::NoneEligible(skipped))
    );
    f.queued_classes = Some(BTreeSet::new());
    assert_eq!(pick(&f, &policy, t(NOW)), go("m1", true));
}

/// Catches: a `Deferred` row of an earlier run treated as this run's deferral: a Mac that
/// missed its wait in the forward run gets a fresh start in a rollback, so `mac_force_after`
/// may take it again; a deferral of the same run is never forced (above).
#[test]
fn a_deferral_of_an_earlier_run_is_a_fresh_start() {
    let policy = Policy {
        mac_force_after: Some(Duration::from_secs(MIN)),
        ..policy()
    };
    let mut f = facts(vec![mac("m1", "arm64"), mac("m2", "arm64")], members(3));
    phase(&mut f, "m1", HostPhase::Deferred);
    host_mut(&mut f, "m1").load = Some(busy(1));
    phase(&mut f, "m2", HostPhase::Done);
    waited(&mut f, 24 * 60 * MIN);
    let skipped = vec![(id("m1"), Skip::Deferred)];
    assert_eq!(
        pick(&f, &policy, t(NOW)),
        wait(WaitReason::NoneEligible(skipped))
    );
    // The next run: `m2` is already at the target, so only `m1` is a candidate.
    f.serial = Serial(SERIAL.0 + 1);
    host_mut(&mut f, "m2").release = Some(Release::new(NEW));
    assert_eq!(pick(&f, &policy, t(NOW)), go("m1", true));
}

// The capacity floor.

/// Catches: the floor (`min_serving`, section 2.8) missing from the picker: a host whose
/// cordon would leave fewer than the floor of its platform serving is not picked, idle or
/// not, forced or not; and its reason is named. Hosts that are cordoned, disconnected,
/// unplaced or of another platform do not serve; the host itself is not counted.
#[test]
fn a_host_that_would_leave_too_few_serving_is_not_picked() {
    let floor = Policy {
        min_serving: 2,
        ..policy()
    };
    let f = facts(vec![linux("a"), linux("b"), linux("c")], members(3));
    // `a` leaves b and c serving: the floor is met.
    assert_eq!(pick(&f, &floor, t(NOW)), go("a", false));

    // One of the others stops serving: nobody may go, every host says why, and waiting
    // longer does not force one.
    for change in 0..4 {
        let mut f = f.clone();
        let c = host_mut(&mut f, "c");
        match change {
            0 => c.cordoned = Some(true),
            1 => c.connected = false,
            2 => c.cordoned = None,
            _ => c.platform = mac("m", "arm64").platform,
        }
        waited(&mut f, 24 * 60 * MIN);
        let Pick::Wait {
            why: WaitReason::NoneEligible(skips),
        } = pick(&f, &floor, t(NOW))
        else {
            panic!("change {change}: a host was picked under the floor");
        };
        let below = Skip::BelowMinServing { serving: 1 };
        assert!(skips.contains(&(id("a"), below.clone())), "change {change}");
        assert!(skips.contains(&(id("b"), below)), "change {change}");
    }

    // The default has no floor: the only Linux host may go.
    let alone = facts(vec![linux("a")], members(3));
    assert_eq!(pick_now(&alone), go("a", false));
}

/// Catches: the floor counted across platforms: a Mac does not serve Linux work, and one
/// Mac class does not serve another's. Each pool is counted alone.
#[test]
fn the_floor_counts_each_platform_alone() {
    let floor = Policy {
        min_serving: 1,
        ..policy()
    };
    let hosts = vec![
        linux("d1"),
        mac("m1", "arm64"),
        mac("m2", "x86"),
        mac("m3", "x86"),
    ];
    let mut f = facts(hosts, members(3));
    // d1 is the only Linux host: nothing serves its work if it goes. m1 is the only arm64
    // Mac. m2 and m3 serve each other.
    assert_eq!(pick(&f, &floor, t(NOW)), go("m2", false));
    phase(&mut f, "m2", HostPhase::Done);
    assert_eq!(pick(&f, &floor, t(NOW)), go("m3", false));
    phase(&mut f, "m3", HostPhase::Done);
    let Pick::Wait {
        why: WaitReason::NoneEligible(skips),
    } = pick(&f, &floor, t(NOW))
    else {
        panic!("a host was picked with nothing left to serve its platform");
    };
    let none = Skip::BelowMinServing { serving: 0 };
    assert!(skips.contains(&(id("d1"), none.clone())));
    assert!(skips.contains(&(id("m1"), none)));
}

// Storage idle includes writes.

/// Catches: storage "idle" read from the daemon half alone (section 2.7: storage idle is
/// "no in-flight writes"): a storage node with an open write session or a routed write in
/// flight is not idle, so a quiet node (or a quiet daemon-only host) goes first; with
/// nothing quiet the picker waits the idle wait, and D3 then forces it.
#[test]
fn a_storage_node_with_writes_in_flight_is_not_idle() {
    for (open, routed) in [(Some(3), Some(0)), (Some(0), Some(2))] {
        let mut f = facts(vec![storage("s1", 1), storage("s2", 2)], members(3));
        let h = host_mut(&mut f, "s1");
        h.open_writes = open;
        h.routed_writes = routed;
        assert_eq!(pick_now(&f), go("s2", false), "{open:?} {routed:?}");

        phase(&mut f, "s2", HostPhase::Done);
        waited(&mut f, MIN);
        let until = t(NOW - MIN + 2 * MIN);
        assert_eq!(pick_now(&f), wait(WaitReason::IdleWait { until }));
        waited(&mut f, 2 * MIN);
        assert_eq!(pick_now(&f), go("s1", true));
    }
}

/// Catches: a storage node that does not report its write counts picked: it could never
/// leave step 4, so it would be cordoned and given back at every look. Neither idle nor
/// forced, however long the wait; a daemon-only host needs no counts.
#[test]
fn a_storage_node_that_does_not_report_its_writes_is_never_picked() {
    for (open, routed) in [(None, Some(0)), (Some(0), None), (None, None)] {
        let mut f = facts(vec![storage("s1", 1), linux("d1")], members(3));
        let h = host_mut(&mut f, "s1");
        h.open_writes = open;
        h.routed_writes = routed;
        assert_eq!(pick_now(&f), go("d1", false), "{open:?} {routed:?}");
        phase(&mut f, "d1", HostPhase::Done);
        waited(&mut f, 24 * 60 * MIN);
        let skipped = vec![(id("s1"), Skip::WritesUnknown)];
        assert_eq!(pick_now(&f), wait(WaitReason::NoneEligible(skipped)));
    }
}

/// Catches: D3 forcing a storage node whose gate is shut because it is the busier choice
/// elsewhere: the forced pick goes through the same eligibility as an idle one. With a debt
/// anywhere the busy storage node is skipped by name and the busy Linux host is forced;
/// with no Linux host left nothing is forced.
#[test]
fn a_forced_pick_never_takes_a_storage_node_with_the_gate_shut() {
    let mut f = facts(
        vec![storage("s1", 1), storage("s2", 2), linux("d1")],
        members(3),
    );
    phase(&mut f, "s2", HostPhase::Done);
    host_mut(&mut f, "s1").load = Some(busy(1));
    host_mut(&mut f, "d1").load = Some(busy(8));
    member_mut(&mut f, 3).repair_debt = Some(2);
    waited(&mut f, 24 * 60 * MIN);
    assert_eq!(pick_now(&f), go("d1", true));

    phase(&mut f, "d1", HostPhase::Done);
    let Pick::Wait {
        why: WaitReason::NoneEligible(skips),
    } = pick_now(&f)
    else {
        panic!("a storage node was forced with G_S shut");
    };
    let fail = GateFail::Debt {
        member: MemberId(3),
        blobs: 2,
    };
    assert_eq!(skips, vec![(id("s1"), Skip::Gate(fail))]);
}

// Rollback.

/// Catches: an ordering of releases instead of equality: a rollback is the same run with
/// the old release as the target, and walks the hosts that run the new one back. The same
/// fleet, driven forward and then back, is picked clean both ways.
#[test]
fn a_rollback_is_the_same_walk_with_the_old_release_as_the_target() {
    let drive = |target: &str, runs: [&str; 3]| -> Vec<String> {
        let hosts = vec![linux("a"), linux("b"), linux("c")];
        let mut f = facts(hosts, members(3));
        f.target = Release::new(target);
        for (name, release) in ["a", "b", "c"].into_iter().zip(runs) {
            host_mut(&mut f, name).release = Some(Release::new(release));
        }
        let mut order = Vec::new();
        while let Pick::Host { host, force } = pick_now(&f) {
            assert!(!force);
            let name = host.as_str().to_owned();
            host_mut(&mut f, &name).release = Some(Release::new(target));
            order.push(name);
        }
        assert_eq!(pick_now(&f), wait(WaitReason::NothingToUpdate));
        order
    };
    // Forward: a and b still run v1, c already runs v2.
    assert_eq!(drive(NEW, [OLD, OLD, NEW]), ["a", "b"]);
    // Back: a and b run v2, c runs v1.
    assert_eq!(drive(OLD, [NEW, NEW, OLD]), ["a", "b"]);
}
