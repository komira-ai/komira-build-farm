//! A closed-loop simulation of the node step and the picker (`docs/design/split-mode-updates.md`
//! sections 2.6, 2.7, 2.11 and 5.3, #898). The other tests give [`pick`] and [`step`] one
//! world and read one answer. This one feeds the answers back: a small farm whose hosts
//! take and finish leases, take writes, restart and register again, repair, and a
//! controller that on every second reads the farm *as it was a few seconds ago*, asks
//! [`pick`] or [`step`], records the move in its rows, and applies the commands.
//!
//! **What lags.** Every reading (placement, load, writes, release, helper status, member
//! rows, heartbeat times, the slot) comes from a snapshot of the world 0 to 4 seconds old.
//! The rows and the commands' effects are the controller's own and are not delayed, except
//! that a command's effect shows in the readings a second later, like any other change.
//!
//! **What is asserted.** On every second: at most one host is in a step; the slot is held
//! by exactly the host in a step or held; at most one storage member is out (its mark
//! written or its process down); a host is never cordoned off from the activate order to
//! the confirm. At every activate order, in the world as it *is*: the host holds no lease
//! and has no write open (drain, never kill; the stop is safe for writes). A host finished
//! in a run is never started again in it (a release that still reads the old one after the
//! host finished cannot happen here, because the steps wait for the release to read the
//! target; `update_pick.rs` and `update_pick_props.rs` cover that); a controller that
//! restarts and re-issues the entry commands of the phase it reads activates no host
//! twice. A run ends with every host at the target, uncordoned, and the slot free; then the
//! same farm is rolled back (the old release, a higher serial) and ends the same way, with
//! every host walked back although the forward run left them all `Done`.
//!
//! The farm has no faults: a held run, or a run that does not end, is a failure of the rules
//! under test. Each seed draws the lag, how busy the farm is, how long a restart takes,
//! whether a restarted node rebuilds, how many times the controller restarts, and whether
//! one host holds a lease that outlasts the lease wait (that host is given back, and picked
//! again when idle; a Mac with such a lease is skipped, not forced).

#[path = "update_support/mod.rs"]
mod support;

use std::collections::{BTreeMap, VecDeque};

use kbf_map::{MemberId, MemberState};
use kbf_server::update::{
    Alert, Command, DaemonLoad, Facts, HelperReport, HelperStatus, HostFacts, HostKind, HostPhase,
    HostRecord, MemberFacts, OrderId, Pick, Platform, PlatformClass, Policy, Release, Serial,
    SlotOp, Transition, Work, pick, step,
};
use kbf_sim::{Chance, SimRng};
use kbf_types::WorkerId;
use support::{BOOT, NEW, NOW, OLD, SERIAL, id, t};

const SEEDS: u64 = 200;
/// The most seconds a run may take: far more than any seed needs.
const LIMIT: u64 = 30_000;
const TTL: u64 = 15;

#[derive(Clone)]
struct HostTruth {
    name: String,
    kind: HostKind,
    platform: Platform,
    member: Option<u64>,
    /// The release its daemon last reported.
    reported: Release,
    connected: bool,
    cordoned: bool,
    /// The seconds at which each lease ends.
    leases: Vec<u64>,
    /// The seconds at which each open write session ends.
    writes: Vec<u64>,
    /// Writes that started this second (the router's in flight).
    routed: u64,
    helper: Option<HelperReport>,
    work: Work,
    uncordoned_at: Option<u64>,
    /// When the restarted process is up.
    restart_at: Option<u64>,
    confirm_at: Option<u64>,
    /// The highest order the helper has acted on: it refuses a repeat.
    last_order: Option<OrderId>,
    /// Restarts in this run.
    activations: u32,
    lease_pct: u32,
    lease_max: u64,
}

#[derive(Clone)]
struct MemberTruth {
    id: u64,
    process_up: bool,
    state: MemberState,
    planned_down: bool,
    down_since: Option<u64>,
    heard_at: u64,
    boot: u64,
    debt: u64,
    catching_up_until: u64,
}

