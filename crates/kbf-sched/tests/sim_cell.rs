//! The scheduler in a simulated cell: a leader running [`Scheduler`], a control log,
//! and two workers, on a network that delays, duplicates and reorders messages, with
//! one worker cut off long enough for its leases to expire and be re-dispatched.
//!
//! The leader carries out the scheduler's effects literally: `Commit` sends the record
//! to the log, `Start` sends it to the worker, `Answer` is recorded. The log commits
//! records in arrival order and returns them numbered; the leader feeds them back in
//! log order. Workers heartbeat every 5 s, keep a [`SelfFence`], run every action for
//! 100 s, and resend unacknowledged reports with each heartbeat (a hermetic run keeps
//! its result through a lost connection).
//!
//! The checks, over a seed sweep:
//! - every `Start` a worker receives names a lease the log had already committed;
//! - every operation is answered exactly once, by the lease of the newest grant that
//!   precedes its result in the log;
//! - no self-fenced operation ever runs on two workers at once;
//! - a seed replays to the same trace.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_sched::{Event as SchedEvent, Input, Request, Scheduler, SelfFence};
use kbf_sim::{Chance, Event, Faults, Node, NodeId, NodeInput, Output, Partition, Sim, TraceHash};
use kbf_types::{
    ActionKey, Answer, ControlRecord, Digest, DigestFunction, Effect, Failure, FarmTime,
    FencePolicy, LeaseId, OperationId, Outcome, Qos, Resources, StartLease, StateMachine, WaiterId,
    WorkerId,
};

const SEEDS: u64 = 48;
const GIB: u64 = 1 << 30;
const TICK: Duration = Duration::from_secs(1);
const HEARTBEAT: Duration = Duration::from_secs(5);
const RUN_FOR: Duration = Duration::from_secs(100);
const OPS: u64 = 12;
/// Worker-1 is cut off from everyone for this window, longer than the grace G.
const CUT: (u64, u64) = (5_000, 80_000);
const END: u64 = 400_000;

#[derive(Clone, Debug)]
enum Msg {
    Append(ControlRecord),
    Committed {
        index: u64,
        record: ControlRecord,
    },
    Hello {
        capacity: Resources,
    },
    Heartbeat {
        sent_at: FarmTime,
    },
    Ack {
        sent_at: FarmTime,
    },
    Start(StartLease),
    Started {
        operation: OperationId,
        lease: LeaseId,
    },
    Report {
        operation: OperationId,
        lease: LeaseId,
        outcome: Outcome,
    },
    ReportAck {
        lease: LeaseId,
    },
}

fn leader_id() -> NodeId {
    NodeId::from("leader")
}

fn log_id() -> NodeId {
    NodeId::from("log")
}

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

struct Leader {
    sched: Scheduler,
    next_index: u64,
    pending: BTreeMap<u64, ControlRecord>,
    answers: Vec<(FarmTime, Answer)>,
    out: Vec<Output<Msg>>,
}

impl Leader {
    fn new() -> Self {
        Self {
            sched: Scheduler::new(1),
            next_index: 0,
            pending: BTreeMap::new(),
            answers: Vec::new(),
            out: Vec::new(),
        }
    }

    fn feed(&mut self, now: FarmTime, event: SchedEvent) -> Vec<Effect> {
        let effects = self.sched.apply(Input::new(now, event));
        for e in &effects {
            match e {
                Effect::Commit(r) => self.send(log_id(), Msg::Append(r.clone())),
                Effect::Start(s) => {
                    self.send(NodeId::new(s.worker.as_str()), Msg::Start(s.clone()));
                }
                Effect::Answer(a) => self.answers.push((now, a.clone())),
                _ => unreachable!("the scheduler emits no other effect"),
            }
        }
        effects
    }

    fn send(&mut self, to: NodeId, msg: Msg) {
        self.out.push(Output::Send { to, msg });
    }
}

#[derive(Default)]
struct Log {
    /// Committed records with their commit time, in log order.
    records: Vec<(FarmTime, ControlRecord)>,
    out: Vec<Output<Msg>>,
}

struct Run {
    operation: OperationId,
    fence: FencePolicy,
    started: FarmTime,
    ended: Option<FarmTime>,
}

