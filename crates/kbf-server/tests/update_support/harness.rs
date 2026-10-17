//! What the controller tests share: a state store on either backend, a controller over a
//! model farm ([`super::world`]), and the invariants that hold at every second of a run,
//! judged in the farm as it *is* and in the rows the controller wrote.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_map::MemberId;
use kbf_server::update::{
    Alert, Controller, Generation, HostPhase, HostRow, OrderId, Policy, Release, Rows, Stored,
    Tick, Transport, TransportError,
};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{Clock, ManualClock, Store};
use kbf_types::WorkerId;

use super::world::{Gave, Inner, Spec, World};
use super::{NEW, OLD, id};

/// Which backend a test runs the controller's rows on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// The in-memory fake.
    Fake,
    /// A SQLite file in a directory of the test's own.
    Sqlite,
}

/// A state store, and for SQLite the file it can be closed and opened again from.
pub struct Cell {
    store: Arc<dyn Store>,
    sqlite: Option<(Arc<SqliteStore>, PathBuf)>,
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kbf-update-{name}-{pid}-{n}"));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

impl Cell {
    /// An empty store on `backing`; `name` names the directory of a SQLite file.
    pub fn new(backing: Backing, name: &str) -> Self {
        match backing {
            Backing::Fake => {
                let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
                Self {
                    store: Arc::new(fake),
                    sqlite: None,
                }
            }
            Backing::Sqlite => {
                let path = scratch(name).join("state.db");
                let store = Self::open(&path);
                Self {
                    store: Arc::clone(&store) as Arc<dyn Store>,
                    sqlite: Some((store, path)),
                }
            }
        }
    }

    fn open(path: &std::path::Path) -> Arc<SqliteStore> {
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        // One writer, and a controller that writes one row at a time: waiting for company in
        // the group commit window would only slow the tests.
        let options = Options {
            window: std::time::Duration::ZERO,
            ..Options::default()
        };
        Arc::new(SqliteStore::open(path, clock, options).expect("a SQLite store"))
    }

    /// The store.
    pub fn store(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store)
    }

    /// The store's epoch: the SQLite file's, which a reopen and a restore move forward; the
    /// fake has none, and stands in with 1.
    pub fn epoch(&self) -> u64 {
        self.sqlite.as_ref().map_or(1, |(store, _)| store.epoch())
    }

    /// The file of a SQLite store.
    pub fn path(&self) -> Option<PathBuf> {
        self.sqlite.as_ref().map(|(_, path)| path.clone())
    }

    /// Closes a SQLite store and opens the same file again, as a restarted process does
    /// (its epoch is then one larger). Every other handle on the store must be gone.
    pub fn reopen(&mut self) {
        let (store, path) = self.sqlite.take().expect("a SQLite store");
        // Put a placeholder in the field `store` so that the old Arc can be the last.
        self.store = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        drop(store);
        let store = Self::open(&path);
        self.store = Arc::clone(&store) as Arc<dyn Store>;
        self.sqlite = Some((store, path));
    }

    /// Opens the file `path` as this cell's store (after a restore replaced the file), in
    /// place of the one it held, which [`Cell::close`] closed.
    pub fn open_at(&mut self, path: PathBuf) {
        self.store = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        self.sqlite = None;
        let store = Self::open(&path);
        self.store = Arc::clone(&store) as Arc<dyn Store>;
        self.sqlite = Some((store, path));
    }

    /// Drops the SQLite store so its file can be copied or restored, and leaves a stand-in.
    pub fn close(&mut self) {
        self.store = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        self.sqlite = None;
    }
}

/// A controller over `store` and `world`.
pub fn controller(store: &Arc<dyn Store>, epoch: u64, world: &World) -> Controller {
    Controller::new(Arc::clone(store), epoch, world.transport(), world.source())
}

/// A host's row, or the unstarted one.
pub fn row_of(rows: &Rows, host: &str) -> HostRow {
    let all = rows.hosts().expect("the rows can be read");
    all.get(&id(host))
        .map_or_else(HostRow::unstarted, |s| s.value.clone())
}

/// Every host's row, by name.
pub fn all_rows(rows: &Rows) -> Vec<(WorkerId, Stored<HostRow>)> {
    rows.hosts()
        .expect("the rows can be read")
        .into_iter()
        .collect()
}

/// What holds at every second of a run, in the farm as it is and in the rows:
///
/// - at most one storage host in a step, and at most `max_hosts_out` others;
/// - a holder of a place is in a step or held, and a host in a step (past the claim) holds
///   one: the slot is the rows' mirror;
/// - at most one storage member is out (its mark written or its process down);
/// - a host between the activate order and the confirm is cordoned;
/// - no host was restarted twice in a run.
pub fn check_world(world: &Inner, rows: &Rows, max_hosts_out: usize, label: &str) {
    check(world, rows, max_hosts_out, label, false);
}

