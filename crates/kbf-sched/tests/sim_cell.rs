//! The scheduler in a simulated cell: a leader running [`Scheduler`], a control log,
//! and two workers, on a network that delays, duplicates and reorders messages. In the
//! base run one worker is cut off long enough for its leases to expire and be
//! re-dispatched.
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
//!
//! Three more runs keep every worker talking, so the grace G never fires, and lose or
//! hide a committed lease another way. Each must still answer every operation once,
//! release every booking, and fence the result of the lost lease:
//! - worker-1 reboots (its runs die) and worker-2's daemon restarts (its runs are
//!   re-adopted), each registering again well inside G;
//! - worker-1 never receives one `Start`, while its session stays up;
//! - worker-1 runs one hermetic lease but leaves it out of every heartbeat's running
//!   set, and reports it late.
//!
//! Each registration opens a new session, as a new stream does on the wire: a `Start`
//! or acknowledgement sent to an earlier session never arrives, the leader drops
//! heartbeats of a session older than the newest, and a duplicated `Hello` registers
//! once. As on the wire, a `Hello` carries no running set, and a worker resends its
//! `Hello` on the same session when its node report changes; only the first `Hello`
//! of a session registers. Workers send their running set with every heartbeat: runs
//! not ended, and ended runs whose result is not yet acknowledged. The leader passes
//! it to the scheduler.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_sched::fence::START_GRACE;
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
/// When worker-1 reboots and worker-2's daemon restarts, in the restart run.
const REBOOT_AT: u64 = 20_000;
const RESTART_AT: u64 = 30_000;
/// A lease lost in a reboot is granted again within this of the reboot: on the new
/// session's first heartbeat the leader hears (within one heartbeat interval, as the
/// network may reorder it before the `Hello`) and the next placement round, not after
/// a grace.
const REGRANT_BOUND: Duration = Duration::from_secs(7);
/// When every worker's node report changes, and it resends its `Hello`.
const REPORT_CHANGE_AT: u64 = 45_000;
/// The timer tag of a worker's reboot or restart (run timers count up from 1).
const RESTART: u64 = u64::MAX;
/// The timer tag of a worker's node report change.
const REPORT_CHANGE: u64 = u64::MAX - 1;

#[derive(Clone, Debug)]
enum Msg {
    Append(ControlRecord),
    Committed {
        index: u64,
        record: ControlRecord,
    },
    /// The wire's `Hello`, which carries no running set; `session` stands for the
    /// stream it is sent on.
    Hello {
        capacity: Resources,
        session: u64,
    },
    Heartbeat {
        sent_at: FarmTime,
        session: u64,
        running: Vec<LeaseId>,
    },
    Ack {
        sent_at: FarmTime,
        session: u64,
    },
    Start {
        start: StartLease,
        session: u64,
    },
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
    /// The newest session each worker registered.
    sessions: BTreeMap<NodeId, u64>,
    answers: Vec<(FarmTime, Answer)>,
    out: Vec<Output<Msg>>,
}