struct Worker {
    capacity: Resources,
    fence: SelfFence,
    runs: BTreeMap<LeaseId, Run>,
    starts: Vec<(FarmTime, LeaseId)>,
    unacked: BTreeMap<LeaseId, (OperationId, Outcome)>,
    timers: BTreeMap<u64, LeaseId>,
    out: Vec<Output<Msg>>,
}

impl Worker {
    fn new(capacity: Resources) -> Self {
        Self {
            capacity,
            fence: SelfFence::new(),
            runs: BTreeMap::new(),
            starts: Vec::new(),
            unacked: BTreeMap::new(),
            timers: BTreeMap::new(),
            out: Vec::new(),
        }
    }

    fn send(&mut self, msg: Msg) {
        self.out.push(Output::Send {
            to: leader_id(),
            msg,
        });
    }

    /// Stops self-fenced runs at their fence deadline (the unit's runtime limit).
    fn enforce_fence(&mut self, now: FarmTime) {
        let deadline = self.fence.deadline().unwrap_or_default();
        for run in self.runs.values_mut() {
            if run.fence == FencePolicy::SelfFence && run.ended.is_none() && now >= deadline {
                run.ended = Some(deadline.max(run.started));
            }
        }
    }

    fn start(&mut self, now: FarmTime, s: StartLease) {
        self.starts.push((now, s.lease));
        if self.runs.contains_key(&s.lease) {
            return;
        }
        if s.fence == FencePolicy::SelfFence && !self.fence.allows(now) {
            self.unacked
                .insert(s.lease, (s.operation, Outcome::Failed(Failure::Infra)));
            return;
        }
        self.runs.insert(
            s.lease,
            Run {
                operation: s.operation,
                fence: s.fence,
                started: now,
                ended: None,
            },
        );
        let tag = 1 + self.timers.len() as u64;
        self.timers.insert(tag, s.lease);
        self.out.push(Output::Timer {
            after: RUN_FOR,
            tag,
        });
        self.send(Msg::Started {
            operation: s.operation,
            lease: s.lease,
        });
    }

    fn finish(&mut self, now: FarmTime, lease: LeaseId) {
        let run = self.runs.get_mut(&lease).expect("timers name runs");
        if run.ended.is_some() {
            return;
        }
        run.ended = Some(now);
        let operation = run.operation;
        let outcome = Outcome::Completed {
            action_result: digest(1_000 + operation.0),
        };
        self.unacked.insert(lease, (operation, outcome));
        self.send(Msg::Report {
            operation,
            lease,
            outcome,
        });
    }

    fn heartbeat(&mut self, now: FarmTime) {
        self.send(Msg::Heartbeat { sent_at: now });
        let resend: Vec<_> = self.unacked.iter().map(|(l, v)| (*l, *v)).collect();
        for (lease, (operation, outcome)) in resend {
            self.send(Msg::Report {
                operation,
                lease,
                outcome,
            });
        }
        self.out.push(Output::Timer {
            after: HEARTBEAT,
            tag: 0,
        });
    }
}

enum Cell {
    Leader(Leader),
    Log(Log),
    Worker(Worker),
}

fn request(n: u64) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(n),
        },
        qos: Qos::Ci,
        resources: Resources::new(1_000, GIB),
        // Every third action is networked, so it self-fences.
        hermetic: !n.is_multiple_of(3),
        do_not_cache: false,
    }
}

