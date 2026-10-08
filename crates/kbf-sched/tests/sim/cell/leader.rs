//! The leader: the scheduler and the server's side of the worker protocol, as
//! `kbf-server`'s farm core carries them out, with a log node in place of the
//! in-process log, a clock that can stop, and a restart (a new process, with a new
//! term).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_sched::fence::LEASE_GRACE;
use kbf_sched::{DaemonInstance, Event as SchedEvent, Input, OpState, Request, Scheduler};
use kbf_sim::{Chance, Event, NodeId, NodeInput, Output, SimRng};
use kbf_types::{
    Answer, ControlRecord, Digest, Effect, FarmTime, LeaseId, OperationId, Outcome, Resources,
    StateMachine, WaiterId, WorkerId,
};

use super::check::Check;
use super::{Ctx, Msg, Plan, TICK, log_id, request};

/// The term of the first leader process.
pub const TERM: u64 = 1;

/// The term of leader incarnation `incarnation`, and the lease epoch its `Welcome`
/// names. As `kbf-server`'s `process_term`, every process has its own term, after
/// every earlier process's (issue #137).
#[must_use]
pub const fn term(incarnation: u64) -> u64 {
    TERM + incarnation
}

const T_TICK: u64 = 0;
const T_BOUNDARY: u64 = 1;
const T_PAUSE: u64 = 2;
const T_RESUME: u64 = 3;
const T_RESTART: u64 = 4;
const T_SUBMIT: u64 = 1 << 20;
const T_HELD_START: u64 = 2 << 20;

/// What the leader does besides serving.
#[derive(Clone, Debug, Default)]
pub struct LeaderPlan {
    /// The process restarts at this time (ms): a fresh scheduler, streams and log.
    pub restart_at: Option<u64>,
    /// The process is suspended at `.0` for `.1` (ms): its clock stops with it.
    pub pause: Option<(u64, u64)>,
    /// The `.0`-th `Start` (counting from 1) is held back for `.1` ms.
    pub late_start: Option<(u64, u64)>,
    /// Ticks also at G - 1 ms, G and G + 1 ms after each time a worker is heard.
    pub boundary_ticks: bool,
    /// Messages from these worker nodes are lost on the way in from `.1` to `.2` (ms).
    pub deaf: Vec<(&'static str, u64, u64)>,
    /// Messages to these worker nodes are lost on the way out from `.1` to `.2` (ms).
    pub mute: Vec<(&'static str, u64, u64)>,
    /// The chance a `Result` is lost on its way in.
    pub lose_reports: Chance,
    /// The chance a `ResultAck` is sent twice.
    pub repeat_acks: Chance,
}

/// One caller: what it asked for, and every answer it got.
#[derive(Clone, Debug)]
pub struct Caller {
    pub request: Request,
    pub arrives: u64,
    /// The waiter it holds in the current incarnation, once it has submitted there.
    pub waiter: Option<WaiterId>,
    /// (incarnation, answer).
    pub answers: Vec<(u64, Answer)>,
}

/// A worker's newest stream.
#[derive(Clone, Copy, Debug)]
struct Link {
    stream: u64,
    newest_beat: u64,
}

/// Counts of what happened, for the scenarios' reach checks.
#[derive(Clone, Debug, Default)]
pub struct LeaderStats {
    pub restarts: u64,
    pub registrations: u64,
    pub resent_hellos: u64,
    pub stale_hellos: u64,
    pub replaced_beats: u64,
    pub unknown_streams: u64,
    pub refused_results: u64,
    pub lost_reports: u64,
    pub cancels: u64,
    /// The held-back `Start`: (lease, operation, when the scheduler emitted it).
    pub late_start: Option<(LeaseId, OperationId, FarmTime)>,
    pub resumed: u64,
}

pub struct Leader {
    plan: LeaderPlan,
    ctx: Ctx,
    sim_now: FarmTime,
    paused_since: Option<FarmTime>,
    paused_total: Duration,
    buffered: Vec<(Event<Msg>, u64)>,
    pub incarnation: u64,
    pub sched: Scheduler,
    pub check: Check,
    /// The checks of earlier incarnations.
    pub past: Vec<Check>,
    /// Each node id's newest stream, and the simulated node that holds it.
    links: BTreeMap<WorkerId, (NodeId, Link)>,
    /// Streams registered in this incarnation, and the node id each claimed.
    streams: BTreeMap<(NodeId, u64), WorkerId>,
    newest_stream: BTreeMap<NodeId, u64>,
    /// Leases whose `Start` was sent, their operations and actions (`State::started`).
    started: BTreeMap<LeaseId, (OperationId, Digest)>,
    next_index: u64,
    pending: BTreeMap<u64, ControlRecord>,
    pub callers: Vec<Caller>,
    waiters: BTreeMap<WaiterId, usize>,
    next_waiter: u64,
    starts_sent: u64,
    held: BTreeMap<u64, (NodeId, Msg)>,
    pub stats: LeaderStats,
    out: Vec<Output<Msg>>,
}

impl Leader {
    #[must_use]
    pub fn new(plan: &Plan) -> Self {
        let mut rng = SimRng::from_seed(plan.ctx.seed ^ 0xca11);
        let callers = (0..plan.callers)
            .map(|n| Caller {
                request: request(n),
                arrives: rng.between(0, plan.arrive_by),
                waiter: None,
                answers: Vec::new(),
            })
            .collect();
        Self {
            plan: plan.leader.clone(),
            ctx: plan.ctx,
            sim_now: FarmTime::default(),
            paused_since: None,
            paused_total: Duration::ZERO,
            buffered: Vec::new(),
            incarnation: 0,
            sched: Scheduler::new(term(0)),
            check: Check::new(plan.ctx, 0),
            past: Vec::new(),
            links: BTreeMap::new(),
            streams: BTreeMap::new(),
            newest_stream: BTreeMap::new(),
            started: BTreeMap::new(),
            next_index: 0,
            pending: BTreeMap::new(),
            callers,
            waiters: BTreeMap::new(),
            next_waiter: 0,
            starts_sent: 0,
            held: BTreeMap::new(),
            stats: LeaderStats::default(),
            out: Vec::new(),
        }
    }

    pub fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }

    /// The leader's own clock: farm time that stops while the process is suspended.
    fn now(&self) -> FarmTime {
        FarmTime::from_millis(self.sim_now.as_millis() - self.paused_total.as_millis() as u64)
    }

    pub fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        self.sim_now = input.now;
        match input.event {
            Event::Timer { tag: T_RESUME } => self.resume(),
            event if self.paused_since.is_some() => self.buffered.push((event, input.entropy)),
            event => self.handle(event, input.entropy),
        }
        Vec::new()
    }

