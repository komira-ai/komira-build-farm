//! A controller that dies (`docs/design/split-mode-updates.md` section 5.3, C20 and C28):
//! killed at every boundary of a run, restarted over the rows, and restored from a backup.
//! The rule under test is the design's: the record is written before the action, so a
//! controller that restarts resumes at the host's phase, activates no host twice, loses no
//! confirm, and never gives an order id it gave before.
//!
//! **The sweep.** A run is made once without a fault to count its operations (every write
//! to the store and every order to the farm). Then it is made again once for every operation:
//! the controller is cut off just before that operation, so that the operation and everything
//! after it never happen, which is what a process killed at that point leaves. A new
//! controller starts over the same store and farm and finishes the run. Every operation is a
//! boundary: the row written but the order not given, the order given but the next row not
//! written, the order id drawn but unused, the slot claimed but the host not moved.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_map::MemberId;
use kbf_server::update::{
    Alert, Controller, HoldReason, HostPhase, HostRow, OrderId, Policy, Release, Stored, Transport,
    TransportError,
};
use kbf_state::sqlite::{backup, restore};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Op, Revision, ScanPage, ScanRange, Store, StoreError,
    StoreTime, Txn, Versioned,
};
use kbf_types::WorkerId;
use support::harness::{Backing, Rig, check_world, finished, row_of};
use support::world::{Gave, Inner, Spec};
use support::{NEW, OLD, id};

fn spec() -> Spec {
    Spec {
        storage: 2,
        linux: 1,
        macs: 1,
        ..Spec::default()
    }
}

fn policy() -> Policy {
    Policy {
        max_hosts_out: 2,
        ..Policy::default()
    }
}

/// Counts the operations of a controller and cuts it off before the one numbered `limit`.
struct Fuse {
    limit: Option<u64>,
    spent: AtomicU64,
    blocked: Mutex<Option<String>>,
    /// The kind of every operation asked for, in order.
    kinds: Mutex<Vec<String>>,
}

impl Fuse {
    fn new(limit: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            limit,
            spent: AtomicU64::new(0),
            blocked: Mutex::new(None),
            kinds: Mutex::new(Vec::new()),
        })
    }

    /// An operation of `kind` (on `host`, if it is an order) is about to happen: whether it
    /// may. Once one is refused, so is every later one.
    fn allow(&self, kind: &str, host: Option<&WorkerId>) -> bool {
        let n = self.spent.fetch_add(1, Ordering::SeqCst);
        let named = host.map_or_else(|| kind.to_owned(), |h| format!("{kind} {h}"));
        self.kinds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(named);
        if self.limit.is_some_and(|limit| n >= limit) {
            let mut blocked = self.blocked.lock().unwrap_or_else(PoisonError::into_inner);
            blocked.get_or_insert_with(|| kind.to_owned());
            return false;
        }
        true
    }

    fn blown(&self) -> bool {
        self.blocked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    fn killed_at(&self) -> Option<String> {
        self.blocked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn spent(&self) -> u64 {
        self.spent.load(Ordering::SeqCst)
    }
}

/// What a transaction is, by its first operation.
fn describe(txn: &Txn) -> String {
    match txn.ops().first() {
        Some(Op::Put { key, .. } | Op::Delete { key, .. }) if key.starts_with("update/host/") => {
            "row".to_owned()
        }
        Some(Op::Put { key, .. } | Op::Delete { key, .. }) if key == "update/slot" => {
            "slot".to_owned()
        }
        Some(Op::Next { .. }) => "order".to_owned(),
        other => format!("{other:?}"),
    }
}

struct Fused {
    inner: Arc<dyn Store>,
    fuse: Arc<Fuse>,
}

impl Store for Fused {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let kind = batch.first().map(describe).unwrap_or_default();
        if !self.fuse.allow(&kind, None) {
            return Err(StoreError::Failed("the process was killed".to_owned()));
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

struct FusedTransport {
    inner: Box<dyn Transport>,
    fuse: Arc<Fuse>,
}

impl FusedTransport {
    fn go(&self, kind: &str, host: &WorkerId) -> Result<(), TransportError> {
        if self.fuse.allow(kind, Some(host)) {
            Ok(())
        } else {
            Err(TransportError::new("the process was killed"))
        }
    }
}

impl Transport for FusedTransport {
    fn cordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.go("cordon", host)?;
        self.inner.cordon(host)
    }

    fn uncordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.go("uncordon", host)?;
        self.inner.uncordon(host)
    }

    fn mark_down(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.go("mark_down", host)?;
        self.inner.mark_down(host, member)
    }

    fn undown(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.go("undown", host)?;
        self.inner.undown(host, member)
    }

    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.go("activate", host)?;
        self.inner.activate(host, order, release)
    }

    fn confirm(&mut self, host: &WorkerId, order: OrderId) -> Result<(), TransportError> {
        self.go("confirm", host)?;
        self.inner.confirm(host, order)
    }

    fn alert(&mut self, host: &WorkerId, alert: &Alert) -> Result<(), TransportError> {
        self.go("alert", host)?;
        self.inner.alert(host, alert)
    }
}

