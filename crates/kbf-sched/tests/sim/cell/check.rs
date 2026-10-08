//! The checks run after every input the leader feeds its scheduler: a shadow of what
//! the scheduler was fed and what it emitted, compared with what it now holds.
//!
//! Numbered as in the catalog (`docs/design/simulation.md`, section 4), plus the
//! failure family's own:
//!
//! - **I1** no lease granted twice; a scheduler's lease ids increase.
//! - **I2** a `Start` names a committed grant that is still its operation's holding.
//! - **I3** one holding per operation; every held lease is its operation's holding.
//! - **I4** each operation and each waiter answered at most once.
//! - **I5** an answer comes from the operation's newest committed grant before the
//!   result in log order, and its outcome is what a run of that very lease of that
//!   very action produced (the daemon model makes one digest per action and lease).
//! - **I6** bookings are the sum of the held leases' requests, within capacity at a
//!   grant.
//! - **I7** a grant goes to a worker heard within G.
//! - **I14** the queue is exactly the queued operations.
//! - **R** a lease is given up only by the rules: at a tick, once its worker has been
//!   silent for G (and then at that tick, not later); on a heartbeat of its worker that
//!   leaves it out, once its `Start` went to an earlier session or has been out for
//!   `START_GRACE`, and only if no result of it was proposed. A registration or a
//!   resent `Hello` gives up nothing.
//! - **P** a result is proposed at most once per holding.
//! - **N** `not_held` names exactly the listed leases this scheduler granted and no
//!   longer holds on that worker, and never a lease of another term.
//!
//! I8 to I11 and I13 are not reached here: no worker is cordoned, every worker runs
//! every request, every request is one core, and callers' keys are unique. A refusal is
//! a failure: no scenario leaves work unservable for the unservable wait.

use std::collections::{BTreeMap, BTreeSet};

use kbf_sched::fence::{LEASE_GRACE, START_GRACE};
use kbf_sched::{Event, Input, OpState, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Effect, FarmTime, LeaseId, OperationId, Outcome, Resources, WaiterId,
    WorkerId,
};

use super::leader::TERM;
use super::{Ctx, result_digest};

#[derive(Clone, Debug)]
struct Shadow {
    last_heard: FarmTime,
    session: u64,
    capacity: Resources,
}

/// Counts of what the checked steps did, for the scenarios' reach checks.
#[derive(Clone, Debug, Default)]
pub struct CheckStats {
    pub steps: u64,
    /// Leases given up at a tick for silence; of those, at exactly G.
    pub requeued_silent: u64,
    pub requeued_at_g: u64,
    /// A tick 1 ms before G with the worker's leases kept.
    pub kept_before_g: u64,
    /// Leases given up on a heartbeat that left them out: their `Start` went to an
    /// earlier session, or had been out for `START_GRACE`.
    pub requeued_earlier_session: u64,
    pub requeued_after_grace: u64,
    /// Leases a heartbeat left out and the scheduler rightly kept: inside the grace,
    /// or with a result proposed.
    pub kept_omitted: u64,
    pub kept_proposed: u64,
    pub answered: u64,
    pub answered_failed: u64,
    pub named_not_held: u64,
    /// Listed leases of another term, or of this term but never granted here.
    pub foreign_listed: u64,
    /// Listed leases this scheduler holds on another worker (they must be named).
    pub held_elsewhere_listed: u64,
    pub capacity_resends: u64,
    /// Grants whose commit came back after their lease was given up (a slow log).
    pub stale_grant_commits: u64,
    /// Results committed after a newer grant of their operation (they must lose).
    pub superseded_results: u64,
}

pub struct Check {
    ctx: Ctx,
    pub incarnation: u64,
    now: FarmTime,
    workers: BTreeMap<WorkerId, Shadow>,
    ops: BTreeMap<OperationId, Request>,
    in_flight: BTreeMap<ActionKey, OperationId>,
    next_op: u64,
    grants: BTreeMap<LeaseId, (OperationId, WorkerId)>,
    grant_times: BTreeMap<LeaseId, FarmTime>,
    newest_grant: Option<LeaseId>,
    committed: BTreeSet<LeaseId>,
    newest_committed: BTreeMap<OperationId, LeaseId>,
    starts: BTreeMap<LeaseId, (FarmTime, u64)>,
    proposed: BTreeMap<LeaseId, u32>,
    /// Every result proposed, with its operation and outcome.
    pub proposals: BTreeMap<LeaseId, (OperationId, Outcome)>,
    pub answered: BTreeMap<OperationId, (LeaseId, Outcome)>,
    answered_waiters: BTreeSet<WaiterId>,
    held: BTreeMap<LeaseId, (WorkerId, OperationId)>,
    pub stats: CheckStats,
}

