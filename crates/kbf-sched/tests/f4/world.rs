//! The F4 world: a fleet of workers with a daemon's rules, arrivals, an operator and
//! an in-process control log, driven one simulated second at a time. Every input goes
//! through the [`Checker`].
//!
//! Workers heartbeat every 5 s with their running set (runs not ended, and ended runs
//! whose result is not yet acknowledged), send a result when a run ends, cancel the
//! runs a heartbeat's reply names as not held, and stop a self-fenced run once T has
//! passed since their newest acknowledged send while they cannot reach the server.
//! A worker can reboot (its runs die), have its daemon restart (its runs go on and
//! are listed again), lose its network (a partition: it registers again on a new
//! stream if it was gone 30 s or more), die and return, or change its node report.
//! A `Start` may take up to 2 s to arrive, and is lost if the worker's stream changed
//! meanwhile. The log commits records in the order they were proposed, after an
//! optional delay, and stalls during F4.4's maintenance.

mod faults;

use std::collections::{BTreeMap, VecDeque};

use kbf_caps::NodeCaps;
use kbf_sched::fence::SELF_FENCE;
use kbf_sched::{Event, Request, Scheduler};
use kbf_sim::{Chance, SimRng};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, Failure, FarmTime, FencePolicy,
    LeaseId, OperationId, Outcome, Qos, Resources, StartLease, WaiterId, WorkerId,
};

use super::check::{Checker, TERM};

pub const GIB: u64 = 1 << 30;
/// The unservable wait the scheduler is built with.
pub const WAIT_S: u64 = 60;
const HEARTBEAT_S: u64 = 5;
/// G, in seconds.
const GRACE_S: u64 = 60;
/// The longest run.
const MAX_RUN_S: u64 = 120;
/// A partition at least this long breaks the stream: the worker registers again.
const NEW_STREAM_AFTER_S: u64 = 30;
/// The mean CPU of the request size mix, in millicores, without the oversized
/// requests (they are refused, and book nothing).
const MEAN_CPU_MILLIS: u64 = 3_940;

/// The platforms actions ask for, as REAPI properties, and how often (of 100).
const PLATFORMS: [(&[(&str, &str)], u64); 13] = [
    (&[], 28),
    (&[("OSFamily", "Linux")], 25),
    (&[("ISA", "x86-64-v3")], 10),
    (&[("ISA", "x86-64-v4")], 5),
    (&[("ISA", "arm-a64")], 6),
    (&[("OSFamily", "Linux"), ("ISA", "arm-a64")], 10),
    (&[("OSFamily", "Darwin")], 4),
    (&[("OSFamily", "Darwin"), ("xcode", "16C5032a")], 3),
    (&[("OSFamily", "Darwin"), ("xcode", "15F31d")], 2),
    // No worker ever has these: always refused.
    (&[("OSFamily", "Darwin"), ("xcode", "99Z999")], 1),
    (&[("label.pool", "nowhere")], 1),
    (&[("ISA", "x86-64-v2")], 3),
    // Only the few release nodes, so a cordon or an outage can take them all.
    (&[("label.pool", "release")], 2),
];
/// How many nodes carry `label.pool=release`: the first ones in the storm set.
const RELEASE_NODES: usize = 4;

/// The scenario a seed runs: `seed % 4`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// F4.1: arrivals at 70 to 95 percent of capacity, no worker or operator faults.
    Steady,
    /// F4.2: 5 percent of workers die or return each minute.
    Churn,
    /// F4.3: every worker registers again within a few seconds, once or twice.
    MassReconnect,
    /// F4.4: cordon, drain and uncordon at random on a tenth of the fleet.
    OperatorStorm,
}

impl Scenario {
    pub const ALL: [Self; 4] = [
        Self::Steady,
        Self::Churn,
        Self::MassReconnect,
        Self::OperatorStorm,
    ];

    pub fn of(seed: u64) -> Self {
        Self::ALL[usize::try_from(seed % 4).expect("small")]
    }
}