impl StateMachine for Cell {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        let now = input.now;
        match (self, input.event) {
            (Cell::Leader(l), Event::Start) => {
                for n in 0..OPS {
                    let waiter = WaiterId(n);
                    l.feed(
                        now,
                        SchedEvent::Submit {
                            waiter,
                            request: request(n),
                        },
                    );
                }
                l.out.push(Output::Timer {
                    after: TICK,
                    tag: 0,
                });
                Vec::new()
            }
            (Cell::Leader(l), Event::Timer { .. }) => {
                l.out.push(Output::Timer {
                    after: TICK,
                    tag: 0,
                });
                l.feed(now, SchedEvent::Tick)
            }
            (Cell::Leader(l), Event::Message { from, msg }) => match msg {
                Msg::Committed { index, record } => {
                    if index >= l.next_index {
                        l.pending.insert(index, record);
                    }
                    let mut effects = Vec::new();
                    while let Some(record) = l.pending.remove(&l.next_index) {
                        l.next_index += 1;
                        effects.extend(l.feed(now, SchedEvent::Committed(record)));
                    }
                    effects
                }
                Msg::Hello { capacity } => {
                    let worker = WorkerId::new(from.as_str());
                    l.feed(now, SchedEvent::WorkerUp { worker, capacity })
                }
                Msg::Heartbeat { sent_at } => {
                    let worker = WorkerId::new(from.as_str());
                    l.send(from, Msg::Ack { sent_at });
                    l.feed(now, SchedEvent::Heartbeat { worker })
                }
                Msg::Started { operation, lease } => {
                    l.feed(now, SchedEvent::Started { operation, lease })
                }
                Msg::Report {
                    operation,
                    lease,
                    outcome,
                } => {
                    l.send(from, Msg::ReportAck { lease });
                    l.feed(
                        now,
                        SchedEvent::Report {
                            operation,
                            lease,
                            outcome,
                        },
                    )
                }
                other => panic!("leader got {other:?}"),
            },
            (
                Cell::Log(g),
                Event::Message {
                    msg: Msg::Append(record),
                    ..
                },
            ) => {
                let index = g.records.len() as u64;
                g.records.push((now, record.clone()));
                g.out.push(Output::Send {
                    to: leader_id(),
                    msg: Msg::Committed { index, record },
                });
                Vec::new()
            }
            (Cell::Log(_), _) => Vec::new(),
            (Cell::Worker(w), event) => {
                w.enforce_fence(now);
                match event {
                    Event::Start => {
                        w.send(Msg::Hello {
                            capacity: w.capacity,
                        });
                        w.heartbeat(now);
                    }
                    Event::Timer { tag: 0 } => w.heartbeat(now),
                    Event::Timer { tag } => {
                        let lease = w.timers[&tag];
                        w.finish(now, lease);
                    }
                    Event::Message { msg, .. } => match msg {
                        Msg::Ack { sent_at } => w.fence.acknowledged(sent_at),
                        Msg::Start(s) => w.start(now, s),
                        Msg::ReportAck { lease } => {
                            w.unacked.remove(&lease);
                        }
                        other => panic!("worker got {other:?}"),
                    },
                }
                Vec::new()
            }
        }
    }
}

impl Node for Cell {
    type Msg = Msg;

    fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        match self {
            Cell::Leader(l) => std::mem::take(&mut l.out),
            Cell::Log(g) => std::mem::take(&mut g.out),
            Cell::Worker(w) => std::mem::take(&mut w.out),
        }
    }
}

fn run(seed: u64) -> Sim<Cell> {
    let faults = Faults {
        min_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(50),
        duplicate: Chance::percent(5),
        reorder: Chance::percent(5),
        ..Faults::default()
    };
    let mut sim = Sim::new(seed, faults);
    sim.add_node("leader", Cell::Leader(Leader::new()));
    sim.add_node("log", Cell::Log(Log::default()));
    // First fit fills worker-1 first: six operations land there and are cut off.
    sim.add_node(
        "worker-1",
        Cell::Worker(Worker::new(Resources::new(6_000, 64 * GIB))),
    );
    sim.add_node(
        "worker-2",
        Cell::Worker(Worker::new(Resources::new(12_000, 64 * GIB))),
    );
    sim.partition_at(
        FarmTime::from_millis(CUT.0),
        Partition::new([vec![NodeId::from("worker-1")]]),
    );
    sim.partition_at(FarmTime::from_millis(CUT.1), Partition::none());
    sim.run_until(FarmTime::from_millis(END));
    sim
}

fn leader(sim: &Sim<Cell>) -> &Leader {
    match sim.node(&leader_id()) {
        Some(Cell::Leader(l)) => l,
        _ => unreachable!(),
    }
}

fn log(sim: &Sim<Cell>) -> &Log {
    match sim.node(&log_id()) {
        Some(Cell::Log(g)) => g,
        _ => unreachable!(),
    }
}

fn workers(sim: &Sim<Cell>) -> impl Iterator<Item = (&NodeId, &Worker)> {
    sim.nodes().filter_map(|(id, n)| match n {
        Cell::Worker(w) => Some((id, w)),
        _ => None,
    })
}

