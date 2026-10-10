//! The step checker: wraps a [`Scheduler`], feeds it every input, and after each one
//! checks the invariants of `docs/design/simulation.md` section 4 against a shadow it
//! keeps of what the scheduler was fed and what it emitted.
//!
//! The shadow is a reference model written from the scheduler's documented rules, not
//! from its code: first fit in queue order (I11), the unservable verdict and its wait
//! (I10, L2), expiry after G and reconciliation with the running set (I3, I9, L3),
//! dedup and promotion (I13), and the log-order result rule (I5). Each input's
//! effects are compared with what the model expects, and the scheduler's observable
//! state with the model's: after every input, each touched operation's state, each
//! touched worker's bookings and the leases it holds, the queue, and every worker's
//! cordon state. The shadow is kept incrementally, so a step costs what it changed
//! (plus one cordon lookup per worker); [`Checker::full_check`] also compares every
//! operation and every worker's bookings and leases, untouched ones included, and is
//! run every simulated half minute.
//!
//! A violation panics with the seed, the step, the invariant and a replay command.
//!
//! The catalog plans one shared checker for every scheduler sim
//! (`tests/sim/check.rs`), built with family F1. This one is local to F4 until the
//! two are merged.

mod round;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Display;

use super::reference::{Verdict, add, blame, fits, sub, verdict};
use kbf_caps::NodeCaps;
use kbf_sched::fence::{HANDOVER_GRACE, LEASE_GRACE, START_GRACE};
use kbf_sched::{
    Cordon, DaemonInstance, Event, Input, OpState, PLACEMENT_ROUND, Request, Scheduler,
};

use kbf_types::{
    ActionKey, Answer, ControlRecord, Effect, FarmTime, FencePolicy, LeaseGrant, LeaseId,
    OperationId, Outcome, Qos, Refusal, RefusalRecord, Resources, ResultRecord, StartLease,
    StateMachine, WaiterId, Waiting, WorkerId,
};

/// The scheduler's term in every F4 run.
pub const TERM: u64 = 1;

fn ms(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).expect("durations fit")
}

#[derive(Clone, Debug)]
struct Worker {
    capacity: Resources,
    caps: NodeCaps,
    booked: Resources,
    last_heard: u64,
    session: u64,
    /// The daemon process of the current session, how many times it changed, and until
    /// when the leases of an earlier one are kept (issue #140).
    instance: DaemonInstance,
    process: u64,
    handover_ends: u64,
    leases: BTreeSet<LeaseId>,
}

impl Worker {
    fn alive(&self, now: u64) -> bool {
        now < self.last_heard + ms(LEASE_GRACE)
    }
}

#[derive(Clone, Debug)]
struct Lease {
    op: OperationId,
    worker: WorkerId,
    committed: bool,
    running: bool,
    /// When its `Start` was emitted, and to which session and daemon process.
    sent: Option<(u64, u64, u64)>,
}

/// A queued operation's unbroken wait with no worker able to run it.
#[derive(Clone, Copy, Debug)]
struct Wait {
    since: u64,
    verdict: Verdict,
}

#[derive(Clone, Debug)]
struct Op {
    request: Request,
    needs: usize,
    waiters: Vec<WaiterId>,
    /// `None` while queued (or waiting for its refusal to commit).
    holding: Option<LeaseId>,
    done: Option<OpState>,
    newest: Option<LeaseId>,
    proposed: bool,
    wait: Option<Wait>,
    /// The last reason its callers were told, while it waits.
    told: Option<String>,
    /// Finished, and dropped once the finished retention was up (I16).
    dropped: bool,
}

/// How often each situation the family means to reach was reached.
pub type Reach = BTreeMap<&'static str, u64>;