/// Which fault kinds a seed switches on, and how often each strikes (per second).
#[derive(Clone, Debug)]
struct Swarm {
    utilisation: u64,
    log_delay_s: u64,
    start_delay_s: u64,
    lose_start: Chance,
    drop_heartbeat: Chance,
    duplicate_report: Chance,
    report_started: bool,
    reboot: Chance,
    restart: Chance,
    partition: Chance,
    capacity: Chance,
    operator: Chance,
    dedup: Chance,
}

impl Swarm {
    fn draw(rng: &mut SimRng, scenario: Scenario) -> Self {
        let mut kind = |max_percent: u64| {
            if rng.chance(Chance::percent(50)) {
                Chance::per_million(
                    u32::try_from(rng.between(1, max_percent * 10_000)).expect("small"),
                )
            } else {
                Chance::never()
            }
        };
        let faults = scenario != Scenario::Steady;
        let mut s = Self {
            utilisation: 0,
            log_delay_s: 0,
            start_delay_s: 0,
            lose_start: kind(2),
            drop_heartbeat: kind(5),
            duplicate_report: kind(10),
            report_started: false,
            reboot: kind(3),
            restart: kind(3),
            partition: kind(5),
            capacity: kind(5),
            operator: kind(3),
            dedup: Chance::percent(u32::try_from(rng.between(5, 30)).expect("small")),
        };
        if !faults {
            s.lose_start = Chance::never();
            s.drop_heartbeat = Chance::never();
            s.reboot = Chance::never();
            s.restart = Chance::never();
            s.partition = Chance::never();
            s.capacity = Chance::never();
            s.operator = Chance::never();
        }
        s.utilisation = rng.between(70, 95);
        // Churn and the operator storm always have a slow log: a grant to a worker
        // that goes silent can then commit after the grant that replaced it (a
        // superseded grant, I2).
        s.log_delay_s = if matches!(scenario, Scenario::Churn | Scenario::OperatorStorm) {
            rng.between(2, 3)
        } else if rng.chance(Chance::percent(50)) {
            rng.between(1, 3)
        } else {
            0
        };
        s.report_started = rng.chance(Chance::percent(50));
        s.start_delay_s = if rng.chance(Chance::percent(50)) {
            rng.between(1, 2)
        } else {
            0
        };
        s
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Up,
    /// The daemon is gone until `until`; its runs go on if `keeps_runs` (a restart),
    /// else they died with the machine (a reboot, or a death if `until` is never).
    Down {
        until: u64,
        keeps_runs: bool,
    },
    /// Cut off from the server from `from` until `until`; its runs go on.
    Cut {
        from: u64,
        until: u64,
    },
}

#[derive(Clone, Copy, Debug)]
struct Run {
    op: OperationId,
    ends: u64,
    fence: FencePolicy,
}

#[derive(Clone, Debug)]
struct Node {
    name: WorkerId,
    report: Vec<(&'static str, String)>,
    caps: NodeCaps,
    base: Resources,
    capacity: Resources,
    phase: u64,
    link: Link,
    runs: BTreeMap<LeaseId, Run>,
    unacked: BTreeMap<LeaseId, (OperationId, Outcome)>,
    last_ack: u64,
    /// Counts the node's streams: a `Start` sent on one is never delivered on another.
    stream: u64,
}

impl Node {
    fn draw(i: usize, rng: &mut SimRng) -> Self {
        let x86 = |level: usize| -> Vec<(&'static str, String)> {
            let mut r = vec![("arch", "x86_64".to_owned()), ("os", "linux".to_owned())];
            for l in kbf_caps::X86Level::ALL.iter().take(level) {
                r.extend(l.adds().iter().map(|f| ("cpu.features", (*f).to_owned())));
            }
            r
        };
        let cores = |rng: &mut SimRng, lo: u64, hi: u64| rng.between(lo, hi) * 4_000;
        let (report, base) = match rng.below(100) {
            0..15 => (
                x86(2),
                Resources::new(cores(rng, 2, 8), rng.between(32, 128) * GIB),
            ),
            15..35 => (
                x86(3),
                Resources::new(cores(rng, 2, 8), rng.between(32, 128) * GIB),
            ),
            35..50 => (
                x86(4),
                Resources::new(cores(rng, 2, 16), rng.between(64, 256) * GIB),
            ),
            50..70 => (
                vec![("arch", "arm64".to_owned()), ("os", "linux".to_owned())],
                Resources::new(cores(rng, 2, 8), rng.between(32, 128) * GIB),
            ),
            70..85 => {
                let mut r = vec![
                    ("arch", "arm64".to_owned()),
                    ("os", "macos".to_owned()),
                    ("xcode", "16C5032a".to_owned()),
                ];
                if rng.chance(Chance::percent(50)) {
                    r.push(("xcode", "15F31d".to_owned()));
                }
                (
                    r,
                    Resources::new(cores(rng, 1, 3), rng.between(16, 64) * GIB),
                )
            }
            _ => {
                let gpus = [1, 2, 4][usize::try_from(rng.below(3)).expect("small")];
                (
                    x86(3),
                    Resources::new(cores(rng, 4, 8), rng.between(64, 128) * GIB).with_gpus(gpus),
                )
            }
        };
        let caps = caps_of(&report);
        Self {
            name: WorkerId::new(format!("w{i:03}")),
            report,
            caps,
            base,
            capacity: base,
            phase: rng.below(HEARTBEAT_S),
            link: Link::Up,
            runs: BTreeMap::new(),
            unacked: BTreeMap::new(),
            last_ack: 0,
            stream: 0,
        }
    }
}

fn caps_of(report: &[(&'static str, String)]) -> NodeCaps {
    NodeCaps::from_report(report.iter().map(|(k, v)| (*k, v.as_str()))).expect("valid report")
}

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

#[derive(Clone, Debug)]
struct Template {
    request: Request,
    duration: u64,
}

/// One seed's run.
pub struct World {
    pub check: Checker,
    pub scenario: Scenario,
    rng: SimRng,
    swarm: Swarm,
    ops: u64,
    t: u64,
    nodes: Vec<Node>,
    by_name: BTreeMap<WorkerId, usize>,
    /// Proposed records: when each commits, in proposal order.
    log: VecDeque<(u64, ControlRecord)>,
    log_last: u64,
    recent: VecDeque<Template>,
    durations: BTreeMap<ActionKey, u64>,
    next_key: u64,
    submitted: u64,
    rate_milli: u64,
    owed_milli: u64,
    storm: Vec<usize>,
    /// `Start`s on their way: when each arrives, to which node and stream.
    starts: BTreeMap<(u64, u64), (usize, u64, StartLease)>,
    sent: u64,
    /// When the release pool is drained, and when it then goes offline (F4.4).
    maintenance: (u64, u64),
    /// When the operator uncordons nodes still offline.
    early_uncordon_at: Option<u64>,
    /// The control log commits nothing in these seconds.
    stall: (u64, u64),
    mass_at: Vec<u64>,
    quiesced_at: Option<u64>,
    bound_s: u64,
    trace: u64,
    /// Seconds from the last arrival until every operation was finished.
    pub drained_in: u64,
}

impl World {
    pub fn new(seed: u64, workers: usize, ops: u64) -> Self {
        let mut rng = SimRng::from_seed(seed);
        let scenario = Scenario::of(seed);
        let swarm = Swarm::draw(&mut rng, scenario);
        let mut nodes: Vec<Node> = (0..workers).map(|i| Node::draw(i, &mut rng)).collect();
        let mut storm: Vec<usize> = (0..workers).collect();
        rng.shuffle(&mut storm);
        storm.truncate(workers.div_ceil(10));
        for &i in storm.iter().take(RELEASE_NODES) {
            let n = &mut nodes[i];
            n.report.push(("label.pool", "release".to_owned()));
            n.caps = caps_of(&n.report);
        }
        let by_name = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.name.clone(), i))
            .collect();
        let cpu: u64 = nodes.iter().map(|n| n.base.cpu_millis).sum();
        let mean_run = (5 + MAX_RUN_S) / 2;
        let rate_milli = swarm.utilisation * cpu * 10 / (MEAN_CPU_MILLIS * mean_run);
        let active_s = ops * 1_000 / rate_milli.max(1);
        let mass_at = (0..rng.between(1, 2))
            .map(|_| rng.between(30, active_s.max(31)))
            .collect();
        // Early, so that its refusals fall before arrivals stop.
        let drain_at = rng.between(10, 20);
        let maintenance = (drain_at, drain_at + rng.between(10, 20));
        let replay = format!(
            "KBF_SIM_SEED={seed} KBF_SIM_OPS={ops} cargo test -p kbf-sched --test f4_fleet -- \
             --ignored --exact replay  ({scenario:?}, {workers} workers)"
        );
        let wait = std::time::Duration::from_secs(WAIT_S);
        let check = Checker::new(
            Scheduler::new(TERM).with_unservable_wait(wait),
            WAIT_S * 1_000,
            replay,
        );
        let mut w = Self {
            check,
            scenario,
            rng,
            swarm,
            ops,
            t: 0,
            nodes,
            by_name,
            log: VecDeque::new(),
            log_last: 0,
            recent: VecDeque::new(),
            durations: BTreeMap::new(),
            next_key: 0,
            submitted: 0,
            rate_milli,
            owed_milli: 0,
            storm,
            starts: BTreeMap::new(),
            sent: 0,
            maintenance,
            early_uncordon_at: None,
            stall: (0, 0),
            mass_at,
            quiesced_at: None,
            bound_s: 0,
            trace: 0xcbf2_9ce4_8422_2325,
            drained_in: 0,
        };
        for i in 0..w.nodes.len() {
            w.register(i);
        }
        w
    }

    /// The run's trace hash: every input and effect, as text.
    pub const fn trace(&self) -> u64 {
        self.trace
    }

    fn record(&mut self, line: &str) {
        for b in line.bytes().chain(std::iter::once(b'\n')) {
            self.trace = (self.trace ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    /// Runs to the end: arrivals until `ops` were submitted, then no faults until
    /// every operation is finished (L1).
    pub fn run(&mut self) {
        loop {
            self.second();
            let Some(at) = self.quiesced_at else { continue };
            if self.check.quiet() && self.log.is_empty() {
                self.drained_in = self.t - at;
                break;
            }
            if self.t > at + self.bound_s {
                let left: Vec<String> = self
                    .check
                    .unfinished()
                    .take(5)
                    .map(|(id, r)| format!("{id} {:?} {:?}", r.qos, r.resources))
                    .collect();
                self.check.violated(
                    "L1",
                    format!(
                        "work unfinished {} s after arrivals and faults stopped: {left:?}",
                        self.bound_s
                    ),
                );
            }
        }
        self.check.check_quiet();
    }

    fn feed(&mut self, event: Event) {
        let t = self.t;
        self.record(&format!("{t} {event:?}"));
        let effects = self.check.feed(t * 1_000, event);
        for effect in effects {
            self.record(&format!("  {effect:?}"));
            self.carry_out(effect);
        }
    }

    /// Feeds `event`, then commits what the log has due.
    fn input(&mut self, event: Event) {
        self.feed(event);
        self.pump();
    }

    /// Commits, in log order, every record due by now.
    fn pump(&mut self) {
        while !(self.stall.0..self.stall.1).contains(&self.t)
            && self.log.front().is_some_and(|(at, _)| *at <= self.t)
        {
            let (_, record) = self.log.pop_front().expect("checked");
            self.feed(Event::Committed(record));
        }
    }

    fn carry_out(&mut self, effect: Effect) {
        match effect {
            Effect::Commit(record) => {
                let delay = if self.swarm.log_delay_s > 0 {
                    self.rng.below(self.swarm.log_delay_s + 1)
                } else {
                    0
                };
                self.log_last = self.log_last.max(self.t + delay);
                self.log.push_back((self.log_last, record));
            }
            Effect::Start(start) => {
                let i = self.by_name[&start.worker];
                let delay = if self.swarm.start_delay_s > 0 {
                    self.rng.below(self.swarm.start_delay_s + 1)
                } else {
                    0
                };
                let stream = self.nodes[i].stream;
                if delay == 0 {
                    self.deliver(i, stream, start);
                } else {
                    self.sent += 1;
                    self.starts
                        .insert((self.t + delay, self.sent), (i, stream, start));
                }
            }
            Effect::Answer(_) | Effect::Refuse(_) | Effect::Waiting(_) => {}
        }
    }

    /// A `Start` reaches node `i`, if it is up on the stream it was sent on.
    fn deliver(&mut self, i: usize, stream: u64, start: StartLease) {
        let n = &self.nodes[i];
        if n.link != Link::Up || n.stream != stream || self.rng.chance(self.swarm.lose_start) {
            self.check.hit("Start lost");
            return;
        }
        let ends = self.t + self.durations[&start.key];
        self.check.run_begins(start.operation, start.fence);
        let run = Run {
            op: start.operation,
            ends,
            fence: start.fence,
        };
        self.nodes[i].runs.insert(start.lease, run);
        if self.swarm.report_started && self.rng.chance(Chance::percent(50)) {
            self.feed(Event::Started {
                operation: start.operation,
                lease: start.lease,
            });
        }
    }

    fn second(&mut self) {
        let active = self.quiesced_at.is_none();
        self.pump();
        if active {
            self.faults();
        }
        self.returns();
        while let Some(entry) = self.starts.first_entry() {
            if entry.key().0 > self.t {
                break;
            }
            let (i, stream, start) = entry.remove();
            self.check.hit("Start delayed");
            self.deliver(i, stream, start);
            self.pump();
        }
        for i in 0..self.nodes.len() {
            self.node_second(i);
        }
        if active {
            self.operator();
            self.arrivals();
        }
        self.input(Event::Tick);
        if self.t.is_multiple_of(30) {
            self.check.full_check();
        }
        self.t += 1;
    }

    fn register(&mut self, i: usize) {
        let n = &mut self.nodes[i];
        n.link = Link::Up;
        n.last_ack = self.t;
        n.stream += 1;
        let event = Event::WorkerUp {
            worker: n.name.clone(),
            capacity: n.capacity,
            caps: n.caps.clone(),
        };
        self.input(event);
    }

    /// Brings back every node whose outage ends now.
    fn returns(&mut self) {
        let t = self.t;
        for i in 0..self.nodes.len() {
            let new_stream = match self.nodes[i].link {
                Link::Up => continue,
                Link::Down { until, .. } if until <= t => true,
                Link::Cut { from, until } if until <= t => t - from >= NEW_STREAM_AFTER_S,
                _ => continue,
            };
            if new_stream {
                self.register(i);
            } else {
                self.nodes[i].link = Link::Up;
            }
            self.heartbeat(i);
            self.report(i);
        }
    }

    fn node_second(&mut self, i: usize) {
        let t = self.t;
        let n = &mut self.nodes[i];
        if matches!(
            n.link,
            Link::Down {
                keeps_runs: false,
                ..
            }
        ) {
            return;
        }
        let fence_s = SELF_FENCE.as_secs();
        let fenced = n.link != Link::Up && t >= n.last_ack + fence_s;
        let ended: Vec<LeaseId> = n
            .runs
            .iter()
            .filter(|(_, r)| r.ends <= t || (fenced && r.fence == FencePolicy::SelfFence))
            .map(|(l, _)| *l)
            .collect();
        for lease in ended {
            let run = self.nodes[i].runs.remove(&lease).expect("listed");
            let outcome = if run.ends > t {
                self.check.hit("self-fence stopped a run");
                Outcome::Failed(Failure::Infra)
            } else if self.rng.chance(Chance::percent(3)) {
                Outcome::Failed(Failure::Timeout)
            } else {
                Outcome::Completed {
                    action_result: digest(lease.seq << 20 | run.op.0),
                }
            };
            self.check.produced(lease, outcome);
            self.check.run_ends(run.op, run.fence);
            self.nodes[i].unacked.insert(lease, (run.op, outcome));
        }
        if self.nodes[i].link == Link::Up {
            self.report(i);
            if (t + self.nodes[i].phase).is_multiple_of(HEARTBEAT_S) {
                self.heartbeat(i);
            }
        }
    }

    /// Node `i` sends every result not yet acknowledged; each is acknowledged.
    fn report(&mut self, i: usize) {
        let unacked = std::mem::take(&mut self.nodes[i].unacked);
        for (lease, (operation, outcome)) in unacked {
            let event = Event::Report {
                operation,
                lease,
                outcome,
            };
            if self.rng.chance(self.swarm.duplicate_report) {
                self.input(event.clone());
            }
            self.input(event);
        }
    }

    fn heartbeat(&mut self, i: usize) {
        let n = &self.nodes[i];
        let mut running: Vec<LeaseId> = n.runs.keys().chain(n.unacked.keys()).copied().collect();
        running.sort_unstable();
        if self.rng.chance(self.swarm.drop_heartbeat) {
            self.check.hit("heartbeat lost");
            return;
        }
        let worker = n.name.clone();
        self.nodes[i].last_ack = self.t;
        self.input(Event::Heartbeat {
            worker: worker.clone(),
            running: running.clone(),
        });
        for lease in self.check.not_held(&worker, &running) {
            if let Some(run) = self.nodes[i].runs.remove(&lease) {
                self.check.run_ends(run.op, run.fence);
            }
            self.nodes[i].unacked.remove(&lease);
        }
    }

    fn operator(&mut self) {
        let storm = self.scenario == Scenario::OperatorStorm;
        let chance = if storm {
            Chance::percent(30)
        } else {
            self.swarm.operator
        };
        if !self.rng.chance(chance) {
            return;
        }
        let i = if storm {
            self.storm[usize::try_from(self.rng.below(self.storm.len() as u64)).expect("small")]
        } else {
            usize::try_from(self.rng.below(self.nodes.len() as u64)).expect("small")
        };
        let worker = self.nodes[i].name.clone();
        let event = match self.rng.below(3) {
            0 => Event::Cordon { worker },
            1 => Event::Drain {
                worker,
                deadline: FarmTime::from_millis((self.t + self.rng.between(1, 120)) * 1_000),
            },
            _ => Event::Uncordon { worker },
        };
        self.input(event);
    }

    fn arrivals(&mut self) {
        self.owed_milli += self.rate_milli;
        while self.owed_milli >= 1_000 && self.submitted < self.ops {
            self.owed_milli -= 1_000;
            self.submit();
        }
        if self.submitted == self.ops {
            self.quiesce();
        }
    }

    fn submit(&mut self) {
        let n = self.submitted;
        self.submitted += 1;
        let reuse = !self.recent.is_empty() && self.rng.chance(self.swarm.dedup);
        let mut template = if reuse {
            let i = usize::try_from(self.rng.below(self.recent.len() as u64)).expect("small");
            let mut twin = self.recent[i].clone();
            // A twin that may not join: networked, or not to be cached.
            if self.rng.chance(Chance::percent(25)) {
                self.check.hit("non-joinable twin");
                if self.rng.chance(Chance::percent(50)) {
                    twin.request.hermetic = false;
                } else {
                    twin.request.do_not_cache = true;
                }
            }
            twin
        } else {
            self.template()
        };
        template.request.qos = match self.rng.below(100) {
            0..15 => Qos::Interactive,
            15..65 => Qos::Ci,
            65..90 => Qos::Batch,
            _ => Qos::custom("nightly", 150).expect("valid"),
        };
        self.durations
            .insert(template.request.key.clone(), template.duration);
        let request = template.request.clone();
        self.recent.push_back(template);
        if self.recent.len() > 64 {
            self.recent.pop_front();
        }
        self.input(Event::Submit {
            waiter: WaiterId(n),
            request,
        });
    }

    fn template(&mut self) -> Template {
        let rng = &mut self.rng;
        let mut pick = rng.below(100);
        let mut platform = PLATFORMS.len() - 1;
        for (i, (_, weight)) in PLATFORMS.iter().enumerate() {
            if pick < *weight {
                platform = i;
                break;
            }
            pick -= weight;
        }
        let resources = match rng.below(100) {
            0..56 => Resources::new(1_000, GIB),
            56..76 => Resources::new(rng.between(2, 8) * 1_000, rng.between(2, 16) * GIB),
            76..86 => Resources::new(2_000, rng.between(24, 96) * GIB),
            86..96 => Resources::new(rng.between(12, 24) * 1_000, 8 * GIB),
            96..98 => {
                platform = usize::try_from(rng.between(1, 2)).expect("small");
                Resources::new(4_000, 16 * GIB).with_gpus(rng.between(1, 4))
            }
            // Larger than any worker.
            _ => Resources::new(128_000, 64 * GIB),
        };
        // Now and then the digest of an earlier action under another instance: a
        // different key, never a join.
        let (instance, number) = if self.next_key > 0 && rng.chance(Chance::percent(3)) {
            ("other", rng.below(self.next_key))
        } else {
            self.next_key += 1;
            ("main", self.next_key - 1)
        };
        let request = Request {
            key: ActionKey {
                instance: instance.to_owned(),
                action: digest(number),
            },
            qos: Qos::Ci,
            resources,
            needs: kbf_caps::Request::from_platform(PLATFORMS[platform].0.iter().copied())
                .expect("valid platform"),
            hermetic: rng.chance(Chance::percent(85)),
            do_not_cache: rng.chance(Chance::percent(5)),
        };
        Template {
            request,
            duration: rng.between(5, MAX_RUN_S),
        }
    }

    /// Arrivals are done: no new fault starts, every dead node comes back (an outage
    /// under way runs its course), every cordon ends, and the L1 bound is computed
    /// from the backlog and the outages left.
    fn quiesce(&mut self) {
        if self.quiesced_at.is_some() {
            return;
        }
        let t = self.t;
        self.quiesced_at = Some(t);
        // A dead node returns now; an outage under way runs its course.
        let mut outage = 0;
        for n in &mut self.nodes {
            match &mut n.link {
                Link::Down { until, .. } | Link::Cut { until, .. } => {
                    if *until == u64::MAX {
                        *until = t + 1;
                    }
                    outage = outage.max(*until - t);
                }
                Link::Up => {}
            }
        }
        for worker in self.check.cordoned() {
            self.input(Event::Uncordon { worker });
        }
        // The backlog, as if each operation ran alone on the capacity that can run it,
        // one after another: a loose bound, but a finite one.
        let mut backlog = 0;
        for (_, request) in self.check.unfinished() {
            let matching: Vec<&Node> = self
                .nodes
                .iter()
                .filter(|n| request.needs.matches(&n.caps))
                .collect();
            let cpu: u64 = matching.iter().map(|n| n.base.cpu_millis).sum();
            let gpus: u64 = matching.iter().map(|n| n.base.gpus).sum();
            let run = self.durations[&request.key];
            backlog += request.resources.cpu_millis * run / cpu.max(1);
            backlog += request.resources.gpus * run / gpus.max(1);
        }
        self.bound_s =
            outage + 2 * backlog + 2 * MAX_RUN_S + GRACE_S + WAIT_S + 2 * HEARTBEAT_S + 10;
    }
}
