//! The controller (`docs/design/split-mode-updates.md` sections 2.1, 2.6, 2.10, 2.11 and 6.1)
//! against a model farm: the order of rows and orders, the places, a pause, a target that
//! stays with the host that began it, the idle wait counted from the right moment, and the
//! few things a controller refuses to guess. The seeded closed loop is `update_sim.rs`; a
//! controller that is killed and restarted, and two that race, are in
//! `update_controller_crash.rs` and `update_controller_race.rs`.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::sync::{Arc, Mutex, PoisonError};

use kbf_map::MemberId;
use kbf_server::update::{
    Alert, Controller, ControllerError, FactsSource, FarmView, GiveBack, HoldReason, HostPhase,
    HostRecord, HostRow, OrderId, Policy, PolicyError, Release, SlotClass, SourceError, Stored,
    Transport, TransportError, WaitReason,
};
use kbf_state::{Cond, Txn};
use kbf_types::{FarmTime, WorkerId};
use support::harness::{Backing, Rig, row_of, spied};
use support::world::{Gave, Inner, Spec};
use support::{NEW, OLD, id};

const BACKINGS: [Backing; 2] = [Backing::Fake, Backing::Sqlite];

fn spec(storage: u64, linux: u64, macs: u64) -> Spec {
    Spec {
        storage,
        linux,
        macs,
        ..Spec::default()
    }
}

fn activates(w: &Inner, host: &str) -> Vec<(OrderId, String)> {
    w.given_to(host)
        .into_iter()
        .filter_map(|g| match g {
            Gave::Activate(order, release) => Some((*order, release.clone())),
            _ => None,
        })
        .collect()
}

fn position(w: &Inner, host: &str, wanted: &Gave) -> Option<usize> {
    w.given_to(host).iter().position(|g| *g == wanted)
}

// A run and its rollback.

/// Catches: a run that skips a host or a step, hands an order the host's row does not
/// carry, or orders in the wrong sequence: every host is cordoned, (a storage host) marked
/// down, activated with the target, confirmed with the same order, and uncordoned, in that
/// order, once; and the rollback does it again with the old release and a higher order id,
/// walking back hosts the forward run left `Done`. On both backends.
#[test]
fn a_run_and_its_rollback_give_every_host_its_orders_once_in_order() {
    for backing in BACKINGS {
        let mut rig = Rig::new(backing, "walk", &spec(3, 1, 1), Policy::default());
        let forward = rig.offer(NEW);
        rig.finish(NEW);
        let hosts = ["s1", "s2", "s3", "d1", "m1"];
        {
            let w = rig.world.lock();
            let mut ids = Vec::new();
            for host in hosts {
                let acts = activates(&w, host);
                assert_eq!(acts.len(), 1, "{host}: {acts:?}");
                let (order, release) = acts[0].clone();
                assert_eq!(release, NEW, "{host}");
                assert_eq!(order.epoch, rig.epoch, "{host}");
                ids.push(order);
                let at =
                    |g: &Gave| position(&w, host, g).unwrap_or_else(|| panic!("{host}: {g:?}"));
                let activate = at(&Gave::Activate(order, NEW.to_owned()));
                assert!(at(&Gave::Cordon) < activate, "{host}: cordoned first");
                assert!(
                    activate < at(&Gave::Confirm(order)),
                    "{host}: confirmed after"
                );
                assert!(
                    at(&Gave::Confirm(order)) < at(&Gave::Uncordon),
                    "{host}: uncordon last"
                );
                let marked = position(&w, host, &Gave::MarkDown);
                assert_eq!(
                    marked.is_some(),
                    host.starts_with('s'),
                    "{host}: only storage is marked"
                );
                assert!(
                    marked.is_none_or(|m| m < activate),
                    "{host}: marked before the flip"
                );
                assert_eq!(position(&w, host, &Gave::Undown), None, "{host}");
            }
            let mut sorted = ids.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                hosts.len(),
                "an order id was drawn twice: {ids:?}"
            );
            assert_eq!(w.repeats, 0);
        }
        let first_ids: Vec<OrderId> = {
            let w = rig.world.lock();
            hosts.iter().map(|h| activates(&w, h)[0].0).collect()
        };

        let back = rig.offer(OLD);
        assert!(back > forward);
        rig.world.lock().new_run();
        rig.finish(OLD);
        let w = rig.world.lock();
        for (host, before) in hosts.into_iter().zip(first_ids) {
            let acts = activates(&w, host);
            assert_eq!(acts.len(), 2, "{host}: {acts:?}");
            assert_eq!(acts[1].1, OLD, "{host}");
            assert!(
                acts[1].0 > before,
                "{host}: the rollback's order is above the first"
            );
            assert_eq!(row_of(&rig.rows, host).record.generation, back, "{host}");
        }
        assert_eq!(w.repeats, 0);
    }
}

