//! Memory kills over a seed sweep: three Linux nodes of 4, 8 and 16 GiB that always
//! heartbeat, a dozen action keys each with a hidden memory need (some past the largest
//! node), submitted at random at 1 GiB, and runs that end at random: killed at their
//! own limit when they booked less than they need, else killed by a busy node at
//! random, else completed. The control log is in process: a `Commit` is fed straight
//! back.
//!
//! The checks, after every input of every seed, against a model of the rules:
//! - an operation's first run books its ask raised to its key's floor at submission;
//! - a run after an own-limit kill books the doubled ask, in whole GiB, capped at
//!   16 GiB; a run after a busy node's kill books the same as the killed run;
//! - an operation finishes with the action's out-of-memory error only after a kill at
//!   16 GiB, with the farm's busy-node error only after `1 + FARM_RERUNS` such kills,
//!   and completed only at a booking it needs; it never runs more often than its
//!   ladder plus the busy-node reruns;
//! - the scheduler's floor of each key equals the largest ask the model raised for it
//!   (busy-node kills never raise one), and each node's memory-pressure count equals
//!   the busy-node kills it made;
//! - every operation is answered exactly once;
//! - a seed replays to the same trace.

use std::collections::BTreeMap;

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, FARM_RERUNS, Input, Request, Scheduler};
use kbf_sim::{Chance, SimRng};
use kbf_types::{
    ActionKey, Digest, DigestFunction, Effect, Failure, FarmTime, LeaseKind, OperationId, Outcome,
    Qos, Resources, StartLease, StateMachine, WaiterId, WorkerId,
};

const SEEDS: u64 = 64;
const GIB: u64 = 1 << 30;
const NODES: [(&str, u64); 3] = [("n4", 4), ("n8", 8), ("n16", 16)];
const CAP: u64 = 16 * GIB;
const KEYS: u8 = 12;
const ACTIVE_UNTIL: u64 = 600;
const END: u64 = 1_500;

/// The model's rung after a kill at `booked`, worked out here rather than by the
/// scheduler's own `raised`, so a wrong step there cannot pass by agreeing with
/// itself: every booking in this world is whole GiB, so twice it, at most the cap;
/// none at the cap.
fn rung(booked: u64) -> Option<u64> {
    (booked < CAP).then(|| (2 * booked).min(CAP))
}

fn key(n: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n)),
    }
}

/// What the model knows of one operation.
#[derive(Debug, Default)]
struct Op {
    key: u8,
    /// The booking its first run must have.
    first: u64,
    /// Each run's booking and how it ended, once it has.
    runs: Vec<(u64, Option<Outcome>)>,
    answers: u32,
}

struct World {
    sched: Scheduler,
    rng: SimRng,
    now: u64,
    /// Each key's memory need, in bytes.
    need: Vec<u64>,
    /// The model's floor of each key.
    floor: BTreeMap<u8, u64>,
    ops: BTreeMap<OperationId, Op>,
    /// The unfinished operation of each key: a submission joins it.
    in_flight: BTreeMap<u8, OperationId>,
    next_op: u64,
    next_waiter: u64,
    /// Running leases, by the second they end.
    running: BTreeMap<u64, Vec<StartLease>>,
    /// Busy-node kills made per node.
    pressure: BTreeMap<WorkerId, u64>,
    /// How each operation ended, counted over the run, to show the sweep reaches each.
    seen: BTreeMap<&'static str, u64>,
    trace: Vec<String>,
}

impl World {
    fn new(seed: u64) -> Self {
        let mut rng = SimRng::from_seed(seed);
        let need = (0..KEYS).map(|_| rng.between(1, 20) * GIB).collect();
        let mut w = Self {
            sched: Scheduler::new(1),
            rng,
            now: 0,
            need,
            floor: BTreeMap::new(),
            ops: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            next_op: 0,
            next_waiter: 0,
            running: BTreeMap::new(),
            pressure: BTreeMap::new(),
            seen: BTreeMap::new(),
            trace: Vec::new(),
        };
        for (name, gib) in NODES {
            let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
                .unwrap()
                .with_drivers(["container"]);
            w.feed(Event::WorkerUp {
                worker: WorkerId::new(name),
                instance: DaemonInstance::new(name),
                capacity: Resources::new(16_000, gib * GIB),
                caps,
            });
        }
        w
    }

