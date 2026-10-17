//! A host that leaves its step, and a run that is held (`docs/design/split-mode-updates.md`
//! sections 2.6, 2.10, 5.1 and 5.2): the one case where the controller writes the host's row
//! after the orders it goes with and not before. A row that says a host is out of its step
//! is not read again for the mark, the cordon or the unconfirmed binary it leaves behind, so
//! the orders that undo them (`Undown`, `Uncordon`, `Confirm`) are given first; if one fails
//! the row is not written, and the next look decides the same move again.
//!
//! The tests of these orders run the controller through a spy that notes what the host's row
//! said at the moment each order reached the farm. The first two run twice: once with the
//! farm answering, and once with the farm not answering when the move is decided, so that the
//! move is retried until it is taken.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_map::MemberState;
use kbf_server::update::{
    Alert, Controller, ControllerError, Generation, GiveBack, HelperStatus, HoldReason, HostPhase,
    HostRecord, HostRow, OrderId, Policy, Release, RowError, SlotClass, Stored, Tick,
};
use kbf_state::{Fenced, Store, StoreError};
use support::harness::{Backing, Rig, Seen, row_of, spied};
use support::world::{Gave, Inner, Spec};
use support::{NEW, id};

fn spec(storage: u64, linux: u64, macs: u64) -> Spec {
    Spec {
        storage,
        linux,
        macs,
        ..Spec::default()
    }
}

/// One second of the farm, and one look of the controller. The harness's own `tick` also
/// asserts the invariants of a run that is never held, which these tests end by breaking.
fn look(rig: &mut Rig) -> Result<Tick, ControllerError> {
    rig.world.advance();
    rig.controller.tick(rig.world.time())
}

fn seconds(w: &Inner) -> u64 {
    w.now
}

fn gave(w: &Inner, host: &str, wanted: &Gave) -> usize {
    w.given_to(host)
        .into_iter()
        .filter(|g| *g == wanted)
        .count()
}

fn is_confirm(g: &Gave) -> bool {
    matches!(g, Gave::Confirm(_))
}

fn is_undown(g: &Gave) -> bool {
    matches!(g, Gave::Undown)
}

fn is_uncordon(g: &Gave) -> bool {
    matches!(g, Gave::Uncordon)
}

/// A test for one kind of order.
type Kind = fn(&Gave) -> bool;

fn of_kind<'a>(seen: &'a [Seen], host: &str, wanted: Kind) -> Vec<&'a Seen> {
    seen.iter()
        .filter(|s| s.host == host && wanted(&s.gave))
        .collect()
}

fn alerts(w: &Inner, host: &str, alert: &Alert) -> usize {
    w.alerts
        .iter()
        .filter(|(h, a)| h == host && a == alert)
        .count()
}