/// Catches: a `Start` sent before its lease is committed (at placement, or with the
/// commit in the same breath). The Start then races the log append and, on some seed,
/// a worker holds a lease the log does not know, which a new leader could grant again.
#[test]
fn every_start_names_a_lease_already_committed() {
    for seed in 0..SEEDS {
        let sim = run(seed);
        // A duplicated append commits a record twice; the first commit counts.
        let mut committed: BTreeMap<LeaseId, FarmTime> = BTreeMap::new();
        for (t, r) in &log(&sim).records {
            if let ControlRecord::Lease(g) = r {
                committed.entry(g.lease).or_insert(*t);
            }
        }
        for (id, w) in workers(&sim) {
            for (received, lease) in &w.starts {
                let at = committed.get(lease);
                assert!(
                    at.is_some_and(|c| c <= received),
                    "seed {seed}: {id} received Start for {lease} at {received:?}, committed at {at:?}"
                );
            }
        }
    }
}

/// Catches: a result accepted from a lease that was superseded (a late result after
/// re-dispatch), a duplicate result answered twice, or an operation never answered.
/// The cut-off worker's hermetic runs finish after their operations were re-dispatched
/// and report late; those reports must lose to the new lease.
#[test]
fn each_operation_answered_once_by_its_newest_grant() {
    for seed in 0..SEEDS {
        let sim = run(seed);
        let mut newest: BTreeMap<OperationId, LeaseId> = BTreeMap::new();
        let mut winner: BTreeMap<OperationId, LeaseId> = BTreeMap::new();
        for (_, r) in &log(&sim).records {
            match r {
                ControlRecord::Lease(g) => {
                    newest.insert(g.operation, g.lease);
                }
                ControlRecord::Result(res) if newest.get(&res.operation) == Some(&res.lease) => {
                    winner.entry(res.operation).or_insert(res.lease);
                }
                _ => {}
            }
        }
        let answers = &leader(&sim).answers;
        let mut seen = BTreeMap::new();
        for (_, a) in answers {
            assert!(
                seen.insert(a.operation, a.lease).is_none(),
                "seed {seed}: {} answered twice",
                a.operation
            );
            assert_eq!(
                winner.get(&a.operation),
                Some(&a.lease),
                "seed {seed}: {} answered by {}, which the log had superseded",
                a.operation,
                a.lease
            );
        }
        assert_eq!(seen.len() as u64, OPS, "seed {seed}: unanswered operations");
        // The scenario did what it claims: some answer comes from a re-dispatch.
        assert!(
            answers.iter().any(|(_, a)| a.lease.seq >= OPS),
            "seed {seed}"
        );
    }
}

/// Catches: a self-fenced run that outlives the grace G on a cut-off worker (a fence
/// measured from the wrong time, or G not exceeding T), so two copies of networked
/// work run at once.
#[test]
fn self_fenced_operations_never_run_twice_at_once() {
    for seed in 0..SEEDS {
        let sim = run(seed);
        let mut spans: BTreeMap<OperationId, Vec<(u64, u64)>> = BTreeMap::new();
        for (_, w) in workers(&sim) {
            for run in w.runs.values() {
                if run.fence == FencePolicy::SelfFence {
                    let end = run.ended.map_or(END, FarmTime::as_millis);
                    spans
                        .entry(run.operation)
                        .or_default()
                        .push((run.started.as_millis(), end));
                }
            }
        }
        assert!(
            spans.values().any(|s| s.len() > 1),
            "seed {seed}: no re-dispatch"
        );
        for (op, mut s) in spans {
            s.sort_unstable();
            for pair in s.windows(2) {
                assert!(
                    pair[0].1 <= pair[1].0,
                    "seed {seed}: {op} ran twice at once: {pair:?}"
                );
            }
        }
    }
}

/// Catches: anything in the scheduler or this cell that depends on more than the seed
/// (a hashed collection, a clock): two runs of one seed must trace identically.
#[test]
fn a_seed_replays_exactly() {
    let hash = |seed| -> TraceHash { run(seed).trace_hash() };
    assert_eq!(hash(7), hash(7));
    assert_ne!(hash(7), hash(8));
}
