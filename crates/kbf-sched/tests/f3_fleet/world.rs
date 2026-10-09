//! The F3 world: the scheduler fed directly (the control log is in process, so a
//! `Commit` is fed straight back), one-second ticks, a small mixed fleet in which one
//! worker is the last of its class (the only Linux arm64 machine, the only Mac), an
//! operator who cordons, drains and uncordons, workers that go down (losing their runs)
//! and come back registering again, and a Mac whose Xcode builds change.
//!
//! A scenario is a script of acts at given seconds plus a swarm of random ones (each
//! kind on or off, at a rate the seed draws). Every input goes through the checker.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use kbf_sim::{Chance, SimRng};
use kbf_types::{
    ActionKey, Digest, DigestFunction, Effect, FarmTime, LeaseId, OperationId, Outcome, Qos,
    Resources, StateMachine, WaiterId, WorkerId,
};

use crate::check::{Check, GRACE_MS, outcome_of};

const GIB: u64 = 1 << 30;
/// The unservable wait the scheduler is built with (shorter than the default, so a
/// sweep reaches refusals quickly).
pub const WAIT: u64 = 60;
/// G, in seconds.
pub const GRACE: u64 = GRACE_MS / 1_000;
pub const HEARTBEAT: u64 = 5;

/// The platforms actions ask for, as REAPI properties.
pub const PLATFORMS: [&[(&str, &str)]; 7] = [
    &[],
    &[("OSFamily", "Linux")],
    &[("OSFamily", "Darwin")],
    &[("OSFamily", "Linux"), ("ISA", "arm-a64")],
    &[("ISA", "x86-64-v4")],
    &[("OSFamily", "Darwin"), ("xcode", "16E140")],
    &[("xcode", "16C5032a")],
];
pub const ANY: usize = 0;
pub const LINUX: usize = 1;
pub const DARWIN: usize = 2;
/// Only `arm` satisfies it: the last worker of its class.
pub const ARM: usize = 3;
pub const NEVER: usize = 4;
/// The Xcode the Mac does not have until its capabilities change.
pub const XCODE_NEW: usize = 5;
pub const XCODE_OLD: usize = 6;

pub const OLD_XCODE: &str = "16C5032a";
pub const NEW_XCODE: &str = "16E140";

/// Request sizes: one core, two cores, and six (larger than the Mac).
pub const SIZES: [(u64, u64); 3] = [(1_000, GIB), (2_000, 2 * GIB), (6_000, 4 * GIB)];

/// The fleet, in name order: `arm` and `mac` are each the only one of their class.
pub const WORKERS: [&str; 4] = ["arm", "mac", "x86-a", "x86-b"];

pub fn mac_caps(xcodes: &[&str]) -> NodeCaps {
    let mut report = vec![("arch", "arm64"), ("os", "macos"), ("label.pool", "darwin")];
    report.extend(xcodes.iter().map(|x| ("xcode", *x)));
    NodeCaps::from_report(report).expect("a valid report")
}

fn caps(name: &str) -> NodeCaps {
    match name {
        "arm" => NodeCaps::from_report([("arch", "arm64"), ("os", "linux")]).expect("valid"),
        _ => {
            let mut report = vec![("arch", "x86_64"), ("os", "linux")];
            let v3 = kbf_caps::X86Level::ALL.iter().take(3);
            report.extend(v3.flat_map(|l| l.adds().iter().map(|f| ("cpu.features", *f))));
            NodeCaps::from_report(report).expect("valid")
        }
    }
}

fn capacity(name: &str) -> Resources {
    if name == "mac" {
        Resources::new(4_000, 8 * GIB)
    } else {
        Resources::new(8_000, 16 * GIB)
    }
}