// Write ahead.

/// Catches: the row written after the order it calls for. At the moment each order reaches
/// the farm the host's row in the store already says the phase the order belongs to, and
/// names the order the helper is given: a controller that dies between the two resumes from
/// the row, and a row written after would lose the order or repeat it with another id.
#[test]
fn the_row_is_written_before_the_order_it_calls_for() {
    for backing in BACKINGS {
        let (mut rig, seen) = spied(backing, "ahead", &spec(2, 1, 1), Policy::default());
        rig.offer(NEW);
        rig.finish(NEW);
        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mut checked = 0;
        for s in &seen {
            let want = match &s.gave {
                Gave::Cordon => Some(HostPhase::Cordoned),
                Gave::MarkDown => Some(HostPhase::PlannedDown),
                Gave::Activate(..) => Some(HostPhase::Activating),
                Gave::Confirm(_) => Some(HostPhase::Confirming),
                Gave::Uncordon => Some(HostPhase::RejoiningWork),
                Gave::Undown => None,
            };
            assert_eq!(Some(&s.phase), want.as_ref(), "{}: {s:?}", s.host);
            if let Gave::Activate(order, _) | Gave::Confirm(order) = &s.gave {
                assert_eq!(
                    s.order, *order,
                    "{s:?}: the row names the order the helper gets"
                );
            }
            checked += 1;
        }
        // Two storage hosts give five orders each, a daemon-only host and a Mac four.
        assert_eq!(checked, 2 * 5 + 2 * 4, "{seen:?}");
    }
}

/// Catches: a give-back whose row says `Pending` before the host is put back. The row of a
/// host out of its step is not read again for its mark or its cordon, so a controller that
/// died (or whose uncordon failed) after writing it would leave the node cordoned for ever.
/// Here the farm does not answer when the lease limit is reached: the orders fail, the row
/// still says `WaitingLeases` and the place is still held; once the farm answers the host is
/// put back, the row says `Pending` and the place is free.
#[test]
fn a_host_is_put_back_before_its_row_says_it_was() {
    let mut rig = Rig::new(Backing::Fake, "giveback", &spec(0, 1, 0), Policy::default());
    rig.world.lease("d1", 2_000);
    rig.offer(NEW);
    rig.until("the lease wait", 400, |r| {
        r.phase("d1") == HostPhase::WaitingLeases
    });
    let since = row_of(&rig.rows, "d1").record.since.as_millis() / 1000;
    let limit = Policy::default().lease_limit.as_secs();
    rig.until("the second before the limit", 2_000, |r| {
        r.world.lock().now + 2 >= since + limit
    });
    let outage = rig.world.lock().now + 30;
    rig.world.lock().down_until = Some(outage);

    let mut failed = 0;
    while rig.world.lock().now < outage - 1 {
        rig.world.advance();
        let tick = rig.controller.tick(rig.world.time()).expect("a tick");
        failed += tick.errors.len();
        let now = rig.world.lock().now;
        if now >= since + limit {
            assert_eq!(
                rig.phase("d1"),
                HostPhase::WaitingLeases,
                "at {now}: the row moved first"
            );
            assert!(rig.world.lock().host("d1").cordoned, "at {now}");
            assert!(rig.rows.slot().unwrap().value.holds(&id("d1")), "at {now}");
        }
    }
    assert!(failed > 0, "the outage failed no order");
    while rig.world.lock().now < outage + 1 {
        rig.world.advance();
    }
    let tick = rig.controller.tick(rig.world.time()).expect("a tick");
    assert!(tick.errors.is_empty(), "{:?}", tick.errors);
    assert_eq!(rig.phase("d1"), HostPhase::Pending);
    assert!(!rig.world.lock().host("d1").cordoned);
    assert!(rig.rows.slot().unwrap().value.is_empty());
}

// The places.

