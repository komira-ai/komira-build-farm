//! Platform routing over a seed sweep: three workers of different platforms (a Linux
//! x86-64 machine, a Linux arm64 machine and an Apple silicon Mac); the arm64 machine
//! and the Mac each go silent for two random windows and come back in between, while
//! actions asking for each platform, and one no worker has, are submitted at random.
//! The Mac has one core and runs an action for 20 to 39 s, so work for it queues
//! behind its running action: work that waited through the Mac's first outage can be
//! servable (the Mac is live but busy) when the second begins, and its wait must then
//! start again. The control log is in process: a `Commit` is fed straight back. A
//! running lease reports its result a few seconds after its `Start` (20 to 39 on the
//! Mac), unless its worker went silent first.
//!
//! The checks, for every seed:
//! - every grant goes to a worker that was live and whose node report satisfies the
//!   action's platform;
//! - every operation is answered exactly once, by a result or by a refusal;
//! - a refusal comes only after its callers were told why the operation waits, and
//!   only when, at every tick of the wait before it, no live worker satisfied the
//!   platform;
//! - the refusal comes at the deadline exactly: at the tick W after the first of an
//!   unbroken run of ticks at which the operation was queued and no live worker
//!   satisfied its platform, neither before (the run restarts whenever one does) nor
//!   after (no operation stays queued past it);
//! - the sweep refuses some operation whose run restarted: it was queued and servable
//!   between two unservable stretches, so a wait that does not start again when the
//!   operation is servable would refuse it early;
//! - an action no worker can ever run (`x86-64-v4`) is always refused, and one the
//!   Linux x86-64 machine (always up) satisfies always runs;
//! - a seed replays to the same effects.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, OpState, Request, Scheduler};
use kbf_sim::SimRng;
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseId, OperationId,
    Outcome, Qos, Resources, StateMachine, WaiterId, WorkerId,
};

const SEEDS: u64 = 64;
const WAIT: Duration = Duration::from_secs(60);
const GIB: u64 = 1 << 30;
const HEARTBEAT: u64 = 5;
/// The scheduler's grace G: a worker not heard from for this long is not live.
const GRACE: u64 = 60;
/// Actions are submitted before this; every worker is back by then (its first outage
/// starts before a quarter of it, and both outages with the gap between them last at
/// most 390 s).
const SUBMIT_UNTIL: u64 = 600;
/// Time for the Mac, one action at a time, to work through what queued for it.
const MAC_BACKLOG: u64 = 900;
const END: u64 = SUBMIT_UNTIL + 2 * GRACE + 2 * WAIT.as_secs() + MAC_BACKLOG;
const OPS: u64 = 40;

/// The platforms actions ask for, as REAPI properties.
const PLATFORMS: [&[(&str, &str)]; 6] = [
    &[],
    &[("OSFamily", "Linux")],
    &[("OSFamily", "Darwin")],
    &[("ISA", "arm-a64")],
    &[("ISA", "x86-64-v4")],
    &[("OSFamily", "darwin"), ("label.pool", "darwin")],
];
/// The platform index no worker ever satisfies.
const NEVER: usize = 4;
/// The worker that never goes silent, and the platforms it satisfies.
const ALWAYS_UP: &str = "linux-x86";
const ALWAYS: [usize; 2] = [0, 1];

fn workers() -> Vec<(&'static str, NodeCaps)> {
    let v3: Vec<&str> = kbf_caps::X86Level::ALL
        .iter()
        .take(3)
        .flat_map(|l| l.adds().iter().copied())
        .collect();
    let mut linux = vec![("arch", "x86_64"), ("os", "linux")];
    linux.extend(v3.iter().map(|f| ("cpu.features", *f)));
    vec![
        (
            "linux-arm",
            NodeCaps::from_report([("arch", "arm64"), ("os", "linux")])
                .unwrap()
                .with_drivers(["container"]),
        ),
        (
            "linux-x86",
            NodeCaps::from_report(linux)
                .unwrap()
                .with_drivers(["container"]),
        ),
        (
            "mac",
            NodeCaps::from_report([("arch", "arm64"), ("os", "macos"), ("label.pool", "darwin")])
                .unwrap()
                .with_drivers(["native"]),
        ),
    ]
}

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