fn holding(state: Option<&OpState>) -> Option<(LeaseId, &WorkerId)> {
    match state? {
        OpState::Leased { lease, worker, .. } | OpState::Running { lease, worker } => {
            Some((*lease, worker))
        }
        _ => None,
    }
}

impl Check {
    #[must_use]
    pub fn new(ctx: Ctx, incarnation: u64) -> Self {
        Self {
            ctx,
            incarnation,
            now: FarmTime::default(),
            workers: BTreeMap::new(),
            ops: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            next_op: 0,
            grants: BTreeMap::new(),
            grant_times: BTreeMap::new(),
            newest_grant: None,
            committed: BTreeSet::new(),
            newest_committed: BTreeMap::new(),
            starts: BTreeMap::new(),
            proposed: BTreeMap::new(),
            proposals: BTreeMap::new(),
            answered: BTreeMap::new(),
            answered_waiters: BTreeSet::new(),
            held: BTreeMap::new(),
            stats: CheckStats::default(),
        }
    }

    #[track_caller]
    fn fail(&self, invariant: &str, detail: impl AsRef<str>) -> ! {
        let detail = format!("incarnation {}: {}", self.incarnation, detail.as_ref());
        self.ctx.fail(self.now, invariant, &detail)
    }

    fn grace_ends(&self, worker: &WorkerId) -> FarmTime {
        self.workers[worker].last_heard.saturating_add(LEASE_GRACE)
    }

    /// Checks one step: `input` was fed and `effects` came back; `sched` is after it.
    pub fn after(&mut self, input: &Input, effects: &[Effect], sched: &Scheduler) {
        self.stats.steps += 1;
        self.now = self.now.max(input.now);
        self.shadow_input(&input.event);
        let mut granted_on = BTreeSet::new();
        let mut started = Vec::new();
        for e in effects {
            self.shadow_effect(&input.event, e, &mut granted_on, &mut started);
        }
        let held = self.held_now(sched);
        self.check_state(sched, &held, &granted_on);
        for (lease, op) in started {
            let current = matches!(
                sched.state(op),
                Some(OpState::Leased { lease: l, committed: true, .. }) if *l == lease
            );
            if !current {
                self.fail("I2", format!("Start of {lease} for {op}, not its holding"));
            }
        }
        self.check_removals(&input.event, sched, &held);
        self.check_kept(&input.event, &held);
        self.held = held;
    }

    fn shadow_input(&mut self, event: &Event) {
        let now = self.now;
        match event {
            Event::WorkerUp {
                worker, capacity, ..
            } => {
                let session = self.workers.get(worker).map_or(0, |w| w.session + 1);
                let shadow = Shadow {
                    last_heard: now,
                    session,
                    capacity: *capacity,
                };
                self.workers.insert(worker.clone(), shadow);
            }
            Event::Capacity {
                worker, capacity, ..
            } => {
                if let Some(w) = self.workers.get_mut(worker) {
                    self.stats.capacity_resends += 1;
                    w.capacity = *capacity;
                    w.last_heard = w.last_heard.max(now);
                }
            }
            Event::Heartbeat { worker, .. } => {
                if let Some(w) = self.workers.get_mut(worker) {
                    w.last_heard = w.last_heard.max(now);
                }
            }
            Event::Submit { request, .. } => {
                let twin = self
                    .in_flight
                    .get(&request.key)
                    .filter(|_| request.joinable());
                if twin.is_none() {
                    let op = OperationId(self.next_op);
                    self.next_op += 1;
                    self.ops.insert(op, request.clone());
                    if request.joinable() {
                        self.in_flight.insert(request.key.clone(), op);
                    }
                }
            }
            Event::Committed(ControlRecord::Lease(g)) => {
                if !self.held.contains_key(&g.lease) && !self.answered.contains_key(&g.operation) {
                    self.stats.stale_grant_commits += 1;
                }
                self.committed.insert(g.lease);
                let newest = self.newest_committed.entry(g.operation).or_insert(g.lease);
                *newest = (*newest).max(g.lease);
            }
            Event::Committed(ControlRecord::Result(r)) => {
                let newer = self
                    .newest_committed
                    .get(&r.operation)
                    .is_some_and(|n| *n > r.lease);
                if newer && !self.answered.contains_key(&r.operation) {
                    self.stats.superseded_results += 1;
                }
            }
            _ => {}
        }
    }