#[derive(Clone)]
struct Snapshot {
    hosts: Vec<HostTruth>,
    members: Vec<MemberTruth>,
    slot: Option<WorkerId>,
}

#[derive(Debug, Default)]
struct Stats {
    forced: u64,
    replays: u64,
    rebuilds: u64,
    lagged_runs: u64,
    busy_runs: u64,
    give_backs: u64,
    ticks: u64,
}

struct Sim {
    seed: u64,
    rng: SimRng,
    now: u64,
    lag: usize,
    rebuild: bool,
    restart_range: (u64, u64),
    replay_pct: u32,
    hosts: Vec<HostTruth>,
    members: Vec<MemberTruth>,
    slot: Option<WorkerId>,
    records: BTreeMap<String, HostRecord>,
    next_order: OrderId,
    target: Release,
    serial: Serial,
    wait_since: u64,
    policy: Policy,
    history: VecDeque<Snapshot>,
    stats: Stats,
}

fn pct(rng: &mut SimRng, p: u32) -> bool {
    rng.chance(Chance::percent(p))
}

impl Sim {
    fn new(seed: u64) -> Self {
        let mut rng = SimRng::from_seed(seed);
        let storage = 2 + rng.below(2);
        let linux = 2;
        let busy = rng.below(3);
        let (lease_pct, lease_max) = [(0, 30), (2, 120), (8, 300)][usize::try_from(busy).unwrap()];
        let mut hosts = Vec::new();
        let mut members = Vec::new();
        let host = |name: String, kind, platform, member, lease_pct, lease_max| HostTruth {
            name,
            kind,
            platform,
            member,
            reported: Release::new(OLD),
            connected: true,
            cordoned: false,
            leases: Vec::new(),
            writes: Vec::new(),
            routed: 0,
            helper: None,
            work: Work::Waiting,
            uncordoned_at: None,
            restart_at: None,
            confirm_at: None,
            last_order: None,
            activations: 0,
            lease_pct,
            lease_max,
        };
        for n in 1..=storage {
            let name = format!("s{n}");
            let kind = HostKind::StorageAndDaemon;
            hosts.push(host(
                name,
                kind,
                Platform::Linux,
                Some(n),
                lease_pct,
                lease_max,
            ));
            members.push(MemberTruth {
                id: n,
                process_up: true,
                state: MemberState::Active,
                planned_down: false,
                down_since: None,
                heard_at: NOW,
                boot: BOOT,
                debt: 0,
                catching_up_until: 0,
            });
        }
        for n in 1..=linux {
            let name = format!("d{n}");
            let kind = HostKind::DaemonOnly;
            hosts.push(host(
                name,
                kind,
                Platform::Linux,
                None,
                lease_pct,
                lease_max,
            ));
        }
        for n in 1..=2 {
            let name = format!("m{n}");
            let platform = Platform::Mac(PlatformClass::new("arm64"));
            // A Mac is idle most of the time: it is only ever updated when it is, and the
            // run must end.
            let mac_pct = lease_pct.min(1);
            hosts.push(host(
                name,
                HostKind::DaemonOnly,
                platform,
                None,
                mac_pct,
                100,
            ));
        }
        // Some farms hold a lease that outlasts the 15 minute wait: its host is given back
        // (the slot freed, the cordon lifted) and picked again when it is idle. A Mac with a
        // lease of over an hour is busy until then and is skipped, not forced.
        let hosts_linux = usize::try_from(storage + linux).unwrap();
        if seed % 4 == 1 {
            let at = usize::try_from(rng.below(hosts_linux as u64)).unwrap();
            hosts[at].leases.push(NOW + 2500 + rng.below(500));
        }
        if seed % 8 == 3 {
            let at = hosts_linux + usize::try_from(rng.below(2)).unwrap();
            hosts[at].leases.push(NOW + 4500 + rng.below(500));
        }
        let records = hosts
            .iter()
            .map(|h| (h.name.clone(), Self::fresh_row()))
            .collect();
        let lag = usize::try_from(rng.below(5)).unwrap();
        let rebuild = pct(&mut rng, 40);
        let replay_pct = [0, 1, 5][usize::try_from(rng.below(3)).unwrap()];
        let restart_range = (3, 3 + rng.below(50));
        let policy = Policy {
            min_serving: usize::try_from(rng.below(2)).unwrap(),
            ..support::policy()
        };
        let mut stats = Stats::default();
        stats.lagged_runs += u64::from(lag > 0);
        stats.busy_runs += u64::from(busy == 2);
        Self {
            seed,
            rng,
            now: NOW,
            lag,
            rebuild,
            restart_range,
            replay_pct,
            hosts,
            members,
            slot: None,
            records,
            next_order: OrderId(1),
            target: Release::new(NEW),
            serial: SERIAL,
            wait_since: NOW,
            policy,
            history: VecDeque::new(),
            stats,
        }
    }