struct World {
    sched: Scheduler,
    caps: BTreeMap<String, NodeCaps>,
    /// Per worker: the outage windows `[from, to)` in seconds, when it sends nothing.
    outage: BTreeMap<String, Vec<(u64, u64)>>,
    last_heard: BTreeMap<String, u64>,
    /// Per operation: its platform index.
    platform: BTreeMap<OperationId, usize>,
    /// Running leases: when they report, and on which worker.
    running: BTreeMap<LeaseId, (u64, OperationId, String)>,
    told: BTreeMap<OperationId, bool>,
    answered: BTreeMap<OperationId, Answered>,
    /// Every tick's live satisfying-worker check, per platform: `servable[t][p]`.
    servable: Vec<[bool; 6]>,
    trace: Vec<String>,
    /// Per operation: the consecutive ticks, up to the last, at which it was queued
    /// and no live worker satisfied its platform.
    unservable_ticks: BTreeMap<OperationId, u64>,
    /// Per refused operation: the second it was refused.
    refused_at: BTreeMap<OperationId, u64>,
    /// The operations that were queued and servable right after an unservable
    /// stretch: their unservable run restarted.
    restarted: BTreeSet<OperationId>,
    /// How many refused operations had a run that restarted before the refusal.
    restarted_refusals: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answered {
    Ran,
    Refused,
}

impl World {
    fn new(rng: &mut SimRng) -> Self {
        let mut caps = BTreeMap::new();
        let mut outage = BTreeMap::new();
        for (name, c) in workers() {
            caps.insert(name.to_owned(), c);
            // Each outage lasts at least the grace, so the worker is seen to leave; the
            // longer ones outlast the grace and the wait, so work is refused.
            let length = |rng: &mut SimRng| rng.between(GRACE, GRACE + 2 * WAIT.as_secs());
            let from = rng.below(SUBMIT_UNTIL / 4);
            let to = from + length(rng);
            let again = to + rng.between(1, SUBMIT_UNTIL / 20);
            let back = again + length(rng);
            // The Linux x86-64 machine stays up, so its work always has a live worker.
            let windows = if name == ALWAYS_UP {
                Vec::new()
            } else {
                vec![(from, to), (again, back)]
            };
            outage.insert(name.to_owned(), windows);
        }
        Self {
            sched: Scheduler::new(1).with_unservable_wait(WAIT),
            caps,
            outage,
            last_heard: BTreeMap::new(),
            platform: BTreeMap::new(),
            running: BTreeMap::new(),
            told: BTreeMap::new(),
            answered: BTreeMap::new(),
            servable: Vec::new(),
            trace: Vec::new(),
            unservable_ticks: BTreeMap::new(),
            refused_at: BTreeMap::new(),
            restarted: BTreeSet::new(),
            restarted_refusals: 0,
        }
    }

    fn up(&self, name: &str, t: u64) -> bool {
        !self.outage[name]
            .iter()
            .any(|&(from, to)| (from..to).contains(&t))
    }

    fn live(&self, name: &str, t: u64) -> bool {
        self.last_heard.get(name).is_some_and(|&h| t < h + GRACE)
    }

