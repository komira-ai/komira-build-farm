//! The in-process world of the F1 family: the scheduler fed directly, every `Commit`
//! fed straight back as committed, one-second ticks, and workers that always heartbeat
//! (every [`HEARTBEAT`] seconds, listing every lease they run) and report each run's
//! result when it ends. Each input and its effects go through the [`Checker`], and both
//! go into the trace hash.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use kbf_sim::SimRng;
use kbf_types::{
    ActionKey, Digest, DigestFunction, Effect, Failure, FarmTime, LeaseId, OperationId, Outcome,
    Qos, Resources, StateMachine, WaiterId, WorkerId,
};

use crate::check::Checker;

pub const GIB: u64 = 1 << 30;
/// Each worker heartbeats this often, staggered by its index.
pub const HEARTBEAT: u64 = 5;

/// The platforms actions ask for, as REAPI properties.
pub const PLATFORMS: [&[(&str, &str)]; 4] = [
    &[],
    &[("OSFamily", "Linux")],
    &[("ISA", "arm-a64")],
    &[("OSFamily", "Darwin")],
];
pub const ANY: usize = 0;
pub const LINUX: usize = 1;
pub const ARM64: usize = 2;
pub const DARWIN: usize = 3;

/// The kinds of node in a fleet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
    LinuxX86,
    LinuxArm,
    Mac,
}

pub fn caps(node: Node) -> NodeCaps {
    match node {
        Node::LinuxX86 => {
            let mut report = vec![("arch", "x86_64"), ("os", "linux")];
            for level in kbf_caps::X86Level::ALL.iter().take(3) {
                report.extend(level.adds().iter().map(|f| ("cpu.features", *f)));
            }
            NodeCaps::from_report(report)
                .unwrap()
                .with_drivers(["container"])
        }
        Node::LinuxArm => NodeCaps::from_report([("arch", "arm64"), ("os", "linux")])
            .unwrap()
            .with_drivers(["container"]),
        Node::Mac => NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
            .unwrap()
            .with_drivers(["native"]),
    }
}

/// One worker of a scenario's fleet.
#[derive(Clone, Debug)]
pub struct Spec {
    pub name: WorkerId,
    pub capacity: Resources,
    pub node: Node,
}

impl Spec {
    pub fn new(name: &str, cores: u64, gib: u64, gpus: u64, node: Node) -> Self {
        Self {
            name: WorkerId::new(name),
            capacity: Resources::new(cores * 1_000, gib * GIB).with_gpus(gpus),
            node,
        }
    }
}

pub fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

/// The custom level between `ci` and `batch`.
pub fn release() -> Qos {
    Qos::custom("release", 150).unwrap()
}

/// The four levels, least urgent first.
pub fn levels() -> [Qos; 4] {
    [Qos::Batch, release(), Qos::Ci, Qos::Interactive]
}

/// A hermetic, cacheable request for action `key` under instance `main`.
pub fn request(key: u64, qos: Qos, resources: Resources, platform: usize) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(key),
        },
        qos,
        kind: kbf_types::LeaseKind::Action,
        resources,
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::from_platform(PLATFORMS[platform].iter().copied()).unwrap(),
    }
}

/// What a scenario adds to the world.
pub trait Scenario {
    /// The scenario's name, as `KBF_SIM_SCENARIO` takes it.
    fn name(&self) -> &'static str;
    /// The scheduler's unservable wait, in seconds.
    fn wait_secs(&self) -> u64 {
        60
    }
    /// The fleet, all registered at second 0.
    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec>;
    /// Each second, after heartbeats and results and before the tick: arrivals and
    /// capacity changes.
    fn second(&mut self, world: &mut World, rng: &mut SimRng);
    /// Each second, right after the tick.
    fn after_tick(&mut self, _world: &mut World, _rng: &mut SimRng) {}
    /// After this second nothing arrives and nothing changes.
    fn quiet_after(&self) -> u64;
    /// The L1 bound: by this second every operation is finished.
    fn horizon(&self) -> u64;
    /// Checks at the end of a run.
    fn finish(&mut self, _world: &World) {}
}

/// One run of a scenario.
pub struct World {
    pub sched: Scheduler,
    pub check: Checker,
    /// The second being simulated.
    pub t: u64,
    pub fleet: Vec<Spec>,
    /// Runs in progress: when each reports, its operation, its worker.
    running: BTreeMap<LeaseId, (u64, OperationId, WorkerId)>,
    /// Per operation: how long each of its runs takes, in seconds.
    run_for: BTreeMap<OperationId, u64>,
    next_waiter: u64,
    /// Operations answered or refused since the scenario last took them.
    pub finished: Vec<OperationId>,
    trace: Fnv,
    /// Inputs fed.
    pub inputs: u64,
}

/// FNV-1a over the trace text, written without allocating.
#[derive(Clone, Copy, Debug)]
pub struct Fnv(pub u64);

impl std::fmt::Write for Fnv {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        for b in s.bytes() {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        Ok(())
    }
}

impl World {
    fn new(name: &str, seed: u64, fleet: Vec<Spec>, wait_secs: u64) -> Self {
        let sched = Scheduler::new(1).with_unservable_wait(Duration::from_secs(wait_secs));
        let mut world = Self {
            sched,
            check: Checker::new(name, seed, wait_secs * 1_000),
            t: 0,
            fleet,
            running: BTreeMap::new(),
            run_for: BTreeMap::new(),
            next_waiter: 0,
            finished: Vec::new(),
            trace: Fnv(0xcbf2_9ce4_8422_2325),
            inputs: 0,
        };
        for spec in world.fleet.clone() {
            world.feed(Event::WorkerUp {
                instance: DaemonInstance::new(spec.name.as_str()),
                worker: spec.name,
                capacity: spec.capacity,
                caps: caps(spec.node),
            });
        }
        world
    }