    fn fresh_row() -> HostRecord {
        HostRecord {
            phase: HostPhase::Pending,
            since: t(0),
            serial: Serial(0),
            order: OrderId(0),
            boot_before: None,
        }
    }

    fn host_index(&self, name: &str) -> usize {
        self.hosts
            .iter()
            .position(|h| h.name == name)
            .expect("a host")
    }

    // The world.

    /// One second of the farm: leases and writes end and begin, restarted processes come
    /// up and register, heartbeats are heard, debts are repaired.
    fn advance(&mut self) {
        let now = self.now;
        for member in &mut self.members {
            if member.process_up {
                member.heard_at = now;
                member.state = if now < member.catching_up_until {
                    MemberState::CatchingUp
                } else {
                    MemberState::Active
                };
                member.debt = member.debt.saturating_sub(1);
            } else if now - member.heard_at > TTL {
                member.state = MemberState::Down;
            }
        }
        for at in 0..self.hosts.len() {
            let host = &mut self.hosts[at];
            host.leases.retain(|end| *end > now);
            host.writes.retain(|end| *end > now);
            host.routed = 0;
            let mut got_lease = false;
            if host.connected && !host.cordoned && pct(&mut self.rng, host.lease_pct) {
                let length = 5 + self.rng.below(host.lease_max);
                host.leases.push(now + length);
                got_lease = true;
            }
            if let Some(n) = host.member {
                let member = self.members.iter().find(|m| m.id == n).expect("member");
                // The router stops choosing a marked or stopped node as a write primary.
                if member.process_up && !member.planned_down && pct(&mut self.rng, 10) {
                    host.writes.push(now + 1 + self.rng.below(15));
                    host.routed = 1;
                }
            }
            if let Some(u) = host.uncordoned_at {
                if got_lease {
                    host.work = Work::Leased;
                } else if now >= u + 2 && host.work == Work::Waiting && host.leases.is_empty() {
                    host.work = Work::QueueEmpty;
                }
            }
            if host.confirm_at.is_some_and(|at| now >= at) {
                host.confirm_at = None;
                if let Some(report) = &mut host.helper {
                    report.status = HelperStatus::Confirmed;
                }
            }
            if host.restart_at.is_some_and(|at| now >= at) {
                self.finish_restart(at);
            }
        }
    }

    /// The restarted process is up: the daemon reconnects at the target release, the helper
    /// reports the flip, and a storage half registers (the mark deleted, the boot counter
    /// larger), owing nothing itself while its peers owe it the copies it missed.
    fn finish_restart(&mut self, at: usize) {
        let now = self.now;
        let target = self.target.clone();
        let rebuild = self.rebuild;
        let rebuild_for = 10 + self.rng.below(200);
        let host = &mut self.hosts[at];
        host.restart_at = None;
        host.connected = true;
        host.reported = target;
        let order = host.last_order.expect("an order was given");
        host.helper = Some(HelperReport {
            order,
            status: HelperStatus::ActivatedUnconfirmed,
        });
        let Some(n) = host.member else {
            return;
        };
        let owed: Vec<u64> = self
            .members
            .iter()
            .map(|_| 1 + self.rng.below(30))
            .collect();
        for (member, owes) in self.members.iter_mut().zip(owed) {
            if member.id == n {
                member.process_up = true;
                member.planned_down = false;
                member.down_since = None;
                member.heard_at = now;
                member.boot += 1;
                member.catching_up_until = if rebuild { now + rebuild_for } else { 0 };
                if rebuild {
                    self.stats.rebuilds += 1;
                }
            } else {
                member.debt = member.debt.max(owes);
            }
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            hosts: self.hosts.clone(),
            members: self.members.clone(),
            slot: self.slot.clone(),
        }
    }

