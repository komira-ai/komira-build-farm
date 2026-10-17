//! A closed-loop simulation of the controller (`docs/design/split-mode-updates.md` sections
//! 2.6, 2.7, 2.11 and 5.3, #898). The other tests give [`kbf_server::update::pick`] and
//! [`kbf_server::update::step`] one world and read one answer. This one feeds the answers
//! back: the real [`Controller`] over its rows in a state store, and a small farm
//! (`update_support/world.rs`) whose hosts take and finish leases, take writes, restart and
//! register again, repair, and a helper that refuses an order id it has seen.
//!
//! **What lags.** Every reading of the farm (placement, load, writes, release, helper
//! status, member rows, heartbeat times) comes from a snapshot of the world 0 to 4 seconds
//! old. The rows are the controller's own and are not delayed.
//!
//! **What is asserted.** On every second: at most one storage host is in a step and at most
//! `max_hosts_out` others (a storage host takes none of their places); the slot holds exactly
//! the hosts in a step or held; at most one storage member is out (its mark written or its
//! process down); a host is never uncordoned from the activate order to the confirm. At every
//! activate order, in the world as it *is*: the host holds no lease and has no write open
//! (drain, never kill; the stop is safe for writes). A host finished in a run is never
//! started again in it. A controller that restarts at random (with the store's epoch kept or
//! moved on) and gives the orders of the rows it finds activates no host twice, and no order
//! id is drawn twice. A run ends with every host at the target, uncordoned, and the slot
//! free; then the same farm is rolled back (the old release, a higher generation) and ends the
//! same way, every host walked back although the forward run left them all `Done`.
//!
//! The farm has no faults: a held run, or a run that does not end, is a failure of the rules
//! under test. Each seed draws the lag, how busy the farm is, how long a restart takes,
//! whether a restarted node rebuilds, the capacity floor, `max_hosts_out`, how often the
//! controller restarts, and whether one host holds a lease that outlasts the lease wait (that
//! host is given back, and picked again when idle; a Mac with such a lease is skipped, not
//! forced).

#[path = "update_support/mod.rs"]
mod support;

use std::collections::BTreeMap;

use kbf_server::update::{Controller, HostPhase, Policy, Release, Rows};
use kbf_sim::{Chance, SimRng};
use support::harness::{Backing, Cell, check_world, controller, finished, phases};
use support::world::{Spec, World};
use support::{NEW, OLD};

const SEEDS: u64 = 200;
/// The most seconds a run may take: far more than any seed needs.
const LIMIT: u64 = 30_000;

#[derive(Debug, Default)]
struct Stats {
    forced: u64,
    restarts: u64,
    rebuilds: u64,
    lagged_runs: u64,
    busy_runs: u64,
    give_backs: u64,
    parallel: u64,
    ticks: u64,
}

struct Sim {
    seed: u64,
    rng: SimRng,
    world: World,
    cell: Cell,
    rows: Rows,
    controller: Controller,
    epoch: u64,
    restart_pct: u32,
    max_hosts_out: usize,
    stats: Stats,
}

fn pct(rng: &mut SimRng, p: u32) -> bool {
    rng.chance(Chance::percent(p))
}

fn index(rng: &mut SimRng, below: u64) -> u64 {
    rng.below(below)
}