/// The scheduler under test and its shadow.
pub struct Checker {
    sched: Scheduler,
    replay: String,
    step: u64,
    now: u64,
    wait_ms: u64,
    /// The finished retention (I16).
    retention_ms: u64,
    /// Finished operations not dropped yet, in the order they finished, with when.
    finished: VecDeque<(u64, OperationId)>,
    /// Operations the scheduler should hold: submitted and not dropped.
    kept: usize,
    workers: BTreeMap<WorkerId, Worker>,
    cordons: BTreeMap<WorkerId, Cordon>,
    ops: Vec<Op>,
    leases: BTreeMap<LeaseId, Lease>,
    queue: BTreeSet<(Reverse<Qos>, OperationId)>,
    in_flight: BTreeMap<ActionKey, OperationId>,
    next_seq: u64,
    needs: Vec<kbf_caps::Request>,
    answered: BTreeSet<WaiterId>,
    waiters: u64,
    /// The outcome each lease's run produced, as its worker reported it.
    produced: BTreeMap<LeaseId, Outcome>,
    /// Self-fenced runs in progress, per operation.
    fenced_runs: BTreeMap<OperationId, u32>,
    touched_ops: BTreeSet<OperationId>,
    touched_workers: BTreeSet<WorkerId>,
    pub reach: Reach,
}

impl Checker {
    /// A checker over `sched`, which refuses after `wait_ms` of unservable wait and
    /// keeps a finished operation for `retention_ms`. `replay` is the command that
    /// replays this run, printed with a violation.
    pub fn new(sched: Scheduler, wait_ms: u64, retention_ms: u64, replay: String) -> Self {
        Self {
            sched,
            replay,
            step: 0,
            now: 0,
            wait_ms,
            retention_ms,
            finished: VecDeque::new(),
            kept: 0,
            workers: BTreeMap::new(),
            cordons: BTreeMap::new(),
            ops: Vec::new(),
            leases: BTreeMap::new(),
            queue: BTreeSet::new(),
            in_flight: BTreeMap::new(),
            next_seq: 0,
            needs: Vec::new(),
            answered: BTreeSet::new(),
            waiters: 0,
            produced: BTreeMap::new(),
            fenced_runs: BTreeMap::new(),
            touched_ops: BTreeSet::new(),
            touched_workers: BTreeSet::new(),
            reach: Reach::new(),
        }
    }

    #[track_caller]
    pub fn violated(&self, invariant: &str, what: impl Display) -> ! {
        panic!(
            "{invariant} violated at step {} (farm time {} ms): {what}\n  replay: {}",
            self.step, self.now, self.replay
        )
    }

    pub fn hit(&mut self, what: &'static str) {
        *self.reach.entry(what).or_default() += 1;
    }

    fn op(&self, id: OperationId) -> &Op {
        &self.ops[usize::try_from(id.0).expect("ids fit")]
    }

    fn op_mut(&mut self, id: OperationId) -> &mut Op {
        self.touched_ops.insert(id);
        &mut self.ops[usize::try_from(id.0).expect("ids fit")]
    }

