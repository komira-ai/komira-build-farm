//! The scheduler state machine.

use std::cmp::Reverse;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use kbf_types::{
    ActionKey, Answer, ControlRecord, Digest, Effect, Failure, FarmTime, LeaseGrant, LeaseId,
    OperationId, Outcome, Qos, Resources, ResultRecord, StartLease, StateMachine, WaiterId,
    WorkerId,
};

use crate::fence::{LEASE_GRACE, START_GRACE};
use crate::input::{Event, Input, Request};

/// At most this many leases are granted per [`Event::Tick`] (one log flush per round).
pub const PLACEMENT_ROUND: usize = 256;

/// How many leases of one operation may be lost before it fails. RFC section 5.8: an
/// `INFRA` failure is retried elsewhere, up to 3 times, then answered `INTERNAL`; a
/// lease lost to a reboot ends `INFRA`. Each lease lost after its `Start` went out is
/// one attempt.
pub const INFRA_ATTEMPTS: usize = 3;

/// Where an operation is.
///
/// `Queued -> Leased -> Running -> Completed | Failed`. A lost lease (its worker went
/// silent for G, or its heartbeats leave the lease out) sends its operation back to
/// `Queued`, to be granted again under a new lease. The lease that spends the last of
/// [`INFRA_ATTEMPTS`] sends it to `Failing` instead, and from there to `Failed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpState {
    /// Waiting for room.
    Queued,
    /// Granted to `worker` under `lease`. Until `committed`, the grant is only proposed
    /// and no `Start` has been sent.
    Leased {
        /// The lease.
        lease: LeaseId,
        /// Where it runs.
        worker: WorkerId,
        /// Whether the grant is committed (and its `Start` emitted).
        committed: bool,
    },
    /// The worker holding `lease` has started it.
    Running {
        /// The lease.
        lease: LeaseId,
        /// Where it runs.
        worker: WorkerId,
    },
    /// Finished: the result of `lease` was committed.
    Completed {
        /// The lease whose result was accepted.
        lease: LeaseId,
        /// The digest of its `ActionResult`.
        action_result: Digest,
    },
    /// Out of infra attempts: `lease`, its last lease, was lost and an `INFRA` failure
    /// from it is proposed. It is neither queued nor held; the commit of that failure
    /// makes it `Failed` and answers its waiters.
    Failing {
        /// The lost lease the failure is proposed from.
        lease: LeaseId,
    },
    /// Failed: the failure of `lease` was committed.
    Failed {
        /// The lease whose outcome was accepted.
        lease: LeaseId,
        /// Why.
        failure: Failure,
    },
}

impl OpState {
    fn holding(&self) -> Option<(LeaseId, &WorkerId)> {
        match self {
            Self::Leased { lease, worker, .. } | Self::Running { lease, worker } => {
                Some((*lease, worker))
            }
            _ => None,
        }
    }

    /// Whether the operation is finished.
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. })
    }
}

#[derive(Clone, Debug)]
struct Operation {
    request: Request,
    waiters: Vec<WaiterId>,
    state: OpState,
    /// The lease of the newest committed grant, in log order. A committed result is
    /// accepted only from this lease.
    committed_lease: Option<LeaseId>,
    /// A result from the current holding has been proposed; later reports for it are
    /// duplicates.
    result_proposed: bool,
    /// The worker of each lease of this operation lost after its `Start` went out, in
    /// order: one `INFRA` attempt each.
    lost_on: Vec<WorkerId>,
}

/// A lease currently held (leased or running).
#[derive(Clone, Copy, Debug)]
struct Held {
    operation: OperationId,
    /// When and to which session its `Start` was emitted; `None` until its grant is
    /// committed.
    start_sent: Option<StartSent>,
}

/// When a `Start` was emitted, and the worker session it was sent to.
#[derive(Clone, Copy, Debug)]
struct StartSent {
    at: FarmTime,
    session: u64,
}

/// A lease given up after its `Start` went out: its worker may run it all the same.
#[derive(Clone, Debug)]
struct GivenUp {
    worker: WorkerId,
    resources: Resources,
}