impl Sim {
    fn new(seed: u64, backing: Backing) -> Self {
        let mut rng = SimRng::from_seed(seed);
        let storage = 2 + rng.below(2);
        let busy = rng.below(3);
        let (lease_pct, lease_max) = [(0, 30), (2, 120), (8, 300)][usize::try_from(busy).unwrap()];
        let lag = usize::try_from(rng.below(5)).unwrap();
        let rebuild = pct(&mut rng, 40);
        let restart_pct = [0, 1, 5][usize::try_from(rng.below(3)).unwrap()];
        let restart = (3, 3 + rng.below(50));
        let min_serving = usize::try_from(rng.below(2)).unwrap();
        let max_hosts_out = 1 + usize::try_from(rng.below(3)).unwrap();
        let spec = Spec {
            storage,
            linux: 2,
            macs: 2,
            lease_pct,
            lease_max,
            lag,
            rebuild,
            restart,
            ..Spec::default()
        };
        let world = World::new(seed, &spec);
        // Some farms hold a lease that outlasts the 15 minute wait: its host is given back
        // (the slot freed, the cordon lifted) and picked again when it is idle. A Mac with a
        // lease of over an hour is busy until then and is skipped, not forced.
        if seed % 4 == 1 {
            let at = index(&mut rng, storage + 2) + 1;
            let name = if at <= storage {
                format!("s{at}")
            } else {
                format!("d{}", at - storage)
            };
            world.lease(&name, 2500 + rng.below(500));
        }
        if seed % 8 == 3 {
            world.lease(&format!("m{}", 1 + rng.below(2)), 4500 + rng.below(500));
        }
        let cell = Cell::new(backing, "sim");
        let rows = Rows::new(cell.store());
        let policy = Policy {
            min_serving,
            max_hosts_out,
            ..Policy::default()
        };
        rows.set_policy(&policy).expect("a policy that can be kept");
        let epoch = 1;
        let controller = controller(&cell.store(), epoch, &world);
        let mut stats = Stats::default();
        stats.lagged_runs += u64::from(lag > 0);
        stats.busy_runs += u64::from(busy == 2);
        Self {
            seed,
            rng,
            world,
            cell,
            rows,
            controller,
            epoch,
            restart_pct,
            max_hosts_out,
            stats,
        }
    }

    fn label(&self, what: &str) -> String {
        format!("seed {} {what}", self.seed)
    }

    /// The controller dies and a new one starts over the same rows, with the store's epoch
    /// kept or moved on (a start over a SQLite file bumps it).
    fn restart(&mut self) {
        self.stats.restarts += 1;
        self.epoch += u64::from(pct(&mut self.rng, 50));
        self.controller = controller(&self.cell.store(), self.epoch, &self.world);
    }

    /// One second: the farm moves, the controller looks once.
    fn tick(&mut self, label: &str) {
        self.stats.ticks += 1;
        self.world.advance();
        let before = self.rows.hosts().expect("rows");
        let desired = self.rows.desired().expect("desired").value.expect("a run");
        let report = self
            .controller
            .tick(self.world.time())
            .unwrap_or_else(|e| panic!("{label}: the tick failed: {e}"));
        assert!(
            !report.conflict,
            "{label}: a lone controller met a conflict"
        );
        assert!(report.errors.is_empty(), "{label}: {:?}", report.errors);
        for host in &report.started {
            let was = before.get(host).map(|s| s.value.record.clone());
            let finished = was
                .as_ref()
                .is_some_and(|r| r.phase == HostPhase::Done && r.generation == desired.generation);
            assert!(
                !finished,
                "{label}: {host} was started again after it finished"
            );
        }
        self.stats.give_backs += report
            .moved
            .iter()
            .filter(|m| {
                m.from.in_step() && matches!(m.to, HostPhase::Pending | HostPhase::Deferred)
            })
            .count() as u64;
        let after = self.rows.hosts().expect("rows");
        let others = after
            .iter()
            .filter(|(h, r)| {
                r.value.record.phase.in_step()
                    && self.world.lock().host(h.as_str()).member.is_none()
            })
            .count();
        self.stats.parallel += u64::from(others >= 2);
        if pct(&mut self.rng, self.restart_pct) {
            self.restart();
        }
        check_world(&self.world.lock(), &self.rows, self.max_hosts_out, label);
    }