    /// The facts as the controller reads them: the readings of the oldest snapshot kept
    /// (`lag` seconds ago), the controller's own rows and numbers as they are.
    fn facts(&self) -> Facts {
        let seen = self.history.front().expect("a snapshot");
        let hosts = seen
            .hosts
            .iter()
            .map(|h| HostFacts {
                id: id(&h.name),
                kind: h.kind,
                platform: h.platform.clone(),
                member: h.member.map(MemberId),
                release: Some(h.reported.clone()),
                connected: h.connected,
                cordoned: Some(h.cordoned),
                load: Some(DaemonLoad {
                    leases: u32::try_from(h.leases.len()).unwrap(),
                    undelivered: 0,
                    whole_machine_hold: false,
                }),
                open_writes: h.member.map(|_| h.writes.len() as u64),
                routed_writes: h.member.map(|_| h.routed),
                helper: h.helper,
                work: Some(h.work),
                record: self.records[&h.name].clone(),
            })
            .collect();
        let members = seen
            .members
            .iter()
            .map(|m| MemberFacts {
                id: MemberId(m.id),
                state: m.state,
                planned_down: m.planned_down,
                heard_at: Some(t(m.heard_at)),
                repair_debt: Some(m.debt),
                down_since: m.down_since.map(t),
                boot: Some(m.boot),
            })
            .collect();
        Facts {
            target: self.target.clone(),
            serial: self.serial,
            next_order: self.next_order,
            hosts,
            members,
            slot: seen.slot.clone(),
            queued_classes: Some(Default::default()),
            wait_since: t(self.wait_since),
        }
    }

    // The controller.

    fn in_step(&self) -> Option<String> {
        let mut steps = self.records.iter().filter(|(_, r)| r.phase.in_step());
        steps.next().map(|(name, _)| name.clone())
    }

    /// One second: the world moves, the controller looks once.
    fn tick(&mut self) {
        self.now += 1;
        self.stats.ticks += 1;
        self.advance();
        let seen = self.snapshot();
        self.history.push_back(seen);
        while self.history.len() > self.lag + 1 {
            self.history.pop_front();
        }
        let facts = self.facts();
        let now = t(self.now);
        let held = self
            .records
            .values()
            .any(|r| matches!(r.phase, HostPhase::Held { .. }));
        assert!(!held, "seed {}: the run was held", self.seed);
        if let Some(name) = self.in_step() {
            let obs = facts.observe(&id(&name)).expect("the host");
            let moved = step(&obs, &self.policy, now);
            self.apply(&name, &moved);
        } else if let Pick::Host { host, force } = pick(&facts, &self.policy, now) {
            let name = host.as_str().to_owned();
            let row = &self.records[&name];
            assert!(
                !(row.phase == HostPhase::Done && row.serial == self.serial),
                "seed {}: {name} was started again after it finished",
                self.seed
            );
            self.stats.forced += u64::from(force);
            self.apply(&name, &Transition::select());
        }
        // The controller restarts: it reads the row again and runs the phase's entry
        // commands once more.
        if pct(&mut self.rng, self.replay_pct)
            && let Some(name) = self.in_step()
        {
            self.stats.replays += 1;
            let commands = self.records[&name].entry_commands();
            self.run(&name, &commands);
        }
        self.check();
    }