    fn handle(&mut self, event: Event<Msg>, entropy: u64) {
        match event {
            Event::Start => {
                self.timer(TICK, T_TICK);
                for i in 0..self.callers.len() {
                    let at = Duration::from_millis(self.callers[i].arrives);
                    self.timer(at, T_SUBMIT + i as u64);
                }
                if let Some(at) = self.plan.restart_at {
                    self.timer(Duration::from_millis(at), T_RESTART);
                }
                if let Some((at, _)) = self.plan.pause {
                    self.timer(Duration::from_millis(at), T_PAUSE);
                }
            }
            Event::Timer { tag: T_TICK } => {
                self.timer(TICK, T_TICK);
                self.feed(SchedEvent::Tick);
            }
            Event::Timer { tag: T_BOUNDARY } => {
                self.feed(SchedEvent::Tick);
            }
            Event::Timer { tag: T_PAUSE } => {
                self.paused_since = Some(self.sim_now);
                if let Some((_, len)) = self.plan.pause {
                    self.timer(Duration::from_millis(len), T_RESUME);
                }
            }
            Event::Timer { tag: T_RESTART } => self.restart(),
            Event::Timer { tag } if (T_SUBMIT..T_HELD_START).contains(&tag) => {
                self.submit(usize::try_from(tag - T_SUBMIT).expect("small"));
            }
            Event::Timer { tag } => {
                let (to, msg) = self.held.remove(&tag).expect("held Starts");
                self.send(to, msg);
            }
            Event::Message { from, msg } => {
                let ms = self.sim_now.as_millis();
                let deaf = self
                    .plan
                    .deaf
                    .iter()
                    .any(|(n, a, b)| from.as_str() == *n && (*a..*b).contains(&ms));
                if !deaf {
                    self.receive(from, msg, entropy);
                }
            }
        }
    }

    fn resume(&mut self) {
        if let Some(since) = self.paused_since.take() {
            self.stats.resumed += 1;
            self.paused_total += self.sim_now.saturating_duration_since(since);
            for (event, entropy) in std::mem::take(&mut self.buffered) {
                self.handle(event, entropy);
            }
        }
    }

    fn submit(&mut self, caller: usize) {
        if !self.callers[caller].answers.is_empty() {
            return;
        }
        let waiter = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        self.waiters.insert(waiter, caller);
        self.callers[caller].waiter = Some(waiter);
        let request = self.callers[caller].request.clone();
        self.feed(SchedEvent::Submit { waiter, request });
    }

