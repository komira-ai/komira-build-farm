//! The scheduler state machine.

mod fault;
mod give_up;
pub(crate) mod loss;
pub(crate) mod memory;
mod outage;
mod phase;
mod recover;
mod state;
mod view;

pub use recover::{Fate, Recovered, RecoveredLease};
pub use state::OpState;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_types::{
    ActionKey, Answer, ControlRecord, Effect, Failure, FarmRun, FarmTime, LeaseGrant, LeaseId,
    LeaseKind, NodePhase, OperationId, Outcome, Qos, Refusal, RefusalRecord, Resources,
    ResultRecord, StartLease, StateMachine, WaiterId, Waiting, WorkerId,
};

use crate::cordon::Cordons;
use crate::fence::{HANDOVER_GRACE, LEASE_GRACE};
use crate::input::{DaemonInstance, Event, Input, Request};
use crate::requeue::{Requeue, RequeueReason};
use crate::servable::{Servable, Verdict};
use memory::{MemoryLadder, MemoryState};

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

/// How long a finished operation is kept after its waiters are answered, so that a
/// caller whose `Execute` stream broke can still `WaitExecution` on it and get its
/// result; after that the operation is gone (issue #165). The default of
/// [`Scheduler::new`]; see [`Scheduler::with_finished_retention`].
///
/// A client reconnects after a broken stream within seconds (its retries back off to
/// a few seconds each), so a minute covers several attempts. The result it would
/// miss is worth keeping that long: a failed or `do_not_cache` result is not in the
/// action cache, so a client that finds the operation gone runs the action again. It
/// is not longer because every operation finished in the last retention is held in
/// memory: at the pilot's rate of about 4 operations a second, a minute is about 240.
pub const FINISHED_RETENTION: Duration = Duration::from_secs(60);

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
    /// The proposed result is the farm's give-up of a silent, replaced or never-started
    /// lease past its budget ([`Scheduler::give_up_charged`]), not a worker's.
    gave_up: bool,
    /// While queued: since when, and why, no live worker can run it. `None` while one
    /// can.
    unservable: Option<Unservable>,
    /// Its runs that ended as the farm's fault, oldest first: the farm reruns it has
    /// used ([`memory::FARM_RERUNS`]).
    farm_runs: Vec<FarmRun>,
    /// The workers those runs ended on: placement prefers any other (see
    /// [`Servable::fit`]).
    avoid: BTreeSet<WorkerId>,
    /// Its leases given up as not started ([`memory::NOT_STARTED_RERUNS`]).
    not_started: u32,
    /// Its leases lost ([`RequeueReason::is_loss`]), counted toward the cap
    /// ([`Scheduler::with_loss_requeues`]).
    losses: u32,
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
#[derive(Clone, Debug)]
struct Held {
    operation: OperationId,
    /// What it booked on its worker.
    booked: Resources,
    /// The iOS device it booked on its worker, by id.
    device: Option<String>,
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
    /// How many leases it holds.
    pub(crate) leases: usize,
    /// A whole-machine lease holds it: nothing else is placed on it.
    pub(crate) whole: bool,
    /// The iOS devices its leases booked, by id. A booking lasts until its lease ends,
    /// whatever the worker's reports say meanwhile ([`crate::devices`]).
    pub(crate) devices: BTreeSet<String>,
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
    /// The earlier process the newest change of process was proven to replace: its
    /// successor's drivers killed its runs before registering, so its leases the worker
    /// leaves out go at once ([`Event::WorkerUp`]'s `predecessor_stopped`).
    stopped_process: Option<u64>,
    /// What its newest `Hello` says it is doing.
    pub(crate) phase: NodePhase,
    /// Until when the lease of a `Preparing` or `Restoring` phase is kept while the
    /// worker is silent ([`crate::fence::SESSION_SWITCH_GRACE`] after that phase was
    /// first reported).
    switch_ends: FarmTime,
}

impl Worker {
    pub(crate) fn free(&self) -> Resources {
        self.capacity.saturating_sub(self.booked)
    }

    pub(crate) fn alive(&self, now: FarmTime) -> bool {
        now < self.last_heard.saturating_add(LEASE_GRACE)
    }

    /// Whether `lease`, which it holds, is kept at `now` although it is silent: it is
    /// switching the GUI session for that lease, within the switch grace.
    fn switching(&self, lease: LeaseId, now: FarmTime) -> bool {
        self.phase.switching() == Some(lease) && now < self.switch_ends
    }