    fn feed(&mut self, event: Event) {
        self.trace.push(format!("{} {event:?}", self.now));
        let now = FarmTime::from_millis(self.now * 1_000);
        let mut effects = self.sched.apply(Input::new(now, event));
        while !effects.is_empty() {
            let mut more = Vec::new();
            for effect in effects {
                self.trace.push(format!("  {effect:?}"));
                match effect {
                    Effect::Commit(record) => {
                        more.extend(self.sched.apply(Input::new(now, Event::Committed(record))));
                    }
                    Effect::Start(start) => self.started(start),
                    Effect::Answer(answer) => self.answered(answer.operation, answer.outcome),
                    Effect::Waiting(waiting) => panic!("every request fits a node: {waiting:?}"),
                    Effect::Refuse(refusal) => panic!("nothing is refused: {refusal:?}"),
                }
            }
            effects = more;
        }
        self.check();
    }

    /// A `Start`: its booking is the one the model expects.
    fn started(&mut self, start: StartLease) {
        let op = self
            .ops
            .get_mut(&start.operation)
            .expect("a submitted operation");
        let booked = start.resources.memory_bytes;
        let want = match op.runs.last() {
            None => op.first,
            Some((before, Some(Outcome::Failed(Failure::OutOfMemory)))) => {
                rung(*before).expect("rerun below the cap")
            }
            Some((before, Some(Outcome::Failed(Failure::NodeMemoryPressure)))) => *before,
            Some(other) => panic!("{} rerun after {other:?}", start.operation),
        };
        assert_eq!(booked, want, "{}: runs {:?}", start.operation, op.runs);
        op.runs.push((booked, None));
        let ends = self.now + self.rng.between(1, 20);
        self.running.entry(ends).or_default().push(start);
    }

    /// An answer: once per operation, and only as the rules allow.
    fn answered(&mut self, id: OperationId, outcome: Outcome) {
        let op = self.ops.get_mut(&id).expect("a submitted operation");
        op.answers += 1;
        assert_eq!(op.answers, 1, "{id} answered twice");
        let (last, ended) = *op.runs.last().expect("answered after a run");
        assert_eq!(ended, Some(outcome), "{id}: answered with another outcome");
        let pressure = op
            .runs
            .iter()
            .filter(|(_, o)| *o == Some(Outcome::Failed(Failure::NodeMemoryPressure)))
            .count();
        let seen = match outcome {
            Outcome::Failed(Failure::OutOfMemory) => {
                assert_eq!(
                    last, CAP,
                    "{id}: told it needs more than any node below the cap"
                );
                "over the cap"
            }
            Outcome::Failed(Failure::NodeMemoryPressure) => {
                let farm_runs = usize::try_from(1 + FARM_RERUNS).unwrap();
                assert_eq!(pressure, farm_runs, "{id}: the farm's error early");
                "farm runs used"
            }
            Outcome::Completed { .. } => {
                assert!(last >= self.need[usize::from(op.key)], "{id}");
                "completed"
            }
            other => panic!("{id}: {other:?}"),
        };
        // 1 GiB doubles to the 16 GiB cap in four rungs.
        let most = 1 + 4 + usize::try_from(FARM_RERUNS).unwrap();
        assert!(op.runs.len() <= most, "{id} ran {} times", op.runs.len());
        *self.seen.entry(seen).or_default() += 1;
        self.in_flight.retain(|_, v| *v != id);
    }

    /// The floors and the pressure counts are the model's.
    fn check(&self) {
        for n in 0..KEYS {
            assert_eq!(
                self.sched.memory_floor(&key(n)),
                self.floor.get(&n).copied()
            );
        }
        for (name, _) in NODES {
            let worker = WorkerId::new(name);
            let want = self.pressure.get(&worker).copied().unwrap_or(0);
            assert_eq!(self.sched.memory_pressure(&worker), want, "{name}");
        }
    }