    /// A new process: an empty scheduler of a new term, no streams, a new log.
    /// Every daemon's stream ends; every caller still waiting submits again.
    fn restart(&mut self) {
        self.stats.restarts += 1;
        self.incarnation += 1;
        let check = Check::new(self.ctx, self.incarnation);
        self.past.push(std::mem::replace(&mut self.check, check));
        self.sched = Scheduler::new(term(self.incarnation));
        for (node, link) in std::mem::take(&mut self.links).into_values() {
            let stream = link.stream;
            self.send(node, Msg::Goodbye { stream });
        }
        self.streams.clear();
        self.newest_stream.clear();
        self.started.clear();
        self.pending.clear();
        self.next_index = 0;
        let mut rng = SimRng::from_seed(self.ctx.seed ^ 0x5e5e);
        for i in 0..self.callers.len() {
            let c = &mut self.callers[i];
            if c.answers.is_empty() && c.waiter.take().is_some() {
                let after = Duration::from_millis(rng.between(0, 5_000));
                self.timer(after, T_SUBMIT + i as u64);
            }
        }
    }

    fn receive(&mut self, from: NodeId, msg: Msg, entropy: u64) {
        let mut rng = SimRng::from_seed(entropy);
        match msg {
            Msg::Committed {
                incarnation,
                index,
                record,
            } => {
                if incarnation != self.incarnation {
                    return;
                }
                if index >= self.next_index {
                    self.pending.insert(index, record);
                }
                while let Some(record) = self.pending.remove(&self.next_index) {
                    self.next_index += 1;
                    self.feed(SchedEvent::Committed(record));
                }
            }
            Msg::Hello {
                node,
                instance,
                stream,
                capacity,
            } => self.hello(from, node, DaemonInstance::new(instance), stream, capacity),
            Msg::Heartbeat {
                stream,
                seq,
                running,
            } => {
                let Some(node) = self.stream_node(&from, stream) else {
                    return;
                };
                let link = match self.links.get_mut(&node) {
                    Some((holder, link)) if *holder == from && link.stream == stream => link,
                    _ => {
                        self.stats.replaced_beats += 1;
                        return;
                    }
                };
                link.newest_beat = link.newest_beat.max(seq);
                self.send(from.clone(), Msg::HeartbeatAck { stream, seq });
                self.feed(SchedEvent::Heartbeat {
                    worker: node.clone(),
                    running: running.clone(),
                });
                self.heard();
                let named: Vec<LeaseId> = self.sched.not_held(&node, &running).collect();
                self.check.not_held(&node, &running, &named, &self.sched);
                for lease in named {
                    self.stats.cancels += 1;
                    self.send(from.clone(), Msg::Cancel { stream, lease });
                }
            }
            Msg::Report {
                stream,
                lease,
                outcome,
                action,
            } => {
                let Some(node) = self.stream_node(&from, stream) else {
                    return;
                };
                if rng.chance(self.plan.lose_reports) {
                    self.stats.lost_reports += 1;
                    return;
                }
                self.report(&from, &node, (lease, action), outcome, &mut rng);
            }
            other => panic!("leader got {other:?}"),
        }
    }

    /// The node id a stream registered as, or `None` for a stream this process never
    /// registered (it belongs to an earlier process): the daemon is told it ended.
    fn stream_node(&mut self, from: &NodeId, stream: u64) -> Option<WorkerId> {
        let node = self.streams.get(&(from.clone(), stream)).cloned();
        if node.is_none() {
            self.stats.unknown_streams += 1;
            self.send(from.clone(), Msg::Goodbye { stream });
        }
        node
    }

    /// Only the first `Hello` of a stream registers; one resent on the stream changes
    /// the node's capacity; a `Hello` of a stream older than the daemon's newest is
    /// ignored (that stream was closed before the newer one opened).
    fn hello(
        &mut self,
        from: NodeId,
        node: WorkerId,
        instance: DaemonInstance,
        stream: u64,
        capacity: Resources,
    ) {
        let caps = || kbf_caps::NodeCaps::from_report([("arch", "x86_64")]).expect("valid");
        if let Some(claimed) = self.streams.get(&(from.clone(), stream)) {
            let current = self
                .links
                .get(claimed)
                .is_some_and(|(holder, link)| *holder == from && link.stream == stream);
            if current {
                self.stats.resent_hellos += 1;
                let worker = claimed.clone();
                self.feed(SchedEvent::Capacity {
                    worker,
                    capacity,
                    caps: caps(),
                });
                self.heard();
            }
            return;
        }
        if self.newest_stream.get(&from).is_some_and(|&s| stream < s) {
            self.stats.stale_hellos += 1;
            return;
        }
        self.stats.registrations += 1;
        self.newest_stream.insert(from.clone(), stream);
        self.streams.insert((from.clone(), stream), node.clone());
        let link = Link {
            stream,
            newest_beat: 0,
        };
        self.links.insert(node.clone(), (from.clone(), link));
        let epoch = term(self.incarnation);
        self.send(from, Msg::Welcome { stream, epoch });
        self.feed(SchedEvent::WorkerUp {
            worker: node,
            instance,
            capacity,
            caps: caps(),
        });
        self.heard();
    }