    fn shadow_effect(
        &mut self,
        event: &Event,
        effect: &Effect,
        granted_on: &mut BTreeSet<WorkerId>,
        started: &mut Vec<(LeaseId, OperationId)>,
    ) {
        match effect {
            Effect::Commit(ControlRecord::Lease(g)) => {
                if self.grants.contains_key(&g.lease) || self.newest_grant >= Some(g.lease) {
                    self.fail("I1", format!("{} granted again or out of order", g.lease));
                }
                if !self.workers.contains_key(&g.worker) || self.now >= self.grace_ends(&g.worker) {
                    self.fail(
                        "I7",
                        format!("{} granted to {}, not heard within G", g.lease, g.worker),
                    );
                }
                self.newest_grant = Some(g.lease);
                self.grants.insert(g.lease, (g.operation, g.worker.clone()));
                self.grant_times.insert(g.lease, self.now);
                granted_on.insert(g.worker.clone());
            }
            Effect::Commit(ControlRecord::Result(r)) => {
                let n = {
                    let n = self.proposed.entry(r.lease).or_default();
                    *n += 1;
                    *n
                };
                if n > 1 {
                    self.fail("P", format!("result of {} proposed {n} times", r.lease));
                }
                self.proposals.insert(r.lease, (r.operation, r.outcome));
                if self.grants.get(&r.lease).map(|g| g.0) != Some(r.operation) {
                    self.fail(
                        "P",
                        format!("result of {} proposed for {}", r.lease, r.operation),
                    );
                }
            }
            Effect::Commit(record) => self.fail("I10", format!("unexpected {record:?}")),
            Effect::Start(s) => {
                let granted = self.grants.get(&s.lease).map(|g| g.0);
                if !self.committed.contains(&s.lease) || granted != Some(s.operation) {
                    self.fail(
                        "I2",
                        format!("Start of {} before its grant committed", s.lease),
                    );
                }
                let session = self.workers[&s.worker].session;
                self.starts.insert(s.lease, (self.now, session));
                started.push((s.lease, s.operation));
            }
            Effect::Answer(a) => self.check_answer(event, a),
            Effect::Refuse(r) => self.fail("I10", format!("unexpected refusal {r:?}")),
            Effect::Waiting(_) => {}
        }
    }

    fn check_answer(&mut self, event: &Event, a: &kbf_types::Answer) {
        let op = a.operation;
        let from_record = matches!(
            event,
            Event::Committed(ControlRecord::Result(r)) if r.lease == a.lease && r.operation == op
        );
        if !from_record {
            self.fail(
                "I5",
                format!("{op} answered without its result record: {event:?}"),
            );
        }
        if self.answered.insert(op, (a.lease, a.outcome)).is_some() {
            self.fail("I4", format!("{op} answered twice"));
        }
        for w in &a.waiters {
            if !self.answered_waiters.insert(*w) {
                self.fail("I4", format!("waiter {} answered twice", w.0));
            }
        }
        let newest = self.newest_committed.get(&op).copied();
        if newest != Some(a.lease) {
            self.fail(
                "I5",
                format!(
                    "{op} answered by {}, its newest committed grant is {newest:?}",
                    a.lease
                ),
            );
        }
        self.stats.answered += 1;
        match a.outcome {
            Outcome::Completed { action_result } => {
                let want = result_digest(&self.ops[&op].key.action, a.lease);
                if action_result != want {
                    self.fail(
                        "I5",
                        format!(
                            "{op} (action {}) answered by {} with a result no run of that lease of that action produced",
                            self.ops[&op].key.action.size_bytes, a.lease
                        ),
                    );
                }
            }
            Outcome::Failed(_) => self.stats.answered_failed += 1,
        }
    }