fn counts(rig: &Rig) -> (usize, usize) {
    let w = rig.world.lock();
    let rows = rig.rows.hosts().unwrap();
    let stepping = rows.iter().filter(|(_, r)| r.value.record.phase.in_step());
    let storage = stepping
        .clone()
        .filter(|(h, _)| w.host(h.as_str()).member.is_some())
        .count();
    (storage, stepping.count() - storage)
}

/// Catches: `max_hosts_out` ignored, read as a total that storage hosts count in, or storage
/// hosts counted past one. Three storage hosts, four Linux hosts and two Macs, all idle, three
/// places: a storage host and three others are in a step together (and never more), and the
/// run still ends with every host updated.
#[test]
fn storage_hosts_go_one_at_a_time_and_the_rest_up_to_max_hosts_out() {
    let policy = Policy {
        max_hosts_out: 3,
        ..Policy::default()
    };
    let mut rig = Rig::new(Backing::Fake, "places", &spec(3, 4, 2), policy);
    rig.offer(NEW);
    let mut most = (0, 0);
    let mut together = 0;
    let target = Release::new(NEW);
    loop {
        let generation = rig.rows.desired().unwrap().value.unwrap().generation;
        if support::harness::finished(&rig.world.lock(), &rig.rows, generation, &target) {
            break;
        }
        rig.tick();
        let (storage, others) = counts(&rig);
        assert!(storage <= 1, "{storage} storage hosts in a step");
        assert!(others <= 3, "{others} others in a step");
        most = (most.0.max(storage), most.1.max(others));
        together += u64::from(storage == 1 && others == 3);
        assert!(
            rig.world.lock().now < support::NOW + 5_000,
            "the run did not end"
        );
    }
    assert_eq!(most, (1, 3), "the places were not all used");
    assert!(
        together > 0,
        "a storage host and three others were never out together"
    );
}

/// Catches: the default of one place read as "one host in all", so that a storage host held
/// back daemon-only hosts (or the other way round): with `max_hosts_out` at its default of
/// 1, one storage host and one other host are in a step at once, no more of either.
#[test]
fn the_default_is_one_storage_host_and_one_other_at_once() {
    let mut rig = Rig::new(Backing::Fake, "default", &spec(2, 2, 0), Policy::default());
    rig.offer(NEW);
    let tick = rig.tick();
    assert_eq!(tick.started.len(), 2, "{tick:?}");
    let names: Vec<_> = tick.started.iter().map(|h| h.as_str()).collect();
    assert_eq!(names, ["s1", "d1"]);
    let (storage, others) = counts(&rig);
    assert_eq!((storage, others), (1, 1));
    let slot = rig.rows.slot().unwrap().value;
    assert!(slot.holds(&id("s1")) && slot.holds(&id("d1")) && !slot.holds(&id("s2")));
}

// Idle first, and the wait counted from the right moment.

fn first_cordon(w: &Inner, host: &str) -> u64 {
    w.given
        .iter()
        .find(|g| g.host == host && g.order == Gave::Cordon)
        .unwrap_or_else(|| panic!("{host} was never cordoned"))
        .at
}

/// Catches: a busy host cordoned while an idle one waits; and D3's wait counted from the
/// wrong moment. Three Linux hosts, `d1` busy: the idle two go first (together), and `d1` is
/// forced only once the idle wait (120 s) has passed since the last host finished, not since
/// the offer: a controller that counted from the offer would force it 120 s after the offer,
/// while the others were still in their steps.
#[test]
fn a_busy_host_waits_for_the_idle_ones_and_is_forced_after_the_idle_wait() {
    let policy = Policy {
        max_hosts_out: 3,
        ..Policy::default()
    };
    let mut rig = Rig::new(Backing::Fake, "d3", &spec(0, 3, 0), policy);
    rig.world.lease("d1", 400);
    let offered = rig.world.lock().now;
    rig.offer(NEW);
    rig.finish(NEW);
    let w = rig.world.lock();
    let (d1, d2, d3) = (
        first_cordon(&w, "d1"),
        first_cordon(&w, "d2"),
        first_cordon(&w, "d3"),
    );
    assert!(
        d2 <= offered + 2 && d3 <= offered + 2,
        "the idle hosts go at once: {d2} {d3}"
    );
    assert!(d1 > d2.max(d3), "the busy host goes after the idle ones");
    let done = ["d2", "d3"]
        .map(|h| row_of(&rig.rows, h).record.since.as_millis() / 1000)
        .into_iter()
        .max()
        .unwrap();
    // The 120 s are counted from the last host's finish, and not before.
    assert!(
        d1 >= done + 120,
        "d1 was forced at {d1}, {} s after the last finish",
        d1 - done
    );
    assert!(
        d1 <= done + 125,
        "d1 was forced late: {} s after",
        d1 - done
    );
    assert_eq!(w.busy_cordons, 1);
}