/// [`check_world`] for two controllers at once: a host whose row says `Selected` and that
/// holds no place is not counted as in a step. It is the harmless trace of the controller
/// that lost the claim for the place: its step gives the host back at the next look, with no
/// order.
pub fn check_world_two_controllers(world: &Inner, rows: &Rows, max_hosts_out: usize, label: &str) {
    check(world, rows, max_hosts_out, label, true);
}

fn check(world: &Inner, rows: &Rows, max_hosts_out: usize, label: &str, unplaced_ok: bool) {
    let hosts = rows.hosts().expect("the rows can be read");
    let slot = rows.slot().expect("the slot can be read").value;
    let in_step = |name: &str, row: &Stored<HostRow>| {
        let storage = world.host(name).member.is_some();
        let phase = &row.value.record.phase;
        let unplaced = unplaced_ok && *phase == HostPhase::Selected && !slot.holds(&id(name));
        (storage, phase.in_step() && !unplaced)
    };
    let mut storage_n = 0;
    let mut others_n = 0;
    for (host, row) in &hosts {
        let (storage, stepping) = in_step(host.as_str(), row);
        if stepping {
            if storage {
                storage_n += 1;
            } else {
                others_n += 1;
            }
        }
        let phase = &row.value.record.phase;
        if phase.holds_place() && *phase != HostPhase::Selected {
            assert!(
                slot.holds(host),
                "{label}: {host} is {phase:?} without a place"
            );
        }
        let late = matches!(
            phase,
            HostPhase::Activating
                | HostPhase::RejoiningStorage
                | HostPhase::Repairing
                | HostPhase::Confirming
        );
        let truth = world.host(host.as_str());
        assert!(
            !late || truth.cordoned,
            "{label}: {host} is uncordoned in {phase:?}"
        );
        assert!(
            truth.activations <= 1,
            "{label}: {host} was activated twice"
        );
    }
    assert!(
        storage_n <= 1,
        "{label}: {storage_n} storage hosts in a step"
    );
    assert!(
        others_n <= max_hosts_out,
        "{label}: {others_n} other hosts in a step, {max_hosts_out} places"
    );
    assert!(
        slot.storage.iter().count() <= 1,
        "{label}: two storage holders"
    );
    assert!(
        slot.hosts.len() <= max_hosts_out,
        "{label}: too many holders: {slot:?}"
    );
    for (_, holder) in slot.holders() {
        let held = hosts
            .get(&holder.host)
            .is_some_and(|row| row.value.record.phase.holds_place());
        assert!(
            held,
            "{label}: {} holds a place and is not in a step",
            holder.host
        );
    }
    let out = world
        .members
        .iter()
        .filter(|m| !m.process_up || m.planned_down)
        .count();
    assert!(
        out <= 1,
        "{label}: {out} storage members are out at {}",
        world.now
    );
    for (host, alert) in &world.alerts {
        assert!(
            !matches!(alert, Alert::Held(_)),
            "{label}: {host} was held: {alert:?}"
        );
    }
}

/// Whether the run `generation` is over: every host `Done` in it, the slot empty, every host
/// reporting `target` uncordoned and connected.
pub fn finished(world: &Inner, rows: &Rows, generation: Generation, target: &Release) -> bool {
    let hosts = rows.hosts().expect("the rows can be read");
    let done = |row: &Stored<HostRow>| {
        row.value.record.phase == HostPhase::Done && row.value.record.generation == generation
    };
    hosts.len() == world.hosts.len()
        && hosts.values().all(done)
        && rows.slot().expect("the slot can be read").value.is_empty()
        && world
            .hosts
            .iter()
            .all(|h| h.reported == *target && !h.cordoned && h.connected && h.restart_at.is_none())
}

/// One controller over one farm and one store, driven a second at a time.
pub struct Rig {
    pub world: World,
    pub cell: Cell,
    pub rows: Rows,
    pub controller: Controller,
    pub epoch: u64,
    pub policy: Policy,
    pub label: String,
}

impl Rig {
    /// A farm of `spec` (seeded), an empty store on `backing`, `policy` written, and a
    /// controller started at epoch 1. Nothing is offered.
    pub fn new(backing: Backing, label: &str, spec: &Spec, policy: Policy) -> Self {
        let world = World::new(7, spec);
        let cell = Cell::new(backing, label);
        let rows = Rows::new(cell.store());
        rows.set_policy(&policy).expect("a policy that can be kept");
        let epoch = 1;
        let controller = controller(&cell.store(), epoch, &world);
        Self {
            world,
            cell,
            rows,
            controller,
            epoch,
            policy,
            label: label.to_owned(),
        }
    }

    /// Offers `target` at the farm's time.
    pub fn offer(&self, target: &str) -> Generation {
        let now = self.world.time();
        self.rows
            .offer(&Release::new(target), now)
            .expect("an offer")
            .generation
    }

    /// A new controller over the same store and farm, as a restart starts one.
    pub fn restart(&mut self) {
        self.controller = controller(&self.cell.store(), self.epoch, &self.world);
    }

    /// Lets go of the store, so that a SQLite file can be copied over: the rows and the
    /// controller are pointed at a throwaway store, and the cell closes the file.
    pub fn close_store(&mut self) {
        let idle = Cell::new(Backing::Fake, "idle").store();
        self.rows = Rows::new(Arc::clone(&idle));
        self.controller = controller(&idle, self.epoch, &self.world);
        self.cell.close();
    }