    /// After hearing a worker: the ticks at the edge of its grace, if asked for.
    fn heard(&mut self) {
        if self.plan.boundary_ticks {
            let ms = Duration::from_millis(1);
            for after in [LEASE_GRACE - ms, LEASE_GRACE, LEASE_GRACE + ms] {
                self.timer(after, T_BOUNDARY);
            }
        }
    }

    /// `State::holder` and `Farm::report`: fed only from the node holding the
    /// operation's current committed lease, and only if the action the result names
    /// (if any) is that lease's, else refused.
    fn report(
        &mut self,
        from: &NodeId,
        node: &WorkerId,
        (lease, action): (LeaseId, Option<Digest>),
        outcome: Outcome,
        rng: &mut SimRng,
    ) {
        let holder = self
            .started
            .get(&lease)
            .copied()
            .filter(|(_, ran)| action.is_none_or(|named| named == *ran))
            .map(|(op, _)| op)
            .filter(|op| match self.sched.state(*op) {
                Some(OpState::Leased {
                    lease: held,
                    worker,
                    committed: true,
                })
                | Some(OpState::Running {
                    lease: held,
                    worker,
                }) => *held == lease && worker == node,
                _ => false,
            });
        let accepted = match holder {
            None => {
                self.stats.refused_results += 1;
                false
            }
            Some(operation) => {
                let effects = self.feed(SchedEvent::Report {
                    operation,
                    lease,
                    outcome,
                });
                effects.iter().any(
                    |e| matches!(e, Effect::Commit(ControlRecord::Result(r)) if r.lease == lease),
                )
            }
        };
        let ack = Msg::ResultAck { lease, accepted };
        if rng.chance(self.plan.repeat_acks) {
            self.send(from.clone(), ack.clone());
        }
        self.send(from.clone(), ack);
    }

    /// Feeds the scheduler, checks the step, and carries out its effects.
    fn feed(&mut self, event: SchedEvent) -> Vec<Effect> {
        let input = Input::new(self.now(), event);
        let effects = self.sched.apply(input.clone());
        self.check.after(&input, &effects, &self.sched);
        for e in &effects {
            match e {
                Effect::Commit(record) => {
                    let msg = Msg::Append {
                        incarnation: self.incarnation,
                        record: record.clone(),
                    };
                    self.send(log_id(), msg);
                }
                Effect::Start(s) => {
                    self.started.insert(s.lease, (s.operation, s.key.action));
                    let Some((to, link)) = self.links.get(&s.worker).cloned() else {
                        continue;
                    };
                    let msg = Msg::Start {
                        stream: link.stream,
                        start: s.clone(),
                        heartbeat_seq: link.newest_beat,
                        incarnation: self.incarnation,
                    };
                    self.starts_sent += 1;
                    match self.plan.late_start {
                        Some((nth, delay)) if nth == self.starts_sent => {
                            self.stats.late_start = Some((s.lease, s.operation, self.now()));
                            let tag = T_HELD_START + self.starts_sent;
                            self.held.insert(tag, (to, msg));
                            self.timer(Duration::from_millis(delay), tag);
                        }
                        _ => self.send(to, msg),
                    }
                }
                Effect::Answer(a) => {
                    for w in &a.waiters {
                        let c = self.waiters[w];
                        self.callers[c].answers.push((self.incarnation, a.clone()));
                    }
                }
                Effect::Waiting(_) | Effect::Refuse(_) => {}
            }
        }
        effects
    }

    fn timer(&mut self, after: Duration, tag: u64) {
        self.out.push(Output::Timer { after, tag });
    }

    fn send(&mut self, to: NodeId, msg: Msg) {
        let ms = self.sim_now.as_millis();
        let mute = self
            .plan
            .mute
            .iter()
            .any(|(n, a, b)| to.as_str() == *n && (*a..*b).contains(&ms));
        if !mute {
            self.out.push(Output::Send { to, msg });
        }
    }

    /// Leases the current scheduler holds on any worker it knows.
    #[must_use]
    pub fn held_leases(&self) -> BTreeSet<LeaseId> {
        self.links
            .keys()
            .flat_map(|w| self.sched.leases_on(w))
            .collect()
    }

    /// The node ids registered with the current process.
    pub fn nodes(&self) -> impl Iterator<Item = &WorkerId> {
        self.links.keys()
    }
}
