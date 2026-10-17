//! The places of the slot as the picker sees them (`docs/design/split-mode-updates.md`
//! sections 2.1 and 2.6, #898): one storage host in a step at a time, and daemon-only hosts
//! and Macs up to `max_hosts_out` beside it. The two are separate budgets, the answer is the
//! set of hosts to start in one look, and the hosts chosen together are checked against each
//! other. The rest of the picker is in `update_pick.rs`, the seeded property test in
//! `update_pick_props.rs`.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::collections::BTreeSet;

use kbf_server::update::{
    Facts, HoldReason, HostPhase, Pick, Picked, PlatformClass, Policy, Skip, SlotClass, WaitReason,
    pick,
};
use support::{NOW, facts, host_mut, id, linux, mac, members, policy, storage, t};

fn pick_now(f: &Facts) -> Pick {
    pick(f, &policy(), t(NOW))
}

/// The answer that starts `name` alone.
fn go(name: &str, force: bool) -> Pick {
    goes(&[(name, force)])
}

/// The answer that starts these hosts, in this order.
fn goes(hosts: &[(&str, bool)]) -> Pick {
    Pick::Hosts(
        hosts
            .iter()
            .map(|(name, force)| Picked {
                host: id(name),
                force: *force,
            })
            .collect(),
    )
}

fn wait(why: WaitReason) -> Pick {
    Pick::Wait { why }
}

fn phase(f: &mut Facts, name: &str, phase: HostPhase) {
    host_mut(f, name).record.phase = phase;
}