    /// Opens the SQLite file at `path` as the store, with the epoch it opens at.
    pub fn open_store(&mut self, path: PathBuf) {
        self.cell.open_at(path);
        self.rows = Rows::new(self.cell.store());
        self.epoch = self.cell.epoch();
        self.restart();
    }

    /// One second: the farm moves, the controller looks once, and the invariants hold.
    pub fn tick(&mut self) -> Tick {
        self.world.advance();
        let tick = self
            .controller
            .tick(self.world.time())
            .unwrap_or_else(|e| panic!("{}: the tick failed: {e}", self.label));
        assert!(
            !tick.conflict,
            "{}: a lone controller met a conflict",
            self.label
        );
        assert!(tick.errors.is_empty(), "{}: {:?}", self.label, tick.errors);
        check_world(
            &self.world.lock(),
            &self.rows,
            self.policy.max_hosts_out,
            &self.label,
        );
        tick
    }

    /// Ticks until `done` or `limit` seconds have passed.
    pub fn until(&mut self, what: &str, limit: u64, mut done: impl FnMut(&Rig) -> bool) {
        let start = self.world.lock().now;
        while !done(self) {
            let now = self.world.lock().now;
            assert!(
                now - start < limit,
                "{}: {what} did not happen in {limit} s: {:?}",
                self.label,
                phases(&self.rows)
            );
            self.tick();
        }
    }

    /// Ticks until the run of `target` is over ([`finished`]).
    pub fn finish(&mut self, target: &str) {
        let target = Release::new(target);
        self.until("the run to end", 30_000, |rig| {
            let generation = rig.rows.desired().unwrap().value.unwrap().generation;
            finished(&rig.world.lock(), &rig.rows, generation, &target)
        });
    }

    /// The phase of a host's row.
    pub fn phase(&self, host: &str) -> HostPhase {
        row_of(&self.rows, host).record.phase
    }
}

/// What a [`Spy`] saw: the order, and the host's row in the store at that moment.
#[derive(Clone, Debug)]
pub struct Seen {
    pub host: String,
    pub gave: Gave,
    pub phase: HostPhase,
    pub order: OrderId,
}

/// A transport that notes, for every order it passes on, what the host's row said when the
/// order reached the farm.
pub struct Spy {
    inner: Box<dyn Transport>,
    rows: Rows,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Spy {
    fn look(&self, host: &WorkerId, gave: Gave) {
        let all = self.rows.hosts().expect("the rows can be read");
        let row = all
            .get(host)
            .expect("a row exists before an order is given");
        let seen = Seen {
            host: host.as_str().to_owned(),
            gave,
            phase: row.value.record.phase.clone(),
            order: row.value.record.order,
        };
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(seen);
    }
}

impl Transport for Spy {
    fn cordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.look(host, Gave::Cordon);
        self.inner.cordon(host)
    }

    fn uncordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.look(host, Gave::Uncordon);
        self.inner.uncordon(host)
    }

    fn mark_down(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.look(host, Gave::MarkDown);
        self.inner.mark_down(host, member)
    }

    fn undown(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.look(host, Gave::Undown);
        self.inner.undown(host, member)
    }

    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.look(host, Gave::Activate(order, release.as_str().to_owned()));
        self.inner.activate(host, order, release)
    }

    fn confirm(&mut self, host: &WorkerId, order: OrderId) -> Result<(), TransportError> {
        self.look(host, Gave::Confirm(order));
        self.inner.confirm(host, order)
    }

    fn alert(&mut self, host: &WorkerId, alert: &Alert) -> Result<(), TransportError> {
        self.inner.alert(host, alert)
    }
}

/// A rig whose controller gives its orders through a [`Spy`].
pub fn spied(
    backing: Backing,
    label: &str,
    spec: &Spec,
    policy: Policy,
) -> (Rig, Arc<Mutex<Vec<Seen>>>) {
    let mut rig = Rig::new(backing, label, spec, policy);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spy = Spy {
        inner: rig.world.transport(),
        rows: Rows::new(rig.cell.store()),
        seen: Arc::clone(&seen),
    };
    rig.controller = Controller::new(
        rig.cell.store(),
        rig.epoch,
        Box::new(spy),
        rig.world.source(),
    );
    (rig, seen)
}

/// A short account of where each host is, for a message.
pub fn phases(rows: &Rows) -> Vec<(String, HostPhase)> {
    all_rows(rows)
        .into_iter()
        .map(|(host, row)| (host.as_str().to_owned(), row.value.record.phase))
        .collect()
}

/// The hosts a tick moved to `Selected`.
pub fn started(tick: &Tick) -> Vec<String> {
    tick.started.iter().map(|h| h.as_str().to_owned()).collect()
}

/// The release the farm starts at, and the one the first offer drives to.
pub fn releases() -> (Release, Release) {
    (Release::new(OLD), Release::new(NEW))
}
