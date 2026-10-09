//! The scheduler state machine.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_types::{
    ActionKey, Answer, ControlRecord, Digest, Effect, Failure, FarmTime, LeaseGrant, LeaseId,
    OperationId, Outcome, Qos, Refusal, RefusalRecord, Resources, ResultRecord, StartLease,
    StateMachine, WaiterId, Waiting, WorkerId,
};

use crate::cordon::{Cordon, Cordons};
use crate::fence::{HANDOVER_GRACE, LEASE_GRACE, START_GRACE};
use crate::input::{DaemonInstance, Event, Input, Request};
use crate::servable::{Servable, Verdict};

/// At most this many leases are granted per [`Event::Tick`] (one log flush per round).
pub const PLACEMENT_ROUND: usize = 256;

/// How long a queued operation may wait while no live worker can run it (none
/// satisfies its platform, or none that does is large enough) before it is refused.
/// The default of [`Scheduler::new`]; see [`Scheduler::with_unservable_wait`].
///
/// The wait restarts whenever a live worker can run it again, and does not run while
/// only cordoned workers could (that work waits for the cordon). It is not zero because a
/// worker that can is often only a moment away: after a server restart daemons
/// reconnect over a few seconds, and a Mac that reboots is gone for a few minutes.
pub const UNSERVABLE_WAIT: Duration = Duration::from_secs(300);

/// Where an operation is.
///
/// `Queued -> Leased -> Running -> Completed | Failed`. A lease that expires sends its
/// operation back to `Queued`, to be granted again under a new lease.
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
    /// Failed: the failure of `lease` was committed.
    Failed {
        /// The lease whose outcome was accepted.
        lease: LeaseId,
        /// Why.
        failure: Failure,
    },
    /// Refused without running: no live worker could run it for the unservable wait,
    /// and the refusal was committed.
    Refused {
        /// Why no worker could run it.
        reason: String,
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
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Refused { .. }
        )
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
    /// While queued: since when, and why, no live worker can run it. `None` while one
    /// can.
    unservable: Option<Unservable>,
}

/// A queued operation no live worker can run: since when, why, and whether the wait
/// counts toward its refusal.
#[derive(Clone, Debug)]
struct Unservable {
    since: FarmTime,
    reason: String,
    /// `false` while only cordoned workers could run it: it waits for the cordon to
    /// end and is never refused for it.
    refusable: bool,
}

/// A lease currently held (leased or running).
#[derive(Clone, Copy, Debug)]
struct Held {
    operation: OperationId,
    /// When and to which session its `Start` was emitted; `None` until its grant is
    /// committed.
    start_sent: Option<StartSent>,
}

/// When a `Start` was emitted, and the worker session and daemon process it was sent to.
#[derive(Clone, Copy, Debug)]
struct StartSent {
    at: FarmTime,
    session: u64,
    process: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Worker {
    pub(crate) capacity: Resources,
    pub(crate) caps: NodeCaps,
    pub(crate) booked: Resources,
    last_heard: FarmTime,
    /// Counts the worker's registrations: the session a `Start` emitted now goes to.
    session: u64,
    /// The daemon process of the current session.
    instance: DaemonInstance,
    /// Counts the registrations that changed the daemon process: the process a `Start`
    /// emitted now goes to.
    process: u64,
    /// Until when leases whose `Start` went to an earlier process are kept: by then that
    /// process has fenced ([`HANDOVER_GRACE`] after it was last heard).
    handover_ends: FarmTime,
}

impl Worker {
    pub(crate) fn free(&self) -> Resources {
        self.capacity.saturating_sub(self.booked)
    }

    pub(crate) fn alive(&self, now: FarmTime) -> bool {
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
/// [`Event::WorkerUp`] and [`Event::Heartbeat`]): at once only if the daemon process
/// that received its `Start` says so, after the handover grace if another process
/// registered as the worker since. A given-up lease can no longer have a
/// result proposed, and once the operation is granted again its result loses to the
/// new grant in the log.
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
    /// How long a queued operation no live worker can run waits before it is refused.
    unservable_wait: Duration,
    /// Workers placement skips, and their drains.
    cordons: Cordons,
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
            unservable_wait: UNSERVABLE_WAIT,
            cordons: Cordons::default(),
        }
    }