/// The activate orders of the run that drove hosts to `release`, by host.
fn orders_of(w: &Inner, release: &str) -> BTreeMap<String, BTreeSet<OrderId>> {
    let mut all: BTreeMap<String, BTreeSet<OrderId>> = BTreeMap::new();
    for given in &w.given {
        if let Gave::Activate(order, to) = &given.order
            && to == release
        {
            all.entry(given.host.clone()).or_default().insert(*order);
        }
    }
    all
}

/// What a run ended with, and where it was cut off.
struct Outcome {
    operations: u64,
    killed_at: Option<String>,
    /// The kind of every operation asked for, in order.
    kinds: Vec<String>,
}

/// Which run is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    /// Every host is updated.
    Forward,
    /// The forward run is made first, and then walked back.
    Rollback,
    /// A host holds a lease for longer than the lease wait: it is forced, waits, and is given
    /// back to `Pending`, again and again until its lease ends and it is updated.
    GiveBack,
}

/// One run in which the controller is cut off before the operation `kill`, restarted over
/// the same rows, and finishes. `None` is the run without a fault, which counts the
/// operations.
fn run_killed_at(backing: Backing, scenario: Scenario, kill: Option<u64>) -> Outcome {
    let rollback = scenario == Scenario::Rollback;
    let policy = Policy {
        lease_limit: Duration::from_secs(if scenario == Scenario::GiveBack {
            30
        } else {
            15 * 60
        }),
        ..policy()
    };
    let mut rig = Rig::new(backing, "sweep", &spec(), policy);
    if scenario == Scenario::GiveBack {
        // Forced when the idle wait ends (between 2 and 4 minutes in, however long the other
        // hosts' steps take), given back 30 s later, and forced again, for 400 s in all.
        rig.world.lease("d1", 400);
    }
    if rollback {
        rig.offer(NEW);
        rig.finish(NEW);
        rig.world.lock().new_run();
    }
    let to = if rollback { OLD } else { NEW };
    let target = Release::new(to);
    let fuse = Fuse::new(kill);
    let store: Arc<dyn Store> = Arc::new(Fused {
        inner: rig.cell.store(),
        fuse: Arc::clone(&fuse),
    });
    let transport = Box::new(FusedTransport {
        inner: rig.world.transport(),
        fuse: Arc::clone(&fuse),
    });
    rig.controller = Controller::new(store, rig.epoch, transport, rig.world.source());
    let generation = rig.offer(to);
    // The controller runs until it is cut off, or the run is over.
    loop {
        rig.world.advance();
        let tick = rig.controller.tick(rig.world.time());
        if tick.is_err() || fuse.blown() {
            break;
        }
        check_world(
            &rig.world.lock(),
            &rig.rows,
            rig.policy.max_hosts_out,
            "before the kill",
        );
        if finished(&rig.world.lock(), &rig.rows, generation, &target) {
            break;
        }
        assert!(
            rig.world.lock().now < support::NOW + 20_000,
            "the run did not end"
        );
    }
    let operations = fuse.spent();
    let killed_at = fuse.killed_at();
    let kinds = fuse
        .kinds
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if killed_at.is_some() {
        rig.restart();
    }
    rig.finish(to);

    // The run ended as it must, however it was cut.
    let w = rig.world.lock();
    let label = format!("{backing:?} {scenario:?} kill={kill:?} ({killed_at:?})");
    if scenario == Scenario::GiveBack {
        let backs = gave_back(&w, "d1");
        assert!(
            backs >= 1,
            "{label}: d1 was never given back: {:?}",
            w.given
                .iter()
                .filter(|g| g.host == "d1")
                .map(|g| (g.at - support::NOW, &g.order))
                .collect::<Vec<_>>()
        );
    }
    let orders = orders_of(&w, to);
    let mut every = BTreeSet::new();
    for host in &w.hosts {
        assert_eq!(
            host.activations, 1,
            "{label}: {} was restarted {} times",
            host.name, host.activations
        );
        assert_eq!(host.reported, target, "{label}: {}", host.name);
        let mine = orders
            .get(&host.name)
            .unwrap_or_else(|| panic!("{label}: {} had no order", host.name));
        assert_eq!(mine.len(), 1, "{label}: {} was given {mine:?}", host.name);
        let order = *mine.iter().next().unwrap();
        assert!(
            every.insert(order),
            "{label}: {order:?} was given to two hosts"
        );
        let confirmed = w
            .given_to(&host.name)
            .iter()
            .any(|g| **g == Gave::Confirm(order));
        assert!(confirmed, "{label}: the confirm of {} was lost", host.name);
        assert_eq!(
            row_of(&rig.rows, &host.name).record.order,
            order,
            "{label}: {}",
            host.name
        );
        assert_eq!(
            row_of(&rig.rows, &host.name).record.generation,
            generation,
            "{label}"
        );
    }
    Outcome {
        operations,
        killed_at,
        kinds,
    }
}