#[derive(Clone, Debug)]
struct Worker {
    capacity: Resources,
    /// Held leases, and the given-up leases in `late`.
    booked: Resources,
    /// Given-up leases the worker's newest heartbeat lists as running, and what each
    /// books.
    late: BTreeMap<LeaseId, Resources>,
    last_heard: FarmTime,
    /// Counts the worker's registrations: the session a `Start` emitted now goes to.
    session: u64,
}

impl Worker {
    fn free(&self) -> Resources {
        self.capacity.saturating_sub(self.booked)
    }

    fn alive(&self, now: FarmTime) -> bool {
        now < self.last_heard.saturating_add(LEASE_GRACE)
    }
}

/// The scheduler core of one leader term.
///
/// It owns the queue, the operations, the workers' bookings and the leases, and decides
/// placements. It reads no clock: farm time arrives in every [`Input`]. Leases are
/// numbered `(term, seq)` from the term it was built with.
///
/// Two rules carry its safety, and each holds whatever order inputs arrive in:
///
/// - **Commit before Start.** Placing an operation emits only [`Effect::Commit`] of
///   the grant. The [`Effect::Start`] is emitted when that record comes back as
///   [`Event::Committed`], and never for a grant that is no longer current by then.
/// - **One accepted result per operation.** A report is proposed only from the lease
///   the operation currently holds under a committed grant, at most once per holding.
///   A committed result is accepted only if its lease is the operation's newest
///   committed grant (log order decides) and the operation is not finished. Results
///   from an expired lease, duplicates and late arrivals are dropped.
///
/// A lease is given up, and its operation requeued, when its worker is silent for G,
/// and when a worker it still hears from does not list it as running (see
/// [`Event::WorkerUp`] and [`Event::Heartbeat`]). A given-up lease can no longer have a
/// result proposed, and once the operation is granted again its result loses to the
/// new grant in the log.
///
/// A lease given up after its `Start` went out is one `INFRA` attempt (RFC section
/// 5.8). The retry goes to a worker that has not lost the operation yet when one has
/// room, else to the first fit. The lease that spends the last of [`INFRA_ATTEMPTS`]
/// is not requeued: an `INFRA` failure from it is proposed, under the same rule as a
/// worker's report, and its commit answers the waiters. While its worker still lists a
/// given-up lease as running, that lease stays booked on the worker.
#[derive(Clone, Debug)]
pub struct Scheduler {
    term: u64,
    next_seq: u64,
    next_op: u64,
    now: FarmTime,
    ops: BTreeMap<OperationId, Operation>,
    /// Queued operations, most urgent first, then oldest first.
    queue: BTreeSet<(Reverse<Qos>, OperationId)>,
    /// Joinable operations not yet finished, by dedup key.
    in_flight: BTreeMap<ActionKey, OperationId>,
    workers: BTreeMap<WorkerId, Worker>,
    /// Every lease currently held (leased or running).
    held: BTreeMap<LeaseId, Held>,
    /// Every lease given up after its `Start` went out. Kept for the scheduler's life,
    /// as operations are: at most [`INFRA_ATTEMPTS`] per operation.
    given_up: BTreeMap<LeaseId, GivenUp>,
}

impl Scheduler {
    /// An empty scheduler for the leader of `term`.
    #[must_use]
    pub fn new(term: u64) -> Self {
        Self {
            term,
            next_seq: 0,
            next_op: 0,
            now: FarmTime::default(),
            ops: BTreeMap::new(),
            queue: BTreeSet::new(),
            in_flight: BTreeMap::new(),
            workers: BTreeMap::new(),
            held: BTreeMap::new(),
            given_up: BTreeMap::new(),
        }
    }

    /// Where `operation` is, if it exists.
    #[must_use]
    pub fn state(&self, operation: OperationId) -> Option<&OpState> {
        self.ops.get(&operation).map(|op| &op.state)
    }

    /// The waiters attached to `operation`, in attach order.
    #[must_use]
    pub fn waiters(&self, operation: OperationId) -> Option<&[WaiterId]> {
        self.ops.get(&operation).map(|op| op.waiters.as_slice())
    }

    /// The QoS level `operation` is queued at (raised when a more urgent caller joins).
    #[must_use]
    pub fn qos(&self, operation: OperationId) -> Option<&Qos> {
        self.ops.get(&operation).map(|op| &op.request.qos)
    }