    /// Feeds `event` at `t` and carries out its effects, committing at once.
    fn feed(&mut self, t: u64, event: Event) {
        let now = FarmTime::from_millis(t * 1_000);
        let mut effects = self.sched.apply(Input::new(now, event));
        while !effects.is_empty() {
            let mut next = Vec::new();
            for effect in effects {
                self.trace.push(format!("{t} {effect:?}"));
                match effect {
                    Effect::Commit(ControlRecord::Lease(grant)) => {
                        let name = grant.worker.as_str().to_owned();
                        let p = self.platform[&grant.operation];
                        let needs = request(0, p).needs;
                        assert!(self.live(&name, t), "t={t}: {grant:?} to a silent worker");
                        assert!(
                            needs.matches(&self.caps[&name]),
                            "t={t}: {grant:?} to a worker that does not satisfy {:?}",
                            PLATFORMS[p]
                        );
                        let commit = Event::Committed(ControlRecord::Lease(grant));
                        next.extend(self.sched.apply(Input::new(now, commit)));
                    }
                    Effect::Commit(record) => {
                        next.extend(self.sched.apply(Input::new(now, Event::Committed(record))));
                    }
                    Effect::Start(start) => {
                        let done = t + run_secs(start.worker.as_str(), start.lease.seq);
                        let name = start.worker.as_str().to_owned();
                        self.running
                            .insert(start.lease, (done, start.operation, name));
                    }
                    Effect::Answer(answer) => self.answer(answer.operation, Answered::Ran),
                    Effect::Refuse(refusal) => {
                        let op = refusal.operation;
                        assert!(
                            self.told.get(&op).copied().unwrap_or(false),
                            "{op}: refused untold"
                        );
                        let p = self.platform[&op];
                        let wait = usize::try_from(WAIT.as_secs()).unwrap();
                        let start = self.servable.len().saturating_sub(wait + 1);
                        for (tick, servable) in self.servable[start..].iter().enumerate() {
                            assert!(
                                !servable[p],
                                "t={t}: {op} refused though a live worker satisfied {:?} at {}",
                                PLATFORMS[p],
                                start + tick
                            );
                        }
                        self.refused_at.insert(op, t);
                        self.answer(op, Answered::Refused);
                    }
                    Effect::Waiting(waiting) => {
                        self.told
                            .insert(waiting.operation, waiting.reason.is_some());
                    }
                }
            }
            effects = next;
        }
    }

    fn answer(&mut self, op: OperationId, how: Answered) {
        assert!(
            self.answered.insert(op, how).is_none(),
            "{op} answered twice"
        );
    }