/// Catches: a controller that starts hosts in the same second as an offer of the release
/// they already run (a wasted cordon and a restart), and one that starts them with no run
/// at all. Both do nothing, and say why.
#[test]
fn nothing_is_done_without_a_run_or_when_every_host_is_at_the_target() {
    let mut rig = Rig::new(Backing::Fake, "none", &spec(1, 1, 1), Policy::default());
    for _ in 0..5 {
        rig.world.advance();
        let tick = rig.controller.tick(rig.world.time()).unwrap();
        assert_eq!(tick, kbf_server::update::Tick::default());
    }
    rig.offer(OLD);
    let tick = rig.tick();
    assert!(tick.started.is_empty() && tick.moved.is_empty(), "{tick:?}");
    assert_eq!(tick.waiting, Some(WaitReason::NothingToUpdate));
    assert!(rig.world.lock().given.is_empty());
}

// Pause.

/// Catches: a pause that is ignored, or one that stops hosts in a step. While the run is
/// paused no new host is started, hosts in a step go on to `Done`, and a resume carries the
/// run on to the end.
#[test]
fn a_paused_run_starts_no_host_and_lets_the_hosts_in_a_step_finish() {
    let mut rig = Rig::new(Backing::Fake, "pause", &spec(2, 3, 0), Policy::default());
    rig.offer(NEW);
    rig.until("a host in a step", 100, |r| counts(r) != (0, 0));
    assert!(rig.rows.set_paused(true).unwrap());
    let in_step: Vec<String> = rig
        .rows
        .hosts()
        .unwrap()
        .iter()
        .filter(|(_, r)| r.value.record.phase.in_step())
        .map(|(h, _)| h.as_str().to_owned())
        .collect();
    assert!(!in_step.is_empty());
    for _ in 0..400 {
        let tick = rig.tick();
        assert!(
            tick.started.is_empty(),
            "a host was started while paused: {tick:?}"
        );
    }
    // Only the hosts that were in a step have rows, and every one of them finished.
    let rows = rig.rows.hosts().unwrap();
    let names: Vec<&str> = rows.keys().map(WorkerId::as_str).collect();
    assert_eq!(
        names,
        in_step.iter().map(String::as_str).collect::<Vec<_>>()
    );
    for (host, row) in &rows {
        assert_eq!(
            row.value.record.phase,
            HostPhase::Done,
            "{host} should have finished"
        );
    }
    assert!(rig.rows.slot().unwrap().value.is_empty());
    assert!(rig.rows.set_paused(false).unwrap());
    rig.finish(NEW);
}

// Alerts.

/// Catches: an alert sent at every look. The step names a rebuilding node that is past the
/// health limit at every look while the rebuild goes on (an alert is level-triggered); the
/// controller sends it once for the phase. Two storage nodes rebuild for 200 s after their
/// restart, 80 s past the limit: each gets one alert.
#[test]
fn an_alert_the_step_repeats_is_sent_once_for_the_phase() {
    let spec = Spec {
        storage: 2,
        linux: 0,
        macs: 0,
        rebuild: true,
        rebuild_for: (200, 1),
        ..Spec::default()
    };
    let mut rig = Rig::new(Backing::Fake, "alerts", &spec, Policy::default());
    rig.offer(NEW);
    rig.finish(NEW);
    let w = rig.world.lock();
    for host in ["s1", "s2"] {
        let waiting = w
            .alerts
            .iter()
            .filter(|(h, alert)| h == host && *alert == Alert::RebuildWaiting)
            .count();
        assert_eq!(waiting, 1, "{host}: {:?}", w.alerts);
    }
}

// A target stays with the host that began it.

