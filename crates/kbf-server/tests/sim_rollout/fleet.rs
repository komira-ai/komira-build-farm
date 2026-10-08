//! The world `tests/sim_rollout.rs` drives: a scheduler fed with virtual time and
//! the workers and work on it ([`Cell`]), the rollout store with writes that can fail
//! ([`FlakyStore`]), the [`Fleet`] the driver acts on and the [`Applier`] that takes
//! hand-overs, each checking what the driver does as it does it.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{Cordon, Event, Input, OpState, Request, Scheduler};
use kbf_server::farm::NodeAction;
use kbf_server::fleet::PlacementView;
use kbf_server::rollout::{
    Applier, Change, Fleet, MemoryRolloutStore, NodePlacement, RolloutStore, StoreError, Update,
};
use kbf_sim::{Chance, SimRng};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseId, NodeStep,
    OperationId, Outcome, Qos, Resources, Rollout, RolloutId, StateMachine, WaiterId, WorkerId,
};

pub const GIB: u64 = 1 << 30;
pub const GRACE: u64 = 60;
pub const HEARTBEAT: u64 = 5;
pub const ID: RolloutId = RolloutId(1);

/// The platforms work asks for.
pub const PLATFORMS: [&[(&str, &str)]; 4] = [
    &[],
    &[("OSFamily", "Linux")],
    &[("OSFamily", "Linux"), ("ISA", "arm-a64")],
    &[("OSFamily", "Darwin")],
];

pub fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

/// Node `i` of the fleet: the first is the only arm64 Linux machine, the second the
/// only Mac, the rest Linux x86-64.
pub fn node_caps(i: usize) -> NodeCaps {
    let report: &[(&str, &str)] = match i {
        0 => &[("arch", "arm64"), ("os", "linux")],
        1 => &[("arch", "arm64"), ("os", "macos")],
        _ => &[("arch", "x86_64"), ("os", "linux")],
    };
    NodeCaps::from_report(report.iter().copied()).expect("a valid report")
}