/// Something that happens at a given second.
#[derive(Clone, Debug)]
pub enum Act {
    Cordon(&'static str),
    /// Drain with a deadline this many seconds from now.
    Drain(&'static str, u64),
    Uncordon(&'static str),
    /// The worker goes down: its runs are lost and it sends nothing.
    Down(&'static str),
    /// It comes back: it registers again (a new session) and heartbeats.
    Up(&'static str),
    /// The Mac's Xcode builds change (a `Hello` resent on its stream).
    Xcodes(Vec<&'static str>),
    /// An action for platform index `.0`, of size index `.1`, runs for `.2` seconds.
    Submit(usize, usize, u64),
}

/// Random acts: each kind on or off, and its rate.
#[derive(Clone, Debug, Default)]
pub struct Swarm {
    /// Per second, the chance of one submission, and the platforms it picks from.
    pub submit: Chance,
    pub platforms: Vec<usize>,
    /// Run times, in seconds.
    pub run: (u64, u64),
    pub operator: Chance,
    pub outage: Chance,
    pub xcodes: Chance,
    /// Random acts stop here; then every worker comes back, uncordoned.
    pub until: u64,
}

pub struct World {
    pub sched: Scheduler,
    pub check: Check,
    rng: SimRng,
    pub t: u64,
    pub up: BTreeMap<&'static str, bool>,
    /// The second each worker was last heard.
    pub heard: BTreeMap<&'static str, u64>,
    /// Running leases: when each reports, its operation and its worker.
    running: BTreeMap<LeaseId, (u64, OperationId, &'static str)>,
    /// The run time each operation was submitted with.
    run_for: BTreeMap<OperationId, u64>,
    /// Per operation: its platform index.
    pub platform: BTreeMap<OperationId, usize>,
    script: BTreeMap<u64, Vec<Act>>,
    swarm: Swarm,
    /// After each act: when, what, and the worker's cordon right after it.
    pub acts: Vec<(u64, Act, Option<kbf_sched::Cordon>)>,
    /// The Mac's capabilities now (its Xcode builds change).
    mac_caps: NodeCaps,
    /// The trace's 64-bit FNV-1a hash: every input and effect.
    pub hash: u64,
}

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

fn worker(name: &'static str) -> WorkerId {
    WorkerId::new(name)
}

impl World {
    pub fn new(scenario: &'static str, seed: u64, script: Vec<(u64, Act)>, swarm: Swarm) -> Self {
        let mut world = Self {
            sched: Scheduler::new(1).with_unservable_wait(Duration::from_secs(WAIT)),
            check: Check::new(scenario, seed, Duration::from_secs(WAIT)),
            rng: SimRng::from_seed(seed),
            t: 0,
            up: WORKERS.iter().map(|w| (*w, true)).collect(),
            heard: BTreeMap::new(),
            running: BTreeMap::new(),
            run_for: BTreeMap::new(),
            platform: BTreeMap::new(),
            script: BTreeMap::new(),
            swarm,
            acts: Vec::new(),
            mac_caps: mac_caps(&[OLD_XCODE]),
            hash: 0xcbf2_9ce4_8422_2325,
        };
        for (at, act) in script {
            world.script.entry(at).or_default().push(act);
        }
        for name in WORKERS {
            world.register(name);
        }
        world
    }

    fn mix(&mut self, line: &str) {
        for b in line.bytes().chain([b'\n']) {
            self.hash ^= u64::from(b);
            self.hash = self.hash.wrapping_mul(0x0100_0000_01b3);
        }
    }

    /// Feeds `event` now, checks it, and carries out its effects (committing at once).
    pub fn feed(&mut self, event: Event) {
        let now = FarmTime::from_millis(self.t * 1_000);
        let mut inputs = vec![Input::new(now, event)];
        while !inputs.is_empty() {
            let mut next = Vec::new();
            for input in inputs {
                self.mix(&format!("{} {:?}", self.t, input.event));
                let round = self.check.before(&input);
                let effects = self.sched.apply(input.clone());
                self.check
                    .after(&self.sched, &input, &effects, round.as_ref());
                for effect in effects {
                    self.mix(&format!("  {effect:?}"));
                    next.extend(self.carry_out(now, effect));
                }
            }
            inputs = next;
        }
    }

    fn carry_out(&mut self, now: FarmTime, effect: Effect) -> Vec<Input> {
        match effect {
            Effect::Commit(record) => vec![Input::new(now, Event::Committed(record))],
            Effect::Start(start) => {
                let name = WORKERS
                    .into_iter()
                    .find(|w| *w == start.worker.as_str())
                    .expect("a fleet worker");
                // A `Start` to a worker that is down is lost with it.
                if self.up[name] {
                    let ends = self.t + self.run_for[&start.operation];
                    self.running
                        .insert(start.lease, (ends, start.operation, name));
                }
                vec![Input::new(
                    now,
                    Event::Started {
                        operation: start.operation,
                        lease: start.lease,
                    },
                )]
            }
            Effect::Answer(_) | Effect::Refuse(_) | Effect::Waiting(_) => Vec::new(),
        }
    }

    fn register(&mut self, name: &'static str) {
        let caps = if name == "mac" {
            self.mac_caps.clone()
        } else {
            caps(name)
        };
        self.heard.insert(name, self.t);
        self.feed(Event::WorkerUp {
            worker: worker(name),
            instance: DaemonInstance::new(format!("{name}@{}", self.t)),
            capacity: capacity(name),
            caps,
        });
    }

    fn heartbeat(&mut self, name: &'static str) {
        let running = self
            .running
            .iter()
            .filter(|(_, (_, _, w))| *w == name)
            .map(|(l, _)| *l)
            .collect();
        self.heard.insert(name, self.t);
        self.feed(Event::Heartbeat {
            worker: worker(name),
            running,
        });
    }

    pub fn act(&mut self, act: Act) {
        let mut subject = None;
        match &act {
            Act::Cordon(w) => {
                subject = Some(*w);
                self.feed(Event::Cordon { worker: worker(w) });
            }
            Act::Drain(w, within) => {
                subject = Some(*w);
                let deadline = FarmTime::from_millis((self.t + within) * 1_000);
                self.feed(Event::Drain {
                    worker: worker(w),
                    deadline,
                });
            }
            Act::Uncordon(w) => {
                subject = Some(*w);
                self.feed(Event::Uncordon { worker: worker(w) });
            }
            Act::Down(w) => {
                if self.up[w] {
                    subject = Some(*w);
                    self.up.insert(w, false);
                    self.running.retain(|_, (_, _, on)| on != w);
                    self.check.down(&worker(w), true);
                }
            }
            Act::Up(w) => {
                if !self.up[w] {
                    subject = Some(*w);
                    self.up.insert(w, true);
                    self.check.down(&worker(w), false);
                    self.register(w);
                    // A daemon heartbeats as soon as it has registered.
                    self.heartbeat(w);
                }
            }
            Act::Xcodes(xcodes) => {
                self.mac_caps = mac_caps(xcodes);
                if self.up["mac"] {
                    subject = Some("mac");
                    self.heard.insert("mac", self.t);
                    self.feed(Event::Capacity {
                        worker: worker("mac"),
                        capacity: capacity("mac"),
                        caps: self.mac_caps.clone(),
                    });
                }
            }
            Act::Submit(platform, size, run) => {
                let id = self.check.next_op();
                self.platform.insert(id, *platform);
                self.run_for.insert(id, *run);
                let (cpu, mem) = SIZES[*size];
                let qos = [Qos::Interactive, Qos::Ci, Qos::Batch]
                    [usize::try_from(id.0 % 3).expect("small")]
                .clone();
                let request = Request {
                    key: ActionKey {
                        instance: "main".to_owned(),
                        action: digest(id.0),
                    },
                    qos,
                    resources: Resources::new(cpu, mem),
                    needs: kbf_caps::Request::from_platform(PLATFORMS[*platform].iter().copied())
                        .expect("a valid platform"),
                    hermetic: true,
                    do_not_cache: false,
                };
                self.feed(Event::Submit {
                    waiter: WaiterId(id.0),
                    request,
                });
            }
        }
        let cordon = subject.and_then(|w| self.sched.cordon(&worker(w)).cloned());
        self.acts.push((self.t, act, cordon));
    }

    /// The swarm's random acts for this second.
    fn random_acts(&mut self) -> Vec<Act> {
        let mut acts = Vec::new();
        let s = self.swarm.clone();
        if self.t >= s.until {
            return acts;
        }
        if !s.platforms.is_empty() && self.rng.chance(s.submit) {
            let p = s.platforms
                [usize::try_from(self.rng.below(s.platforms.len() as u64)).expect("small")];
            let size = match self.rng.below(10) {
                0..=6 => 0,
                7 | 8 => 1,
                _ => 2,
            };
            let run = self.rng.between(s.run.0, s.run.1);
            acts.push(Act::Submit(p, size, run));
        }
        if self.rng.chance(s.operator) {
            let w = WORKERS[usize::try_from(self.rng.below(4)).expect("small")];
            acts.push(match self.rng.below(3) {
                0 => Act::Cordon(w),
                1 => Act::Drain(w, self.rng.between(1, 120)),
                _ => Act::Uncordon(w),
            });
        }
        if self.rng.chance(s.outage) {
            // `x86-a` stays up, so work any Linux worker can run always has one.
            let w = ["arm", "mac", "x86-b"][usize::try_from(self.rng.below(3)).expect("small")];
            if self.up[w] {
                let back = self.t + self.rng.between(5, 2 * GRACE + WAIT);
                acts.push(Act::Down(w));
                self.script.entry(back).or_default().push(Act::Up(w));
            }
        }
        if self.rng.chance(s.xcodes) {
            let sets: [&[&'static str]; 3] = [&[OLD_XCODE], &[OLD_XCODE, NEW_XCODE], &[NEW_XCODE]];
            let pick = sets[usize::try_from(self.rng.below(3)).expect("small")];
            acts.push(Act::Xcodes(pick.to_vec()));
        }
        if self.t + 1 == s.until {
            for w in WORKERS {
                acts.push(Act::Up(w));
                acts.push(Act::Uncordon(w));
            }
        }
        acts
    }

    /// One second: scripted and random acts, heartbeats, results, then a tick.
    pub fn second(&mut self) {
        let mut acts = self.script.remove(&self.t).unwrap_or_default();
        acts.extend(self.random_acts());
        for act in acts {
            self.act(act);
        }
        for (i, name) in WORKERS.into_iter().enumerate() {
            if self.up[name] && (self.t + i as u64).is_multiple_of(HEARTBEAT) {
                self.heartbeat(name);
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
            self.feed(Event::Report {
                operation,
                lease,
                outcome: Outcome::Completed {
                    action_result: outcome_of(lease),
                },
            });
        }
        self.feed(Event::Tick);
    }

    /// Runs to `end` (seconds), calling `probe` after each second (`t` is still that
    /// second), then checks L1.
    pub fn run(mut self, end: u64, probe: &mut dyn FnMut(&Self)) -> Self {
        while self.t < end {
            self.second();
            probe(&self);
            self.t += 1;
        }
        self.check.finish(&self.sched);
        self
    }
}
