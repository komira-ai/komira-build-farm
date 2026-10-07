//! The simulated cell the `sim_cell` scenarios run in: a leader carrying out a
//! [`Scheduler`]'s effects, a control log, and workers with their faults. The file
//! `sim_cell.rs` says what the cell models and checks.
//!
//! [`Scheduler`]: kbf_sched::Scheduler

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_sched::{Event as SchedEvent, Input, Request, Scheduler, SelfFence};
use kbf_sim::{Chance, Event, Faults, Node, NodeId, NodeInput, Output, Partition, Sim};
use kbf_types::{
    ActionKey, Answer, ControlRecord, Digest, DigestFunction, Effect, Failure, FarmTime,
    FencePolicy, LeaseId, OperationId, Outcome, Qos, Resources, StartLease, StateMachine, WaiterId,
    WorkerId,
};

pub const SEEDS: u64 = 48;
const GIB: u64 = 1 << 30;
const TICK: Duration = Duration::from_secs(1);
const HEARTBEAT: Duration = Duration::from_secs(5);
pub const RUN_FOR: Duration = Duration::from_secs(100);
pub const OPS: u64 = 12;
/// Worker-1 is cut off from everyone for this window, longer than the grace G.
const CUT: (u64, u64) = (5_000, 80_000);
pub const END: u64 = 400_000;
/// When worker-1 reboots and worker-2's daemon restarts, in the restart run.
pub const REBOOT_AT: u64 = 20_000;
pub const RESTART_AT: u64 = 30_000;
/// A lease lost in a reboot is granted again within this of the reboot: on the new
/// session's first heartbeat the leader hears (within one heartbeat interval, as the
/// network may reorder it before the `Hello`) and the next placement round, not after
/// a grace.
pub const REGRANT_BOUND: Duration = Duration::from_secs(7);
/// When every worker's node report changes, and it resends its `Hello`.
const REPORT_CHANGE_AT: u64 = 45_000;
/// The timer tag of a worker's reboot or restart (run timers count up from 1).
const RESTART: u64 = u64::MAX;
/// The timer tag of a worker's node report change.
const REPORT_CHANGE: u64 = u64::MAX - 1;

#[derive(Clone, Debug)]
pub enum Msg {
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

pub struct Leader {
    pub sched: Scheduler,
    next_index: u64,
    pending: BTreeMap<u64, ControlRecord>,
    /// The newest session each worker registered.
    sessions: BTreeMap<NodeId, u64>,
    pub answers: Vec<(FarmTime, Answer)>,
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
pub struct Log {
    /// Committed records with their commit time, in log order.
    pub records: Vec<(FarmTime, ControlRecord)>,
    out: Vec<Output<Msg>>,
}

pub struct Run {
    pub operation: OperationId,
    pub fence: FencePolicy,
    pub started: FarmTime,
    pub ended: Option<FarmTime>,
}

/// How a worker misbehaves, while it keeps heartbeating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
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
pub struct Scenario {
    pub cut: bool,
    pub worker_1: Fault,
    pub worker_2: Fault,
}

const BASE: Scenario = Scenario {
    cut: true,
    worker_1: Fault::None,
    worker_2: Fault::None,
};

pub struct Worker {
    capacity: Resources,
    fault: Fault,
    session: u64,
    /// The lease whose `Start` was lost, and its operation ([`Fault::DropFirstStart`]).
    pub dropped: Option<(LeaseId, OperationId)>,
    /// The lease left out of the running set ([`Fault::HideFirstHermetic`]).
    pub hidden: Option<LeaseId>,
    fence: SelfFence,
    pub runs: BTreeMap<LeaseId, Run>,
    pub starts: Vec<(FarmTime, LeaseId)>,
    pub unacked: BTreeMap<LeaseId, (OperationId, Outcome)>,
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

pub enum Cell {
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

pub fn run(seed: u64) -> Sim<Cell> {
    run_scenario(seed, BASE)
}

pub fn run_scenario(seed: u64, scenario: Scenario) -> Sim<Cell> {
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

pub fn leader(sim: &Sim<Cell>) -> &Leader {
    match sim.node(&leader_id()) {
        Some(Cell::Leader(l)) => l,
        _ => unreachable!(),
    }
}

pub fn log(sim: &Sim<Cell>) -> &Log {
    match sim.node(&log_id()) {
        Some(Cell::Log(g)) => g,
        _ => unreachable!(),
    }
}

pub fn workers(sim: &Sim<Cell>) -> impl Iterator<Item = (&NodeId, &Worker)> {
    sim.nodes().filter_map(|(id, n)| match n {
        Cell::Worker(w) => Some((id, w)),
        _ => None,
    })
}

pub fn worker<'a>(sim: &'a Sim<Cell>, name: &str) -> &'a Worker {
    match sim.node(&NodeId::from(name)) {
        Some(Cell::Worker(w)) => w,
        _ => unreachable!(),
    }
}