    /// Queued operations in the order placement will consider them.
    pub fn queued(&self) -> impl Iterator<Item = OperationId> + '_ {
        self.queue.iter().map(|(_, id)| *id)
    }

    /// What is booked on `worker`, if it is registered.
    #[must_use]
    pub fn booked(&self, worker: &WorkerId) -> Option<Resources> {
        self.workers.get(worker).map(|w| w.booked)
    }

    fn submit(&mut self, waiter: WaiterId, request: Request) {
        if request.joinable()
            && let Some(&id) = self.in_flight.get(&request.key)
        {
            let op = self
                .ops
                .get_mut(&id)
                .expect("in-flight entries name live operations");
            op.waiters.push(waiter);
            if request.qos > op.request.qos {
                if self.queue.remove(&(Reverse(op.request.qos.clone()), id)) {
                    self.queue.insert((Reverse(request.qos.clone()), id));
                }
                op.request.qos = request.qos;
            }
            return;
        }
        let id = OperationId(self.next_op);
        self.next_op += 1;
        if request.joinable() {
            self.in_flight.insert(request.key.clone(), id);
        }
        self.queue.insert((Reverse(request.qos.clone()), id));
        self.ops.insert(
            id,
            Operation {
                request,
                waiters: vec![waiter],
                state: OpState::Queued,
                committed_lease: None,
                result_proposed: false,
                lost_on: Vec::new(),
            },
        );
    }

    /// Releases `operation`'s holding (booking and lease), if it has one.
    fn release(&mut self, id: OperationId) {
        let op = self.ops.get_mut(&id).expect("released operations exist");
        if let Some((lease, worker)) = op.state.holding() {
            if let Some(w) = self.workers.get_mut(worker) {
                w.booked = w.booked.saturating_sub(op.request.resources);
            }
            self.held.remove(&lease);
        }
        op.result_proposed = false;
    }

    /// Releases `operation`'s holding and puts it back in the queue.
    fn requeue(&mut self, id: OperationId) {
        self.release(id);
        let op = self
            .ops
            .get_mut(&id)
            .expect("held leases name live operations");
        op.state = OpState::Queued;
        self.queue.insert((Reverse(op.request.qos.clone()), id));
    }

    /// Gives up the lease operation `id` holds, which its worker lost. If the lease's
    /// `Start` went out, that is one `INFRA` attempt on the worker, and the worker may
    /// still run the lease (see [`Scheduler::book_late`]). If that spends the last of
    /// [`INFRA_ATTEMPTS`], an `INFRA` failure from the lease is proposed; else the
    /// operation goes back to the queue.
    fn lose(&mut self, id: OperationId, effects: &mut Vec<Effect>) {
        let op = self
            .ops
            .get_mut(&id)
            .expect("held leases name live operations");
        let Some((lease, worker)) = op.state.holding() else {
            return;
        };
        let worker = worker.clone();
        if self.held.get(&lease).is_some_and(|h| h.start_sent.is_some()) {
            op.lost_on.push(worker.clone());
            let resources = op.request.resources;
            self.given_up.insert(lease, GivenUp { worker, resources });
        }
        if op.lost_on.len() < INFRA_ATTEMPTS {
            self.requeue(id);
            return;
        }
        self.release(id);
        let op = self.ops.get_mut(&id).expect("checked above");
        op.state = OpState::Failing { lease };
        effects.push(Effect::Commit(ControlRecord::Result(ResultRecord {
            lease,
            operation: id,
            outcome: Outcome::Failed(Failure::Infra),
        })));
    }

    /// Books on `worker` each lease given up on it that `running` lists: its `Start`
    /// arrived after the Start grace, or the worker kept running it through G of
    /// silence, and it uses room until it leaves the running set. Frees the room of
    /// those that left it. A listed lease that is held is booked already; one never
    /// granted, or given up on another worker, books nothing.
    fn book_late(&mut self, worker: &WorkerId, running: &BTreeSet<LeaseId>) {
        let Some(w) = self.workers.get_mut(worker) else {
            return;
        };
        let mut freed = Resources::default();
        w.late.retain(|lease, resources| {
            let listed = running.contains(lease);
            if !listed {
                freed = freed.saturating_add(*resources);
            }
            listed
        });
        w.booked = w.booked.saturating_sub(freed);
        for lease in running {
            if let Some(given_up) = self.given_up.get(lease)
                && given_up.worker == *worker
                && let Entry::Vacant(late) = w.late.entry(*lease)
            {
                late.insert(given_up.resources);
                w.booked = w.booked.saturating_add(given_up.resources);
            }
        }
    }

    /// Gives up every lease held on a worker not heard from for [`LEASE_GRACE`].
    fn expire(&mut self, effects: &mut Vec<Effect>) {
        let now = self.now;
        let expired: Vec<OperationId> = self
            .held
            .values()
            .map(|held| held.operation)
            .filter(|id| {
                let worker = self.ops[id].state.holding().map(|(_, w)| w);
                worker
                    .and_then(|w| self.workers.get(w))
                    .is_none_or(|w| !w.alive(now))
            })
            .collect();
        for id in expired {
            self.lose(id, effects);
        }
    }

    /// Gives up every committed lease held on `worker` that `running` leaves out, if its
    /// `Start` went to an earlier session of the worker or has been out for
    /// [`START_GRACE`]. A lease whose result was reported is kept: that result is on its
    /// way to the log.
    fn reconcile(
        &mut self,
        worker: &WorkerId,
        running: &BTreeSet<LeaseId>,
        effects: &mut Vec<Effect>,
    ) {
        let Some(session) = self.workers.get(worker).map(|w| w.session) else {
            return;
        };
        let now = self.now;
        let lost: Vec<OperationId> = self
            .held
            .iter()
            .filter(|(lease, held)| {
                let Some(sent) = held.start_sent else {
                    return false;
                };
                // A `Start` sent to an earlier session reached the worker before it
                // registered again, and then the worker lists it, or it never will.
                let due = sent.session < session || now >= sent.at.saturating_add(START_GRACE);
                let op = &self.ops[&held.operation];
                due && !running.contains(*lease)
                    && !op.result_proposed
                    && op.state.holding().is_some_and(|(_, w)| w == worker)
            })
            .map(|(_, held)| held.operation)
            .collect();
        for id in lost {
            self.lose(id, effects);
        }
    }

    /// One placement round: each queued operation, most urgent first, goes to the
    /// first live worker (in name order) with room for its whole request vector,
    /// skipping workers that lost a lease of it while another has room.
    fn place(&mut self, effects: &mut Vec<Effect>) {
        let now = self.now;
        let mut placed = Vec::new();
        for &(_, id) in &self.queue {
            if placed.len() == PLACEMENT_ROUND {
                break;
            }
            let op = &self.ops[&id];
            let request = &op.request.resources;
            // RFC section 5.8: an `INFRA` failure retries elsewhere.
            let fit = self
                .workers
                .iter()
                .filter(|(_, w)| w.alive(now) && w.free().fits(request))
                .min_by_key(|(name, _)| op.lost_on.contains(*name))
                .map(|(name, _)| name.clone());
            if let Some(name) = fit {
                let w = self.workers.get_mut(&name).expect("found above");
                w.booked = w.booked.saturating_add(*request);
                placed.push((id, name));
            }
        }
        for (id, worker) in placed {
            let lease = LeaseId::new(self.term, self.next_seq);
            self.next_seq += 1;
            let op = self.ops.get_mut(&id).expect("queued operations exist");
            self.queue.remove(&(Reverse(op.request.qos.clone()), id));
            op.state = OpState::Leased {
                lease,
                worker: worker.clone(),
                committed: false,
            };
            self.held.insert(
                lease,
                Held {
                    operation: id,
                    start_sent: None,
                },
            );
            effects.push(Effect::Commit(ControlRecord::Lease(LeaseGrant {
                lease,
                operation: id,
                worker,
            })));
        }
    }

    fn lease_committed(&mut self, grant: &LeaseGrant, effects: &mut Vec<Effect>) {
        let Some(op) = self.ops.get_mut(&grant.operation) else {
            return;
        };
        if op.state.is_done() {
            return;
        }
        op.committed_lease = op.committed_lease.max(Some(grant.lease));
        if let OpState::Leased {
            lease,
            worker,
            committed,
        } = &mut op.state
            && *lease == grant.lease
            && !*committed
        {
            *committed = true;
            if let Some(held) = self.held.get_mut(lease) {
                let session = self.workers.get(&*worker).map_or(0, |w| w.session);
                held.start_sent = Some(StartSent {
                    at: self.now,
                    session,
                });
            }
            effects.push(Effect::Start(StartLease {
                worker: worker.clone(),
                lease: *lease,
                operation: grant.operation,
                key: op.request.key.clone(),
                resources: op.request.resources,
                fence: op.request.fence(),
            }));
        }
    }

    fn started(&mut self, id: OperationId, lease: LeaseId) {
        let Some(op) = self.ops.get_mut(&id) else {
            return;
        };
        if let OpState::Leased {
            lease: held,
            worker,
            committed: true,
        } = &op.state
            && *held == lease
        {
            op.state = OpState::Running {
                lease,
                worker: worker.clone(),
            };
        }
    }

    fn report(&mut self, id: OperationId, lease: LeaseId, outcome: Outcome) -> Vec<Effect> {
        let Some(op) = self.ops.get_mut(&id) else {
            return Vec::new();
        };
        let current = match &op.state {
            OpState::Leased {
                lease, committed, ..
            } => committed.then_some(*lease),
            OpState::Running { lease, .. } => Some(*lease),
            _ => None,
        };
        if current != Some(lease) || op.result_proposed {
            return Vec::new();
        }
        op.result_proposed = true;
        vec![Effect::Commit(ControlRecord::Result(ResultRecord {
            lease,
            operation: id,
            outcome,
        }))]
    }

    fn result_committed(&mut self, record: &ResultRecord) -> Vec<Effect> {
        let id = record.operation;
        let Some(op) = self.ops.get(&id) else {
            return Vec::new();
        };
        if op.state.is_done() || op.committed_lease != Some(record.lease) {
            return Vec::new();
        }
        self.release(id);
        let op = self.ops.get_mut(&id).expect("checked above");
        op.state = match record.outcome {
            Outcome::Completed { action_result } => OpState::Completed {
                lease: record.lease,
                action_result,
            },
            Outcome::Failed(failure) => OpState::Failed {
                lease: record.lease,
                failure,
            },
        };
        // A queued operation (expired, not yet re-granted) leaves the queue too.
        self.queue.remove(&(Reverse(op.request.qos.clone()), id));
        if self.in_flight.get(&op.request.key) == Some(&id) {
            self.in_flight.remove(&op.request.key);
        }
        vec![Effect::Answer(Answer {
            operation: id,
            lease: record.lease,
            waiters: op.waiters.clone(),
            outcome: record.outcome,
        })]
    }
}