/// Catches: an in-step host stepped toward a release offered after it began. A rollback is
/// offered while `s1` is between the activate order and its confirm: it finishes the forward
/// run on `v2` (against the target of its own run: judging it against `v1` would hold the
/// run, because it reports `v2`), and is then walked back to `v1` under the new generation
/// with a higher order. `s2` still runs `v1` and is never touched.
#[test]
fn a_host_finishes_the_offer_that_began_it_and_is_then_walked_back() {
    let mut rig = Rig::new(Backing::Fake, "target", &spec(2, 0, 0), Policy::default());
    let forward = rig.offer(NEW);
    rig.until("the activate order", 200, |r| {
        r.phase("s1") == HostPhase::Activating
    });
    let back = rig.offer(OLD);
    // The forward restart is s1's one for the forward run; the walk back is another.
    rig.until("the restart", 300, |r| {
        r.phase("s1") == HostPhase::RejoiningStorage
    });
    rig.world.lock().new_run();
    rig.until("s1 walked back", 3_000, |r| {
        let row = row_of(&r.rows, "s1");
        row.record.phase == HostPhase::Done && row.record.generation == back
    });
    let w = rig.world.lock();
    let acts = activates(&w, "s1");
    assert_eq!(
        acts.iter().map(|a| a.1.as_str()).collect::<Vec<_>>(),
        [NEW, OLD]
    );
    assert!(acts[1].0 > acts[0].0);
    assert_eq!(w.host("s1").reported, Release::new(OLD));
    assert!(activates(&w, "s2").is_empty(), "s2 already runs the target");
    assert!(forward < back);
    assert_eq!(rig.phase("s2"), HostPhase::Pending);
}

/// Catches: a host that has done nothing irreversible activating a release that a newer
/// offer has replaced. `d1` is busy (a lease of 600 s), forced after the idle wait, and
/// waiting for its lease when `v3` is offered. It is given back at the next look: uncordoned,
/// its place freed, a warning sent, and `v2` is never ordered on it. The new run picks it
/// after its own idle wait, and the only activation the helper ever sees is `v3`.
#[test]
fn a_host_waiting_for_its_leases_is_given_back_when_a_newer_release_is_offered() {
    let mut rig = Rig::new(Backing::Fake, "newer", &spec(0, 1, 0), Policy::default());
    rig.world.lease("d1", 600);
    let first = rig.offer(NEW);
    rig.until("d1 to wait for its lease", 400, |r| {
        r.phase("d1") == HostPhase::WaitingLeases
    });
    let second = rig.offer("v3");
    assert!(second > first);

    rig.tick();
    assert_eq!(rig.phase("d1"), HostPhase::Pending, "given back");
    assert!(!rig.world.lock().host("d1").cordoned, "uncordoned");
    assert!(
        rig.rows.slot().unwrap().value.is_empty(),
        "its place is free"
    );
    assert_eq!(
        rig.world.lock().alerts,
        [("d1".to_owned(), Alert::GaveBack(GiveBack::Superseded))]
    );

    rig.finish("v3");
    let w = rig.world.lock();
    let acts = activates(&w, "d1");
    assert_eq!(
        acts.iter().map(|a| a.1.as_str()).collect::<Vec<_>>(),
        ["v3"],
        "the replaced release was never ordered"
    );
    assert_eq!(row_of(&rig.rows, "d1").record.generation, second);
    assert_eq!(w.repeats, 0);
}

/// Catches: a claim for a class with no free place spending the order counter, and with it
/// a durable write at every look of a host that waits for one. `x1` holds the one place for
/// hosts (held for a person) and `d1` is found `Selected` by a controller that has just
/// started (its claim was never made): the replay of its row tries the claim, finds the
/// class full without drawing an id, and the step gives `d1` back with no order. The next id
/// drawn is the very next number.
#[test]
fn a_claim_for_a_full_class_draws_no_order_id() {
    let mut rig = Rig::new(Backing::Fake, "full", &spec(0, 2, 0), Policy::default());
    let generation = rig.offer(NEW);
    let row = |phase: HostPhase| HostRow {
        record: HostRecord {
            phase,
            since: rig.world.time(),
            generation,
            order: OrderId::new(rig.epoch, 1),
            boot_before: None,
        },
        commit: Release::new(NEW),
    };
    let absent = Stored {
        value: HostRow::unstarted(),
        version: None,
    };
    let held = row(HostPhase::Held {
        reason: HoldReason::ActivateStalled,
    });
    rig.rows.put_host(&id("x1"), &absent, &held).unwrap();
    rig.rows
        .claim(SlotClass::Host, &id("x1"), held.record.order, 1)
        .unwrap();
    rig.rows
        .put_host(&id("d1"), &absent, &row(HostPhase::Selected))
        .unwrap();

    let before = rig.rows.mint_order(rig.epoch).unwrap();
    rig.world.advance();
    let tick = rig.controller.tick(rig.world.time()).unwrap();
    assert!(tick.errors.is_empty() && !tick.conflict, "{tick:?}");
    let after = rig.rows.mint_order(rig.epoch).unwrap();
    assert_eq!(after.seq, before.seq + 1, "the tick drew an order id");
    assert_eq!(rig.phase("d1"), HostPhase::Pending, "given back");
    assert!(rig.world.lock().given_to("d1").is_empty(), "no order");
    assert!(!rig.rows.slot().unwrap().value.holds(&id("d1")));
}