fn spy_log(seen: &Arc<Mutex<Vec<Seen>>>) -> Vec<Seen> {
    seen.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Catches: the row of a hold written before the confirm of a healthy binary. A storage
/// node's repair debts do not settle within the repair wait; the run is held, but the binary
/// is healthy and must be confirmed, or the helper reverts it by its own timer (section 2.6,
/// "no revert (the binary is healthy)"). The `Held` row says the host is out of its step, and
/// nothing would read it again for the confirm: written first, a farm that did not answer at
/// that moment would lose the confirm for good. So the confirm reaches the helper while the
/// row still says `Repairing`; with the farm silent the row stays `Repairing` and the confirm
/// is given again at every look, and once it is taken, the row is `Held`, the cordon is kept
/// (the debts are the other owner's to settle), the helper reports `CONFIRMED`, and the
/// alert goes out once.
#[test]
fn a_repair_wait_hold_confirms_the_healthy_binary_before_its_row_says_held() {
    for outage in [false, true] {
        let (mut rig, seen) = spied(Backing::Fake, "repair", &spec(2, 0, 0), Policy::default());
        rig.offer(NEW);
        rig.until("s1 to repair", 400, |r| {
            r.phase("s1") == HostPhase::Repairing
        });
        // The other member never settles what it owes: the gate with the node in stays shut.
        rig.world.lock().members[1].debt = 1_000_000;
        let row = row_of(&rig.rows, "s1");
        let hold_at = row.record.since.as_millis() / 1000 + Policy::default().repair_wait.as_secs();
        while seconds(&rig.world.lock()) + 1 < hold_at {
            look(&mut rig).expect("a look");
        }
        let until = hold_at + 20;
        if outage {
            rig.world.lock().down_until = Some(until);
        }
        let mut errors = 0;
        while seconds(&rig.world.lock()) < until {
            errors += look(&mut rig).expect("a look").errors.len();
            let now = seconds(&rig.world.lock());
            if outage && now < until {
                assert_eq!(
                    rig.phase("s1"),
                    HostPhase::Repairing,
                    "at {now}: the row moved while the confirm was not taken"
                );
            }
        }
        assert_eq!(errors > 0, outage, "outage={outage}: {errors} errors");
        let held = HostPhase::Held {
            reason: HoldReason::RepairWait,
        };
        assert_eq!(rig.phase("s1"), held, "outage={outage}");
        for _ in 0..10 {
            look(&mut rig).expect("a look");
        }

        let log = spy_log(&seen);
        let confirms = of_kind(&log, "s1", is_confirm);
        assert!(!confirms.is_empty(), "outage={outage}: no confirm");
        for c in &confirms {
            assert_eq!(
                c.phase,
                HostPhase::Repairing,
                "outage={outage}: the row said {:?} when the confirm was given",
                c.phase
            );
        }
        let w = rig.world.lock();
        let confirm = Gave::Confirm(row.record.order);
        assert_eq!(gave(&w, "s1", &confirm), 1, "outage={outage}");
        assert_eq!(
            gave(&w, "s1", &Gave::Uncordon),
            0,
            "the hold keeps the cordon"
        );
        assert!(w.host("s1").cordoned, "outage={outage}");
        let status = w.host("s1").helper.map(|r| r.status);
        assert_eq!(status, Some(HelperStatus::Confirmed), "outage={outage}");
        assert_eq!(alerts(&w, "s1", &Alert::Held(HoldReason::RepairWait)), 1);
        drop(w);
        assert!(rig.rows.slot().unwrap().value.holds(&id("s1")));
        assert_eq!(rig.phase("s2"), HostPhase::Pending, "the run is held");
    }
}

/// Catches: the row of a hold written before the node is put back. A second member goes
/// down while `s1` is marked planned-down: the run is held, and `s1` is put back first (the
/// mark undone, then the cordon lifted, so the two nodes are not out together). Neither
/// order may follow the `Held` row: nothing would read that row for them, and a node left
/// marked and cordoned with the other owner down is the data loss the gate exists for. The
/// farm is silent for ten seconds in the second run: the row stays `PlannedDown` and both
/// orders are given again until they are taken, in order, once each.
#[test]
fn a_second_member_down_puts_the_node_back_before_the_row_says_held() {
    for outage in [false, true] {
        let (mut rig, seen) = spied(Backing::Fake, "second", &spec(2, 0, 0), Policy::default());
        rig.offer(NEW);
        rig.until("s1 to be marked down", 400, |r| {
            r.phase("s1") == HostPhase::PlannedDown
        });
        let until = {
            let mut w = rig.world.lock();
            let now = w.now;
            let other = &mut w.members[1];
            other.process_up = false;
            other.state = MemberState::Down;
            other.heard_at = now - 100;
            now + 10
        };
        if outage {
            rig.world.lock().down_until = Some(until);
        }
        let mut errors = 0;
        while seconds(&rig.world.lock()) < until {
            errors += look(&mut rig).expect("a look").errors.len();
            let now = seconds(&rig.world.lock());
            if outage && now < until {
                assert_eq!(
                    rig.phase("s1"),
                    HostPhase::PlannedDown,
                    "at {now}: the row moved while the node was not put back"
                );
            }
        }
        assert_eq!(errors > 0, outage, "outage={outage}: {errors} errors");
        let held = HostPhase::Held {
            reason: HoldReason::SecondMemberDown,
        };
        assert_eq!(rig.phase("s1"), held, "outage={outage}");
        for _ in 0..10 {
            look(&mut rig).expect("a look");
        }

        let log = spy_log(&seen);
        let kinds: [(&str, Kind); 2] = [("undown", is_undown), ("uncordon", is_uncordon)];
        for (what, wanted) in kinds {
            let orders = of_kind(&log, "s1", wanted);
            assert!(!orders.is_empty(), "outage={outage}: no {what}");
            for o in orders {
                assert_eq!(
                    o.phase,
                    HostPhase::PlannedDown,
                    "outage={outage}: the row said {:?} when the {what} was given",
                    o.phase
                );
            }
        }
        let w = rig.world.lock();
        assert_eq!(gave(&w, "s1", &Gave::Undown), 1, "outage={outage}");
        assert_eq!(gave(&w, "s1", &Gave::Uncordon), 1, "outage={outage}");
        let order_of = |g: &Gave| w.given_to("s1").iter().position(|x| *x == g).unwrap();
        assert!(order_of(&Gave::Undown) < order_of(&Gave::Uncordon));
        assert!(!w.member(1).planned_down, "the mark is undone");
        assert!(!w.host("s1").cordoned, "the node is back in placement");
        assert_eq!(
            alerts(&w, "s1", &Alert::Held(HoldReason::SecondMemberDown)),
            1
        );
        assert_eq!(
            gave(&w, "s1", &Gave::Cordon),
            1,
            "no cordon is ordered again"
        );
        drop(w);
        assert!(rig.rows.slot().unwrap().value.holds(&id("s1")));
        assert_eq!(rig.phase("s2"), HostPhase::Pending, "the run is held");
    }
}

/// Catches: a Mac given back with its row first, or given back for good. A Mac is forced
/// when no host has been idle for the idle wait and its own force window has passed, and its
/// lease outlasts the Mac's lease wait (30 s here, an hour by default): it is uncordoned
/// (before the row says `Deferred`: a row that says it is out of its step is not read again
/// for the cordon), its place is freed, people are told once, and the run goes on without
/// it. It is not forced again; when its lease ends it is picked, updated once, and the run
/// ends.
#[test]
fn a_mac_whose_leases_outlast_the_wait_is_put_back_deferred_and_updated_later() {
    let policy = Policy {
        mac_lease_limit: Duration::from_secs(30),
        mac_force_after: Some(Duration::from_secs(10)),
        ..Policy::default()
    };
    let (mut rig, seen) = spied(Backing::Fake, "mac", &spec(0, 1, 1), policy);
    let lease_ends = rig.world.lock().now + 400;
    rig.world.lease("m1", 400);
    rig.offer(NEW);
    rig.until("m1 to be deferred", 390, |r| {
        r.phase("m1") == HostPhase::Deferred
    });
    let log = spy_log(&seen);
    let uncordons = of_kind(&log, "m1", is_uncordon);
    assert_eq!(uncordons.len(), 1, "{log:?}");
    assert_eq!(
        uncordons[0].phase,
        HostPhase::WaitingLeases,
        "the row said {:?} when the Mac was uncordoned",
        uncordons[0].phase
    );
    {
        let w = rig.world.lock();
        assert!(!w.host("m1").cordoned);
        assert_eq!(w.host("m1").activations, 0, "nothing was restarted");
        let told = Alert::GaveBack(GiveBack::LeasesNotEnded);
        assert_eq!(alerts(&w, "m1", &told), 1);
    }
    assert!(!rig.rows.slot().unwrap().value.holds(&id("m1")));
    assert_eq!(row_of(&rig.rows, "m1").record.phase, HostPhase::Deferred);

    rig.finish(NEW);
    let w = rig.world.lock();
    assert_eq!(w.host("m1").activations, 1, "updated once, after the lease");
    assert_eq!(w.host("d1").activations, 1);
    assert_eq!(gave(&w, "m1", &Gave::Uncordon), 2, "given back, then done");
    let second_cordon = w
        .given
        .iter()
        .filter(|g| g.host == "m1" && g.order == Gave::Cordon)
        .map(|g| g.at)
        .nth(1)
        .expect("m1 was cordoned a second time");
    assert!(
        second_cordon >= lease_ends,
        "m1 was forced again at {second_cordon}, before its lease ended at {lease_ends}"
    );
}

fn held_row(rig: &Rig, generation: Generation, reason: HoldReason) -> HostRow {
    HostRow {
        record: HostRecord {
            phase: HostPhase::Held { reason },
            since: rig.world.time(),
            generation,
            order: OrderId::new(rig.epoch, 1),
            boot_before: None,
        },
        commit: Release::new(NEW),
    }
}

/// A run with `d1` held for `reason` by a controller that has gone: its row and its place.
fn held(rig: &Rig, reason: HoldReason) {
    let generation = rig.offer(NEW);
    let row = held_row(rig, generation, reason);
    let absent = Stored {
        value: HostRow::unstarted(),
        version: None,
    };
    rig.rows.put_host(&id("d1"), &absent, &row).unwrap();
    rig.rows
        .claim(SlotClass::Host, &id("d1"), row.record.order, 1)
        .unwrap();
}

/// Catches: a held run whose cordon is not kept, or kept when it must not be. Placement
/// loses the cordon of a held host (a frontend restarted, an operator): a hold that keeps it
/// (the activation stalled; the helper reverts the node itself) orders it again, once, and
/// the host is cordoned; a hold that does not (the node was put back when a second member
/// went down, and the confirmed node's uncordon is what did not take) is left uncordoned.
/// The row is unchanged either way: the order is given without a write.
#[test]
fn a_held_host_keeps_the_cordon_only_when_the_hold_does() {
    for (reason, keeps) in [
        (HoldReason::ActivateStalled, true),
        (HoldReason::RevertRefused, true),
        (HoldReason::SecondMemberDown, false),
        (HoldReason::StillCordoned, false),
    ] {
        let mut rig = Rig::new(Backing::Fake, "keeps", &spec(0, 2, 0), Policy::default());
        held(&rig, reason);
        rig.world.lock().hosts[0].cordoned = true;
        look(&mut rig).expect("a look");
        let row = row_of(&rig.rows, "d1");
        rig.world.lock().hosts[0].cordoned = false;
        for _ in 0..5 {
            let tick = look(&mut rig).expect("a look");
            assert!(tick.moved.is_empty() && tick.started.is_empty(), "{tick:?}");
        }
        assert_eq!(row_of(&rig.rows, "d1"), row, "{reason:?}: the row moved");
        let w = rig.world.lock();
        assert_eq!(w.host("d1").cordoned, keeps, "{reason:?}");
        assert_eq!(
            gave(&w, "d1", &Gave::Cordon),
            usize::from(keeps),
            "{reason:?}"
        );
        assert_eq!(w.host("d2").activations, 0, "the run is held");
    }
}

/// Catches: a controller that was taken over going on giving the orders that need no row.
/// A held host's cordon is ordered again without a write, so no write of its own would stop
/// a controller whose store handle another writer has fenced out. It has looked once (so its
/// first-look replay is behind it), the next writer takes over, placement then loses the
/// cordon: its look fails, and the farm is given nothing.
#[test]
fn a_fenced_controller_does_not_order_a_cordon_again() {
    let rig = Rig::new(
        Backing::Fake,
        "fencedcordon",
        &spec(0, 2, 0),
        Policy::default(),
    );
    held(&rig, HoldReason::ActivateStalled);
    rig.world.lock().hosts[0].cordoned = true;
    let store = rig.cell.store();
    let old: Arc<dyn Store> =
        Arc::new(Fenced::take(Arc::clone(&store), None).expect("generation 1"));
    let mut a = Controller::new(old, rig.epoch, rig.world.transport(), rig.world.source());
    for _ in 0..2 {
        rig.world.advance();
        a.tick(rig.world.time()).expect("a look as the writer");
    }
    let _taker = Fenced::take(Arc::clone(&store), None).expect("generation 2");
    rig.world.lock().hosts[0].cordoned = false;
    let before = rig.world.lock().given.len();
    rig.world.advance();
    let error = a.tick(rig.world.time()).unwrap_err();
    assert!(
        matches!(
            error,
            ControllerError::Rows(RowError::Store(StoreError::Fenced))
        ),
        "{error}"
    );
    let w = rig.world.lock();
    assert_eq!(w.given.len(), before, "a fenced controller gave an order");
    assert!(!w.host("d1").cordoned);
}