/// How often `host` was uncordoned without having been activated since its last cordon: a
/// give-back. (The uncordon that ends a host's update follows its activate order.)
fn gave_back(w: &Inner, host: &str) -> usize {
    let mut cordoned_without_activation = false;
    let mut backs = 0;
    for order in w.given_to(host) {
        match order {
            Gave::Cordon => cordoned_without_activation = true,
            Gave::Activate(..) => cordoned_without_activation = false,
            Gave::Uncordon if cordoned_without_activation => {
                backs += 1;
                cordoned_without_activation = false;
            }
            _ => {}
        }
    }
    backs
}

/// Every `stride`th operation of the run: the controller is cut off before it, and a new one
/// finishes the run. The sweep must have reached each kind of operation.
fn sweep(backing: Backing, scenario: Scenario, stride: usize) {
    let mut cut_off_at = BTreeSet::new();
    let whole = run_killed_at(backing, scenario, None);
    assert!(whole.operations > 40, "{} operations", whole.operations);
    for kill in (0..=whole.operations).step_by(stride) {
        let outcome = run_killed_at(backing, scenario, Some(kill));
        if let Some(kind) = outcome.killed_at {
            cut_off_at.insert(kind);
        }
    }
    // Every kind of operation is reached when every operation is; a sample reaches the
    // common ones.
    let kinds: &[&str] = if stride == 1 {
        &[
            "row",
            "slot",
            "order",
            "cordon",
            "mark_down",
            "activate",
            "confirm",
            "uncordon",
        ]
    } else {
        &["row", "slot", "order"]
    };
    for kind in kinds {
        assert!(
            cut_off_at.contains(*kind),
            "nothing was cut off at a {kind}: {cut_off_at:?}"
        );
    }
}

/// Catches: every way a controller can lose its place. Cut off before each of the run's
/// operations in turn — each row write, each claim and release of a place, each order id
/// drawn, each order to the farm — a new controller over the same rows finishes the run: no
/// host is restarted twice (it asserts at every second and at the end), none is given two
/// order ids, no confirm is lost, no two hosts share an id, the slot ends empty.
#[test]
fn a_controller_killed_before_any_operation_is_resumed_by_the_next_one() {
    sweep(Backing::Fake, Scenario::Forward, 1);
}

/// Catches: the same in a rollback, which walks hosts the forward run left `Done` and draws
/// its ids above the forward run's.
#[test]
fn a_rollback_killed_before_any_operation_is_resumed_by_the_next_controller() {
    sweep(Backing::Fake, Scenario::Rollback, 1);
}

/// Catches: the same in a run with a give-back in it. A host that outlasts the lease wait is
/// put back (the uncordon precedes the row that says so, so that a death between them
/// repeats the uncordon and loses nothing), its place freed, and it is forced again until its
/// lease ends. Cut off before each operation of that run, among them the uncordon of a
/// give-back, the row that follows it and the release of the place after that, a new
/// controller finishes the run with every host restarted once and no host left cordoned.
#[test]
fn a_controller_killed_in_a_run_with_a_give_back_is_resumed_by_the_next_one() {
    sweep(Backing::Fake, Scenario::GiveBack, 1);
}

/// Catches: the same on a SQLite file, where an acknowledged write is a synced commit and a
/// version is the store's revision. Every fourth operation: each write waits for a sync, and
/// the fake's sweep above covers every one; the two that follow cover the boundaries that a
/// sample can miss.
#[test]
fn a_controller_killed_before_any_operation_is_resumed_on_a_sqlite_file() {
    sweep(Backing::Sqlite, Scenario::Forward, 4);
}