// What the controller refuses to guess.

/// Catches: a controller that goes on with a policy it cannot keep (no place for any
/// daemon-only host): the tick says so and nothing moves, no row is written, no order given.
#[test]
fn a_policy_that_cannot_be_kept_stops_the_controller() {
    let mut rig = Rig::new(
        Backing::Fake,
        "badpolicy",
        &spec(1, 1, 0),
        Policy::default(),
    );
    rig.offer(NEW);
    // max_hosts_out written as 0 (absent would be the default of one).
    let txn = Txn::new().put("update/policy", vec![0x28, 0x00], Cond::Any);
    rig.cell.store().txn(txn).unwrap().unwrap();
    rig.world.advance();
    let error = rig.controller.tick(rig.world.time()).unwrap_err();
    assert!(
        matches!(error, ControllerError::Policy(PolicyError::NoHostsOut)),
        "{error}"
    );
    assert!(rig.rows.hosts().unwrap().is_empty());
    assert!(rig.world.lock().given.is_empty());
}

/// Hides one host from the facts, as a farm that lost a daemon's registration would.
struct Hide {
    inner: Box<dyn FactsSource>,
    host: String,
    hidden: Arc<Mutex<bool>>,
}

impl FactsSource for Hide {
    fn look(&mut self, now: FarmTime) -> Result<FarmView, SourceError> {
        let mut view = self.inner.look(now)?;
        if *self.hidden.lock().unwrap_or_else(PoisonError::into_inner) {
            view.hosts.retain(|h| h.id.as_str() != self.host);
        }
        Ok(view)
    }
}

/// Catches: a host the rows hold in a step that the farm does not show being forgotten, so
/// that its place is counted free and another host starts beside it. The host is missing and
/// its place is gone from the slot as well (a place lost, as an operator's edit might lose it):
/// the picker, which counts only the hosts it is shown and the places held, sees room. Nothing
/// new starts while the host is missing, it is not moved, and the tick names it; when the farm
/// shows it again it takes its place back and the run goes on.
#[test]
fn a_host_in_a_step_that_the_farm_does_not_show_holds_every_new_start() {
    let mut rig = Rig::new(Backing::Fake, "missing", &spec(0, 3, 0), Policy::default());
    rig.offer(NEW);
    let hidden = Arc::new(Mutex::new(false));
    let hide = Hide {
        inner: rig.world.source(),
        host: "d1".to_owned(),
        hidden: Arc::clone(&hidden),
    };
    rig.controller = Controller::new(
        rig.cell.store(),
        rig.epoch,
        rig.world.transport(),
        Box::new(hide),
    );
    rig.until("d1 in a step", 50, |r| r.phase("d1").in_step());
    *hidden.lock().unwrap() = true;
    rig.rows.release(&id("d1")).unwrap();
    let before = rig.phase("d1");
    for _ in 0..200 {
        let tick = rig.tick();
        assert_eq!(tick.missing, [id("d1")]);
        assert!(
            tick.started.is_empty(),
            "a host was started beside the missing one: {tick:?}"
        );
        assert!(tick.moved.iter().all(|m| m.host != id("d1")));
    }
    assert_eq!(rig.phase("d1"), before, "the missing host was moved");
    assert!(
        rig.rows.slot().unwrap().value.is_empty(),
        "a place was taken beside it"
    );
    *hidden.lock().unwrap() = false;
    rig.finish(NEW);
}

