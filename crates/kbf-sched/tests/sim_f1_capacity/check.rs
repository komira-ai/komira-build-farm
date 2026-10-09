//! The invariant checker: a shadow of what the scheduler was fed and what it emitted,
//! checked after every input (catalog section 4, `docs/design/simulation.md`).
//!
//! The shadow is written from the scheduler's contract, not from its code: it keeps
//! its own queue, bookings, leases and dedup map, and its own reference first fit and
//! unservable verdict, and compares the scheduler's effects and public state with them
//! after each input. Nothing here calls `Resources::fits` or `saturating_sub`: a
//! mutant there must not also bend the reference.
//!
//! Checked after every input: I1 to I7, I10, I11, I13 and I14, plus L2 (a refusal comes
//! at the first tick at or after the end of its unservable wait, never later). With
//! I3 it checks that a lease is given up exactly when the scheduler's contract says:
//! at the first tick at which its worker is not live, or on a heartbeat that leaves it
//! out once its `Start` went to an earlier session or has been out for `START_GRACE`.
//! The world checks L1 at the end of a run ([`Checker::quiescent`]) and I15 by
//! replaying a seed. I8 and the cordoned verdict are checked whenever cordons are fed,
//! but F1 feeds none, so F1 does not exercise them. Not checked here: I9 (drain states)
//! and I12 (no daemon model in the in-process world).
//!
//! I6 is checked per axis the request uses: a grant is allowed when, on every axis on
//! which the request is not zero, what is booked plus the request fits the capacity.
//! A capacity that shrank below the bookings on one axis does not keep a request that
//! books nothing on that axis away (a CPU-only action may go to a node whose GPUs
//! shrank below its GPU bookings: it makes nothing worse).
//!
//! This file is local to the F1 family for now; it is written to move unchanged to the
//! shared `tests/sim/check.rs` the catalog plans once the other families land theirs.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use kbf_caps::NodeCaps;
use kbf_sched::fence::{LEASE_GRACE, START_GRACE};
use kbf_sched::{Event, Input, OpState, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Effect, LeaseId, OperationId, Outcome, Qos, Resources, WaiterId,
    WorkerId,
};

mod round;

/// The name of this test target, for the replay command.
pub const TARGET: &str = "sim_f1_capacity";

/// A request vector as three plain numbers: millicores, bytes, GPUs.
type Axes = [u64; 3];

/// Where an operation stands in the queue the shadow keeps: more urgent first, then
/// (for custom levels of equal urgency) by name, then oldest first. Written from
/// `Qos::urgency` and the level's name, not from `Qos`'s own `Ord`, so that a change to
/// that ordering (which the scheduler's queue uses) shows up as I14 and I11, not as
/// the same mistake on both sides.
type QueueKey = (Reverse<u16>, Reverse<String>, OperationId);

fn queue_key(qos: &Qos, id: OperationId) -> QueueKey {
    (Reverse(qos.urgency()), Reverse(qos.name().to_owned()), id)
}

/// Whether `a` is more urgent than `b`, by the same rule as [`queue_key`].
fn more_urgent(a: &Qos, b: &Qos) -> bool {
    (a.urgency(), a.name()) > (b.urgency(), b.name())
}

fn axes(r: Resources) -> Axes {
    [r.cpu_millis, r.memory_bytes, r.gpus]
}