    fn submit(&mut self) {
        let n = u8::try_from(self.rng.below(u64::from(KEYS))).unwrap();
        let waiter = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        if !self.in_flight.contains_key(&n) {
            let id = OperationId(self.next_op);
            self.next_op += 1;
            let first = self.floor.get(&n).copied().unwrap_or(0).max(GIB);
            let op = Op {
                key: n,
                first,
                ..Op::default()
            };
            self.ops.insert(id, op);
            self.in_flight.insert(n, id);
        }
        let request = Request {
            key: key(n),
            qos: Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(1_000, GIB),
            hermetic: true,
            do_not_cache: false,
            needs: kbf_caps::Request::default(),
        };
        self.feed(Event::Submit { waiter, request });
    }

    /// `start`'s run ends: killed at its own limit if it booked less than it needs,
    /// else killed by a busy node one time in five, else completed.
    fn end(&mut self, start: &StartLease) {
        let op = self
            .ops
            .get_mut(&start.operation)
            .expect("a submitted operation");
        let booked = start.resources.memory_bytes;
        let outcome = if booked < self.need[usize::from(op.key)] {
            if let Some(up) = rung(booked) {
                let floor = self.floor.entry(op.key).or_default();
                *floor = (*floor).max(up);
            }
            Outcome::Failed(Failure::OutOfMemory)
        } else if self.rng.chance(Chance::percent(20)) {
            *self.pressure.entry(start.worker.clone()).or_default() += 1;
            Outcome::Failed(Failure::NodeMemoryPressure)
        } else {
            Outcome::Completed {
                action_result: Digest::new(DigestFunction::Sha256, [255; 32], start.lease.seq),
            }
        };
        let run = op.runs.last_mut().expect("a started run");
        run.1 = Some(outcome);
        self.feed(Event::Report {
            operation: start.operation,
            lease: start.lease,
            outcome,
        });
    }

    fn second(&mut self, active: bool) {
        for (name, _) in NODES {
            let worker = WorkerId::new(name);
            let running = self.sched.leases_on(&worker);
            self.feed(Event::Heartbeat { worker, running });
        }
        if active && self.rng.chance(Chance::percent(10)) {
            self.submit();
        }
        if let Some(done) = self.running.remove(&self.now) {
            for start in done {
                self.end(&start);
            }
        }
        self.feed(Event::Tick);
    }
}

fn run(seed: u64) -> World {
    let mut w = World::new(seed);
    while w.now < END {
        w.second(w.now < ACTIVE_UNTIL);
        w.now += 1;
    }
    w
}

/// Catches, over many interleavings: a ladder that does not double or passes the
/// largest node, a busy node's kill that raises the ask or a floor, a floor not
/// applied to a later submission (or applied to another key), either budget spending
/// the other, an answer given early, late or twice, and pressure counted against the
/// wrong node.
#[test]
fn memory_kills_follow_the_rules_over_seeds() {
    let mut seen: BTreeMap<&str, u64> = BTreeMap::new();
    let mut floored = 0;
    for seed in 0..SEEDS {
        let w = run(seed);
        for (id, op) in &w.ops {
            assert_eq!(
                op.answers, 1,
                "seed {seed}: {id} answered {} times",
                op.answers
            );
            if op.first > GIB {
                floored += 1;
            }
        }
        for (what, n) in &w.seen {
            *seen.entry(what).or_default() += n;
        }
    }
    // The sweep reaches every ending, and later submissions that start at a floor.
    for what in ["completed", "over the cap", "farm runs used"] {
        assert!(
            seen.get(what).is_some_and(|&n| n > 0),
            "{what} never seen: {seen:?}"
        );
    }
    assert!(floored > 0, "no submission started at a floor");
}

/// Catches: memory decisions that depend on anything but the seed.
#[test]
fn a_seed_replays() {
    assert_eq!(run(7).trace, run(7).trace);
}