/// Catches: the orders of a row being lost because the farm did not show the host when the
/// restarted controller made its first look. A controller died after it wrote `d1`'s row
/// `Activating` and before it gave the order; the next one cannot see `d1` for three looks
/// (it is named in `missing`, nothing is given it), and when it shows again the order is
/// given, once, with the id of the row. A controller that counts its first look as the
/// replay whether or not the host was there would never give it: the step waits for a
/// report that nothing was asked for, and holds the run at the activate limit.
#[test]
fn a_host_the_farm_does_not_show_at_the_restart_still_gets_the_orders_of_its_row() {
    let mut rig = Rig::new(Backing::Fake, "late", &spec(0, 2, 0), Policy::default());
    let generation = rig.offer(NEW);
    let order = rig.rows.mint_order(rig.epoch).unwrap();
    let row = HostRow {
        record: HostRecord {
            phase: HostPhase::Activating,
            since: rig.world.time(),
            generation,
            order,
            boot_before: None,
        },
        commit: Release::new(NEW),
    };
    let absent = Stored {
        value: HostRow::unstarted(),
        version: None,
    };
    rig.rows.put_host(&id("d1"), &absent, &row).unwrap();
    rig.rows
        .claim(SlotClass::Host, &id("d1"), order, 1)
        .unwrap();
    for host in &mut rig.world.lock().hosts {
        host.cordoned = host.name == "d1";
    }
    let hidden = Arc::new(Mutex::new(true));
    let hide = Hide {
        inner: rig.world.source(),
        host: "d1".to_owned(),
        hidden: Arc::clone(&hidden),
    };
    rig.controller = Controller::new(
        rig.cell.store(),
        rig.epoch,
        rig.world.transport(),
        Box::new(hide),
    );
    for _ in 0..3 {
        rig.world.advance();
        let tick = rig.controller.tick(rig.world.time()).unwrap();
        assert_eq!(tick.missing, [id("d1")]);
        assert!(activates(&rig.world.lock(), "d1").is_empty());
    }
    *hidden.lock().unwrap() = false;
    rig.world.advance();
    rig.controller.tick(rig.world.time()).unwrap();
    assert_eq!(
        activates(&rig.world.lock(), "d1"),
        [(order, NEW.to_owned())]
    );
    rig.finish(NEW);
    assert_eq!(rig.world.lock().host("d1").activations, 1);
}

/// Catches: a clock that restarted below the times the rows hold being trusted. Every limit
/// is `now` less a stored time, and a subtraction against a clock that is behind saturates
/// to zero: no lease wait, no health limit and no restart deadline would ever pass, while
/// the store's own dwell on a planned-down mark ran on. The controller refuses to look on a
/// clock behind the run's start, and on one behind a host's row; it writes nothing and
/// gives no order; on the right clock it goes on.
#[test]
fn a_clock_that_is_behind_the_rows_stops_the_controller() {
    let mut rig = Rig::new(Backing::Fake, "clock", &spec(2, 1, 0), Policy::default());
    rig.offer(NEW);
    let offered = rig.world.time().as_millis();
    let behind = |ms: u64| FarmTime::from_millis(ms);

    let error = rig.controller.tick(behind(offered - 1)).unwrap_err();
    assert!(
        matches!(error, ControllerError::ClockBehind { now_ms, latest_ms }
            if now_ms == offered - 1 && latest_ms == offered),
        "{error}"
    );
    assert!(rig.rows.hosts().unwrap().is_empty());
    assert!(rig.world.lock().given.is_empty());

    rig.until("a host in a step", 50, |r| r.phase("d1").in_step());
    let since = row_of(&rig.rows, "d1").record.since.as_millis();
    assert!(since > offered, "the row is later than the offer");
    let (rows, given) = (rig.rows.hosts().unwrap(), rig.world.lock().given.len());
    let error = rig.controller.tick(behind(since - 1)).unwrap_err();
    assert!(
        matches!(error, ControllerError::ClockBehind { now_ms, latest_ms }
            if now_ms == since - 1 && latest_ms >= since),
        "{error}"
    );
    assert_eq!(rig.rows.hosts().unwrap(), rows, "a row was written");
    assert_eq!(rig.world.lock().given.len(), given, "an order was given");
    rig.finish(NEW);
}