    /// The leases `sched` holds, by lease, with their worker and operation.
    fn held_now(&self, sched: &Scheduler) -> BTreeMap<LeaseId, (WorkerId, OperationId)> {
        let mut held = BTreeMap::new();
        for w in self.workers.keys() {
            for lease in sched.leases_on(w) {
                let Some((op, _)) = self.grants.get(&lease) else {
                    self.fail("I3", format!("{w} holds {lease}, never granted"));
                };
                if held.insert(lease, (w.clone(), *op)).is_some() {
                    self.fail("I3", format!("{lease} held on two workers"));
                }
            }
        }
        held
    }

    fn check_state(
        &self,
        sched: &Scheduler,
        held: &BTreeMap<LeaseId, (WorkerId, OperationId)>,
        granted_on: &BTreeSet<WorkerId>,
    ) {
        for (lease, (w, op)) in held {
            if holding(sched.state(*op)) != Some((*lease, w)) {
                self.fail("I3", format!("{w} holds {lease}, not {op}'s holding"));
            }
        }
        let queued: Vec<OperationId> = sched.queued().collect();
        let queue: BTreeSet<OperationId> = queued.iter().copied().collect();
        if queue.len() != queued.len() {
            self.fail("I14", "an operation queued twice");
        }
        for op in self.ops.keys() {
            let state = sched.state(*op);
            if let Some((lease, w)) = holding(state)
                && held.get(&lease) != Some(&(w.clone(), *op))
            {
                self.fail("I3", format!("{op} holds {lease} on {w}, not listed there"));
            }
            if (state == Some(&OpState::Queued)) != queue.contains(op) {
                self.fail(
                    "I14",
                    format!("{op} is {state:?}; in the queue: {}", queue.contains(op)),
                );
            }
        }
        for (w, shadow) in &self.workers {
            let mut sum = Resources::default();
            for (_, op) in held.values().filter(|(on, _)| on == w) {
                sum = sum.saturating_add(self.ops[op].resources);
            }
            let booked = sched.booked(w).unwrap_or_default();
            if booked != sum {
                self.fail(
                    "I6",
                    format!("{w} booked {booked:?}, its leases need {sum:?}"),
                );
            }
            let cap = shadow.capacity;
            let over = booked.cpu_millis > cap.cpu_millis
                || booked.memory_bytes > cap.memory_bytes
                || booked.gpus > cap.gpus;
            if granted_on.contains(w) && over {
                self.fail(
                    "I6",
                    format!("{w} granted past its capacity: {booked:?} > {cap:?}"),
                );
            }
        }
    }

    /// R: every lease given up in this step was given up by a rule.
    fn check_removals(
        &mut self,
        event: &Event,
        sched: &Scheduler,
        held: &BTreeMap<LeaseId, (WorkerId, OperationId)>,
    ) {
        let now = self.now;
        let gone: Vec<(LeaseId, WorkerId, OperationId)> = self
            .held
            .iter()
            .filter(|(l, _)| !held.contains_key(l))
            .map(|(l, (w, op))| (*l, w.clone(), *op))
            .collect();
        for (lease, w, op) in gone {
            if sched.state(op).is_some_and(OpState::is_done) {
                continue;
            }
            match event {
                Event::Tick => {
                    let ends = self.grace_ends(&w);
                    if now < ends {
                        self.fail(
                            "R",
                            format!("{lease} on {w} given up at a tick, {w} heard within G"),
                        );
                    }
                    self.stats.requeued_silent += 1;
                    if now == ends {
                        self.stats.requeued_at_g += 1;
                    }
                }
                Event::Heartbeat { worker, running } if *worker == w => {
                    let Some(&(sent, session)) = self.starts.get(&lease) else {
                        self.fail(
                            "R",
                            format!("{lease} given up on a heartbeat before its Start"),
                        );
                    };
                    if running.contains(&lease) {
                        self.fail(
                            "R",
                            format!("{lease} given up on a heartbeat that lists it"),
                        );
                    }
                    if self.proposed.contains_key(&lease) {
                        self.fail("R", format!("{lease} given up with its result proposed"));
                    }
                    if session < self.workers[&w].session {
                        self.stats.requeued_earlier_session += 1;
                    } else if now >= sent.saturating_add(START_GRACE) {
                        self.stats.requeued_after_grace += 1;
                    } else {
                        self.fail(
                            "R",
                            format!("{lease} left out by a heartbeat of its own session, given up {} ms after its Start", now.saturating_duration_since(sent).as_millis()),
                        );
                    }
                }
                other => self.fail("R", format!("{lease} on {w} given up by {other:?}")),
            }
        }
    }