    /// Operations not finished yet.
    pub fn unfinished(&self) -> impl Iterator<Item = (OperationId, &Request)> + '_ {
        self.ops
            .iter()
            .enumerate()
            .filter(|(_, op)| op.done.is_none())
            .map(|(i, op)| (OperationId(i as u64), &op.request))
    }

    /// Workers the operator has cordoned, by the shadow.
    pub fn cordoned(&self) -> Vec<WorkerId> {
        self.cordons.keys().cloned().collect()
    }

    /// Records the outcome lease `lease`'s run produced (I5).
    pub fn produced(&mut self, lease: LeaseId, outcome: Outcome) {
        self.produced.insert(lease, outcome);
    }

    /// A run of `op` begins on some worker (I12).
    pub fn run_begins(&mut self, op: OperationId, fence: FencePolicy) {
        if fence == FencePolicy::SelfFence {
            let n = self.fenced_runs.entry(op).or_default();
            *n += 1;
            if *n > 1 {
                self.violated("I12", format!("{op}: two self-fenced runs at once"));
            }
        }
    }

    /// A run of `op` ends: it finished, was fenced, cancelled, or died with its node.
    pub fn run_ends(&mut self, op: OperationId, fence: FencePolicy) {
        if fence == FencePolicy::SelfFence {
            let n = self.fenced_runs.entry(op).or_default();
            *n = n.checked_sub(1).expect("ended runs began");
        }
    }

    /// Feeds `event` at `now_ms` and checks the step. Returns the effects, for the
    /// caller to carry out.
    pub fn feed(&mut self, now_ms: u64, event: Event) -> Vec<Effect> {
        self.step += 1;
        self.now = self.now.max(now_ms);
        let effects = self
            .sched
            .apply(Input::new(FarmTime::from_millis(now_ms), event.clone()));
        let expected = self.model(event, &effects);
        if let Some(expected) = expected
            && expected != effects
        {
            let invariant = blame(&expected, &effects);
            self.violated(
                invariant,
                format!(
                    "effects differ from the model\n  expected {expected:?}\n  got      {effects:?}"
                ),
            );
        }
        self.retire();
        self.progress();
        self.compare();
        effects
    }

    /// Drops every finished operation whose retention is up (I16), as the scheduler
    /// does after every input.
    fn retire(&mut self) {
        while let Some(&(at, id)) = self.finished.front()
            && self.now >= at + self.retention_ms
        {
            self.finished.pop_front();
            self.op_mut(id).dropped = true;
            self.kept -= 1;
            self.hit("dropped after the retention");
        }
    }

    /// Applies `event` to the shadow. Returns the effects the model expects, or
    /// `None` when it checked them itself (a placement round).
    fn model(&mut self, event: Event, effects: &[Effect]) -> Option<Vec<Effect>> {
        let now = self.now;
        Some(match event {
            Event::WorkerUp {
                worker,
                instance,
                capacity,
                caps,
            } => {
                self.touched_workers.insert(worker.clone());
                match self.workers.get_mut(&worker) {
                    Some(w) => {
                        if instance != w.instance {
                            w.process += 1;
                            w.handover_ends = w.last_heard + ms(HANDOVER_GRACE);
                        }
                        w.instance = instance;
                        w.capacity = capacity;
                        w.caps = caps;
                        w.last_heard = now;
                        w.session += 1;
                    }
                    None => {
                        let w = Worker {
                            capacity,
                            caps,
                            booked: Resources::default(),
                            last_heard: now,
                            session: 0,
                            instance,
                            process: 0,
                            handover_ends: 0,
                            leases: BTreeSet::new(),
                        };
                        self.workers.insert(worker, w);
                    }
                }
                Vec::new()
            }
            Event::Capacity {
                worker,
                capacity,
                caps,
            } => {
                if let Some(w) = self.workers.get_mut(&worker) {
                    w.capacity = capacity;
                    w.caps = caps;
                    w.last_heard = w.last_heard.max(now);
                    let below = !fits(capacity, w.booked);
                    self.touched_workers.insert(worker);
                    if below {
                        self.hit("capacity below bookings");
                    }
                }
                Vec::new()
            }
            Event::Heartbeat { worker, running } => {
                self.heartbeat(&worker, &running);
                Vec::new()
            }
            Event::Submit { waiter, request } => self.submit(waiter, request),
            Event::Committed(ControlRecord::Lease(grant)) => self.lease_committed(&grant),
            Event::Committed(ControlRecord::Result(record)) => self.result_committed(&record),
            Event::Committed(ControlRecord::Refusal(record)) => self.refusal_committed(&record),
            Event::Committed(_) => Vec::new(),
            Event::Started { operation, lease } => {
                if self.leases.get(&lease).is_some_and(|l| l.committed)
                    && self.op(operation).holding == Some(lease)
                {
                    self.leases.get_mut(&lease).expect("checked").running = true;
                    self.touched_ops.insert(operation);
                }
                Vec::new()
            }
            Event::Report {
                operation,
                lease,
                outcome,
            } => {
                let op = self.op(operation);
                let current = op
                    .holding
                    .filter(|l| self.leases.get(l).is_some_and(|l| l.committed));
                if current == Some(lease) && !op.proposed {
                    self.op_mut(operation).proposed = true;
                    vec![Effect::Commit(ControlRecord::Result(ResultRecord {
                        lease,
                        operation,
                        outcome,
                    }))]
                } else {
                    self.hit("report dropped");
                    Vec::new()
                }
            }
            Event::Tick => {
                self.expire();
                self.round(effects);
                return None;
            }
            Event::Cordon { worker } => {
                self.cordons
                    .entry(worker.clone())
                    .or_insert(Cordon::Cordoned);
                self.touched_workers.insert(worker);
                Vec::new()
            }
            Event::Drain { worker, deadline } => {
                self.cordons
                    .insert(worker.clone(), Cordon::Draining { deadline });
                self.touched_workers.insert(worker);
                Vec::new()
            }
            Event::Uncordon { worker } => {
                self.cordons.remove(&worker);
                self.touched_workers.insert(worker);
                self.round(effects);
                return None;
            }
        })
    }

    fn heartbeat(&mut self, worker: &WorkerId, running: &[LeaseId]) {
        let now = self.now;
        let Some(w) = self.workers.get_mut(worker) else {
            return;
        };
        w.last_heard = w.last_heard.max(now);
        let (session, process, handover_ends) = (w.session, w.process, w.handover_ends);
        let listed: BTreeSet<LeaseId> = running.iter().copied().collect();
        let mut lost = Vec::new();
        let mut kept_for_handover = 0;
        for lease in &w.leases {
            let l = &self.leases[lease];
            let Some((at, sent_to, sent_process)) = l.sent else {
                continue;
            };
            let op = &self.ops[usize::try_from(l.op.0).expect("ids fit")];
            if listed.contains(lease) || op.proposed {
                continue;
            }
            // Another daemon process may still run it until its fence (issue #140).
            let why = if sent_process != process {
                if now < handover_ends {
                    kept_for_handover += 1;
                    continue;
                }
                "requeued: Start sent to a replaced daemon process, after the handover grace"
            } else if sent_to < session {
                "requeued: Start sent to an earlier session (L3)"
            } else if now >= at + ms(START_GRACE) {
                "requeued: Start out for START_GRACE"
            } else {
                continue;
            };
            lost.push((l.op, why));
        }
        if kept_for_handover > 0 {
            self.hit("kept: Start sent to a replaced daemon process, inside the handover grace");
        }
        for (op, why) in lost {
            self.hit(why);
            self.requeue(op);
        }
    }

    fn submit(&mut self, waiter: WaiterId, request: Request) -> Vec<Effect> {
        self.waiters += 1;
        let joined = request
            .joinable()
            .then(|| self.in_flight.get(&request.key).copied())
            .flatten();
        if let Some(id) = joined {
            self.hit("join");
            let promoted = request.qos > self.op(id).request.qos;
            let old = self.op(id).request.qos.clone();
            if promoted {
                self.hit("promotion");
                if self.queue.remove(&(Reverse(old), id)) {
                    self.queue.insert((Reverse(request.qos.clone()), id));
                    self.hit("promotion while queued");
                }
            }
            let op = self.op_mut(id);
            op.waiters.push(waiter);
            if promoted {
                op.request.qos = request.qos;
            }
            let told = op.told.clone();
            self.check_dedup(id);
            return told
                .map(|reason| {
                    Effect::Waiting(Waiting {
                        operation: id,
                        reason: Some(reason),
                    })
                })
                .into_iter()
                .collect();
        }
        let id = OperationId(self.ops.len() as u64);
        if request.joinable() {
            self.in_flight.insert(request.key.clone(), id);
        }
        let needs = match self.needs.iter().position(|n| *n == request.needs) {
            Some(i) => i,
            None => {
                self.needs.push(request.needs.clone());
                self.needs.len() - 1
            }
        };
        self.queue.insert((Reverse(request.qos.clone()), id));
        self.ops.push(Op {
            request,
            needs,
            waiters: vec![waiter],
            holding: None,
            done: None,
            newest: None,
            proposed: false,
            wait: None,
            told: None,
            dropped: false,
        });
        self.kept += 1;
        self.touched_ops.insert(id);
        self.check_dedup(id);
        Vec::new()
    }

    /// I13: the scheduler attached the same waiters, at the same QoS.
    fn check_dedup(&self, id: OperationId) {
        let op = self.op(id);
        if self.sched.waiters(id) != Some(op.waiters.as_slice()) {
            self.violated(
                "I13",
                format!(
                    "{id}: waiters {:?}, expected {:?}",
                    self.sched.waiters(id),
                    op.waiters
                ),
            );
        }
        if self.sched.qos(id) != Some(&op.request.qos) {
            self.violated(
                "I13",
                format!(
                    "{id}: QoS {:?}, expected {:?}",
                    self.sched.qos(id),
                    op.request.qos
                ),
            );
        }
    }

    fn lease_committed(&mut self, grant: &LeaseGrant) -> Vec<Effect> {
        let id = grant.operation;
        if self.op(id).done.is_some() {
            return Vec::new();
        }
        let current = self.op(id).holding == Some(grant.lease)
            && self.leases.get(&grant.lease).is_some_and(|l| !l.committed);
        let op = self.op_mut(id);
        op.newest = op.newest.max(Some(grant.lease));
        if !current {
            self.hit("superseded grant committed");
            return Vec::new();
        }
        let (session, process) = {
            let w = &self.workers[&grant.worker];
            (w.session, w.process)
        };
        let now = self.now;
        let l = self.leases.get_mut(&grant.lease).expect("held");
        l.committed = true;
        l.sent = Some((now, session, process));
        let op = self.op(id);
        vec![Effect::Start(StartLease {
            worker: grant.worker.clone(),
            lease: grant.lease,
            operation: id,
            key: op.request.key.clone(),
            kind: op.request.kind,
            resources: op.request.resources,
            fence: op.request.fence(),
        })]
    }

    fn result_committed(&mut self, record: &ResultRecord) -> Vec<Effect> {
        let id = record.operation;
        let op = self.op(id);
        if op.done.is_some() || op.newest != Some(record.lease) {
            self.hit("result of a superseded lease dropped");
            return Vec::new();
        }
        // I5, with the worker model: the outcome is one this very lease's run produced.
        if self.produced.get(&record.lease) != Some(&record.outcome) {
            self.violated(
                "I5",
                format!(
                    "{id}: result {:?} of {} was not produced by that lease's run ({:?})",
                    record.outcome,
                    record.lease,
                    self.produced.get(&record.lease)
                ),
            );
        }
        self.release(id);
        let state = match record.outcome {
            Outcome::Completed { action_result } => OpState::Completed {
                lease: record.lease,
                action_result,
            },
            Outcome::Failed(failure) => OpState::Failed {
                lease: record.lease,
                failure,
            },
        };
        let waiters = self.finish(id, state);
        self.hit("answered");
        vec![Effect::Answer(Answer {
            operation: id,
            lease: record.lease,
            waiters,
            outcome: record.outcome,
            // The model's runs are never killed for memory.
            memory_runs: Vec::new(),
        })]
    }

    fn refusal_committed(&mut self, record: &RefusalRecord) -> Vec<Effect> {
        let id = record.operation;
        let op = self.op(id);
        if op.done.is_some() || op.holding.is_some() {
            self.hit("stale refusal dropped");
            return Vec::new();
        }
        let waiters = self.finish(
            id,
            OpState::Refused {
                reason: record.reason.clone(),
            },
        );
        self.hit("refused");
        vec![Effect::Refuse(Refusal {
            operation: id,
            waiters,
            reason: record.reason.clone(),
        })]
    }

    /// Finishes `id` (I4): out of the queue and the dedup map, its waiters answered.
    fn finish(&mut self, id: OperationId, state: OpState) -> Vec<WaiterId> {
        let qos = self.op(id).request.qos.clone();
        self.queue.remove(&(Reverse(qos), id));
        self.finished.push_back((self.now, id));
        let op = self.op_mut(id);
        op.done = Some(state);
        op.wait = None;
        op.told = None;
        let (key, waiters) = (op.request.key.clone(), op.waiters.clone());
        if self.in_flight.get(&key) == Some(&id) {
            self.in_flight.remove(&key);
        }
        for w in &waiters {
            if !self.answered.insert(*w) {
                self.violated("I4", format!("{w:?} answered twice"));
            }
        }
        waiters
    }

    /// Releases `id`'s holding: its lease and booking.
    fn release(&mut self, id: OperationId) {
        let op = self.op_mut(id);
        op.proposed = false;
        let Some(lease) = op.holding.take() else {
            return;
        };
        let resources = op.request.resources;
        let l = self
            .leases
            .remove(&lease)
            .expect("held leases are shadowed");
        let w = self.workers.get_mut(&l.worker).expect("registered");
        w.booked = sub(w.booked, resources);
        w.leases.remove(&lease);
        self.touched_workers.insert(l.worker);
    }

    fn requeue(&mut self, id: OperationId) {
        self.release(id);
        let qos = self.op(id).request.qos.clone();
        self.queue.insert((Reverse(qos), id));
    }

    fn expire(&mut self) {
        let now = self.now;
        let silent: Vec<LeaseId> = self
            .workers
            .values()
            .filter(|w| !w.alive(now))
            .flat_map(|w| w.leases.iter().copied())
            .collect();
        for lease in silent {
            self.hit("requeued: worker silent for G");
            let op = self.leases[&lease].op;
            self.requeue(op);
        }
    }

    /// Moves drains on, as the scheduler does after every input.
    fn progress(&mut self) {
        let now = self.now;
        for (name, cordon) in &mut self.cordons {
            if let Cordon::Draining { deadline } = *cordon {
                let holds = self.workers.get(name).is_some_and(|w| !w.leases.is_empty());
                if !holds {
                    *cordon = Cordon::Drained;
                    self.touched_workers.insert(name.clone());
                } else if now >= deadline.as_millis() {
                    *cordon = Cordon::Paused { deadline };
                    self.touched_workers.insert(name.clone());
                }
            }
        }
    }

    /// The scheduler's observable state against the shadow, for what this step touched.
    fn compare(&mut self) {
        // I16: the scheduler holds exactly the operations not dropped.
        if self.sched.operations() != self.kept {
            self.violated(
                "I16",
                format!(
                    "holds {} operations, expected {} (unfinished, or finished within the \
                     retention)",
                    self.sched.operations(),
                    self.kept
                ),
            );
        }
        for id in std::mem::take(&mut self.touched_ops) {
            self.compare_op(id);
        }
        for name in std::mem::take(&mut self.touched_workers) {
            self.compare_worker(&name, true);
        }
        // I8, I9: every worker's cordon state, touched or not (a map lookup each).
        for (name, cordon) in self.workers.keys().map(|n| (n, self.cordons.get(n))) {
            if self.sched.cordon(name) != cordon {
                self.violated(
                    "I8/I9",
                    format!(
                        "{name} (not touched this step) cordon {:?}, expected {cordon:?}",
                        self.sched.cordon(name)
                    ),
                );
            }
        }
        // I14: the queue is exactly the queued operations, in (urgency, id) order.
        if !self.sched.queued().eq(self.queue.iter().map(|(_, id)| *id)) {
            let got: Vec<OperationId> = self.sched.queued().collect();
            let want: Vec<OperationId> = self.queue.iter().map(|(_, id)| *id).collect();
            self.violated("I14", format!("queue {got:?}, expected {want:?}"));
        }
    }

    fn expected_state(&self, id: OperationId) -> OpState {
        let op = self.op(id);
        if let Some(done) = &op.done {
            return done.clone();
        }
        match op.holding {
            None => OpState::Queued,
            Some(lease) => {
                let l = &self.leases[&lease];
                if l.running {
                    OpState::Running {
                        lease,
                        worker: l.worker.clone(),
                    }
                } else {
                    OpState::Leased {
                        lease,
                        worker: l.worker.clone(),
                        committed: l.committed,
                    }
                }
            }
        }
    }

    fn compare_op(&self, id: OperationId) {
        if self.op(id).dropped {
            if self.sched.state(id).is_some() || self.sched.waiters(id).is_some() {
                self.violated(
                    "I16",
                    format!("{id} is {:?} after its retention", self.sched.state(id)),
                );
            }
            return;
        }
        let want = self.expected_state(id);
        let got = self.sched.state(id);
        if got != Some(&want) {
            let invariant = match (&want, got) {
                (
                    OpState::Leased { worker, .. } | OpState::Running { worker, .. },
                    Some(OpState::Queued),
                ) if self.cordons.contains_key(worker) => "I9 (a drain gave up a lease)",
                (_, Some(s)) if s.is_done() || want.is_done() => "I4/I5",
                _ => "I3",
            };
            self.violated(invariant, format!("{id} is {got:?}, expected {want:?}"));
        }
    }

    fn compare_worker(&self, name: &WorkerId, leases: bool) {
        let w = self.workers.get(name);
        let want = w.map(|w| w.booked);
        if self.sched.booked(name) != want {
            self.violated(
                "I6",
                format!(
                    "{name} booked {:?}, expected {want:?} (the sum of its leases)",
                    self.sched.booked(name)
                ),
            );
        }
        if leases {
            let want: Vec<LeaseId> = w
                .map(|w| w.leases.iter().copied().collect())
                .unwrap_or_default();
            if self.sched.leases_on(name) != want {
                self.violated(
                    "I3",
                    format!(
                        "{name} holds {:?}, expected {want:?}",
                        self.sched.leases_on(name)
                    ),
                );
            }
        }
        let cordon = self.cordons.get(name);
        if self.sched.cordon(name) != cordon {
            let invariant = if cordon.is_none() || self.sched.cordon(name).is_none() {
                "I8"
            } else {
                "I9"
            };
            self.violated(
                invariant,
                format!(
                    "{name} cordon {:?}, expected {cordon:?}",
                    self.sched.cordon(name)
                ),
            );
        }
    }

    /// Compares every operation and worker, and the leases each holds (I3).
    pub fn full_check(&mut self) {
        for i in 0..self.ops.len() {
            self.compare_op(OperationId(i as u64));
        }
        let names: Vec<WorkerId> = self.workers.keys().cloned().collect();
        for name in &names {
            self.compare_worker(name, true);
        }
        let counts: Vec<&'static str> = self
            .cordons
            .values()
            .filter_map(|c| match c {
                Cordon::Drained => Some("drained"),
                Cordon::Paused { .. } => Some("paused"),
                Cordon::Draining { .. } => Some("draining"),
                Cordon::Cordoned => None,
            })
            .collect();
        for what in counts {
            self.hit(what);
        }
    }

    /// `sched.not_held` for a heartbeat's running set, checked: it names exactly the
    /// leases of this term, granted, that the worker no longer holds.
    pub fn not_held(&mut self, worker: &WorkerId, running: &[LeaseId]) -> Vec<LeaseId> {
        let got: Vec<LeaseId> = self.sched.not_held(worker, running).collect();
        let want: Vec<LeaseId> = running
            .iter()
            .copied()
            .filter(|l| {
                l.term == TERM
                    && l.seq < self.next_seq
                    && self.leases.get(l).is_none_or(|h| h.worker != *worker)
            })
            .collect();
        if got != want {
            self.violated("not_held", format!("{worker}: {got:?}, expected {want:?}"));
        }
        if !got.is_empty() {
            self.hit("not_held cancels");
        }
        got
    }

    /// L1, at the end: every operation finished, every waiter answered once, nothing
    /// held or booked, the queue empty.
    pub fn quiet(&self) -> bool {
        self.ops.iter().all(|op| op.done.is_some())
    }

    /// Asserts L1 holds now.
    pub fn check_quiet(&mut self) {
        self.full_check();
        if let Some((id, _)) = self.unfinished().next() {
            self.violated("L1", format!("{id} unfinished: {:?}", self.sched.state(id)));
        }
        if self.answered.len() as u64 != self.waiters {
            self.violated(
                "L1",
                format!(
                    "{} of {} waiters answered",
                    self.answered.len(),
                    self.waiters
                ),
            );
        }
        if !self.leases.is_empty() || self.sched.queued().next().is_some() {
            self.violated(
                "L1",
                "leases held or work queued after every operation finished",
            );
        }
        for (name, w) in &self.workers {
            if w.booked != Resources::default() {
                self.violated("L1", format!("{name} still books {:?}", w.booked));
            }
        }
    }
}
