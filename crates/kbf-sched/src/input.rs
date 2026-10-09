//! What the scheduler is fed: requests, worker reports, committed records and ticks.

use kbf_caps::NodeCaps;
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
    /// What a worker must offer to run it, from the action's platform properties. The
    /// default asks for nothing: any worker may run it.
    pub needs: kbf_caps::Request,
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

/// Which daemon process registered a worker: the `instance_id` of its `Hello`. A daemon
/// draws one at random when it starts and sends it on every stream it opens, so two
/// registrations name the same instance only if one process made both. A restarted
/// daemon, a second daemon started with the same certificate and a cloned machine each
/// name another.
///
/// The empty id (a daemon that predates the field) is the same as no other, itself
/// included: every registration without one is taken as another process's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DaemonInstance(String);

impl DaemonInstance {
    /// The instance named `id`.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Whether this and `other` are known to be one daemon process.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        !self.0.is_empty() && self.0 == other.0
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
    /// A registration is the first `Hello` of a new stream, and opens a new session. A
    /// `Hello` the daemon resends on the same stream because its node report changed is
    /// not a registration. `Hello` carries no running set, so a registration requeues
    /// nothing by itself: the worker's next heartbeat decides (see [`Event::Heartbeat`]).
    ///
    /// The caller must keep two things true. A `Start` emitted before this input is sent
    /// on the earlier stream, never on the new one. A heartbeat from an earlier stream
    /// that arrives after this input is not fed. `kbf-server` holds both: a resent
    /// `Hello` becomes [`Event::Capacity`], and a stream replaced by a newer one has its
    /// heartbeats dropped (issue #25).
    ///
    /// A registration names the daemon process that made it (`instance`). The new
    /// session's first heartbeat decides at once only about leases whose `Start` went
    /// to an earlier session of the same process; leases whose `Start` went to another
    /// process are kept until that process has fenced (issue #140). See
    /// [`Event::Heartbeat`].
    WorkerUp {
        /// The worker.
        worker: WorkerId,
        /// The daemon process that registered it.
        instance: DaemonInstance,
        /// What placement may book on it.
        capacity: Resources,
        /// What it offers, from its node report: placement gives it only work whose
        /// platform it satisfies.
        caps: NodeCaps,
    },
    /// A registered worker's capacity or capabilities changed without a new registration: the daemon
    /// resent `Hello` on the same stream because its node report changed. Counts as
    /// hearing from it. Opens no session, so it requeues nothing and leaves every
    /// `Start` counted against the session it was sent to (issue #25). Ignored for a
    /// worker that never registered.
    Capacity {
        /// The worker.
        worker: WorkerId,
        /// What placement may book on it from now on.
        capacity: Resources,
        /// What it offers from now on.
        caps: NodeCaps,
    },
    /// A worker's heartbeat arrived on its newest stream, with the leases it holds.
    ///
    /// A committed lease the scheduler holds on the worker that `running` leaves out goes
    /// back to the queue:
    ///
    /// - at once if its `Start` was sent to an earlier session of the daemon process
    ///   that holds this one (the daemon either received it before registering again,
    ///   and then lists it, or never will);
    /// - if its `Start` was sent to a session of another daemon process, once
    ///   [`HANDOVER_GRACE`] has passed since the scheduler last heard the worker before
    ///   the newest change of process (issue #140). Only the process that ran a lease
    ///   can say it no longer runs it. The other one may still be running (two daemons
    ///   share the node's certificate), but its stream has not been acknowledged since
    ///   then, so it has fenced by the end of that grace;
    /// - if its `Start` went to this session, once it has been out for [`START_GRACE`]
    ///   (the `Start` was lost, or the worker no longer runs it).
    ///
    /// A lease whose `Start` is not yet sent, or whose result has already been reported,
    /// is kept whether listed or not.
    ///
    /// Worker contract: a lease whose `Start` went to an earlier session of the same
    /// process is requeued on the first heartbeat that leaves it out, with no grace. So
    /// a daemon must list every lease it holds in its first heartbeat on a new stream.
    /// A restarted daemon is another process: a lease it does not list is given up
    /// after the handover grace, not at once. The set must also include leases that
    /// ended with a result not yet acknowledged (issue #26); the server side of the
    /// session boundary is issue #25.
    ///
    /// [`START_GRACE`]: crate::fence::START_GRACE
    /// [`HANDOVER_GRACE`]: crate::fence::HANDOVER_GRACE
    Heartbeat {
        /// The worker.
        worker: WorkerId,
        /// The leases it holds: running (a restarted daemon re-adopts its runs; after a
        /// reboot there are none), or ended with a result not yet acknowledged.
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
    /// Time passed: expire leases on silent workers and run one placement round, which
    /// also notes which queued operations no live worker can run, and refuses those
    /// that have waited so for the scheduler's unservable wait.
    Tick,
    /// An operator cordons `worker`: placement offers it no new lease, and the leases it
    /// holds run on. A drain already under way is kept. Holds across the worker's
    /// sessions, and for a worker not registered yet.
    Cordon {
        /// The worker.
        worker: WorkerId,
    },
    /// An operator drains `worker`: cordons it and waits for its leases to end. It is
    /// [`Cordon::Drained`] once it holds none, and [`Cordon::Paused`] if `deadline`
    /// comes first; no lease is killed or given up for it. Restarts a drain that is
    /// drained or paused.
    ///
    /// [`Cordon::Drained`]: crate::Cordon::Drained
    /// [`Cordon::Paused`]: crate::Cordon::Paused
    Drain {
        /// The worker.
        worker: WorkerId,
        /// When the drain pauses if leases still run.
        deadline: FarmTime,
    },
    /// An operator returns `worker` to placement, ending its cordon and any drain, and
    /// queued work is placed at once.
    Uncordon {
        /// The worker.
        worker: WorkerId,
    },
}