    /// Whether `request`, an action or a VM, is at the whole host's memory (the last step
    /// of the memory ladder, which the cap is): it runs with no other lease on the node,
    /// and none is placed beside it, as a whole-machine lease does. A request for no
    /// memory is not.
    pub(crate) fn exclusive(&self, request: &Request) -> bool {
        request.resources.memory_bytes > 0
            && request.resources.memory_bytes >= self.capacity.memory_bytes
    }

    /// Whether `request` can be placed on it now, platform and kind aside: an action
    /// if no whole-machine lease holds it, its free CPUs, GPUs and VM slots hold the
    /// request, and its memory capacity is at least the request's memory; a
    /// whole-machine lease if it holds no lease and its capacity holds the request. A
    /// request for an iOS device also needs a free device that satisfies it
    /// ([`Worker::free_device`]).
    pub(crate) fn has_room(&self, request: &Request) -> bool {
        let room = match request.kind {
            LeaseKind::WholeMachine => self.leases == 0 && self.capacity.fits(&request.resources),
            // An action's memory is the cgroup limit it first runs under, not a booking:
            // what other leases hold of the node's memory never keeps it out. The node
            // bounds how many run at once by CPUs (its lease count), and its memory
            // pressure backstop pauses starts. The limit must still be one the node can
            // give. The one limit that is not a share is the whole host ([`Worker::exclusive`]):
            // that action runs alone.
            LeaseKind::Action | LeaseKind::Vm => {
                !self.whole
                    && self.free().fits(&request.resources.without_memory())
                    && request.resources.memory_bytes <= self.capacity.memory_bytes
                    && (!self.exclusive(request) || self.leases == 0)
            }
        };
        room && (!request.needs.wants_device() || self.free_device(&request.needs).is_some())
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
///   from an expired lease, duplicates and late arrivals are dropped. A committed
///   memory kill or lost run that runs the operation again ([`memory`], [`fault`])
///   finishes nothing: the operation is queued again, and only a later run's result
///   can finish it.
///
/// A lease is given up, and its operation requeued, when its worker is silent for G,
/// and when a worker it still hears from does not list it as running (see
/// [`Event::WorkerUp`] and [`Event::Heartbeat`]): at once only if the daemon process
/// that received its `Start` says so, or was proven stopped by the process after it,
/// after the handover grace if another process registered as the worker since. A given-up lease can no longer have a
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
    /// How long a finished operation is kept.
    finished_retention: Duration,
    /// Finished operations still kept, in the order they finished, with when they did
    /// (so in time order: farm time never goes back).
    finished: VecDeque<(FarmTime, OperationId)>,
    /// Requeues not yet taken, while they are recorded at all
    /// ([`Scheduler::recording_requeues`]).
    requeues: Option<Vec<Requeue>>,
    /// Queued whole-machine operations that fit nowhere, and the worker each holds:
    /// work after it in queue order is not placed there (see [`Scheduler::place`]).
    reservations: BTreeMap<OperationId, WorkerId>,
    /// The memory floors, each operation's memory kills and each node's busy-node
    /// kills ([`memory`]). Boxed: most schedulers hold none, and the scheduler is
    /// moved by value.
    memory: Box<MemoryState>,
    /// The rungs of an action's memory booking ([`memory`]).
    ladder: MemoryLadder,
    /// Workers a recovered scheduler holds leases for that have not registered yet, and
    /// until when they may take to ([`Scheduler::recover`]).
    awaited: BTreeMap<WorkerId, FarmTime>,
    /// The state store refused a write and has not taken one since
    /// ([`Scheduler::store_unavailable`]).
    store_down: bool,
    /// Until when no lease is given up, after the store returned
    /// ([`Scheduler::store_available`]).
    grace_until: FarmTime,
    /// How many lost leases refuse an operation ([`Scheduler::with_loss_requeues`]).
    loss_cap: u32,
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
            finished_retention: FINISHED_RETENTION,
            finished: VecDeque::new(),
            requeues: None,
            reservations: BTreeMap::new(),
            memory: Box::default(),
            ladder: MemoryLadder::default(),
            awaited: BTreeMap::new(),
            store_down: false,
            grace_until: FarmTime::default(),
            loss_cap: u32::MAX,
        }
    }

    /// This scheduler, keeping a finished operation for `retention` after its waiters
    /// are answered (instead of [`FINISHED_RETENTION`]), then dropping it.
    #[must_use]
    pub const fn with_finished_retention(mut self, retention: Duration) -> Self {
        self.finished_retention = retention;
        self
    }

    /// How many operations it holds: every unfinished one, and each finished one for
    /// the finished retention.
    #[must_use]
    pub fn operations(&self) -> usize {
        self.ops.len()
    }

    /// This scheduler, keeping a [`Requeue`] for each lease it gives up until
    /// [`Scheduler::take_requeues`] takes it. Off by default, so a caller that never
    /// takes them (a simulation) does not collect them without end.
    #[must_use]
    pub fn recording_requeues(mut self) -> Self {
        self.requeues = Some(Vec::new());
        self
    }

    /// The leases given up since the last call, oldest first; always empty unless
    /// built with [`Scheduler::recording_requeues`].
    pub fn take_requeues(&mut self) -> Vec<Requeue> {
        self.requeues
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// This scheduler, refusing a queued operation once no live worker has been able to
    /// run it for `wait` (instead of [`UNSERVABLE_WAIT`]).
    #[must_use]
    pub const fn with_unservable_wait(mut self, wait: Duration) -> Self {
        self.unservable_wait = wait;
        self
    }

    /// Drops every finished operation kept for the finished retention or longer. A
    /// finished operation holds no lease and is in no queue or in-flight entry, so
    /// nothing else names it; an input that does (a late report, a stale record) finds
    /// no operation and is dropped, as for any unknown one.
    fn retire(&mut self) {
        while let Some(&(at, id)) = self.finished.front()
            && self.now >= at.saturating_add(self.finished_retention)
        {
            self.finished.pop_front();
            self.ops.remove(&id);
            self.memory.forget(id);
        }
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
        let request = self.floored(request);
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
                gave_up: false,
                unservable: None,
                farm_runs: Vec::new(),
                not_started: 0,
                losses: 0,
                avoid: BTreeSet::new(),
            },
        );
        Vec::new()
    }

    /// Releases `operation`'s holding (booking and lease), if it has one.
    fn release(&mut self, id: OperationId) {
        let op = self.ops.get_mut(&id).expect("released operations exist");
        if let Some((lease, worker)) = op.state.holding() {
            let held = self.held.remove(&lease);
            if let Some((w, held)) = self.workers.get_mut(worker).zip(held) {
                w.booked = w.booked.saturating_sub(held.booked);
                w.leases -= 1;
                if let Some(device) = &held.device {
                    w.devices.remove(device);
                }
                // A whole-machine lease, or an action at the whole host's memory, is
                // the node's only lease.
                if w.leases == 0 {
                    w.whole = false;
                }
            }
        }
        op.result_proposed = false;
        op.gave_up = false;
    }

    /// Releases `operation`'s holding and puts it back in the queue, recording why.
    fn requeue(&mut self, id: OperationId, reason: RequeueReason) {
        let given_up = self.ops[&id]
            .state
            .holding()
            .map(|(lease, worker)| Requeue {
                operation: id,
                lease,
                worker: worker.clone(),
                reason,
            });
        if let Some(log) = &mut self.requeues {
            log.extend(given_up);
        }
        self.release(id);
        let op = self
            .ops
            .get_mut(&id)
            .expect("held leases name live operations");
        // Only an operation that holds a lease is requeued, so every call is a give-up.
        op.losses += u32::from(reason.is_loss());
        op.state = OpState::Queued;
        self.queue.insert((Reverse(op.request.qos.clone()), id));
    }

    /// One placement round. Each queued operation, most urgent first, goes to the first
    /// live worker (in name order) that serves its lease kind, satisfies its platform
    /// and has room for it, up to [`PLACEMENT_ROUND`] grants. An action has room where
    /// its CPUs, GPUs and VM slots are free and no whole-machine lease is held; its
    /// memory is only checked against the worker's, never against what other leases
    /// hold ([`Worker::has_room`]); a
    /// whole-machine lease only on a worker that holds no lease, and it books all of it.
    ///
    /// A whole-machine operation that fits nowhere holds one worker that could run it
    /// (the one it held last round while that still could, else the one with fewest
    /// leases): no operation after it in queue order (less urgent, or as urgent and
    /// younger) is placed there, so the worker empties as its leases end. Work before
    /// it in queue order still is, so a reservation never holds back more urgent work.
    /// It lasts while the operation is queued and the worker could run it.
    ///
    /// Every queued operation is also checked against what live workers could ever give
    /// it: one that no live worker satisfies, or that is larger than every one that
    /// does, waits with a reason ([`Effect::Waiting`] when the reason changes), and is
    /// refused once it has waited so for the unservable wait. Its refusal is committed
    /// first and leaves the queue at once, so nothing places it meanwhile.
    fn place(&mut self, effects: &mut Vec<Effect>) {
        // A grant the store cannot take is useless, and a refusal as much.
        if self.store_down {
            return;
        }
        self.refuse_lost(effects);
        let now = self.now;
        let mut servable = Servable::new(&self.workers, &self.cordons, now);
        let mut placed = Vec::new();
        let mut verdicts = Vec::new();
        let mut reserved = BTreeSet::new();
        let mut reservations = BTreeMap::new();
        for &(_, id) in &self.queue {
            let op = &self.ops[&id];
            let request = &op.request;
            let fitted = placed.len() < PLACEMENT_ROUND
                && servable
                    .fit(&mut self.workers, request, &reserved, &op.avoid)
                    .map(|(name, booked, device)| placed.push((id, name, booked, device)))
                    .is_some();
            let verdict = if fitted {
                Verdict::Servable
            } else {
                servable.verdict(&self.workers, request)
            };
            if !fitted
                && request.kind == LeaseKind::WholeMachine
                && verdict == Verdict::Servable
                && let Some(name) = servable.reserve(
                    &self.workers,
                    request,
                    &reserved,
                    &op.avoid,
                    self.reservations.get(&id),
                )
            {
                reserved.insert(name.clone());
                reservations.insert(id, name);
            }
            if op.unservable.is_some() || verdict != Verdict::Servable {
                verdicts.push((id, verdict));
            }
        }
        self.reservations = reservations;
        for (id, verdict) in verdicts {
            self.note(id, verdict, effects);
        }
        for (id, worker, booked, device) in placed {
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
                    booked,
                    device,
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
        self.finished.push_back((self.now, id));
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
            let (session, process) = self
                .workers
                .get(&*worker)
                .map_or((0, 0), |w| (w.session, w.process));
            let sent = StartSent {
                at: self.now,
                session,
                process,
            };
            // A leased operation's lease is always held: no branch on it.
            let (booked, device) =
                self.held
                    .get_mut(&*lease)
                    .map_or((op.request.resources, None), |held| {
                        held.start_sent = Some(sent);
                        (held.booked, held.device.clone())
                    });
            effects.push(Effect::Start(StartLease {
                worker: worker.clone(),
                lease: *lease,
                operation: grant.operation,
                key: op.request.key.clone(),
                kind: op.request.kind,
                resources: booked,
                fence: op.request.fence(),
                device,
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
        if let Outcome::Failed(kill @ (Failure::OutOfMemory | Failure::NodeMemoryPressure)) =
            record.outcome
            && self.rerun_after_memory_kill(record, kill)
        {
            return Vec::new();
        }
        if let Outcome::Failed(Failure::Lost(why)) = record.outcome
            && self.rerun_after_lost(record, why)
        {
            return Vec::new();
        }
        // The scheduler's own give-up for a lease that never started is not a run to charge.
        if record.outcome == Outcome::Failed(Failure::Infra)
            && !self.ops[&id].gave_up
            && self.rerun_after_infra(record)
        {
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
        self.finished.push_back((self.now, id));
        vec![Effect::Answer(Answer {
            operation: id,
            lease: record.lease,
            waiters: op.waiters.clone(),
            outcome: record.outcome,
            memory_runs: self.memory.runs(id).to_vec(),
            farm_runs: op.farm_runs.clone(),
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
                predecessor_stopped,
                capacity,
                caps,
            } => {
                let now = self.now;
                match self.workers.get_mut(&worker) {
                    Some(w) => {
                        if !instance.same_as(&w.instance) {
                            // The replaced process was last heard no later than now,
                            // and is not acknowledged from now on. Its leases are not
                            // given up here even when it is proven stopped: the phase,
                            // taken next, may send a switching lease's `Start` to the
                            // new process, and the first heartbeat decides the rest.
                            if predecessor_stopped {
                                w.stopped_process = Some(w.process);
                            }
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
                        let mut fresh = Worker {
                            capacity,
                            caps,
                            booked: Resources::default(),
                            leases: 0,
                            whole: false,
                            devices: BTreeSet::new(),
                            last_heard: now,
                            session: 0,
                            instance,
                            process: 0,
                            handover_ends: FarmTime::default(),
                            stopped_process: None,
                            phase: NodePhase::Serving,
                            switch_ends: FarmTime::default(),
                        };
                        self.adopt(&worker, &mut fresh);
                        self.workers.insert(worker, fresh);
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
            Event::Heartbeat { worker, running } => match self.workers.get_mut(&worker) {
                Some(w) => {
                    w.last_heard = w.last_heard.max(self.now);
                    self.reconcile(&worker, &running)
                }
                None => Vec::new(),
            },
            Event::Phase { worker, phase } => self.take_phase(&worker, phase),
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
                effects.extend(self.expire());
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
        self.retire();
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