    /// One second of the world: heartbeats, results, submissions, then a tick.
    fn second(&mut self, t: u64, rng: &mut SimRng) {
        for name in self.caps.keys().cloned().collect::<Vec<_>>() {
            if !self.up(&name, t) {
                // Its runs die with it.
                self.running.retain(|_, (_, _, w)| *w != name);
                continue;
            }
            let silent_too_long = !self.live(&name, t);
            if silent_too_long || (t > 0 && !self.up(&name, t - 1)) {
                let event = Event::WorkerUp {
                    worker: WorkerId::new(name.as_str()),
                    instance: DaemonInstance::new(name.as_str()),
                    capacity: capacity(&name),
                    caps: self.caps[&name].clone(),
                };
                self.last_heard.insert(name.clone(), t);
                self.feed(t, event);
            }
            if t.is_multiple_of(HEARTBEAT) {
                let running = self
                    .running
                    .iter()
                    .filter(|(_, (_, _, w))| *w == name)
                    .map(|(l, _)| *l)
                    .collect();
                self.last_heard.insert(name.clone(), t);
                let worker = WorkerId::new(name.as_str());
                self.feed(t, Event::Heartbeat { worker, running });
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
            let outcome = Outcome::Completed {
                action_result: digest(1_000 + operation.0),
            };
            self.feed(
                t,
                Event::Report {
                    operation,
                    lease,
                    outcome,
                },
            );
        }
        if t < SUBMIT_UNTIL && rng.below(SUBMIT_UNTIL / OPS) == 0 {
            let p = usize::try_from(rng.below(PLATFORMS.len() as u64)).unwrap();
            let n = self.platform.len() as u64;
            self.platform.insert(OperationId(n), p);
            let waiter = WaiterId(n);
            self.feed(
                t,
                Event::Submit {
                    waiter,
                    request: request(n, p),
                },
            );
        }
        let mut servable = [false; 6];
        for (p, s) in servable.iter_mut().enumerate() {
            let needs = request(0, p).needs;
            *s = self
                .caps
                .iter()
                .any(|(name, c)| self.live(name, t) && needs.matches(c));
        }
        self.servable.push(servable);
        self.feed(t, Event::Tick);
        self.check_deadline(t);
    }

    /// After the tick at `t`: counts, per operation, the consecutive ticks at which it
    /// was queued with no live worker satisfying its platform, and checks that it is
    /// refused at exactly the tick that makes W+1 of them (the first at 0 s, the last
    /// at W s): never earlier, never later.
    fn check_deadline(&mut self, t: u64) {
        let wait = WAIT.as_secs();
        let servable = *self.servable.last().expect("pushed before the tick");
        for (&op, &p) in &self.platform {
            let refused_now = self.refused_at.get(&op) == Some(&t);
            let queued = refused_now || self.sched.state(op) == Some(&OpState::Queued);
            let count = self.unservable_ticks.entry(op).or_default();
            if queued && servable[p] && *count > 0 {
                self.restarted.insert(op);
            }
            *count = if queued && !servable[p] {
                *count + 1
            } else {
                0
            };
            if refused_now {
                self.restarted_refusals += usize::from(self.restarted.contains(&op));
                assert_eq!(
                    *count,
                    wait + 1,
                    "t={t}: {op} refused after {count} unservable ticks, not at the deadline"
                );
            } else {
                assert!(
                    *count <= wait,
                    "t={t}: {op} still queued after {count} unservable ticks: refused late"
                );
            }
        }
    }
}

/// What worker `name` offers: the Mac one core, so its work queues behind itself.
fn capacity(name: &str) -> Resources {
    let cores = if name == "mac" { 1 } else { 8 };
    Resources::new(cores * 1_000, 16 * GIB)
}

/// How long lease `seq` runs on worker `name`: a few seconds, or longer on the Mac,
/// so work for it is still queued when it leaves again.
fn run_secs(name: &str, seq: u64) -> u64 {
    if name == "mac" {
        20 + seq % 20
    } else {
        3 + seq % 5
    }
}

fn request(n: u64, platform: usize) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(n),
        },
        qos: Qos::Ci,
        kind: kbf_types::LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::from_platform(PLATFORMS[platform].iter().copied()).unwrap(),
    }
}

fn run(seed: u64) -> World {
    let mut rng = SimRng::from_seed(seed);
    let mut world = World::new(&mut rng);
    for t in 0..END {
        world.second(t, &mut rng);
    }
    world
}

#[test]
fn every_action_runs_where_its_platform_is_satisfied_or_is_refused() {
    let mut refused = 0;
    let mut ran = 0;
    let mut restarted_refusals = 0;
    for seed in 0..SEEDS {
        let world = run(seed);
        restarted_refusals += world.restarted_refusals;
        for (op, p) in &world.platform {
            let how = world.answered.get(op).copied();
            assert!(
                how.is_some(),
                "seed {seed}: {op} ({:?}) never answered",
                PLATFORMS[*p]
            );
            match *p {
                NEVER => assert_eq!(how, Some(Answered::Refused), "seed {seed}: {op}"),
                p if ALWAYS.contains(&p) => {
                    assert_eq!(how, Some(Answered::Ran), "seed {seed}: {op}");
                }
                _ => {}
            }
            refused += usize::from(how == Some(Answered::Refused) && *p != NEVER);
            ran += usize::from(how == Some(Answered::Ran));
        }
    }
    // The sweep exercises both answers for satisfiable platforms, not only the
    // never-satisfiable one.
    assert!(
        refused > 0,
        "no satisfiable action was ever refused: outages too short"
    );
    assert!(ran > 0);
    // And a wait that started again: an operation servable between two unservable
    // stretches, refused a full wait after the second began.
    assert!(
        restarted_refusals > 0,
        "no refused operation was ever servable between unservable stretches"
    );
}

#[test]
fn a_seed_replays_exactly() {
    assert_eq!(run(7).trace, run(7).trace);
    assert_ne!(run(7).trace, run(8).trace);
}