    /// Records a move in the host's row and runs its commands.
    fn apply(&mut self, name: &str, moved: &Transition) {
        let now = self.now;
        let serial = self.serial;
        let row = self.records.get_mut(name).expect("a row");
        if moved.next != row.phase {
            row.since = t(now);
        }
        row.phase = moved.next.clone();
        row.serial = serial;
        if let Some(boot) = moved.boot {
            row.boot_before = Some(boot);
        }
        if let Some(order) = moved.order {
            row.order = order;
            self.next_order = order.next();
        }
        if moved.next == HostPhase::Done {
            self.wait_since = now;
        }
        self.run(name, &moved.commands);
    }

    fn run(&mut self, name: &str, commands: &[Command]) {
        let now = self.now;
        let at = self.host_index(name);
        let member_at = self.hosts[at]
            .member
            .map(|n| self.members.iter().position(|m| m.id == n).expect("member"));
        for command in commands {
            match command {
                Command::CasSlot(SlotOp::Claim) => self.slot = Some(id(name)),
                Command::CasSlot(SlotOp::Release) => {
                    if self.slot == Some(id(name)) {
                        self.slot = None;
                    }
                }
                Command::Cordon => self.hosts[at].cordoned = true,
                Command::Uncordon => {
                    let host = &mut self.hosts[at];
                    if host.cordoned {
                        host.cordoned = false;
                        host.uncordoned_at = Some(now);
                        host.work = Work::Waiting;
                    }
                }
                Command::MarkDown => {
                    let m = &mut self.members[member_at.expect("a storage host")];
                    m.planned_down = true;
                    m.down_since.get_or_insert(now);
                }
                Command::Undown => {
                    let m = &mut self.members[member_at.expect("a storage host")];
                    m.planned_down = false;
                    m.down_since = None;
                }
                Command::Activate { order } => self.activate(at, member_at, *order),
                Command::Confirm { order } => {
                    let host = &mut self.hosts[at];
                    if host.last_order == Some(*order) && host.confirm_at.is_none() {
                        host.confirm_at = Some(now + 1);
                    }
                }
                Command::Alert(Alert::Held(reason)) => {
                    panic!("seed {}: {name} held: {reason:?}", self.seed)
                }
                Command::Alert(Alert::GaveBack(_)) => self.stats.give_backs += 1,
                Command::Alert(Alert::RebuildWaiting) => {}
            }
        }
    }

    /// The helper acts on an order once; a repeat of an id it has seen is refused. The
    /// world as it is, not as the controller read it, must hold no lease and no write.
    fn activate(&mut self, at: usize, member_at: Option<usize>, order: OrderId) {
        let seed = self.seed;
        let restart_in = self.restart_range.0 + self.rng.below(self.restart_range.1);
        let now = self.now;
        let host = &mut self.hosts[at];
        if host.last_order.is_some_and(|last| last >= order) {
            return;
        }
        host.last_order = Some(order);
        let name = &host.name;
        assert!(
            host.leases.is_empty(),
            "seed {seed}: {name} was activated at {now} with leases to {:?}",
            host.leases
        );
        assert!(
            host.writes.is_empty(),
            "seed {seed}: {name} was activated at {now} with writes open"
        );
        assert!(
            host.cordoned,
            "seed {seed}: {name} was activated uncordoned"
        );
        host.activations += 1;
        host.connected = false;
        host.helper = None;
        host.restart_at = Some(now + restart_in);
        if let Some(m) = member_at {
            self.members[m].process_up = false;
        }
    }

    // The invariants.

    fn check(&self) {
        let seed = self.seed;
        let steps: Vec<_> = self
            .records
            .iter()
            .filter(|(_, r)| r.phase.in_step())
            .map(|(name, _)| name.clone())
            .collect();
        assert!(steps.len() <= 1, "seed {seed}: hosts in a step: {steps:?}");
        match steps.first() {
            Some(name) => assert_eq!(
                self.slot,
                Some(id(name)),
                "seed {seed}: {name} is in a step without the slot"
            ),
            None => assert_eq!(self.slot, None, "seed {seed}: the slot is held by nobody"),
        }
        let out = self
            .members
            .iter()
            .filter(|m| !m.process_up || m.planned_down)
            .count();
        assert!(
            out <= 1,
            "seed {seed}: {out} storage members are out at {}",
            self.now
        );
        for (name, row) in &self.records {
            let late = matches!(
                row.phase,
                HostPhase::Activating
                    | HostPhase::RejoiningStorage
                    | HostPhase::Repairing
                    | HostPhase::Confirming
            );
            let host = &self.hosts[self.host_index(name)];
            assert!(
                !late || host.cordoned,
                "seed {seed}: {name} is uncordoned in {:?}",
                row.phase
            );
            assert!(
                host.activations <= 1,
                "seed {seed}: {name} was activated twice"
            );
        }
    }