    /// R, the other way: what the rules give up is given up in this step.
    fn check_kept(&mut self, event: &Event, held: &BTreeMap<LeaseId, (WorkerId, OperationId)>) {
        let now = self.now;
        match event {
            Event::Tick => {
                let mut kept_before_g = BTreeSet::new();
                for (lease, (w, _)) in held {
                    let ends = self.grace_ends(w);
                    if now >= ends {
                        self.fail("R", format!("{lease} kept on {w}, silent for G"));
                    }
                    if now.saturating_add(std::time::Duration::from_millis(1)) == ends {
                        kept_before_g.insert(w.clone());
                    }
                }
                self.stats.kept_before_g += kept_before_g.len() as u64;
            }
            Event::Heartbeat { worker, running } => {
                for (lease, (w, _)) in held.iter().filter(|(_, (w, _))| w == worker) {
                    let Some(&(sent, session)) = self.starts.get(lease) else {
                        continue;
                    };
                    if running.contains(lease) {
                        continue;
                    }
                    let due = session < self.workers[w].session
                        || now >= sent.saturating_add(START_GRACE);
                    if self.proposed.contains_key(lease) {
                        self.stats.kept_proposed += 1;
                    } else if due {
                        self.fail(
                            "R",
                            format!("{lease} left out by {w}'s heartbeat and kept past its rule"),
                        );
                    } else {
                        self.stats.kept_omitted += 1;
                    }
                }
            }
            _ => {}
        }
    }

    /// N: checks what `not_held` named for a heartbeat of `worker` listing `running`.
    pub fn not_held(
        &mut self,
        worker: &WorkerId,
        running: &[LeaseId],
        named: &[LeaseId],
        sched: &Scheduler,
    ) {
        let here: BTreeSet<LeaseId> = sched.leases_on(worker).into_iter().collect();
        for lease in named {
            if !running.contains(lease) {
                self.fail("N", format!("{lease} named, not listed"));
            }
            if lease.term != TERM {
                self.fail("N", format!("{lease} of another term named for cancelling"));
            }
            if !self.grants.contains_key(lease) {
                self.fail(
                    "N",
                    format!("{lease} named, never granted by this scheduler"),
                );
            }
            if here.contains(lease) {
                self.fail("N", format!("{lease} named, but held on {worker}"));
            }
        }
        for lease in running {
            let ours = lease.term == TERM && self.grants.contains_key(lease);
            if !ours {
                self.stats.foreign_listed += 1;
            } else if !here.contains(lease) {
                let elsewhere = self
                    .workers
                    .keys()
                    .any(|w| w != worker && sched.leases_on(w).contains(lease));
                self.stats.held_elsewhere_listed += u64::from(elsewhere);
                if !named.contains(lease) {
                    self.fail(
                        "N",
                        format!("{lease} listed by {worker}, not held there, not named"),
                    );
                }
            }
        }
        self.stats.named_not_held += named.len() as u64;
    }

    /// When `lease` was granted.
    #[must_use]
    pub fn granted_at(&self, lease: LeaseId) -> Option<FarmTime> {
        self.grant_times.get(&lease).copied()
    }

    /// Every lease this scheduler granted `op`, in grant order.
    #[must_use]
    pub fn leases_of(&self, op: OperationId) -> Vec<LeaseId> {
        self.grants
            .iter()
            .filter(|(_, (o, _))| *o == op)
            .map(|(l, _)| *l)
            .collect()
    }
}