    /// This scheduler, refusing a queued operation once no live worker has been able to
    /// run it for `wait` (instead of [`UNSERVABLE_WAIT`]).
    #[must_use]
    pub const fn with_unservable_wait(mut self, wait: Duration) -> Self {
        self.unservable_wait = wait;
        self
    }

    /// Why no live worker can run `operation` now, while it is queued and none can.
    #[must_use]
    pub fn waiting(&self, operation: OperationId) -> Option<&str> {
        let op = self.ops.get(&operation)?;
        op.unservable.as_ref().map(|u| u.reason.as_str())
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

    /// Whether `worker` is cordoned, and where its drain is; `None` while placement
    /// may use it.
    #[must_use]
    pub fn cordon(&self, worker: &WorkerId) -> Option<&Cordon> {
        self.cordons.get(worker)
    }

    /// The leases `worker` holds (granted or running), in lease order.
    #[must_use]
    pub fn leases_on(&self, worker: &WorkerId) -> Vec<LeaseId> {
        self.held
            .iter()
            .filter(|(_, h)| {
                self.ops[&h.operation]
                    .state
                    .holding()
                    .is_some_and(|(_, w)| w == worker)
            })
            .map(|(lease, _)| *lease)
            .collect()
    }

    /// What is booked on `worker`, if it is registered.
    #[must_use]
    pub fn booked(&self, worker: &WorkerId) -> Option<Resources> {
        self.workers.get(worker).map(|w| w.booked)
    }

    /// The leases in `running` (a heartbeat's running set) that this scheduler granted
    /// and no longer holds on `worker`: given up, granted elsewhere, or finished. Their
    /// results can no longer be accepted, so a run of one is wasted, and a run of one
    /// whose operation was granted again runs beside the retry. The caller cancels
    /// them. A lease of another term is never named: this scheduler does not know
    /// whether a newer leader granted it.
    pub fn not_held<'a>(
        &'a self,
        worker: &'a WorkerId,
        running: &'a [LeaseId],
    ) -> impl Iterator<Item = LeaseId> + 'a {
        running.iter().copied().filter(move |lease| {
            let granted = lease.term == self.term && lease.seq < self.next_seq;
            let held_here = self.held.get(lease).is_some_and(|held| {
                self.ops[&held.operation]
                    .state
                    .holding()
                    .is_some_and(|(_, w)| w == worker)
            });
            granted && !held_here
        })
    }

    /// Queues `request` for `waiter`, or attaches `waiter` to a running twin. A twin
    /// that waits for a worker that can run it tells its waiters why again, so the new
    /// one learns it too.
    fn submit(&mut self, waiter: WaiterId, request: Request) -> Vec<Effect> {
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
            return op
                .unservable
                .iter()
                .map(|u| {
                    Effect::Waiting(Waiting {
                        operation: id,
                        reason: Some(u.reason.clone()),
                    })
                })
                .collect();
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
                unservable: None,
            },
        );
        Vec::new()
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

    /// Sends every lease held on a worker not heard from for [`LEASE_GRACE`] back to
    /// the queue.
    fn expire(&mut self) {
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
            self.requeue(id);
        }
    }

    /// Sends back to the queue every committed lease held on `worker` that `running`
    /// leaves out, if its `Start` went to an earlier session of the current daemon
    /// process, to an earlier process once the handover grace has ended, or to the
    /// current session and has been out for [`START_GRACE`]. A lease whose result was
    /// reported is kept: that result is on its way to the log.
    fn reconcile(&mut self, worker: &WorkerId, running: &[LeaseId]) {
        let (session, process, handover_ends) = self
            .workers
            .get(worker)
            .map_or((0, 0, FarmTime::default()), |w| {
                (w.session, w.process, w.handover_ends)
            });
        let running: BTreeSet<LeaseId> = running.iter().copied().collect();
        let now = self.now;
        let lost: Vec<OperationId> = self
            .held
            .iter()
            .filter(|(lease, held)| {
                let Some(sent) = held.start_sent else {
                    return false;
                };
                let due = if sent.process != process {
                    // Another process may still run it: only its fence ends that.
                    now >= handover_ends
                } else if sent.session != session {
                    // A `Start` sent to an earlier session of this process reached it
                    // before it registered again, and then it lists it, or never will.
                    true
                } else {
                    now >= sent.at.saturating_add(START_GRACE)
                };
                let op = &self.ops[&held.operation];
                due && !running.contains(*lease)
                    && !op.result_proposed
                    && op.state.holding().is_some_and(|(_, w)| w == worker)
            })
            .map(|(_, held)| held.operation)
            .collect();
        for id in lost {
            self.requeue(id);
        }
    }

    /// One placement round. Each queued operation, most urgent first, goes to the first
    /// live worker (in name order) that satisfies its platform and has room for its
    /// whole request vector, up to [`PLACEMENT_ROUND`] grants.
    ///
    /// Every queued operation is also checked against what live workers could ever give
    /// it: one that no live worker satisfies, or that is larger than every one that
    /// does, waits with a reason ([`Effect::Waiting`] when the reason changes), and is
    /// refused once it has waited so for the unservable wait. Its refusal is committed
    /// first and leaves the queue at once, so nothing places it meanwhile.
    fn place(&mut self, effects: &mut Vec<Effect>) {
        let now = self.now;
        let mut servable = Servable::new(&self.workers, &self.cordons, now);
        let mut placed = Vec::new();
        let mut verdicts = Vec::new();
        for &(_, id) in &self.queue {
            let op = &self.ops[&id];
            let request = &op.request;
            let verdict = if placed.len() < PLACEMENT_ROUND
                && let Some(name) = servable.fit(&mut self.workers, request)
            {
                placed.push((id, name));
                Verdict::Servable
            } else {
                servable.verdict(&self.workers, request)
            };
            if op.unservable.is_some() || verdict != Verdict::Servable {
                verdicts.push((id, verdict));
            }
        }
        for (id, verdict) in verdicts {
            self.note(id, verdict, effects);
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

    /// Records this round's verdict on queued operation `id`: tells its waiters when
    /// the reason it waits changes, and proposes its refusal once no worker, cordoned or
    /// not, could run it for the unservable wait. Time spent waiting only for a cordon
    /// does not count: the wait starts again when the work becomes unservable.
    fn note(&mut self, id: OperationId, verdict: Verdict, effects: &mut Vec<Effect>) {
        let now = self.now;
        let wait = self.unservable_wait;
        let op = self.ops.get_mut(&id).expect("queued operations exist");
        let (reason, refusable) = match verdict {
            Verdict::Servable => {
                // Only an operation that was waiting has a servable verdict noted.
                op.unservable = None;
                effects.push(Effect::Waiting(Waiting {
                    operation: id,
                    reason: None,
                }));
                return;
            }
            Verdict::Unservable(reason) => (reason, true),
            Verdict::Cordoned(reason) => (reason, false),
        };
        let unservable = match &mut op.unservable {
            Some(u) if u.reason == reason => u,
            slot => {
                let since = match slot {
                    Some(u) if u.refusable && refusable => u.since,
                    _ => now,
                };
                effects.push(Effect::Waiting(Waiting {
                    operation: id,
                    reason: Some(reason.clone()),
                }));
                slot.insert(Unservable {
                    since,
                    reason,
                    refusable,
                })
            }
        };
        if unservable.refusable && now >= unservable.since.saturating_add(wait) {
            let reason = format!(
                "{} (waited {} s for a worker that can run it)",
                unservable.reason,
                wait.as_secs()
            );
            self.queue.remove(&(Reverse(op.request.qos.clone()), id));
            effects.push(Effect::Commit(ControlRecord::Refusal(RefusalRecord {
                operation: id,
                reason,
            })));
        }
    }

    /// A committed refusal: the operation is finished, and its waiters are answered.
    /// A refusal of an operation that is no longer queued is stale and dropped.
    fn refusal_committed(&mut self, record: &RefusalRecord) -> Vec<Effect> {
        let id = record.operation;
        let Some(op) = self.ops.get_mut(&id) else {
            return Vec::new();
        };
        if op.state != OpState::Queued {
            return Vec::new();
        }
        op.state = OpState::Refused {
            reason: record.reason.clone(),
        };
        op.unservable = None;
        self.queue.remove(&(Reverse(op.request.qos.clone()), id));
        if self.in_flight.get(&op.request.key) == Some(&id) {
            self.in_flight.remove(&op.request.key);
        }
        vec![Effect::Refuse(Refusal {
            operation: id,
            waiters: op.waiters.clone(),
            reason: record.reason.clone(),
        })]
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
            // A leased operation's lease is always held: no branch on it.
            let (session, process) = self
                .workers
                .get(&*worker)
                .map_or((0, 0), |w| (w.session, w.process));
            let sent = StartSent {
                at: self.now,
                session,
                process,
            };
            self.held
                .entry(*lease)
                .and_modify(|held| held.start_sent = Some(sent));
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
        let effects = match input.event {
            Event::WorkerUp {
                worker,
                instance,
                capacity,
                caps,
            } => {
                let now = self.now;
                match self.workers.get_mut(&worker) {
                    Some(w) => {
                        if !instance.same_as(&w.instance) {
                            // The replaced process was last heard no later than now,
                            // and is not acknowledged from now on.
                            w.process += 1;
                            w.handover_ends = w.last_heard.saturating_add(HANDOVER_GRACE);
                        }
                        w.instance = instance;
                        w.capacity = capacity;
                        w.caps = caps;
                        w.last_heard = now;
                        w.session += 1;
                    }
                    None => {
                        self.workers.insert(
                            worker,
                            Worker {
                                capacity,
                                caps,
                                booked: Resources::default(),
                                last_heard: now,
                                session: 0,
                                instance,
                                process: 0,
                                handover_ends: FarmTime::default(),
                            },
                        );
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
                    w.last_heard = w.last_heard.max(self.now);
                }
                Vec::new()
            }
            Event::Heartbeat { worker, running } => {
                if let Some(w) = self.workers.get_mut(&worker) {
                    w.last_heard = w.last_heard.max(self.now);
                    self.reconcile(&worker, &running);
                }
                Vec::new()
            }
            Event::Submit { waiter, request } => self.submit(waiter, request),
            Event::Committed(ControlRecord::Lease(grant)) => {
                let mut effects = Vec::new();
                self.lease_committed(&grant, &mut effects);
                effects
            }
            Event::Committed(ControlRecord::Result(record)) => self.result_committed(&record),
            Event::Committed(ControlRecord::Refusal(record)) => self.refusal_committed(&record),
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
                self.expire();
                self.place(&mut effects);
                effects
            }
            Event::Cordon { worker } => {
                self.cordons.cordon(worker);
                Vec::new()
            }
            Event::Drain { worker, deadline } => {
                self.cordons.drain(worker, deadline);
                Vec::new()
            }
            Event::Uncordon { worker } => {
                // Work may have waited for this worker alone: place it now, not at the
                // next tick.
                self.cordons.uncordon(&worker);
                let mut effects = Vec::new();
                self.place(&mut effects);
                effects
            }
        };
        let (now, held) = (self.now, &self.held);
        let ops = &self.ops;
        self.cordons.progress(now, |worker| {
            held.values().any(|h| {
                ops[&h.operation]
                    .state
                    .holding()
                    .is_some_and(|(_, w)| w == worker)
            })
        });
        effects
    }
}