/// Catches: the epoch contract of [`Controller::new`] left to the caller. A controller whose
/// epoch is below an epoch the rows hold would draw ids a host has already seen (the store
/// was restored, or the controller was given the wrong epoch): it stops, naming the row, in
/// the slot's holders and in a host's row alike, and gives no order. The same epoch (a
/// restart) and a higher one (a restore) go on.
#[test]
fn a_controller_whose_epoch_is_below_the_rows_stops() {
    let mut rig = Rig::new(Backing::Fake, "epoch", &spec(2, 0, 0), Policy::default());
    rig.epoch = 5;
    rig.restart();
    rig.offer(NEW);
    rig.tick();
    assert!(rig.rows.slot().unwrap().value.holds(&id("s1")));
    let under = |epoch: u64, rig: &Rig| {
        let before = rig.world.lock().given.len();
        let mut behind = Controller::new(
            rig.cell.store(),
            epoch,
            rig.world.transport(),
            rig.world.source(),
        );
        rig.world.advance();
        let error = behind.tick(rig.world.time()).unwrap_err();
        assert_eq!(rig.world.lock().given.len(), before, "an order was given");
        error
    };
    // The slot's holder was claimed under epoch 5; the host's row has no order yet.
    for epoch in [4, 1] {
        let error = under(epoch, &rig);
        assert!(
            matches!(&error, ControllerError::EpochBehind { row, found: 5, epoch: e }
                if row == "update/slot" && *e == epoch),
            "{error}"
        );
    }
    // The order of a host's row, with the slot's holder gone.
    rig.until("the activate order", 200, |r| {
        r.phase("s1") == HostPhase::Activating
    });
    rig.rows.release(&id("s1")).unwrap();
    let error = under(4, &rig);
    assert!(
        matches!(&error, ControllerError::EpochBehind { row, found: 5, .. }
            if row == "update/host/s1"),
        "{error}"
    );
    for epoch in [5, 6] {
        let mut ahead = Controller::new(
            rig.cell.store(),
            epoch,
            rig.world.transport(),
            rig.world.source(),
        );
        rig.world.advance();
        ahead
            .tick(rig.world.time())
            .expect("a controller at or above the rows' epoch");
    }
}

/// Catches: an order that did not reach the farm being forgotten, or being given again
/// under a new id. The farm refuses the first activate order; the next tick gives the same
/// order (the row's), once it is taken the host restarts once. And when the order was taken
/// but the answer was lost ("swapped, maybe"), giving it again is a repeat the helper
/// refuses: one restart, the same id, never a second activation.
#[test]
fn an_activate_order_is_given_again_with_the_same_id_and_never_a_new_one() {
    for lost_answer in [false, true] {
        let mut rig = Rig::new(Backing::Fake, "again", &spec(0, 1, 0), Policy::default());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let failing = Flaky {
            inner: rig.world.transport(),
            calls: Arc::clone(&calls),
            failures: 1,
            deliver_first: lost_answer,
        };
        rig.controller = Controller::new(
            rig.cell.store(),
            rig.epoch,
            Box::new(failing),
            rig.world.source(),
        );
        rig.offer(NEW);
        let mut errors = Vec::new();
        for _ in 0..40 {
            rig.world.advance();
            let tick = rig.controller.tick(rig.world.time()).expect("a tick");
            errors.extend(tick.errors);
        }
        assert_eq!(errors.len(), 1, "{errors:?}");
        let kbf_server::update::Command::Activate { order } = errors[0].command else {
            panic!("the failed order was {:?}", errors[0].command);
        };
        let calls = calls.lock().unwrap().clone();
        assert!(
            calls.len() >= 2 && calls.iter().all(|o| *o == order),
            "{calls:?}"
        );
        let w = rig.world.lock();
        assert_eq!(w.host("d1").activations, 1);
        assert_eq!(w.repeats, u64::from(lost_answer));
        assert_eq!(activates(&w, "d1")[0].0, order);
        assert_eq!(row_of(&rig.rows, "d1").record.order, order);
    }
}

/// A transport whose first activate order fails, after it was taken or before.
struct Flaky {
    inner: Box<dyn Transport>,
    calls: Arc<Mutex<Vec<OrderId>>>,
    failures: u32,
    deliver_first: bool,
}

impl Transport for Flaky {
    fn cordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.inner.cordon(host)
    }

    fn uncordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.inner.uncordon(host)
    }

    fn mark_down(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.inner.mark_down(host, member)
    }

    fn undown(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.inner.undown(host, member)
    }

    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.calls.lock().unwrap().push(order);
        if self.failures > 0 {
            self.failures -= 1;
            if self.deliver_first {
                self.inner.activate(host, order, release)?;
            }
            return Err(TransportError::new("the answer did not come"));
        }
        self.inner.activate(host, order, release)
    }

    fn confirm(&mut self, host: &WorkerId, order: OrderId) -> Result<(), TransportError> {
        self.inner.confirm(host, order)
    }

    fn alert(&mut self, host: &WorkerId, alert: &Alert) -> Result<(), TransportError> {
        self.inner.alert(host, alert)
    }
}
