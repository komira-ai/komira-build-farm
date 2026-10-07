//! What the scheduler is fed: requests, worker reports, committed records and ticks.

use kbf_types::{
    ActionKey, ControlRecord, FarmTime, FencePolicy, LeaseId, OperationId, Outcome, Qos, Resources,
    WaiterId, WorkerId,
};

/// One execution request, as the front submits it after the cache missed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The dedup key: instance name and action digest.
    pub key: ActionKey,
    /// How urgent the caller says it is.
    pub qos: Qos,
    /// The CPU and memory to book for it.
    pub resources: Resources,
    /// Whether the action is hermetic (no network). Hermetic work runs on through a
    /// lost connection; networked work self-fences.
    pub hermetic: bool,
    /// The client asked for the result not to be cached.
    pub do_not_cache: bool,
}

impl Request {
    /// The fence policy of the lease that runs it: `RunOn` if hermetic, else `SelfFence`.
    #[must_use]
    pub fn fence(&self) -> FencePolicy {
        if self.hermetic {
            FencePolicy::RunOn
        } else {
            FencePolicy::SelfFence
        }
    }

    /// Whether another caller may join a running twin of this request. `do_not_cache`
    /// and networked actions are never joined: their runs are not interchangeable.
    #[must_use]
    pub fn joinable(&self) -> bool {
        self.hermetic && !self.do_not_cache
    }
}

/// One input: what happened, and the farm time at which it did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    /// The farm time. The scheduler never reads a clock; an input older than one already
    /// seen does not move its time back.
    pub now: FarmTime,
    /// What happened.
    pub event: Event,
}

impl Input {
    /// `event` at `now`.
    #[must_use]
    pub const fn new(now: FarmTime, event: Event) -> Self {
        Self { now, event }
    }
}

/// What happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A worker registered (or registered again) with `capacity` for actions: its CPUs
    /// and RAM minus protected floors. Counts as hearing from it.
    ///
    /// A registration opens a new session. Every committed lease the scheduler holds on
    /// the worker that `running` leaves out goes back to the queue at once: its `Start`
    /// was sent to an earlier session, so the worker either received it and would list
    /// it, or never will. The caller must keep that true: a `Start` emitted before this
    /// input is sent on the earlier session, never on the new one.
    WorkerUp {
        /// The worker.
        worker: WorkerId,
        /// What placement may book on it.
        capacity: Resources,
        /// The leases the worker holds as it registers (a restarted daemon re-adopts
        /// its runs; after a reboot there are none).
        running: Vec<LeaseId>,
    },
    /// A worker's heartbeat arrived, with the leases it holds.
    ///
    /// A committed lease the scheduler holds on the worker that `running` leaves out goes
    /// back to the queue once its `Start` has been out for [`START_GRACE`]: the `Start`
    /// was lost, or the worker no longer runs it. A lease whose result has already been
    /// reported is kept whether listed or not.
    ///
    /// [`START_GRACE`]: crate::fence::START_GRACE
    Heartbeat {
        /// The worker.
        worker: WorkerId,
        /// The leases it holds: running, or ended with a result not yet acknowledged.
        running: Vec<LeaseId>,
    },
    /// A caller asks for `request` to run. A joinable request with a running twin
    /// attaches `waiter` to the twin instead of queueing a new operation.
    Submit {
        /// The caller.
        waiter: WaiterId,
        /// What to run.
        request: Request,
    },
    /// A record the scheduler asked to commit is now committed. Records must be fed in
    /// log order: the order decides which result wins.
    Committed(ControlRecord),
    /// The worker holding `lease` started the operation.
    Started {
        /// The operation.
        operation: OperationId,
        /// The lease the worker holds.
        lease: LeaseId,
    },
    /// The worker holding `lease` reports how the operation ended.
    Report {
        /// The operation.
        operation: OperationId,
        /// The lease the worker holds.
        lease: LeaseId,
        /// What happened.
        outcome: Outcome,
    },
    /// Time passed: expire leases on silent workers and run one placement round.
    Tick,
}