/// Catches: a place left held by a host that has finished. The controller dies between the
/// row that says `Done` and the release of the host's place; the next one finds the place
/// held by a host that is in no step and frees it. Without that the storage place would stay
/// taken for ever and the second storage host could never start: the run would not end. The
/// cut is found, not counted: the first row written straight before a write of the slot is
/// the one that finishes a host (a claim's row is followed by the order id it draws). Not
/// by sampling, and on the fake and on a SQLite file.
#[test]
fn a_place_left_by_a_finished_host_is_freed_by_the_next_controller() {
    place_left_by_a_finished_host(Backing::Fake);
}

#[test]
fn a_place_left_by_a_finished_host_is_freed_on_a_sqlite_file() {
    place_left_by_a_finished_host(Backing::Sqlite);
}

fn place_left_by_a_finished_host(backing: Backing) {
    let whole = run_killed_at(backing, Scenario::Forward, None);
    let cut = whole
        .kinds
        .windows(2)
        .position(|ops| ops[0] == "row" && ops[1] == "slot")
        .expect("a row followed by a write of the slot")
        + 1;
    let outcome = run_killed_at(backing, Scenario::Forward, Some(cut as u64));
    assert_eq!(outcome.killed_at.as_deref(), Some("slot"), "{backing:?}");
}

/// Catches: a give-back lost to a death between the uncordon and the row. The same cut, for
/// the first give-back of a run: the uncordon is given and the row still says the host waits
/// for its leases; the next controller decides the give-back again, repeats the uncordon
/// (which does nothing the second time) and writes the row.
#[test]
fn a_give_back_cut_off_after_its_uncordon_is_made_again_by_the_next_controller() {
    give_back_cut_off_after_its_uncordon(Backing::Fake);
}

#[test]
fn a_give_back_cut_off_after_its_uncordon_is_made_again_on_a_sqlite_file() {
    give_back_cut_off_after_its_uncordon(Backing::Sqlite);
}

fn give_back_cut_off_after_its_uncordon(backing: Backing) {
    let whole = run_killed_at(backing, Scenario::GiveBack, None);
    // `d1` is busy until after it has been given back: its first uncordon is the
    // give-back's, and the row that says so is the next write.
    let cut = whole
        .kinds
        .iter()
        .position(|op| op == "uncordon d1")
        .expect("d1 was uncordoned")
        + 1;
    assert_eq!(whole.kinds[cut], "row", "the give-back's row follows");
    let outcome = run_killed_at(backing, Scenario::GiveBack, Some(cut as u64));
    assert_eq!(outcome.killed_at.as_deref(), Some("row"), "{backing:?}");
}

// Order ids.

/// Catches: an order id drawn from memory, so that a restart starts the count again and
/// gives the next host an id an earlier host holds, which the helper's `refuse a lower or
/// equal id` then drops. A controller that restarts every few seconds, over a store whose
/// epoch it keeps, draws every id once and ascending.
#[test]
fn restarts_never_repeat_an_order_id() {
    for backing in [Backing::Fake, Backing::Sqlite] {
        let mut rig = Rig::new(backing, "storm", &spec(), policy());
        rig.offer(NEW);
        let target = Release::new(NEW);
        let mut n = 0;
        loop {
            if n % 3 == 0 {
                rig.restart();
            }
            n += 1;
            rig.tick();
            let generation = rig.rows.desired().unwrap().value.unwrap().generation;
            if finished(&rig.world.lock(), &rig.rows, generation, &target) {
                break;
            }
            assert!(n < 5_000, "the run did not end");
        }
        let w = rig.world.lock();
        // A restart gives the order of a host in `Activating` again: the same id.
        let orders = orders_of(&w, NEW);
        assert_eq!(orders.len(), 4, "{orders:?}");
        assert!(
            orders.values().all(|ids| ids.len() == 1),
            "a host had two ids: {orders:?}"
        );
        let distinct: BTreeSet<OrderId> = orders.values().flatten().copied().collect();
        assert_eq!(
            distinct.len(),
            4,
            "an id was given to two hosts: {orders:?}"
        );
        assert!(distinct.iter().all(|o| o.epoch == rig.epoch));
    }
}

