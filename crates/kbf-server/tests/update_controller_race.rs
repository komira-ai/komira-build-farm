//! Two controllers over one store (`docs/design/split-mode-updates.md` sections 2.9 and 6.1:
//! the new frontend's controller starts while the old one is going, and an old controller is
//! fenced with the rest of its farm). Every write to a row names the version the controller
//! read, so a controller that was overtaken is refused, re-reads and never overwrites.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, PoisonError};

use kbf_map::MemberId;
use kbf_server::update::{
    Alert, Controller, ControllerError, HoldReason, HostPhase, HostRecord, HostRow, OrderId,
    Policy, Release, RowError, Rows, SlotClass, Stored, Transport, TransportError,
};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Fenced, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::WorkerId;
use support::harness::{
    Backing, Rig, check_world, check_world_two_controllers, controller, finished, row_of,
};
use support::world::{Gave, Spec};
use support::{NEW, id};

fn spec() -> Spec {
    Spec {
        storage: 2,
        linux: 2,
        macs: 0,
        ..Spec::default()
    }
}

/// A store that runs a hook once, just before the first write to a key under `prefix` goes
/// through: the moment between a controller's reads and its write.
struct Racing {
    inner: Arc<dyn Store>,
    prefix: &'static str,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Store for Racing {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let written = |txn: &Txn| {
            txn.ops().iter().any(|op| {
                op.key_cond()
                    .is_some_and(|(key, _)| key.starts_with(self.prefix))
            })
        };
        if batch.iter().any(written)
            && let Some(hook) = self
                .hook
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            hook();
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// A store that runs a hook once, just after a `scan_prefix` of `prefix` has read its rows:
/// the rows the reader holds are then as old as the hook's writes are new.
struct AfterScan {
    inner: Arc<dyn Store>,
    prefix: &'static str,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Store for AfterScan {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let rows = self.inner.scan_prefix(prefix)?;
        if prefix == self.prefix
            && let Some(hook) = self
                .hook
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            hook();
        }
        Ok(rows)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// Counts the orders given through it, and refuses the activate order while told to.
struct Counting {
    inner: Box<dyn Transport>,
    given: Arc<AtomicU64>,
    refuse_activate: Arc<AtomicBool>,
}

impl Counting {
    fn one(&self) {
        self.given.fetch_add(1, Ordering::SeqCst);
    }
}

impl Transport for Counting {
    fn cordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.one();
        self.inner.cordon(host)
    }

    fn uncordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.one();
        self.inner.uncordon(host)
    }

    fn mark_down(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.one();
        self.inner.mark_down(host, member)
    }

    fn undown(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.one();
        self.inner.undown(host, member)
    }

    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.one();
        if self.refuse_activate.load(Ordering::SeqCst) {
            return Err(TransportError::new("the farm is not answering"));
        }
        self.inner.activate(host, order, release)
    }

    fn confirm(&mut self, host: &WorkerId, order: OrderId) -> Result<(), TransportError> {
        self.one();
        self.inner.confirm(host, order)
    }

    fn alert(&mut self, host: &WorkerId, alert: &Alert) -> Result<(), TransportError> {
        self.one();
        self.inner.alert(host, alert)
    }
}

fn names(hosts: &[WorkerId]) -> Vec<&str> {
    hosts.iter().map(WorkerId::as_str).collect()
}