const STEPS: [HostPhase; 10] = [
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

/// Catches: a host picked while every place a candidate could take is held — by another
/// host, or by a controller of the old release. Two daemon-only hosts and one place for
/// hosts: the holder's place is the only one, so the other waits for it.
#[test]
fn nothing_starts_while_every_place_is_held() {
    let mut f = facts(vec![linux("d1"), linux("d2")], members(3));
    support::hold(&mut f, "d1");
    assert_eq!(pick_now(&f), wait(WaitReason::SlotHeld(id("d1"))));
}

/// Catches: a second storage host starting while one is in a step, for any of the ten
/// steps. Out comes from the record: here the busy host's member still reads `Active` and
/// fresh, its placement `Some(false)` (a cordon not yet shown), and the slot is not yet
/// visible; only the record says a step has begun.
#[test]
fn no_second_storage_host_starts_whatever_the_member_rows_say() {
    for step in STEPS {
        let hosts = vec![storage("s1", 1), storage("s2", 2), storage("s3", 3)];
        let mut f = facts(hosts, members(3));
        phase(&mut f, "s1", step.clone());
        assert_eq!(
            pick_now(&f),
            wait(WaitReason::HostInStep(id("s1"))),
            "{step:?}"
        );
    }
}

/// Catches: the storage place and the places for other hosts read as one: a storage host
/// in a step used to hold the whole run. It takes no place from a daemon-only host, and a
/// daemon-only host in a step takes none from a storage host (the design: daemon-only hosts
/// "have no storage coupling and may be in a step in parallel").
#[test]
fn a_storage_host_in_a_step_blocks_storage_hosts_only() {
    for step in STEPS {
        let hosts = vec![
            storage("s1", 1),
            storage("s2", 2),
            storage("s3", 3),
            linux("d1"),
        ];
        let mut f = facts(hosts, members(3));
        phase(&mut f, "s1", step.clone());
        assert_eq!(pick_now(&f), go("d1", false), "{step:?}");

        // And the other way round: d1 in a step, so the one place for hosts is taken;
        // storage hosts are not held back by it.
        let mut f = facts(
            vec![storage("s1", 1), storage("s2", 2), linux("d1"), linux("d2")],
            members(3),
        );
        phase(&mut f, "d1", step.clone());
        assert_eq!(pick_now(&f), go("s1", false), "{step:?}");
    }
}

/// Catches: `max_hosts_out` ignored or read as a total that storage hosts count in. With
/// three places, three daemon-only hosts and Macs start together, whatever the kind, and a
/// storage host starts beside them; the fourth waits. With two taken by the record, one is
/// left.
#[test]
fn up_to_max_hosts_out_daemon_only_hosts_and_macs_start_together() {
    let three = Policy {
        max_hosts_out: 3,
        ..policy()
    };
    let hosts = vec![
        storage("s1", 1),
        storage("s2", 2),
        linux("d1"),
        linux("d2"),
        mac("m1", "arm64"),
        mac("m2", "x86"),
    ];
    let mut f = facts(hosts, members(3));
    let want = goes(&[("s1", false), ("d1", false), ("d2", false), ("m1", false)]);
    assert_eq!(pick(&f, &three, t(NOW)), want);

    // One in a step by its record and one holding a place the record does not show yet:
    // one place is left.
    phase(&mut f, "d1", HostPhase::WaitingLeases);
    support::hold_as(&mut f, "somebody", SlotClass::Host);
    phase(&mut f, "s1", HostPhase::Done);
    assert_eq!(
        pick(&f, &three, t(NOW)),
        goes(&[("s2", false), ("d2", false)])
    );
}

/// Catches: a held host letting the run go on in another kind of place: "a failure holds
/// the run" whatever is free. A storage host that is `Held` stops daemon-only hosts too, and
/// the other way round.
#[test]
fn a_held_host_stops_every_kind_of_host() {
    let reason = HoldReason::RepairWait;
    for held in ["s1", "d1"] {
        let hosts = vec![storage("s1", 1), storage("s2", 2), linux("d1"), linux("d2")];
        let mut f = facts(hosts, members(3));
        phase(&mut f, held, HostPhase::Held { reason });
        assert_eq!(
            pick_now(&f),
            wait(WaitReason::RunHeld(id(held))),
            "held {held}"
        );
    }
}

/// Catches: the picker choosing two hosts that the floor allows one at a time. With a floor
/// of one and two idle Linux hosts and room for both, taking both leaves nobody serving:
/// the second is checked as if the first were already gone. A third, with one left, may go.
/// The same for the last Mac of a class a queue needs.
#[test]
fn hosts_chosen_together_are_checked_against_each_other() {
    let policy = Policy {
        max_hosts_out: 3,
        min_serving: 1,
        ..policy()
    };
    let two = facts(vec![linux("d1"), linux("d2")], members(3));
    assert_eq!(pick(&two, &policy, t(NOW)), go("d1", false));
    let three = facts(vec![linux("d1"), linux("d2"), linux("d3")], members(3));
    assert_eq!(
        pick(&three, &policy, t(NOW)),
        goes(&[("d1", false), ("d2", false)])
    );

    let mut macs = facts(vec![mac("m1", "arm64"), mac("m2", "arm64")], members(3));
    macs.queued_classes = Some(BTreeSet::from([PlatformClass::new("arm64")]));
    let open = Policy {
        max_hosts_out: 3,
        ..support::policy()
    };
    assert_eq!(pick(&macs, &open, t(NOW)), go("m1", false));
    macs.queued_classes = Some(BTreeSet::new());
    assert_eq!(
        pick(&macs, &open, t(NOW)),
        goes(&[("m1", false), ("m2", false)])
    );
}

/// Catches: a host that is in a step still counted as serving the floor because placement
/// has not shown its cordon yet. d1 is in a step by its record, so the floor of one has
/// only d3 left once d2 goes: d2 is not picked.
#[test]
fn a_host_in_a_step_does_not_serve_the_floor_whatever_placement_shows() {
    let policy = Policy {
        max_hosts_out: 3,
        min_serving: 1,
        ..policy()
    };
    let mut f = facts(vec![linux("d1"), linux("d2"), linux("d3")], members(3));
    phase(&mut f, "d1", HostPhase::Cordoned);
    // d1 is out; d2's peers are d3 only (one, the floor): d2 may go; then d3 may not.
    assert_eq!(pick(&f, &policy, t(NOW)), go("d2", false));
    host_mut(&mut f, "d3").connected = false;
    let Pick::Wait {
        why: WaitReason::NoneEligible(skips),
    } = pick(&f, &policy, t(NOW))
    else {
        panic!("a host was picked with nothing left to serve");
    };
    assert!(skips.contains(&(id("d2"), Skip::BelowMinServing { serving: 0 })));
}