impl Leader {
    fn new() -> Self {
        Self {
            sched: Scheduler::new(1),
            next_index: 0,
            pending: BTreeMap::new(),
            sessions: BTreeMap::new(),
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
                    let to = NodeId::new(s.worker.as_str());
                    let session = self.session(&to);
                    let start = s.clone();
                    self.send(to, Msg::Start { start, session });
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

    fn session(&self, worker: &NodeId) -> u64 {
        self.sessions.get(worker).copied().unwrap_or_default()
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

/// How a worker misbehaves, while it keeps heartbeating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    /// At this many milliseconds the machine reboots: every run dies and the self-fence
    /// is lost; results already on disk are kept. The daemon registers again at once.
    Reboot(u64),
    /// At this many milliseconds the daemon restarts: its runs keep going and are
    /// re-adopted. It registers again at once.
    Restart(u64),
    /// Every copy of the first `Start` sent to this worker is lost.
    DropFirstStart,
    /// The first hermetic lease this worker starts runs, but never appears in its
    /// running set.
    HideFirstHermetic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scenario {
    cut: bool,
    worker_1: Fault,
    worker_2: Fault,
}

const BASE: Scenario = Scenario {
    cut: true,
    worker_1: Fault::None,
    worker_2: Fault::None,
};

struct Worker {
    capacity: Resources,
    fault: Fault,
    session: u64,
    /// The lease whose `Start` was lost, and its operation ([`Fault::DropFirstStart`]).
    dropped: Option<(LeaseId, OperationId)>,
    /// The lease left out of the running set ([`Fault::HideFirstHermetic`]).
    hidden: Option<LeaseId>,
    fence: SelfFence,
    runs: BTreeMap<LeaseId, Run>,
    starts: Vec<(FarmTime, LeaseId)>,
    unacked: BTreeMap<LeaseId, (OperationId, Outcome)>,
    timers: BTreeMap<u64, LeaseId>,
    out: Vec<Output<Msg>>,
}

impl Worker {
    fn new(capacity: Resources, fault: Fault) -> Self {
        Self {
            capacity,
            fault,
            session: 1,
            dropped: None,
            hidden: None,
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

    fn start(&mut self, now: FarmTime, s: StartLease, session: u64) {
        // Sent to an earlier session: it went down with that stream.
        if session != self.session {
            return;
        }
        if self.fault == Fault::DropFirstStart
            && self.dropped.is_none_or(|(lease, _)| lease == s.lease)
        {
            self.dropped = Some((s.lease, s.operation));
            return;
        }
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
        if self.fault == Fault::HideFirstHermetic
            && self.hidden.is_none()
            && s.fence == FencePolicy::RunOn
        {
            self.hidden = Some(s.lease);
        }
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

    /// The leases this worker reports holding: runs not ended, and ended runs whose
    /// result is not yet acknowledged (it is on disk and resent until it is).
    fn running(&self) -> Vec<LeaseId> {
        let mut held: BTreeSet<LeaseId> = self.unacked.keys().copied().collect();
        held.extend(
            self.runs
                .iter()
                .filter(|(_, r)| r.ended.is_none())
                .map(|(l, _)| *l),
        );
        if let Some(hidden) = self.hidden {
            held.remove(&hidden);
        }
        held.into_iter().collect()
    }

    fn hello(&mut self) {
        self.send(Msg::Hello {
            capacity: self.capacity,
            session: self.session,
        });
    }

    /// A reboot or daemon restart: a new session, registered at once.
    fn restart(&mut self, now: FarmTime) {
        if let Fault::Reboot(_) = self.fault {
            for run in self.runs.values_mut() {
                if run.ended.is_none() {
                    run.ended = Some(now);
                }
            }
            self.fence = SelfFence::new();
        }
        self.session += 1;
        self.hello();
        self.beat(now);
    }

    fn heartbeat(&mut self, now: FarmTime) {
        self.beat(now);
        self.out.push(Output::Timer {
            after: HEARTBEAT,
            tag: 0,
        });
    }

    /// Sends one heartbeat and resends every unacknowledged report.
    fn beat(&mut self, now: FarmTime) {
        let running = self.running();
        self.send(Msg::Heartbeat {
            sent_at: now,
            session: self.session,
            running,
        });
        let resend: Vec<_> = self.unacked.iter().map(|(l, v)| (*l, *v)).collect();
        for (lease, (operation, outcome)) in resend {
            self.send(Msg::Report {
                operation,
                lease,
                outcome,
            });
        }
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
                // Only the first Hello of a session registers: a duplicate, or one
                // resent because the node report changed, does not.
                Msg::Hello { capacity, session } => {
                    if session <= l.session(&from) {
                        return Vec::new();
                    }
                    l.sessions.insert(from.clone(), session);
                    let worker = WorkerId::new(from.as_str());
                    l.feed(now, SchedEvent::WorkerUp { worker, capacity })
                }
                // A heartbeat of an older session belongs to a closed stream.
                Msg::Heartbeat {
                    sent_at,
                    session,
                    running,
                } => {
                    if session < l.session(&from) {
                        return Vec::new();
                    }
                    let worker = WorkerId::new(from.as_str());
                    l.send(from, Msg::Ack { sent_at, session });
                    l.feed(now, SchedEvent::Heartbeat { worker, running })
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
                        if let Fault::Reboot(at) | Fault::Restart(at) = w.fault {
                            w.out.push(Output::Timer {
                                after: Duration::from_millis(at),
                                tag: RESTART,
                            });
                        }
                        w.out.push(Output::Timer {
                            after: Duration::from_millis(REPORT_CHANGE_AT),
                            tag: REPORT_CHANGE,
                        });
                        w.hello();
                        w.heartbeat(now);
                    }
                    Event::Timer { tag: 0 } => w.heartbeat(now),
                    Event::Timer { tag: RESTART } => w.restart(now),
                    Event::Timer { tag: REPORT_CHANGE } => w.hello(),
                    Event::Timer { tag } => {
                        let lease = w.timers[&tag];
                        w.finish(now, lease);
                    }
                    Event::Message { msg, .. } => match msg {
                        Msg::Ack { sent_at, session } => {
                            if session == w.session {
                                w.fence.acknowledged(sent_at);
                            }
                        }
                        Msg::Start { start, session } => w.start(now, start, session),
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
    run_scenario(seed, BASE)
}

fn run_scenario(seed: u64, scenario: Scenario) -> Sim<Cell> {
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
    let worker_1 = Worker::new(Resources::new(6_000, 64 * GIB), scenario.worker_1);
    sim.add_node("worker-1", Cell::Worker(worker_1));
    let worker_2 = Worker::new(Resources::new(12_000, 64 * GIB), scenario.worker_2);
    sim.add_node("worker-2", Cell::Worker(worker_2));
    if scenario.cut {
        sim.partition_at(
            FarmTime::from_millis(CUT.0),
            Partition::new([vec![NodeId::from("worker-1")]]),
        );
        sim.partition_at(FarmTime::from_millis(CUT.1), Partition::none());
    }
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

fn worker<'a>(sim: &'a Sim<Cell>, name: &str) -> &'a Worker {
    match sim.node(&NodeId::from(name)) {
        Some(Cell::Worker(w)) => w,
        _ => unreachable!(),
    }
}

/// Every grant in the log: per operation, each lease with the time it first committed.
fn grants(sim: &Sim<Cell>) -> BTreeMap<OperationId, BTreeMap<LeaseId, FarmTime>> {
    let mut grants: BTreeMap<OperationId, BTreeMap<LeaseId, FarmTime>> = BTreeMap::new();
    for (t, r) in &log(sim).records {
        if let ControlRecord::Lease(g) = r {
            grants
                .entry(g.operation)
                .or_default()
                .entry(g.lease)
                .or_insert(*t);
        }
    }
    grants
}

/// Checks that every operation was answered exactly once, by the lease of the newest
/// grant that precedes its result in the log, and returns the answering leases.
fn assert_answered_once(sim: &Sim<Cell>, seed: u64) -> BTreeMap<OperationId, LeaseId> {
    let mut newest: BTreeMap<OperationId, LeaseId> = BTreeMap::new();
    let mut winner: BTreeMap<OperationId, LeaseId> = BTreeMap::new();
    for (_, r) in &log(sim).records {
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
    let mut seen = BTreeMap::new();
    for (_, a) in &leader(sim).answers {
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
    seen
}

/// Checks that once every operation is answered, no worker has anything booked.
fn assert_bookings_released(sim: &Sim<Cell>, seed: u64) {
    for (id, _) in workers(sim) {
        let booked = leader(sim).sched.booked(&WorkerId::new(id.as_str()));
        assert_eq!(
            booked,
            Some(Resources::default()),
            "seed {seed}: {id} still booked at the end"
        );
    }
}

/// Checks that no self-fenced operation ran on two workers at once, and returns how
/// many ran more than once.
fn assert_never_twice_at_once(sim: &Sim<Cell>, seed: u64) -> usize {
    let mut spans: BTreeMap<OperationId, Vec<(u64, u64)>> = BTreeMap::new();
    for (_, w) in workers(sim) {
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
    let rerun = spans.values().filter(|s| s.len() > 1).count();
    for (op, mut s) in spans {
        s.sort_unstable();
        for pair in s.windows(2) {
            assert!(
                pair[0].1 <= pair[1].0,
                "seed {seed}: {op} ran twice at once: {pair:?}"
            );
        }
    }
    rerun
}

/// Checks that `lost`, a committed lease of `op`, was replaced: `op` was granted again
/// no sooner than the grace after `lost` committed (its `Start` may still have been on
/// its way before then), and answered by a later lease.
fn assert_replaced_after_grace(
    sim: &Sim<Cell>,
    seed: u64,
    answers: &BTreeMap<OperationId, LeaseId>,
    op: OperationId,
    lost: LeaseId,
) {
    let grants = grants(sim);
    let committed = grants[&op][&lost];
    let again = grants[&op].iter().find(|(l, _)| **l > lost);
    assert!(
        again.is_some_and(|(_, t)| *t >= committed.saturating_add(START_GRACE)),
        "seed {seed}: {op} lost {lost} committed at {committed:?}; granted again {again:?}"
    );
    assert!(
        answers[&op] > lost,
        "seed {seed}: {op} answered by {}, the lease it lost",
        answers[&op]
    );
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
        let answers = assert_answered_once(&sim, seed);
        // The scenario did what it claims: some answer comes from a re-dispatch.
        assert!(answers.values().any(|l| l.seq >= OPS), "seed {seed}");
    }
}

/// Catches: a self-fenced run that outlives the grace G on a cut-off worker (a fence
/// measured from the wrong time, or G not exceeding T), so two copies of networked
/// work run at once.
#[test]
fn self_fenced_operations_never_run_twice_at_once() {
    for seed in 0..SEEDS {
        let sim = run(seed);
        let rerun = assert_never_twice_at_once(&sim, seed);
        assert!(rerun > 0, "seed {seed}: no re-dispatch");
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

/// Catches: a worker that registers again inside G keeping its old leases for good
/// (worker-1 rebooted, so their runs are gone: the operations are never answered and
/// the bookings leak), a re-registration that waits for a grace before letting them go,
/// and one that drops the leases a restarted daemon re-adopted and still runs (they
/// would run twice), as a registration fed the running set the wire's `Hello` lacks
/// would.
#[test]
fn a_worker_that_registers_again_keeps_only_what_it_still_runs() {
    let scenario = Scenario {
        cut: false,
        worker_1: Fault::Reboot(REBOOT_AT),
        worker_2: Fault::Restart(RESTART_AT),
    };
    let reboot = FarmTime::from_millis(REBOOT_AT);
    let restart = FarmTime::from_millis(RESTART_AT);
    for seed in 0..SEEDS {
        let sim = run_scenario(seed, scenario);
        let answers = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        assert_never_twice_at_once(&sim, seed);
        let grants = grants(&sim);

        let killed: Vec<(&LeaseId, &Run)> = worker(&sim, "worker-1")
            .runs
            .iter()
            .filter(|(_, r)| r.ended == Some(reboot))
            .collect();
        assert!(!killed.is_empty(), "seed {seed}: the reboot killed no run");
        for (&lost, run) in killed {
            let op = run.operation;
            let again = grants[&op].iter().find(|(l, _)| **l > lost);
            assert!(
                again.is_some_and(|(_, t)| *t < reboot.saturating_add(REGRANT_BOUND)),
                "seed {seed}: {op} lost {lost} in the reboot; granted again {again:?}"
            );
            assert!(answers[&op] > lost, "seed {seed}: {op} answered by {lost}");
        }

        let adopted: Vec<&Run> = worker(&sim, "worker-2")
            .runs
            .values()
            .filter(|r| r.started < restart)
            .collect();
        assert!(
            !adopted.is_empty(),
            "seed {seed}: worker-2 re-adopted no run"
        );
        for run in adopted {
            let op = run.operation;
            assert_eq!(
                grants[&op].len(),
                1,
                "seed {seed}: {op}, re-adopted by worker-2, was granted again"
            );
        }
    }
}

/// Catches: a committed lease whose `Start` never reached a worker that stays connected
/// kept for good (its operation is never answered and its booking leaks), and one
/// given up before the Start grace, while that `Start` could still arrive.
#[test]
fn a_start_lost_on_a_live_session_is_granted_again_after_the_grace() {
    let scenario = Scenario {
        cut: false,
        worker_1: Fault::DropFirstStart,
        worker_2: Fault::None,
    };
    for seed in 0..SEEDS {
        let sim = run_scenario(seed, scenario);
        let answers = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        assert_never_twice_at_once(&sim, seed);
        let (lost, op) = worker(&sim, "worker-1")
            .dropped
            .expect("worker-1 lost a Start");
        assert_replaced_after_grace(&sim, seed, &answers, op, lost);
    }
}

/// Catches: a committed lease the worker's running set leaves out kept past the Start
/// grace (its operation waits on a run nobody accounts for), and the result that worker
/// reports late for it proposed or accepted over the re-grant.
#[test]
fn a_lease_missing_from_the_running_set_is_granted_again_and_its_late_result_fenced() {
    let scenario = Scenario {
        cut: false,
        worker_1: Fault::HideFirstHermetic,
        worker_2: Fault::None,
    };
    for seed in 0..SEEDS {
        let sim = run_scenario(seed, scenario);
        let answers = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        assert_never_twice_at_once(&sim, seed);
        let w1 = worker(&sim, "worker-1");
        let hidden = w1.hidden.expect("worker-1 hid a lease");
        let run = &w1.runs[&hidden];
        // The scenario did what it claims: the hidden run finished and its late report
        // reached the leader.
        assert_eq!(
            run.ended,
            Some(run.started.saturating_add(RUN_FOR)),
            "seed {seed}"
        );
        assert!(!w1.unacked.contains_key(&hidden), "seed {seed}");
        assert_replaced_after_grace(&sim, seed, &answers, run.operation, hidden);
        let proposed = log(&sim)
            .records
            .iter()
            .any(|(_, r)| matches!(r, ControlRecord::Result(res) if res.lease == hidden));
        assert!(
            !proposed,
            "seed {seed}: the late result of {hidden} was proposed"
        );
    }
}