    /// The trace hash so far.
    pub fn trace_hash(&self) -> u64 {
        self.trace.0
    }

    /// Feeds `event` now, then every record it proposes as committed and every `Start`
    /// as started, in order.
    pub fn feed(&mut self, event: Event) {
        let now = FarmTime::from_millis(self.t * 1_000);
        let mut pending = VecDeque::from([event]);
        while let Some(event) = pending.pop_front() {
            let input = Input::new(now, event);
            // The trace covers every input as well as every effect, so two runs that
            // fed different inputs never share a hash.
            let _ = write!(self.trace, "{} in {:?};", self.t, input.event);
            let effects = self.sched.apply(input.clone());
            self.inputs += 1;
            self.check.observe(&input, &effects, &self.sched);
            for effect in effects {
                self.record(&effect);
                match effect {
                    Effect::Commit(record) => pending.push_back(Event::Committed(record)),
                    Effect::Start(start) => {
                        let secs = self.run_for[&start.operation];
                        self.running
                            .insert(start.lease, (self.t + secs, start.operation, start.worker));
                        pending.push_back(Event::Started {
                            operation: start.operation,
                            lease: start.lease,
                        });
                    }
                    Effect::Answer(answer) => self.finished.push(answer.operation),
                    Effect::Refuse(refusal) => self.finished.push(refusal.operation),
                    Effect::Waiting(_) => {}
                }
            }
        }
    }

    fn record(&mut self, effect: &Effect) {
        let t = self.t;
        let tr = &mut self.trace;
        let _ = match effect {
            Effect::Commit(kbf_types::ControlRecord::Lease(g)) => {
                write!(
                    tr,
                    "{t} grant {} {} {};",
                    g.lease.seq, g.operation.0, g.worker
                )
            }
            Effect::Commit(kbf_types::ControlRecord::Refusal(r)) => {
                write!(tr, "{t} refuse {};", r.operation.0)
            }
            Effect::Commit(_) => write!(tr, "{t} commit;"),
            Effect::Start(s) => write!(tr, "{t} start {};", s.lease.seq),
            Effect::Answer(a) => write!(tr, "{t} answer {} {};", a.operation.0, a.lease.seq),
            Effect::Refuse(r) => write!(tr, "{t} refused {};", r.operation.0),
            Effect::Waiting(w) => write!(tr, "{t} wait {} {};", w.operation.0, w.reason.is_some()),
        };
    }

    /// Submits `request` for a new waiter; a new operation runs for `secs` each time
    /// it is granted. Returns the waiter.
    pub fn submit(&mut self, request: Request, secs: u64) -> WaiterId {
        let waiter = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        self.feed(Event::Submit { waiter, request });
        let op = self
            .check
            .op_of(waiter)
            .expect("the checker saw the submission");
        self.run_for.entry(op).or_insert(secs);
        waiter
    }

    /// Changes `worker`'s capacity, as a resent `Hello` does.
    pub fn set_capacity(&mut self, worker: usize, capacity: Resources) {
        let spec = &self.fleet[worker];
        let event = Event::Capacity {
            worker: spec.name.clone(),
            capacity,
            caps: caps(spec.node),
        };
        self.feed(event);
    }

    /// Whether a run is in progress.
    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    /// One second: heartbeats, results that are due, the scenario, the tick.
    fn second(&mut self, scenario: &mut dyn Scenario, rng: &mut SimRng) {
        let t = self.t;
        for i in 0..self.fleet.len() {
            if (t + i as u64).is_multiple_of(HEARTBEAT) {
                let worker = self.fleet[i].name.clone();
                let running = self
                    .running
                    .iter()
                    .filter(|(_, (_, _, w))| *w == worker)
                    .map(|(l, _)| *l)
                    .collect();
                self.feed(Event::Heartbeat { worker, running });
            }
        }
        let due: Vec<(LeaseId, OperationId)> = self
            .running
            .iter()
            .filter(|(_, (at, _, _))| *at <= t)
            .map(|(l, (_, op, _))| (*l, *op))
            .collect();
        for (lease, operation) in due {
            self.running.remove(&lease);
            // One run in eleven fails; the rest complete with a result that names
            // their lease, so I5 can tell whose result answered.
            let outcome = if lease.seq % 11 == 5 {
                Outcome::Failed(Failure::Timeout)
            } else {
                Outcome::Completed {
                    action_result: digest(1_000_000 + lease.seq),
                }
            };
            self.feed(Event::Report {
                operation,
                lease,
                outcome,
            });
        }
        scenario.second(self, rng);
        self.feed(Event::Tick);
        scenario.after_tick(self, rng);
        self.t += 1;
    }
}

/// Runs `scenario` under `seed` to its end, checking every input, then L1 and the F1
/// base world's own rule: workers always heartbeat and list what they run, so no lease
/// is ever given up.
pub fn run(scenario: &mut dyn Scenario, seed: u64) -> World {
    let mut rng = SimRng::from_seed(seed);
    let fleet = scenario.fleet(&mut rng);
    let mut world = World::new(scenario.name(), seed, fleet, scenario.wait_secs());
    loop {
        world.second(scenario, &mut rng);
        let quiet = world.t > scenario.quiet_after();
        if quiet && world.check.all_done() && !world.busy() {
            break;
        }
        if world.t > scenario.horizon() {
            break;
        }
    }
    world.check.quiescent(&world.sched);
    if world.check.stats.given_up > 0 {
        world.check.fail(
            "F1 base",
            &format!(
                "{} leases given up though every worker heartbeats",
                world.check.stats.given_up
            ),
        );
    }
    scenario.finish(&world);
    world
}