    // A run.

    fn finished(&self) -> bool {
        let done = |r: &HostRecord| r.phase == HostPhase::Done && r.serial == self.serial;
        self.records.values().all(done)
            && self.slot.is_none()
            && self.hosts.iter().all(|h| {
                h.reported == self.target && !h.cordoned && h.connected && h.restart_at.is_none()
            })
    }

    fn run_to_the_end(&mut self, label: &str) {
        let start = self.now;
        let boots: Vec<u64> = self.members.iter().map(|m| m.boot).collect();
        while !self.finished() {
            assert!(
                self.now - start < LIMIT,
                "seed {} {label}: the run did not end in {LIMIT} s: {:?}",
                self.seed,
                self.records
                    .iter()
                    .map(|(n, r)| (n.clone(), r.phase.clone()))
                    .collect::<Vec<_>>()
            );
            self.tick();
        }
        // Every host restarted once, every storage half registered once.
        for host in &self.hosts {
            assert_eq!(
                host.activations, 1,
                "seed {} {label}: {}",
                self.seed, host.name
            );
        }
        for (member, before) in self.members.iter().zip(boots) {
            assert_eq!(member.boot, before + 1, "seed {} {label}", self.seed);
            assert!(
                member.process_up && !member.planned_down,
                "seed {} {label}",
                self.seed
            );
        }
    }

    /// Offers the previous release again: the same walk with a higher serial.
    fn start_rollback(&mut self, to: &str) {
        self.target = Release::new(to);
        self.serial = Serial(self.serial.0 + 1);
        self.wait_since = self.now;
        for host in &mut self.hosts {
            host.activations = 0;
            // What a host has been given is "since its uncordon": nothing, until the
            // run uncordons it again.
            host.work = Work::Waiting;
            host.uncordoned_at = None;
        }
    }
}

/// Catches, over 200 seeded farms with a controller that reads a farm up to four seconds
/// stale, restarts and replays its entry commands, in a forward run and its rollback: two
/// hosts in a step; a slot held by nobody, or not held by the host in a step; two storage
/// members out at once; a node activated with a lease or a write open in the world as it
/// is; a cordon lost between the activate order and the confirm; a host started again after
/// it finished (the release it reports lags); a host activated twice by a replayed entry
/// command (the order id must be the row's); a held run; a host stuck in a step; a rollback
/// that cannot reach hosts the forward run finished.
#[test]
fn a_run_and_its_rollback_end_with_every_host_walked_under_lagging_facts() {
    let mut total = Stats::default();
    for seed in 0..SEEDS {
        let mut sim = Sim::new(seed);
        sim.run_to_the_end("forward");
        sim.start_rollback(OLD);
        sim.run_to_the_end("rollback");
        total.forced += sim.stats.forced;
        total.replays += sim.stats.replays;
        total.rebuilds += sim.stats.rebuilds;
        total.lagged_runs += sim.stats.lagged_runs;
        total.busy_runs += sim.stats.busy_runs;
        total.give_backs += sim.stats.give_backs;
        total.ticks += sim.stats.ticks;
    }
    // The farms reached the paths the rules are about.
    let reached = total.forced > 10
        && total.replays > 50
        && total.rebuilds > 20
        && total.lagged_runs > 20
        && total.busy_runs > 10
        && total.give_backs > 10;
    assert!(reached, "the farms did not reach every rule: {total:?}");
}