/// The world the driver acts on: the scheduler, the workers and their work.
pub struct Cell {
    pub label: (&'static str, u64),
    pub sched: Scheduler,
    pub t: u64,
    pub step: u64,
    pub nodes: Vec<WorkerId>,
    pub up: BTreeMap<WorkerId, bool>,
    pub downs: BTreeMap<WorkerId, u64>,
    pub running: BTreeMap<LeaseId, (u64, OperationId, WorkerId)>,
    pub run_for: BTreeMap<OperationId, u64>,
    /// Per held operation: its lease, worker, the worker's downs at the grant, and
    /// whether the worker was down then (its `Start` lost).
    pub holding: BTreeMap<OperationId, (LeaseId, WorkerId, u64, bool)>,
    /// Cordoned in the scheduler, in any state, by what was fed.
    pub cordoned: BTreeSet<WorkerId>,
    /// Cordoned or drained by the driver, not yet returned.
    pub by_driver: BTreeSet<WorkerId>,
    pub answered: BTreeMap<OperationId, u32>,
    pub submitted: u64,
    /// Hand-overs: when, and to which node.
    pub handed: Vec<(u64, WorkerId)>,
    /// Calls the driver made on the fleet and the applier.
    pub driver_calls: u64,
    pub reached: BTreeMap<&'static str, u64>,
    pub hash: u64,
    /// Print the trace (`KBF_SIM_TRACE=1`, with `replay`).
    pub print: bool,
}

impl Cell {
    pub fn new(label: (&'static str, u64), n: usize) -> Self {
        let nodes: Vec<WorkerId> = (0..n)
            .map(|i| WorkerId::new(format!("node-{i:02}")))
            .collect();
        let mut cell = Self {
            label,
            sched: Scheduler::new(1).with_unservable_wait(Duration::from_secs(60)),
            t: 0,
            step: 0,
            up: nodes.iter().map(|w| (w.clone(), true)).collect(),
            downs: nodes.iter().map(|w| (w.clone(), 0)).collect(),
            nodes,
            running: BTreeMap::new(),
            run_for: BTreeMap::new(),
            holding: BTreeMap::new(),
            cordoned: BTreeSet::new(),
            by_driver: BTreeSet::new(),
            answered: BTreeMap::new(),
            submitted: 0,
            handed: Vec::new(),
            driver_calls: 0,
            reached: BTreeMap::new(),
            hash: 0xcbf2_9ce4_8422_2325,
            print: std::env::var_os("KBF_SIM_TRACE").is_some(),
        };
        for i in 0..n {
            cell.register(i);
        }
        cell
    }

    #[track_caller]
    pub fn fail(&self, invariant: &str, what: &str) -> ! {
        let (scenario, seed) = self.label;
        panic!(
            "{invariant} violated: {what}\n  scenario {scenario}, seed {seed}, step {}, t={} s\n  \
             replay: KBF_SIM_SCENARIO={scenario} KBF_SIM_SEED={seed} cargo test -p kbf-server \
             --test sim_rollout -- --ignored --exact replay",
            self.step, self.t
        );
    }

    pub fn reach(&mut self, what: &'static str) {
        *self.reached.entry(what).or_default() += 1;
    }

    pub fn mix(&mut self, line: &str) {
        if self.print {
            println!("{line}");
        }
        for b in line.bytes().chain([b'\n']) {
            self.hash ^= u64::from(b);
            self.hash = self.hash.wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn now(&self) -> FarmTime {
        FarmTime::from_millis(self.t * 1_000)
    }

    /// Feeds `event`, commits what it proposes at once, and checks every step.
    pub fn feed(&mut self, event: Event) {
        let mut inputs = vec![Input::new(self.now(), event)];
        while !inputs.is_empty() {
            let mut next = Vec::new();
            for input in inputs {
                self.step += 1;
                self.mix(&format!("{} {:?}", self.t, input.event));
                match &input.event {
                    Event::Cordon { worker } | Event::Drain { worker, .. } => {
                        self.cordoned.insert(worker.clone());
                    }
                    Event::Uncordon { worker } => {
                        self.cordoned.remove(worker);
                        self.by_driver.remove(worker);
                    }
                    _ => {}
                }
                let effects = self.sched.apply(input);
                for effect in effects {
                    self.mix(&format!("  {effect:?}"));
                    next.extend(self.carry_out(effect));
                }
                self.given_up();
            }
            inputs = next;
        }
    }

    pub fn carry_out(&mut self, effect: Effect) -> Vec<Input> {
        let now = self.now();
        match effect {
            Effect::Commit(record) => {
                if let ControlRecord::Lease(g) = &record {
                    if self.cordoned.contains(&g.worker) {
                        self.fail("I8", &format!("{g:?} to a cordoned worker"));
                    }
                    let (downs, lost) = (self.downs[&g.worker], !self.up[&g.worker]);
                    let held = (g.lease, g.worker.clone(), downs, lost);
                    self.holding.insert(g.operation, held);
                }
                vec![Input::new(now, Event::Committed(record))]
            }
            Effect::Start(s) => {
                if self.up[&s.worker] {
                    let ends = self.t + self.run_for[&s.operation];
                    self.running.insert(s.lease, (ends, s.operation, s.worker));
                }
                Vec::new()
            }
            Effect::Answer(a) => {
                self.holding.remove(&a.operation);
                self.answer(a.operation);
                Vec::new()
            }
            Effect::Refuse(r) => {
                self.answer(r.operation);
                Vec::new()
            }
            Effect::Waiting(_) => Vec::new(),
        }
    }

    pub fn answer(&mut self, op: OperationId) {
        let n = self.answered.entry(op).or_default();
        *n += 1;
        if *n > 1 {
            self.fail("I4", &format!("{op} answered twice"));
        }
    }

    /// I9: a holding the scheduler gave up was lost with its worker.
    pub fn given_up(&mut self) {
        let gone: Vec<OperationId> = self
            .holding
            .iter()
            .filter(|(op, (lease, ..))| {
                !matches!(self.sched.state(**op), Some(OpState::Leased { lease: l, .. } | OpState::Running { lease: l, .. }) if l == lease)
            })
            .map(|(op, _)| *op)
            .collect();
        for op in gone {
            let (lease, worker, downs, lost) = self.holding.remove(&op).expect("listed");
            if !lost && self.downs[&worker] == downs {
                self.fail(
                    "I9",
                    &format!("{lease:?} of {op} given up on {worker}, which never went down"),
                );
            }
            if self.cordoned.contains(&worker) {
                self.reach("lease of a cordoned node given up after it went down");
            }
        }
    }

    pub fn register(&mut self, i: usize) {
        let worker = self.nodes[i].clone();
        let capacity = Resources::new(4_000, 8 * GIB);
        self.feed(Event::WorkerUp {
            worker,
            capacity,
            caps: node_caps(i),
        });
    }

    pub fn heartbeat(&mut self, worker: &WorkerId) {
        let running = self
            .running
            .iter()
            .filter(|(_, (_, _, w))| w == worker)
            .map(|(l, _)| *l)
            .collect();
        self.feed(Event::Heartbeat {
            worker: worker.clone(),
            running,
        });
    }

    pub fn down(&mut self, worker: &WorkerId) {
        if self.up[worker] {
            self.up.insert(worker.clone(), false);
            *self.downs.get_mut(worker).expect("known") += 1;
            self.running.retain(|_, (_, _, w)| w != worker);
        }
    }

    pub fn back(&mut self, worker: &WorkerId) {
        if !self.up[worker] {
            self.up.insert(worker.clone(), true);
            let i = self.nodes.iter().position(|w| w == worker).expect("known");
            self.register(i);
            self.heartbeat(worker);
        }
    }

    pub fn submit(&mut self, rng: &mut SimRng) {
        let n = self.submitted;
        self.submitted += 1;
        // Mostly work any Linux node runs; some for the only arm64 node and the only
        // Mac, at a rate each keeps up with between its updates.
        let p = [0, 0, 0, 1, 1, 1, 2, 3][usize::try_from(rng.below(8)).expect("small")];
        let op = OperationId(n);
        self.run_for.insert(op, rng.between(5, 90));
        let request = Request {
            key: ActionKey {
                instance: "main".to_owned(),
                action: digest(n),
            },
            qos: Qos::Ci,
            resources: Resources::new(1_000 * rng.between(1, 2), GIB),
            needs: kbf_caps::Request::from_platform(PLATFORMS[p].iter().copied()).expect("valid"),
            hermetic: true,
            do_not_cache: false,
        };
        self.feed(Event::Submit {
            waiter: WaiterId(n),
            request,
        });
    }

    /// One second of the world: heartbeats, results, a tick.
    pub fn second(&mut self) {
        for (i, w) in self.nodes.clone().iter().enumerate() {
            if self.up[w] && (self.t + i as u64).is_multiple_of(HEARTBEAT) {
                self.heartbeat(w);
            }
        }
        let due: Vec<(LeaseId, OperationId)> = self
            .running
            .iter()
            .filter(|(_, (at, _, _))| *at <= self.t)
            .map(|(l, (_, op, _))| (*l, *op))
            .collect();
        for (lease, operation) in due {
            self.running.remove(&lease);
            let outcome = Outcome::Completed {
                action_result: digest(1_000_000 + lease.seq),
            };
            self.feed(Event::Report {
                operation,
                lease,
                outcome,
            });
        }
        self.feed(Event::Tick);
    }

    /// Where `node` is in placement, as `Farm::node_view` reports it.
    pub fn view(&self, node: &WorkerId) -> NodePlacement {
        let leases = || {
            self.sched
                .leases_on(node)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        let placement = match self.sched.cordon(node) {
            None => PlacementView::Serving,
            Some(Cordon::Cordoned) => PlacementView::Cordoned,
            Some(Cordon::Draining { deadline }) => PlacementView::Draining {
                deadline_unix_ms: deadline.as_millis(),
                leases: leases(),
            },
            Some(Cordon::Drained) => PlacementView::Drained,
            Some(Cordon::Paused { deadline }) => PlacementView::DrainPaused {
                deadline_unix_ms: deadline.as_millis(),
                leases: leases(),
            },
        };
        NodePlacement {
            placement,
            connected: self.up[node],
        }
    }
}

/// The rollout store, with writes that fail at random once armed (F3.9).
pub struct FlakyStore {
    pub inner: MemoryRolloutStore,
    pub fail: Mutex<(SimRng, Chance, bool)>,
}

impl FlakyStore {
    pub fn new(seed: u64, chance: Chance) -> Self {
        Self {
            inner: MemoryRolloutStore::default(),
            fail: Mutex::new((SimRng::from_seed(seed ^ 0x5707e), chance, false)),
        }
    }

    pub fn arm(&self) {
        self.fail.lock().expect("unpoisoned").2 = true;
    }

    pub fn record(&self) -> Rollout {
        self.inner.get(ID).expect("recorded")
    }
}

impl RolloutStore for FlakyStore {
    fn create(&self, rollout: Rollout) -> Result<(), StoreError> {
        self.inner.create(rollout)
    }

    fn get(&self, id: RolloutId) -> Option<Rollout> {
        self.inner.get(id)
    }

    fn update(&self, id: RolloutId, change: Change<'_>) -> Result<Rollout, StoreError> {
        let mut fail = self.fail.lock().expect("unpoisoned");
        let (rng, chance, armed) = &mut *fail;
        if *armed && rng.chance(*chance) {
            return Err(StoreError::Unavailable("injected".to_owned()));
        }
        drop(fail);
        self.inner.update(id, change)
    }
}

/// The fleet the driver acts on: the scheduler of `cell`.
pub struct SimFleet<'a> {
    pub cell: &'a RefCell<Cell>,
    pub store: &'a FlakyStore,
    /// Uncordon this node right after a read that finds it ready (F3.6).
    pub inject: RefCell<Option<WorkerId>>,
}

impl Fleet for SimFleet<'_> {
    fn place(&self, node: &WorkerId, action: NodeAction) -> Result<(), String> {
        let mut cell = self.cell.borrow_mut();
        cell.driver_calls += 1;
        let step = self.store.record().node(node).map(|p| p.step());
        let (want, event) = match action {
            NodeAction::Cordon => (
                NodeStep::Cordoned,
                Event::Cordon {
                    worker: node.clone(),
                },
            ),
            NodeAction::Drain(within) => {
                let deadline = cell.now().saturating_add(within);
                let worker = node.clone();
                (NodeStep::Draining, Event::Drain { worker, deadline })
            }
            NodeAction::Uncordon => cell.fail("R4", "the driver uncordoned a node"),
        };
        if step != Some(want) {
            cell.fail(
                "R4",
                &format!("{action:?} on {node} while the record has it at {step:?}"),
            );
        }
        cell.by_driver.insert(node.clone());
        cell.feed(event);
        Ok(())
    }

    fn placement(&self, node: &WorkerId) -> Option<NodePlacement> {
        let mut cell = self.cell.borrow_mut();
        let view = cell.view(node);
        if view.ready() && self.inject.borrow().as_ref() == Some(node) {
            self.inject.borrow_mut().take();
            cell.feed(Event::Uncordon {
                worker: node.clone(),
            });
            cell.reach("uncordon between the driver's two reads");
        }
        Some(view)
    }
}

/// Records hand-overs, refusing them at `refuse`.
pub struct SimApplier<'a> {
    pub cell: &'a RefCell<Cell>,
    pub store: &'a FlakyStore,
    pub refuse: RefCell<(SimRng, Chance)>,
}

impl Applier for SimApplier<'_> {
    fn apply(&self, update: &Update) -> Result<(), String> {
        let mut cell = self.cell.borrow_mut();
        cell.driver_calls += 1;
        let node = &update.node;
        let step = self.store.record().node(node).map(|p| p.step());
        let (cordon, leases) = (cell.sched.cordon(node).cloned(), cell.sched.leases_on(node));
        let ok = update.rollout == ID
            && update.step == NodeStep::Updating
            && step == Some(NodeStep::Updating)
            && cordon == Some(Cordon::Drained)
            && leases.is_empty()
            && cell.up[node];
        if !ok {
            cell.fail(
                "R3",
                &format!(
                    "{update:?} handed over: record {step:?}, cordon {cordon:?}, leases \
                     {leases:?}, up {}",
                    cell.up[node]
                ),
            );
        }
        let mut refuse = self.refuse.borrow_mut();
        let (rng, chance) = &mut *refuse;
        if rng.chance(*chance) {
            cell.reach("hand-over refused");
            return Err("the node refused the update".to_owned());
        }
        let t = cell.t;
        cell.handed.push((t, node.clone()));
        Ok(())
    }
}