impl StateMachine for Scheduler {
    type Input = Input;

    fn apply(&mut self, input: Input) -> Vec<Effect> {
        self.now = self.now.max(input.now);
        match input.event {
            Event::WorkerUp { worker, capacity } => {
                let now = self.now;
                self.workers
                    .entry(worker)
                    .and_modify(|w| {
                        w.capacity = capacity;
                        w.last_heard = now;
                        w.session += 1;
                    })
                    .or_insert(Worker {
                        capacity,
                        booked: Resources::default(),
                        late: BTreeMap::new(),
                        last_heard: now,
                        session: 0,
                    });
                Vec::new()
            }
            Event::Heartbeat { worker, running } => {
                let mut effects = Vec::new();
                if let Some(w) = self.workers.get_mut(&worker) {
                    w.last_heard = w.last_heard.max(self.now);
                    let running: BTreeSet<LeaseId> = running.into_iter().collect();
                    self.reconcile(&worker, &running, &mut effects);
                    self.book_late(&worker, &running);
                }
                effects
            }
            Event::Submit { waiter, request } => {
                self.submit(waiter, request);
                Vec::new()
            }
            Event::Committed(ControlRecord::Lease(grant)) => {
                let mut effects = Vec::new();
                self.lease_committed(&grant, &mut effects);
                effects
            }
            Event::Committed(ControlRecord::Result(record)) => self.result_committed(&record),
            // Control records of other cores (jobs, alerts) are not the scheduler's.
            Event::Committed(_) => Vec::new(),
            Event::Started { operation, lease } => {
                self.started(operation, lease);
                Vec::new()
            }
            Event::Report {
                operation,
                lease,
                outcome,
            } => self.report(operation, lease, outcome),
            Event::Tick => {
                let mut effects = Vec::new();
                self.expire(&mut effects);
                self.place(&mut effects);
                effects
            }
        }
    }
}