fn add(a: Axes, b: Axes) -> Axes {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: Axes, b: Axes) -> Axes {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// Whether `request` may be booked on a worker of `capacity` that has `booked`: on
/// every axis the request uses, the sum fits.
fn fits_beside(capacity: Axes, booked: Axes, request: Axes) -> bool {
    (0..3).all(|i| request[i] == 0 || booked[i] + request[i] <= capacity[i])
}

/// Whether `request` fits in a whole `capacity`.
fn fits_whole(capacity: Axes, request: Axes) -> bool {
    (0..3).all(|i| request[i] <= capacity[i])
}

#[derive(Clone, Debug)]
struct ShadowWorker {
    capacity: Axes,
    caps: NodeCaps,
    last_heard: u64,
    session: u64,
    booked: Axes,
    /// Per interned platform request: whether `caps` satisfy it.
    matches: BTreeMap<usize, bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Queued,
    Leased {
        lease: LeaseId,
        worker: WorkerId,
        committed: bool,
    },
    Running {
        lease: LeaseId,
        worker: WorkerId,
    },
    Completed(LeaseId),
    Failed(LeaseId),
    Refused,
}

impl State {
    fn holding(&self) -> Option<(LeaseId, &WorkerId)> {
        match self {
            Self::Leased { lease, worker, .. } | Self::Running { lease, worker } => {
                Some((*lease, worker))
            }
            _ => None,
        }
    }

    fn done(&self) -> bool {
        matches!(self, Self::Completed(_) | Self::Failed(_) | Self::Refused)
    }

    fn same_as(&self, real: &OpState) -> bool {
        match (self, real) {
            (Self::Queued, OpState::Queued) | (Self::Refused, OpState::Refused { .. }) => true,
            (
                Self::Leased {
                    lease,
                    worker,
                    committed,
                },
                OpState::Leased {
                    lease: l,
                    worker: w,
                    committed: c,
                },
            ) => lease == l && worker == w && committed == c,
            (
                Self::Running { lease, worker },
                OpState::Running {
                    lease: l,
                    worker: w,
                },
            ) => lease == l && worker == w,
            (Self::Completed(lease), OpState::Completed { lease: l, .. })
            | (Self::Failed(lease), OpState::Failed { lease: l, .. }) => lease == l,
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
struct ShadowOp {
    key: ActionKey,
    qos: Qos,
    resources: Axes,
    need: usize,
    waiters: Vec<WaiterId>,
    state: State,
    newest_grant: Option<LeaseId>,
    result_proposed: bool,
    /// The last reason its callers were told, while it waits.
    told: Option<String>,
    /// Since when (ms) its current run of refusable unservable rounds began.
    since: Option<u64>,
    /// Its refusal was proposed: it left the queue and waits for the commit.
    refusing: bool,
}

#[derive(Clone, Copy, Debug)]
struct Held {
    op: OperationId,
    /// When and to which session the `Start` went.
    start: Option<(u64, u64)>,
}

/// Counts of the situations a sweep means to reach, so a check that cannot fail is
/// caught: each scenario asserts the ones it is about were reached.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Placement rounds.
    pub rounds: u64,
    /// Rounds that granted exactly [`round::ROUND`] leases.
    pub full_rounds: u64,
    /// Rounds that hit the limit with work still placeable left over.
    pub cut_rounds: u64,
    /// Operations behind a full round's cut that no live worker could run: their
    /// verdict is still checked (I10, L2).
    pub verdicts_past_cut: u64,
    /// Grants.
    pub grants: u64,
    /// Grants that left their worker with nothing free on some axis the request used.
    pub exact_fills: u64,
    /// Submissions that joined a twin.
    pub joins: u64,
    /// Joins that raised the QoS of a queued twin.
    pub promotions: u64,
    /// Joinable submissions whose key matched a finished operation: queued anew.
    pub rejoins_after_finish: u64,
    /// Non-joinable submissions with an unfinished twin of the same key.
    pub unjoinable_twins: u64,
    /// Operations answered with a result.
    pub answers: u64,
    /// Operations refused.
    pub refusals: u64,
    /// Leases given up (requeued) by expiry or reconciliation.
    pub given_up: u64,
    /// Rounds in which a queued request was kept off a worker only because its
    /// bookings exceed a shrunken capacity.
    pub overbooked_skips: u64,
}

/// What [`Checker::observe`] keeps between inputs.
#[derive(Debug)]
pub struct Checker {
    scenario: String,
    seed: u64,
    step: u64,
    now: u64,
    wait: u64,
    grace: u64,
    workers: BTreeMap<WorkerId, ShadowWorker>,
    cordoned: BTreeSet<WorkerId>,
    needs: Vec<kbf_caps::Request>,
    ops: BTreeMap<OperationId, ShadowOp>,
    next_op: u64,
    queue: BTreeSet<QueueKey>,
    in_flight: BTreeMap<ActionKey, OperationId>,
    finished_keys: BTreeSet<ActionKey>,
    held: BTreeMap<LeaseId, Held>,
    last_lease: Option<LeaseId>,
    reported: BTreeMap<LeaseId, Outcome>,
    waiter_op: BTreeMap<WaiterId, OperationId>,
    answered: BTreeMap<WaiterId, u32>,
    touched_ops: BTreeSet<OperationId>,
    touched_workers: BTreeSet<WorkerId>,
    /// The grants of the last round, in order.
    pub last_round: Vec<(OperationId, WorkerId)>,
    /// Per operation: the farm time (ms) of its first grant.
    pub granted_at: BTreeMap<OperationId, u64>,
    /// Per operation: the farm time (ms) it was finished.
    pub finished_at: BTreeMap<OperationId, u64>,
    /// Per operation: the reason it was refused with.
    pub refused_reason: BTreeMap<OperationId, String>,
    /// Per waiter: the operation that answered it and how.
    pub outcome_of: BTreeMap<WaiterId, (OperationId, Option<Outcome>)>,
    /// The situations reached.
    pub stats: Stats,
}

impl Checker {
    /// A checker for `scenario` under `seed`, for a scheduler refusing after `wait_ms`.
    pub fn new(scenario: &str, seed: u64, wait_ms: u64) -> Self {
        Self {
            scenario: scenario.to_owned(),
            seed,
            step: 0,
            now: 0,
            wait: wait_ms,
            grace: u64::try_from(LEASE_GRACE.as_millis()).unwrap(),
            workers: BTreeMap::new(),
            cordoned: BTreeSet::new(),
            needs: Vec::new(),
            ops: BTreeMap::new(),
            next_op: 0,
            queue: BTreeSet::new(),
            in_flight: BTreeMap::new(),
            finished_keys: BTreeSet::new(),
            held: BTreeMap::new(),
            last_lease: None,
            reported: BTreeMap::new(),
            waiter_op: BTreeMap::new(),
            answered: BTreeMap::new(),
            touched_ops: BTreeSet::new(),
            touched_workers: BTreeSet::new(),
            last_round: Vec::new(),
            granted_at: BTreeMap::new(),
            finished_at: BTreeMap::new(),
            refused_reason: BTreeMap::new(),
            outcome_of: BTreeMap::new(),
            stats: Stats::default(),
        }
    }

    /// The command that replays this run.
    pub fn replay_command(&self) -> String {
        format!(
            "KBF_SIM_SCENARIO={} KBF_SIM_SEED={} cargo test -p kbf-sched --test {TARGET} -- \
             --ignored --exact replay",
            self.scenario, self.seed
        )
    }

    /// Fails the run: names the scenario, seed, step, invariant and replay command.
    #[track_caller]
    pub fn fail(&self, invariant: &str, what: &str) -> ! {
        panic!(
            "{} seed {} step {} at {} ms: {invariant} violated: {what}\n  replay: {}",
            self.scenario,
            self.seed,
            self.step,
            self.now,
            self.replay_command()
        )
    }

    #[track_caller]
    fn ensure(&self, ok: bool, invariant: &str, what: impl FnOnce() -> String) {
        if !ok {
            self.fail(invariant, &what());
        }
    }

    fn live(&self, worker: &WorkerId) -> bool {
        self.workers
            .get(worker)
            .is_some_and(|w| self.now < w.last_heard + self.grace)
    }

    fn intern(&mut self, needs: &kbf_caps::Request) -> usize {
        if let Some(i) = self.needs.iter().position(|n| n == needs) {
            return i;
        }
        self.needs.push(needs.clone());
        self.needs.len() - 1
    }

    fn matches(&mut self, need: usize, worker: &WorkerId) -> bool {
        let needs = &self.needs[need];
        let w = self.workers.get_mut(worker).expect("registered");
        *w.matches
            .entry(need)
            .or_insert_with(|| needs.matches(&w.caps))
    }

    /// Whether the operation is in the queue the shadow keeps.
    fn queued(op: &ShadowOp) -> bool {
        op.state == State::Queued && !op.refusing
    }

    fn enqueue(&mut self, id: OperationId) {
        let op = self.ops.get_mut(&id).expect("exists");
        op.state = State::Queued;
        op.result_proposed = false;
        self.queue.insert(queue_key(&op.qos, id));
    }

    /// Drops `id`'s holding: its lease and its booking.
    fn release(&mut self, id: OperationId) {
        let op = self.ops.get_mut(&id).expect("exists");
        if let Some((lease, worker)) = op.state.holding() {
            let worker = worker.clone();
            let res = op.resources;
            let w = self.workers.get_mut(&worker).expect("registered");
            w.booked = sub(w.booked, res);
            self.held.remove(&lease);
            self.touched_workers.insert(worker);
        }
        op.result_proposed = false;
    }

    fn finish(&mut self, id: OperationId, state: State) {
        self.release(id);
        let op = self.ops.get_mut(&id).expect("exists");
        self.queue.remove(&queue_key(&op.qos, id));
        op.state = state;
        op.told = None;
        op.since = None;
        op.refusing = false;
        if self.in_flight.get(&op.key) == Some(&id) {
            self.in_flight.remove(&op.key);
            self.finished_keys.insert(op.key.clone());
        }
        self.finished_at.insert(id, self.now);
    }

    /// Checks one input the scheduler was fed and the effects it returned, then the
    /// scheduler's public state against the shadow.
    pub fn observe(&mut self, input: &Input, effects: &[Effect], sched: &Scheduler) {
        self.step += 1;
        self.now = self.now.max(input.now.as_millis());
        self.touched_ops.clear();
        self.touched_workers.clear();
        match &input.event {
            Event::WorkerUp {
                worker,
                capacity,
                caps,
                ..
            } => {
                self.none(effects, "WorkerUp");
                let now = self.now;
                let entry = self
                    .workers
                    .entry(worker.clone())
                    .and_modify(|w| w.session += 1)
                    .or_insert_with(|| ShadowWorker {
                        capacity: [0; 3],
                        caps: caps.clone(),
                        last_heard: now,
                        session: 0,
                        booked: [0; 3],
                        matches: BTreeMap::new(),
                    });
                entry.capacity = axes(*capacity);
                entry.caps = caps.clone();
                entry.last_heard = now;
                entry.matches.clear();
                self.touched_workers.insert(worker.clone());
            }
            Event::Capacity {
                worker,
                capacity,
                caps,
            } => {
                self.none(effects, "Capacity");
                let now = self.now;
                if let Some(w) = self.workers.get_mut(worker) {
                    w.capacity = axes(*capacity);
                    w.caps = caps.clone();
                    w.last_heard = w.last_heard.max(now);
                    w.matches.clear();
                    self.touched_workers.insert(worker.clone());
                }
            }
            Event::Heartbeat { worker, running } => {
                self.none(effects, "Heartbeat");
                self.heartbeat(worker, running, sched);
            }
            Event::Submit { waiter, request } => self.submit(*waiter, request, effects, sched),
            Event::Committed(ControlRecord::Lease(grant)) => {
                self.lease_committed(grant.lease, grant.operation, effects);
            }
            Event::Committed(ControlRecord::Result(record)) => {
                self.result_committed(record.lease, record.operation, record.outcome, effects);
            }
            Event::Committed(ControlRecord::Refusal(record)) => {
                self.refusal_committed(record.operation, &record.reason, effects);
            }
            Event::Committed(_) => self.none(effects, "another core's record"),
            Event::Started { operation, lease } => {
                self.none(effects, "Started");
                if let Some(op) = self.ops.get_mut(operation)
                    && let State::Leased {
                        lease: l,
                        worker,
                        committed: true,
                    } = &op.state
                    && l == lease
                {
                    op.state = State::Running {
                        lease: *lease,
                        worker: worker.clone(),
                    };
                    self.touched_ops.insert(*operation);
                }
            }
            Event::Report {
                operation,
                lease,
                outcome,
            } => self.report(*operation, *lease, *outcome, effects),
            Event::Tick => {
                self.expire(sched);
                self.round(effects);
            }
            Event::Cordon { worker } | Event::Drain { worker, .. } => {
                self.none(effects, "Cordon");
                self.cordoned.insert(worker.clone());
            }
            Event::Uncordon { worker } => {
                self.cordoned.remove(worker);
                self.round(effects);
            }
        }
        self.compare(sched);
    }

    #[track_caller]
    fn none(&self, effects: &[Effect], what: &str) {
        self.ensure(effects.is_empty(), "contract", || {
            format!("{what} must emit nothing, emitted {effects:?}")
        });
    }

    /// A heartbeat: a listed lease on this worker is never given up; one left out is
    /// given up when, and only when, its `Start` went to an earlier session or has
    /// been out for `START_GRACE`, and no result was proposed for it.
    fn heartbeat(&mut self, worker: &WorkerId, running: &[LeaseId], sched: &Scheduler) {
        let now = self.now;
        let Some(w) = self.workers.get_mut(worker) else {
            return;
        };
        w.last_heard = w.last_heard.max(now);
        let session = w.session;
        let start_grace = u64::try_from(START_GRACE.as_millis()).unwrap();
        let mine: Vec<(LeaseId, Held)> = self
            .held
            .iter()
            .filter(|(_, h)| {
                self.ops[&h.op]
                    .state
                    .holding()
                    .is_some_and(|(_, w)| w == worker)
            })
            .map(|(l, h)| (*l, *h))
            .collect();
        for (lease, held) in mine {
            let op = &self.ops[&held.op];
            let due = held
                .start
                .is_some_and(|(at, s)| s < session || now >= at + start_grace)
                && !running.contains(&lease)
                && !op.result_proposed;
            let gone = sched
                .state(held.op)
                .is_none_or(|s| !matches!(s, OpState::Leased { lease: l, .. } | OpState::Running { lease: l, .. } if *l == lease));
            self.ensure(due == gone, "I3/reconcile", || {
                format!(
                    "{lease} of {} on {worker}: requeue due {due}, given up {gone} (listed {})",
                    held.op,
                    running.contains(&lease)
                )
            });
            if gone {
                self.give_up(held.op);
            }
        }
    }

    fn give_up(&mut self, id: OperationId) {
        self.release(id);
        self.enqueue(id);
        self.touched_ops.insert(id);
        self.stats.given_up += 1;
    }

    /// The tick's expiry: exactly the leases on workers not heard from within G go
    /// back to the queue.
    fn expire(&mut self, sched: &Scheduler) {
        let held: Vec<(LeaseId, OperationId)> = self.held.iter().map(|(l, h)| (*l, h.op)).collect();
        for (lease, id) in held {
            let worker = self.ops[&id].state.holding().expect("held").1.clone();
            let expect_gone = !self.live(&worker);
            let gone = sched
                .state(id)
                .is_none_or(|s| !matches!(s, OpState::Leased { lease: l, .. } | OpState::Running { lease: l, .. } if *l == lease));
            // The scheduler requeues inside the tick and may grant again in the same
            // round, so a lease of an expired worker can be gone while the operation
            // holds a newer one. Compare by lease, not by state.
            self.ensure(expect_gone == gone, "I3/expiry", || {
                format!("{lease} of {id} on {worker}: expired {expect_gone}, given up {gone}")
            });
            if expect_gone {
                self.give_up(id);
            }
        }
    }

    /// A submission: I13 (joins) and I14 (where it is queued).
    fn submit(
        &mut self,
        waiter: WaiterId,
        request: &Request,
        effects: &[Effect],
        sched: &Scheduler,
    ) {
        self.ensure(!self.waiter_op.contains_key(&waiter), "harness", || {
            format!("waiter {waiter:?} submitted twice")
        });
        let joinable = request.hermetic && !request.do_not_cache;
        let twin = self.in_flight.get(&request.key).copied();
        if !joinable && twin.is_some() {
            self.stats.unjoinable_twins += 1;
        }
        if let (true, Some(id)) = (joinable, twin) {
            self.stats.joins += 1;
            let op = self.ops.get_mut(&id).expect("in flight");
            op.waiters.push(waiter);
            if more_urgent(&request.qos, &op.qos) {
                if Self::queued(op) {
                    self.queue.remove(&queue_key(&op.qos, id));
                    self.queue.insert(queue_key(&request.qos, id));
                    self.stats.promotions += 1;
                }
                op.qos = request.qos.clone();
            }
            let expect: Vec<Effect> = op
                .told
                .iter()
                .map(|r| {
                    Effect::Waiting(kbf_types::Waiting {
                        operation: id,
                        reason: Some(r.clone()),
                    })
                })
                .collect();
            let qos = op.qos.clone();
            self.ensure(effects == expect.as_slice(), "I10", || {
                format!("a join onto {id} emitted {effects:?}, want {expect:?}")
            });
            self.ensure(
                sched.waiters(id).and_then(<[WaiterId]>::last) == Some(&waiter),
                "I13",
                || {
                    format!(
                        "{waiter:?} should have joined {id}, its waiters are {:?}",
                        sched.waiters(id)
                    )
                },
            );
            self.ensure(sched.qos(id) == Some(&qos), "I13", || {
                format!("{id} after a join is at {:?}, want {qos}", sched.qos(id))
            });
            self.ensure(
                sched.state(OperationId(self.next_op)).is_none(),
                "I13",
                || format!("{waiter:?} joined {id} but a new operation was queued too"),
            );
            self.waiter_op.insert(waiter, id);
            self.touched_ops.insert(id);
            return;
        }
        self.none(effects, "Submit");
        let id = OperationId(self.next_op);
        self.next_op += 1;
        if joinable && self.finished_keys.contains(&request.key) {
            self.stats.rejoins_after_finish += 1;
        }
        let ok = sched.waiters(id) == Some(&[waiter][..]);
        self.ensure(ok, "I13", || {
            let joined = self
                .ops
                .keys()
                .find(|o| sched.waiters(**o).is_some_and(|w| w.contains(&waiter)));
            format!(
                "{waiter:?} ({}) should queue a new {id}; its waiters are {:?}, joined {joined:?}",
                if joinable { "joinable" } else { "not joinable" },
                sched.waiters(id)
            )
        });
        let need = self.intern(&request.needs);
        if joinable {
            self.in_flight.insert(request.key.clone(), id);
        }
        self.ops.insert(
            id,
            ShadowOp {
                key: request.key.clone(),
                qos: request.qos.clone(),
                resources: axes(request.resources),
                need,
                waiters: vec![waiter],
                state: State::Queued,
                newest_grant: None,
                result_proposed: false,
                told: None,
                since: None,
                refusing: false,
            },
        );
        self.queue.insert(queue_key(&request.qos, id));
        self.waiter_op.insert(waiter, id);
        self.touched_ops.insert(id);
    }

    /// A committed grant: I2 (its `Start`, and only for the current holding).
    fn lease_committed(&mut self, lease: LeaseId, id: OperationId, effects: &[Effect]) {
        let now = self.now;
        let Some(op) = self.ops.get_mut(&id) else {
            return self.none(effects, "a grant of an unknown operation");
        };
        if op.state.done() {
            return self.none(effects, "a grant of a finished operation");
        }
        op.newest_grant = op.newest_grant.max(Some(lease));
        self.touched_ops.insert(id);
        let State::Leased {
            lease: l,
            worker,
            committed: false,
        } = &op.state
        else {
            return self.none(effects, "a grant that is not the current holding");
        };
        if *l != lease {
            return self.none(effects, "a superseded grant");
        }
        let worker = worker.clone();
        let ok = matches!(effects, [Effect::Start(s)]
            if s.lease == lease && s.operation == id && s.worker == worker
                && s.key == op.key && axes(s.resources) == op.resources);
        self.ensure(ok, "I2", || {
            format!("the commit of {lease} for {id} on {worker} emitted {effects:?}")
        });
        let op = self.ops.get_mut(&id).expect("checked above");
        op.state = State::Leased {
            lease,
            worker: worker.clone(),
            committed: true,
        };
        let session = self.workers[&worker].session;
        self.held.get_mut(&lease).expect("held").start = Some((now, session));
    }

    /// A report: its result is proposed once per holding, from the holding only.
    fn report(&mut self, id: OperationId, lease: LeaseId, outcome: Outcome, effects: &[Effect]) {
        self.reported.entry(lease).or_insert(outcome);
        let Some(op) = self.ops.get_mut(&id) else {
            return self.none(effects, "a report of an unknown operation");
        };
        let current = match &op.state {
            State::Leased {
                lease, committed, ..
            } => committed.then_some(*lease),
            State::Running { lease, .. } => Some(*lease),
            _ => None,
        };
        if current != Some(lease) || op.result_proposed {
            return self.none(effects, "a stale or repeated report");
        }
        op.result_proposed = true;
        let ok = matches!(effects, [Effect::Commit(ControlRecord::Result(r))]
            if r.lease == lease && r.operation == id && r.outcome == outcome);
        self.ensure(ok, "I5", || {
            format!("the report of {lease} for {id} emitted {effects:?}")
        });
    }

    /// A committed result: I4 and I5.
    fn result_committed(
        &mut self,
        lease: LeaseId,
        id: OperationId,
        outcome: Outcome,
        effects: &[Effect],
    ) {
        let Some(op) = self.ops.get(&id) else {
            return self.none(effects, "a result of an unknown operation");
        };
        if op.state.done() || op.newest_grant != Some(lease) {
            return self.none(effects, "a result of a finished operation or an old grant");
        }
        let ok = matches!(effects, [Effect::Answer(a)]
            if a.operation == id && a.lease == lease && a.outcome == outcome
                && a.waiters == op.waiters);
        self.ensure(ok, "I4/I5", || {
            format!(
                "the result of {lease} for {id} emitted {effects:?}, waiters {:?}",
                op.waiters
            )
        });
        self.ensure(self.reported.get(&lease) == Some(&outcome), "I5", || {
            format!("{id} answered with {outcome:?}, which {lease} never reported")
        });
        let waiters = op.waiters.clone();
        self.answer(id, &waiters, Some(outcome));
        let state = match outcome {
            Outcome::Completed { .. } => State::Completed(lease),
            Outcome::Failed(_) => State::Failed(lease),
        };
        self.finish(id, state);
        self.touched_ops.insert(id);
        self.stats.answers += 1;
    }

    /// A committed refusal: I4 and I10.
    fn refusal_committed(&mut self, id: OperationId, reason: &str, effects: &[Effect]) {
        let Some(op) = self.ops.get(&id) else {
            return self.none(effects, "a refusal of an unknown operation");
        };
        if op.state != State::Queued {
            return self.none(effects, "a stale refusal");
        }
        self.ensure(op.refusing, "I10", || {
            format!("{id} refused without a proposal")
        });
        let ok = matches!(effects, [Effect::Refuse(r)]
            if r.operation == id && r.reason == reason && r.waiters == op.waiters);
        self.ensure(ok, "I4/I10", || {
            format!(
                "the refusal of {id} emitted {effects:?}, waiters {:?}",
                op.waiters
            )
        });
        let waiters = op.waiters.clone();
        self.answer(id, &waiters, None);
        self.refused_reason.insert(id, reason.to_owned());
        self.finish(id, State::Refused);
        self.touched_ops.insert(id);
        self.stats.refusals += 1;
    }

    fn answer(&mut self, id: OperationId, waiters: &[WaiterId], outcome: Option<Outcome>) {
        for w in waiters {
            let n = self.answered.entry(*w).or_default();
            *n += 1;
            let n = *n;
            self.ensure(n == 1, "I4", || format!("waiter {w:?} answered {n} times"));
            self.ensure(self.waiter_op.get(w) == Some(&id), "I4", || {
                format!(
                    "waiter {w:?} of {:?} answered by {id}",
                    self.waiter_op.get(w)
                )
            });
            self.outcome_of.insert(*w, (id, outcome));
        }
    }

    /// The scheduler's public state against the shadow: I3 and I6 (bookings) for what
    /// this input touched, and I14 (the queue) always.
    fn compare(&mut self, sched: &Scheduler) {
        for id in std::mem::take(&mut self.touched_ops) {
            let shadow = &self.ops[&id].state;
            let real = sched.state(id);
            self.ensure(real.is_some_and(|r| shadow.same_as(r)), "I3", || {
                format!("{id} is {real:?}, the shadow says {shadow:?}")
            });
        }
        for name in std::mem::take(&mut self.touched_workers) {
            let w = &self.workers[&name];
            let real = sched.booked(&name).map(axes);
            self.ensure(real == Some(w.booked), "I6", || {
                format!("{name} has {real:?} booked, its leases book {:?}", w.booked)
            });
            let shadow: Vec<LeaseId> = self
                .held
                .iter()
                .filter(|(_, h)| {
                    self.ops[&h.op]
                        .state
                        .holding()
                        .is_some_and(|(_, w)| *w == name)
                })
                .map(|(l, _)| *l)
                .collect();
            let real = sched.leases_on(&name);
            self.ensure(real == shadow, "I3", || {
                format!("{name} holds {real:?}, the shadow says {shadow:?}")
            });
        }
        let same = sched.queued().eq(self.queue.iter().map(|(_, _, id)| *id));
        self.ensure(same, "I14", || {
            let real: Vec<OperationId> = sched.queued().take(12).collect();
            let want: Vec<OperationId> = self.queue.iter().map(|(_, _, id)| *id).take(12).collect();
            format!(
                "the queue starts {real:?} ({} queued), the queued operations in (urgency, \
                 submission) order start {want:?} ({})",
                sched.queued().count(),
                self.queue.len()
            )
        });
    }

    /// L1 at the end of a run: every operation finished, every waiter answered exactly
    /// once, nothing held or booked, the queue empty.
    pub fn quiescent(&self, sched: &Scheduler) {
        if let Some((id, op)) = self.ops.iter().find(|(_, op)| !op.state.done()) {
            self.fail("L1", &format!("{id} is still {:?} at the end", op.state));
        }
        if let Some(w) = self
            .waiter_op
            .keys()
            .find(|w| self.answered.get(w) != Some(&1))
        {
            self.fail(
                "L1",
                &format!("waiter {w:?} answered {:?} times", self.answered.get(w)),
            );
        }
        self.ensure(sched.queued().next().is_none(), "L1", || {
            "the queue is not empty at the end".to_owned()
        });
        for name in self.workers.keys() {
            let booked = sched.booked(name).map(axes);
            self.ensure(booked == Some([0; 3]), "L1", || {
                format!("{name} still has {booked:?} booked at the end")
            });
            self.ensure(sched.leases_on(name).is_empty(), "L1", || {
                format!("{name} still holds leases at the end")
            });
        }
    }

    /// Whether every operation is finished.
    pub fn all_done(&self) -> bool {
        self.ops.values().all(|op| op.state.done())
    }

    /// Operations submitted so far.
    pub fn op_count(&self) -> u64 {
        self.next_op
    }

    /// Whether `id` is queued (in the shadow's queue).
    pub fn is_queued(&self, id: OperationId) -> bool {
        self.ops.get(&id).is_some_and(Self::queued)
    }

    /// The operation `waiter` is attached to.
    pub fn op_of(&self, waiter: WaiterId) -> Option<OperationId> {
        self.waiter_op.get(&waiter).copied()
    }

    /// `id`'s QoS level now.
    pub fn qos_of(&self, id: OperationId) -> &Qos {
        &self.ops[&id].qos
    }

    /// What is booked on `worker` and its capacity, as (millicores, bytes, GPUs).
    pub fn room(&self, worker: &WorkerId) -> ([u64; 3], [u64; 3]) {
        let w = &self.workers[worker];
        (w.booked, w.capacity)
    }

    /// The queued operations, most urgent first.
    pub fn queue(&self) -> impl Iterator<Item = OperationId> + '_ {
        self.queue.iter().map(|(_, _, id)| *id)
    }

    /// `id`'s request vector, as (millicores, bytes, GPUs).
    pub fn resources_of(&self, id: OperationId) -> [u64; 3] {
        self.ops[&id].resources
    }
}