/// Catches: a controller that overwrites a host's row it read before another controller
/// moved it, and gives the order of its own stale decision. A reads the rows, picks `s1` and
/// `d1`, and just before its write B (which read the same rows) starts them and wins: A's
/// write is refused, A gives no order at all and says so, the rows are B's, and A's next look
/// reads them and goes on from there. Then they take turns, and every host is activated once
/// with one id.
#[test]
fn the_controller_that_loses_a_race_for_a_host_moves_nothing() {
    for backing in [Backing::Fake, Backing::Sqlite] {
        let rig = Rig::new(backing, "race", &spec(), Policy::default());
        let store = rig.cell.store();
        rig.offer(NEW);

        let b = Arc::new(Mutex::new(controller(&store, rig.epoch, &rig.world)));
        let b_tick = Arc::new(Mutex::new(None));
        let hook = {
            let (b, b_tick, world) = (Arc::clone(&b), Arc::clone(&b_tick), rig.world.clone());
            move || {
                let mut b = b.lock().unwrap_or_else(PoisonError::into_inner);
                let tick = b.tick(world.time()).expect("B's tick");
                *b_tick.lock().unwrap_or_else(PoisonError::into_inner) = Some(tick);
            }
        };
        let racing: Arc<dyn Store> = Arc::new(Racing {
            inner: Arc::clone(&store),
            prefix: "update/host/",
            hook: Mutex::new(Some(Box::new(hook))),
        });
        let given = Arc::new(AtomicU64::new(0));
        let transport = Counting {
            inner: rig.world.transport(),
            given: Arc::clone(&given),
            refuse_activate: Arc::new(AtomicBool::new(false)),
        };
        let mut a = Controller::new(racing, rig.epoch, Box::new(transport), rig.world.source());

        rig.world.advance();
        let a_tick = a.tick(rig.world.time()).expect("A's tick");
        let b_tick = b_tick
            .lock()
            .unwrap()
            .take()
            .expect("B ticked inside A's write");
        assert_eq!(names(&b_tick.started), ["s1", "d1"], "{b_tick:?}");
        assert!(a_tick.conflict, "A was not refused: {a_tick:?}");
        assert!(
            a_tick.started.is_empty() && a_tick.moved.is_empty(),
            "{a_tick:?}"
        );
        assert_eq!(
            given.load(Ordering::SeqCst),
            0,
            "A gave an order for a move it lost"
        );
        assert_eq!(rig.phase("s1"), HostPhase::Selected);
        assert_eq!(rig.phase("d1"), HostPhase::Selected);
        assert!(
            rig.rows.slot().unwrap().value.holds(&id("s1")),
            "B's claim stands"
        );

        // They take turns from here, each reading what the other wrote.
        let target = Release::new(NEW);
        let mut n = 0;
        loop {
            n += 1;
            rig.world.advance();
            for who in [&mut a as &mut Controller, &mut *b.lock().unwrap()] {
                let tick = who.tick(rig.world.time()).expect("a tick");
                assert!(!tick.conflict, "turns taken: {tick:?}");
            }
            check_world(&rig.world.lock(), &rig.rows, 1, "turns");
            let generation = rig.rows.desired().unwrap().value.unwrap().generation;
            if finished(&rig.world.lock(), &rig.rows, generation, &target) {
                break;
            }
            assert!(n < 5_000, "the run did not end");
        }
        let w = rig.world.lock();
        for host in &w.hosts {
            assert_eq!(host.activations, 1, "{}", host.name);
            let ids: BTreeSet<OrderId> = w
                .given_to(&host.name)
                .into_iter()
                .filter_map(|g| match g {
                    Gave::Activate(order, _) => Some(*order),
                    _ => None,
                })
                .collect();
            assert_eq!(ids.len(), 1, "{} was given {ids:?}", host.name);
        }
    }
}