/// Catches: a restored store replaying an order id. A SQLite file is backed up part way
/// through a run, the run finishes, and the file is replaced by the backup (a disaster
/// recovery: its counters are back where they were, below ids the hosts' helpers have
/// already seen). A new controller, at the epoch the restored store opens at, walks every
/// host back: each of its ids is above every id given before the restore, and the helper
/// refuses none as a repeat. A controller that left the epoch out of the id, or reused the
/// old one, would give `(epoch, n)` again and be refused.
#[test]
fn a_restored_store_never_replays_an_order_id() {
    let mut rig = Rig::new(Backing::Sqlite, "restore", &spec(), policy());
    rig.epoch = rig.cell.epoch();
    rig.restart();
    // The forward run is done, and backed up.
    rig.offer(NEW);
    rig.finish(NEW);
    let path = rig.cell.path().expect("a SQLite file");
    let copy = path.with_file_name("backup.db");
    backup(&path, &copy).expect("a backup");
    // The farm goes on: the rollback is run, with ids the backup has not seen.
    rig.world.lock().new_run();
    rig.offer(OLD);
    rig.finish(OLD);
    let before: Vec<OrderId> = rig
        .world
        .lock()
        .activations()
        .into_iter()
        .map(|(_, o)| o)
        .collect();
    assert_eq!(before.len(), 8);
    let highest = *before.iter().max().unwrap();
    let epoch_before = rig.cell.epoch();

    // The disaster: the live file is replaced by the backup, whose counters are back where
    // they were after the forward run; the farm keeps what its helpers have seen.
    rig.close_store();
    restore(&copy, &path).expect("a restore");
    rig.open_store(path);
    assert!(
        rig.cell.epoch() > epoch_before,
        "a restore moves the epoch forward"
    );
    let generation = rig.rows.desired().unwrap().value.unwrap().generation;
    assert_eq!(generation.0, 1, "the rows are the backup's");

    // The restored rows say the hosts are done at `NEW` while the farm runs `OLD`, and
    // offering `NEW` again would change nothing (it is the desired release). So the operator
    // sets `OLD` and then `NEW` again, under higher generations: every host is walked
    // forward under new ids.
    rig.world.lock().new_run();
    rig.offer(OLD);
    rig.offer(NEW);
    rig.finish(NEW);
    let w = rig.world.lock();
    let after: Vec<OrderId> = w
        .activations()
        .into_iter()
        .skip(before.len())
        .map(|(_, o)| o)
        .collect();
    assert_eq!(after.len(), 4, "{after:?}");
    for order in &after {
        assert!(*order > highest, "{order:?} is not above {highest:?}");
        assert!(
            !before.contains(order),
            "{order:?} was given before the restore"
        );
    }
    assert_eq!(w.repeats, 0, "the helper refused an id as a repeat");
}

// Held.

/// Catches: the page of a held run lost with the controller that was to send it. A restart
/// finds a host `Held` and sends its alert (the controller that wrote the row may have died
/// before it did), once; it keeps the cordon the hold keeps; and the next look sends nothing
/// more.
#[test]
fn a_restarted_controller_sends_the_alert_of_a_held_run_once() {
    let mut rig = Rig::new(Backing::Fake, "held", &spec(), Policy::default());
    let generation = rig.offer(NEW);
    let reason = HoldReason::ActivateStalled;
    let row = {
        let mut row = HostRow::unstarted();
        row.record.phase = HostPhase::Held { reason };
        row.record.generation = generation;
        row.commit = Release::new(NEW);
        row
    };
    let absent = Stored {
        value: HostRow::unstarted(),
        version: None,
    };
    rig.rows.put_host(&id("d1"), &absent, &row).unwrap();
    rig.rows
        .claim(
            kbf_server::update::SlotClass::Host,
            &id("d1"),
            OrderId::new(1, 1),
            1,
        )
        .unwrap();
    rig.world
        .lock()
        .hosts
        .iter_mut()
        .find(|h| h.name == "d1")
        .unwrap()
        .cordoned = true;

    for restart in 0..2 {
        rig.restart();
        for _ in 0..3 {
            rig.world.advance();
            let tick = rig.controller.tick(rig.world.time()).unwrap();
            assert!(
                tick.started.is_empty(),
                "a host was started under a held run: {tick:?}"
            );
        }
        let held = Alert::Held(reason);
        let w = rig.world.lock();
        let sent = w
            .alerts
            .iter()
            .filter(|(h, a)| h == "d1" && *a == held)
            .count();
        assert_eq!(sent, restart + 1, "one alert per start of the controller");
        assert!(w.host("d1").cordoned, "the hold keeps the cordon");
    }
}