    fn run_to_the_end(&mut self, what: &str, target: &str) {
        let label = self.label(what);
        let start = self.world.lock().now;
        let boots: Vec<u64> = self.world.lock().members.iter().map(|m| m.boot).collect();
        let target = Release::new(target);
        loop {
            let desired = self.rows.desired().expect("desired").value.expect("a run");
            if finished(&self.world.lock(), &self.rows, desired.generation, &target) {
                break;
            }
            let now = self.world.lock().now;
            assert!(
                now - start < LIMIT,
                "{label}: the run did not end in {LIMIT} s: {:?}",
                phases(&self.rows)
            );
            self.tick(&label);
        }
        let w = self.world.lock();
        // Every host restarted once, every storage half registered once.
        for host in &w.hosts {
            assert_eq!(host.activations, 1, "{label}: {}", host.name);
        }
        for (member, before) in w.members.iter().zip(boots) {
            assert_eq!(member.boot, before + 1, "{label}");
            assert!(member.process_up && !member.planned_down, "{label}");
        }
        // No order id was drawn twice, and a host's ids only grow.
        let mut seen = BTreeMap::new();
        for (host, order) in w.activations() {
            let last = seen.insert(host.clone(), order);
            assert!(
                last.is_none_or(|l| l <= order),
                "{label}: {host} went back to {order:?}"
            );
        }
        let mut ids: Vec<_> = w.activations().into_iter().map(|(_, o)| o).collect();
        ids.sort();
        let all = ids.len();
        ids.dedup();
        assert!(all >= w.hosts.len() && !ids.is_empty(), "{label}");
    }

    /// Offers the previous release again: the same walk with a higher generation.
    fn start_rollback(&mut self, to: &str) {
        let now = self.world.time();
        self.rows.offer(&Release::new(to), now).expect("an offer");
        self.world.lock().new_run();
    }
}

fn run_seed(seed: u64, backing: Backing, total: &mut Stats) {
    let mut sim = Sim::new(seed, backing);
    let now = sim.world.time();
    sim.rows.offer(&Release::new(NEW), now).expect("an offer");
    sim.run_to_the_end("forward", NEW);
    sim.start_rollback(OLD);
    sim.run_to_the_end("rollback", OLD);
    let w = sim.world.lock();
    total.forced += w.busy_cordons;
    total.rebuilds += w.rebuilds;
    drop(w);
    total.restarts += sim.stats.restarts;
    total.lagged_runs += sim.stats.lagged_runs;
    total.busy_runs += sim.stats.busy_runs;
    total.give_backs += sim.stats.give_backs;
    total.parallel += sim.stats.parallel;
    total.ticks += sim.stats.ticks;
}

/// Catches, over 200 seeded farms with a controller that reads a farm up to four seconds
/// stale and restarts at random, in a forward run and its rollback: two storage hosts in a
/// step, or more daemon-only hosts and Macs than `max_hosts_out`; a slot that does not
/// mirror the rows; two storage members out at once; a node activated with a lease or a write
/// open in the world as it is; a cordon lost between the activate order and the confirm; a
/// host started again after it finished (the release it reports lags); a host activated twice
/// by an order given again after a restart (the order id must be the row's); an order id
/// drawn twice; a held run; a host stuck in a step; a rollback that cannot reach hosts the
/// forward run finished.
#[test]
fn a_run_and_its_rollback_end_with_every_host_walked_under_lagging_facts() {
    let mut total = Stats::default();
    for seed in 0..SEEDS {
        run_seed(seed, Backing::Fake, &mut total);
    }
    // The farms reached the paths the rules are about.
    let reached = total.forced > 10
        && total.restarts > 50
        && total.rebuilds > 20
        && total.lagged_runs > 20
        && total.busy_runs > 10
        && total.give_backs > 10
        && total.parallel > 20;
    assert!(reached, "the farms did not reach every rule: {total:?}");
}

/// Catches: the same rules on the SQLite backend, where a row is durable across the store's
/// own commits and a version is the store's revision: a handful of seeds, since every row
/// write waits for a sync.
#[test]
fn a_run_and_its_rollback_end_the_same_way_on_a_sqlite_file() {
    let mut total = Stats::default();
    for seed in [0, 1, 7, 9] {
        run_seed(seed, Backing::Sqlite, &mut total);
    }
    assert!(total.ticks > 100, "{total:?}");
}