/// Catches: two controllers running at once breaking the places or the one activation per
/// host. They tick at the same moments on two threads over one store, five times, on both
/// backends; a refused write is fine, a second storage host in a step, a host past its place,
/// a node activated with a lease open, a host activated twice or under two ids is not.
#[test]
fn two_controllers_ticking_at_once_keep_the_places_and_activate_each_host_once() {
    for backing in [Backing::Fake, Backing::Sqlite] {
        for round in 0..5 {
            let rig = Rig::new(backing, "threads", &spec(), Policy::default());
            let store = rig.cell.store();
            rig.offer(NEW);
            let barrier = Arc::new(Barrier::new(2));
            let done = Arc::new(AtomicBool::new(false));
            let target = Release::new(NEW);
            let workers: Vec<_> = (0..2)
                .map(|who| {
                    let mut me = controller(&store, rig.epoch, &rig.world);
                    let (barrier, done) = (Arc::clone(&barrier), Arc::clone(&done));
                    let (world, rows, target) =
                        (rig.world.clone(), rig.rows.clone(), target.clone());
                    std::thread::spawn(move || {
                        let mut turns = 0;
                        loop {
                            barrier.wait();
                            if who == 0 {
                                world.advance();
                            }
                            barrier.wait();
                            me.tick(world.time()).expect("a tick");
                            barrier.wait();
                            if who == 0 {
                                check_world_two_controllers(&world.lock(), &rows, 1, "threads");
                                let generation = rows.desired().unwrap().value.unwrap().generation;
                                if finished(&world.lock(), &rows, generation, &target) {
                                    done.store(true, Ordering::SeqCst);
                                }
                            }
                            barrier.wait();
                            turns += 1;
                            assert!(turns < 5_000, "the run did not end");
                            if done.load(Ordering::SeqCst) {
                                break;
                            }
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            }
            let w = rig.world.lock();
            for host in &w.hosts {
                assert_eq!(host.activations, 1, "round {round}: {}", host.name);
                let ids: BTreeSet<OrderId> = w
                    .given_to(&host.name)
                    .into_iter()
                    .filter_map(|g| match g {
                        Gave::Activate(order, _) => Some(*order),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    ids.len(),
                    1,
                    "round {round}: {} was given {ids:?}",
                    host.name
                );
            }
        }
    }
}

/// Catches: a controller that was taken over going on giving orders. Its store handle is
/// fenced when the next writer takes the generation (the old frontend's farm is fenced the
/// same way): its next tick fails, and the farm gets nothing from it, not even the orders
/// that undo a step, which precede the row they belong to.
#[test]
fn a_controller_that_was_fenced_out_gives_no_order() {
    let rig = Rig::new(Backing::Fake, "fenced", &spec(), Policy::default());
    let store = rig.cell.store();
    let old: Arc<dyn Store> =
        Arc::new(Fenced::take(Arc::clone(&store), None).expect("generation 1"));
    let given = Arc::new(AtomicU64::new(0));
    let transport = Counting {
        inner: rig.world.transport(),
        given: Arc::clone(&given),
        refuse_activate: Arc::new(AtomicBool::new(false)),
    };
    let mut a = Controller::new(
        Arc::clone(&old),
        rig.epoch,
        Box::new(transport),
        rig.world.source(),
    );
    // A busy host that will have to be given back: the undo orders precede the row.
    rig.world.lease("d1", 3_000);
    rig.offer(NEW);
    let mut now = 0;
    while rig.phase("d1") != HostPhase::WaitingLeases {
        now += 1;
        rig.world.advance();
        a.tick(rig.world.time())
            .expect("a tick while it is the writer");
        assert!(now < 1_000, "{:?}", support::harness::phases(&rig.rows));
    }
    let before = given.load(Ordering::SeqCst);

    // The next writer takes the generation; A is fenced.
    let _next = Fenced::take(Arc::clone(&store), None).expect("generation 2");
    let since = row_of(&rig.rows, "d1").record.since.as_millis() / 1000;
    let limit = Policy::default().lease_limit.as_secs();
    while rig.world.lock().now < since + limit + 5 {
        rig.world.advance();
        let moved = a.tick(rig.world.time());
        // A's ticks may do nothing and succeed (no row to write); at the lease limit the
        // give-back would write, and is refused.
        if let Err(error) = moved {
            assert!(
                matches!(
                    error,
                    ControllerError::Rows(RowError::Store(StoreError::Fenced))
                ),
                "{error}"
            );
        }
    }
    assert_eq!(
        given.load(Ordering::SeqCst),
        before,
        "a fenced controller gave an order"
    );
    assert_eq!(
        rig.phase("d1"),
        HostPhase::WaitingLeases,
        "a fenced controller moved a row"
    );
}

/// Catches: a controller that was taken over giving the orders of the rows it finds, which
/// no write of its own precedes. A host is in `Activating`, its restart 200 s away. A
/// controller that has not yet looked (its first tick gives the orders of every row it
/// finds again), and one whose order failed (the next tick gives it again), are fenced
/// out by the next writer: each tick fails, and the farm is given nothing. On both
/// backends, since the fence is a transaction that writes nothing.
#[test]
fn a_controller_that_was_fenced_out_replays_no_order() {
    for backing in [Backing::Fake, Backing::Sqlite] {
        for after_a_failure in [false, true] {
            let label = format!("{backing:?} after_a_failure={after_a_failure}");
            let spec = Spec {
                storage: 0,
                linux: 1,
                macs: 0,
                restart: (200, 1),
                ..Spec::default()
            };
            let rig = Rig::new(backing, "replay", &spec, Policy::default());
            let store = rig.cell.store();
            let old: Arc<dyn Store> =
                Arc::new(Fenced::take(Arc::clone(&store), None).expect("generation 1"));
            let refuse = Arc::new(AtomicBool::new(after_a_failure));
            let given = Arc::new(AtomicU64::new(0));
            let counting = || Counting {
                inner: rig.world.transport(),
                given: Arc::clone(&given),
                refuse_activate: Arc::clone(&refuse),
            };
            let mut a = Controller::new(
                Arc::clone(&old),
                rig.epoch,
                Box::new(counting()),
                rig.world.source(),
            );
            rig.offer(NEW);
            let mut n = 0;
            while rig.phase("d1") != HostPhase::Activating {
                n += 1;
                assert!(n < 1_000, "{label}");
                rig.world.advance();
                a.tick(rig.world.time()).expect("a tick as the writer");
            }
            // The controller that will look next: A itself, with the order that failed to
            // give again, or one that has never looked.
            let mut next = if after_a_failure {
                a
            } else {
                Controller::new(
                    Arc::clone(&old),
                    rig.epoch,
                    Box::new(counting()),
                    rig.world.source(),
                )
            };
            refuse.store(false, Ordering::SeqCst);
            let _taker = Fenced::take(Arc::clone(&store), None).expect("generation 2");
            let before = rig.world.lock().given.len();
            let orders = given.load(Ordering::SeqCst);

            rig.world.advance();
            let error = next
                .tick(rig.world.time())
                .expect_err(&format!("{label}: a fenced controller looked"));
            assert!(
                matches!(
                    error,
                    ControllerError::Rows(RowError::Store(StoreError::Fenced))
                ),
                "{label}: {error}"
            );
            assert_eq!(
                rig.world.lock().given.len(),
                before,
                "{label}: a fenced controller gave an order"
            );
            assert_eq!(given.load(Ordering::SeqCst), orders, "{label}");
        }
    }
}

/// Catches: a controller that frees a place another controller has just claimed, because
/// its rows are older than its slot. A reads the rows; then B (which read the same rows)
/// starts `s1` and `d1` and claims their places; then A reads the slot, finds two holders
/// that its rows say are in no step, and frees them. With the slot read first a holder it
/// shows always has a row at least as new as the slot: A's slot is the one from before B's
/// claims, it frees nothing, and its own starts are refused because B's rows exist.
#[test]
fn a_stale_read_of_the_rows_never_frees_a_place_another_controller_claimed() {
    for backing in [Backing::Fake, Backing::Sqlite] {
        let rig = Rig::new(backing, "stale", &spec(), Policy::default());
        let store = rig.cell.store();
        rig.offer(NEW);

        let mut b = controller(&store, rig.epoch, &rig.world);
        let b_tick = Arc::new(Mutex::new(None));
        let hook = {
            let (b_tick, world) = (Arc::clone(&b_tick), rig.world.clone());
            move || {
                let tick = b.tick(world.time()).expect("B's tick");
                *b_tick.lock().unwrap_or_else(PoisonError::into_inner) = Some(tick);
            }
        };
        let scanning: Arc<dyn Store> = Arc::new(AfterScan {
            inner: Arc::clone(&store),
            prefix: "update/host/",
            hook: Mutex::new(Some(Box::new(hook))),
        });
        let mut a = Controller::new(
            scanning,
            rig.epoch,
            rig.world.transport(),
            rig.world.source(),
        );

        rig.world.advance();
        let a_tick = a.tick(rig.world.time()).expect("A's tick");
        let b_tick = b_tick
            .lock()
            .unwrap()
            .take()
            .expect("B ticked inside A's read");
        assert_eq!(names(&b_tick.started), ["s1", "d1"], "{b_tick:?}");
        assert!(a_tick.conflict, "A's starts were not refused: {a_tick:?}");
        let slot = rig.rows.slot().unwrap().value;
        assert!(
            slot.holds(&id("s1")) && slot.holds(&id("d1")),
            "A freed a place B had claimed: {slot:?}"
        );
        check_world(&rig.world.lock(), &rig.rows, 1, "stale");
    }
}

/// Catches: a claim that gives up when another writer got to the slot first, or that takes
/// a place its class does not have, and a host left in a step without one. Three Linux
/// hosts and three places; as the controller makes its first claim another controller's
/// host takes a place (its row says `Held`, so its place is not freed). The claim is
/// refused for the version it read, reads again, and gets a place; so does the second; the
/// third finds the class full and is refused, and at the next look its step gives it back
/// to `Pending` with no order, since a host with no place was never cordoned. The slot never
/// holds more than three.
#[test]
fn a_claim_that_loses_the_race_for_a_place_reads_again_and_a_full_class_refuses_it() {
    let spec = Spec {
        storage: 0,
        linux: 3,
        macs: 0,
        ..Spec::default()
    };
    let policy = Policy {
        max_hosts_out: 3,
        ..Policy::default()
    };
    let rig = Rig::new(Backing::Fake, "claim", &spec, policy);
    let store = rig.cell.store();
    let generation = rig.offer(NEW);
    let hook = {
        let (rows, world, epoch) = (Rows::new(Arc::clone(&store)), rig.world.clone(), rig.epoch);
        move || {
            let order = OrderId::new(epoch, 900);
            let held = HostRow {
                record: HostRecord {
                    phase: HostPhase::Held {
                        reason: HoldReason::ActivateStalled,
                    },
                    since: world.time(),
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
            rows.put_host(&id("x1"), &absent, &held).unwrap();
            assert!(
                rows.claim(SlotClass::Host, &id("x1"), order, 3)
                    .unwrap()
                    .is_some()
            );
        }
    };
    let racing: Arc<dyn Store> = Arc::new(Racing {
        inner: Arc::clone(&store),
        prefix: "update/slot",
        hook: Mutex::new(Some(Box::new(hook))),
    });
    let mut a = Controller::new(racing, rig.epoch, rig.world.transport(), rig.world.source());

    rig.world.advance();
    let first = a
        .tick(rig.world.time())
        .expect("a lost race is not an error");
    assert_eq!(first.started.len(), 3, "{first:?}");
    let slot = rig.rows.slot().unwrap().value;
    assert_eq!(slot.hosts.len(), 3, "{slot:?}");
    assert!(slot.holds(&id("x1")), "the other controller's place stands");
    let unplaced: Vec<&WorkerId> = first.started.iter().filter(|h| !slot.holds(h)).collect();
    assert_eq!(unplaced.len(), 1, "{slot:?} for {:?}", first.started);
    assert_eq!(
        unplaced[0],
        first.started.last().unwrap(),
        "the claims are made in order: the last found the class full"
    );

    rig.world.advance();
    a.tick(rig.world.time()).expect("a tick");
    let left = unplaced[0].as_str();
    assert_eq!(rig.phase(left), HostPhase::Pending, "given back");
    assert!(
        rig.world.lock().given_to(left).is_empty(),
        "an order for a host that never had a place"
    );
    for placed in first.started.iter().filter(|h| slot.holds(h)) {
        assert!(rig.phase(placed.as_str()).in_step(), "{placed}");
    }
    assert_eq!(rig.rows.slot().unwrap().value.hosts.len(), 3);
}

/// Catches: a release decided from an older look that frees a place claimed since. `d1`
/// finished and its controller died before it freed the place; A's look sees the place held
/// by a host that is in no step and frees it; just before A's write another controller frees
/// that place and claims it again for `d1` (a newer claim, as a run that walks `d1` again
/// makes). A's write is refused, A reads the slot again, finds a claim that is not the one
/// it decided on, and leaves it. A release by the host alone would free the newer claim.
#[test]
fn a_place_claimed_again_since_the_look_is_not_freed_by_the_release_the_look_decided() {
    let spec = Spec {
        storage: 0,
        linux: 1,
        macs: 0,
        ..Spec::default()
    };
    let rig = Rig::new(Backing::Fake, "reclaim", &spec, Policy::default());
    let store = rig.cell.store();
    let generation = rig.offer(NEW);
    let done = HostRow {
        record: HostRecord {
            phase: HostPhase::Done,
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
    rig.rows.put_host(&id("d1"), &absent, &done).unwrap();
    let older = OrderId::new(rig.epoch, 2);
    rig.rows
        .claim(SlotClass::Host, &id("d1"), older, 1)
        .unwrap();

    let newer = OrderId::new(rig.epoch, 50);
    let hook = {
        let rows = Rows::new(Arc::clone(&store));
        move || {
            rows.release(&id("d1")).unwrap();
            let claimed = rows.claim(SlotClass::Host, &id("d1"), newer, 1).unwrap();
            assert!(claimed.is_some());
        }
    };
    let racing: Arc<dyn Store> = Arc::new(Racing {
        inner: store,
        prefix: "update/slot",
        hook: Mutex::new(Some(Box::new(hook))),
    });
    let mut a = Controller::new(racing, rig.epoch, rig.world.transport(), rig.world.source());

    rig.world.advance();
    a.tick(rig.world.time()).expect("a tick");
    let slot = rig.rows.slot().unwrap().value;
    assert_eq!(
        slot.holder(&id("d1")).map(|h| h.order),
        Some(newer),
        "A freed the place the other controller claimed: {slot:?}"
    );
}
