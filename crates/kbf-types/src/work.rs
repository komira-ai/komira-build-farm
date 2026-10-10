//! The vocabulary of scheduled work: operations, workers, waiters, the dedup key, the
//! request vector, fence policies, outcomes, and the records the scheduler commits to
//! the control log.
//!
//! These are shared because the front (which answers waiters), the server (which
//! carries out scheduler effects) and the daemon (which runs leases) all speak them.

use std::fmt;

use crate::{Digest, LeaseId, LeaseKind};

/// One operation: one execution of an action that one or more waiters are attached to.
/// Numbered by the scheduler in submission order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(pub u64);

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "op-{}", self.0)
    }
}

/// One client call waiting on an operation (an `Execute` or `WaitExecution` stream).
/// Numbered by the front; the scheduler only hands it back in [`Answer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WaiterId(pub u64);

/// A worker node, by the name it registered with.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerId(String);

impl WorkerId {
    /// The worker called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The worker's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What makes two executions the same work: the REAPI instance name and the action
/// digest. In-flight dedup joins a second call to a running twin only when both match;
/// the same digest under another instance is different work (another namespace, maybe
/// another cache), so it never joins.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionKey {
    /// The REAPI instance name the client sent.
    pub instance: String,
    /// The digest of the `Action` message.
    pub action: Digest,
}

/// A request vector, or a node's capacity in the same units.
///
/// GPUs are whole and exclusive: a request books whole GPUs, and a booked GPU is
/// another lease's only once the lease holding it ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Resources {
    /// CPU in thousandths of a core (1000 = one core).
    pub cpu_millis: u64,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Whole GPUs.
    pub gpus: u64,
}

impl Resources {
    /// A vector of `cpu_millis` thousandths of a core and `memory_bytes` bytes, and no
    /// GPU.
    #[must_use]
    pub const fn new(cpu_millis: u64, memory_bytes: u64) -> Self {
        Self {
            cpu_millis,
            memory_bytes,
            gpus: 0,
        }
    }

    /// `self` with `gpus` whole GPUs.
    #[must_use]
    pub const fn with_gpus(self, gpus: u64) -> Self {
        Self { gpus, ..self }
    }

    /// Whether `request` fits in `self` on every axis.
    #[must_use]
    pub const fn fits(&self, request: &Self) -> bool {
        request.cpu_millis <= self.cpu_millis
            && request.memory_bytes <= self.memory_bytes
            && request.gpus <= self.gpus
    }

    /// `self + other` on every axis, saturating.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            cpu_millis: self.cpu_millis.saturating_add(other.cpu_millis),
            memory_bytes: self.memory_bytes.saturating_add(other.memory_bytes),
            gpus: self.gpus.saturating_add(other.gpus),
        }
    }

    /// `self - other` on every axis, saturating at zero.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self {
            cpu_millis: self.cpu_millis.saturating_sub(other.cpu_millis),
            memory_bytes: self.memory_bytes.saturating_sub(other.memory_bytes),
            gpus: self.gpus.saturating_sub(other.gpus),
        }
    }
}

/// What a worker does with a lease when it loses touch with the scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FencePolicy {
    /// Hermetic work: run to its timeout and keep the result. A duplicate run after
    /// re-dispatch costs only compute.
    RunOn,
    /// Networked work: stop once the newest acknowledged heartbeat is older than the
    /// self-fence time, so two copies never run at once.
    SelfFence,
}

/// Why an attempt failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Failure {
    /// The action ran past its timeout.
    Timeout,
    /// The farm failed the action (a kernel OOM, a lost container), not the action itself.
    Infra,
    /// The action itself cannot run as written (an image named by tag, an output that
    /// is also an input): the client's error, answered INVALID_ARGUMENT. Running it
    /// elsewhere would fail the same way, so it is never retried and never cached.
    Invalid,
    /// The lease was killed because the action passed its own memory limit. Reported
    /// by a worker, the scheduler requeues it with its memory booking doubled, up to
    /// the largest node that fits it. An operation that finishes with it was killed
    /// at that largest node: the action needs more memory than any node offers.
    OutOfMemory,
    /// The node killed the lease for memory while the action was under its own limit
    /// (a node-wide out-of-memory kill, or the node's backstop): the busy node's
    /// fault, not the action's. The scheduler requeues it with the same booking while
    /// its farm reruns last; an operation that finishes with it has used them up.
    NodeMemoryPressure,
}

/// How an attempt ended, as its worker reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outcome {
    /// The action ran to completion (a failing test is still a completion); its
    /// `ActionResult` is stored under `action_result`.
    Completed {
        /// The digest of the uploaded `ActionResult`.
        action_result: Digest,
    },
    /// The attempt failed.
    Failed(Failure),
}

/// How a run was killed for memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKill {
    /// The action passed its own memory limit. `cap` is the memory of the largest node
    /// that could run it when the kill was committed, in bytes.
    OwnLimit {
        /// The cap, in bytes.
        cap: u64,
    },
    /// The node killed it under memory pressure, below its own limit.
    NodePressure,
}

/// One run of an operation that was killed for memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryRun {
    /// The node it ran on.
    pub worker: WorkerId,
    /// Its memory booking, in bytes.
    pub booked: u64,
    /// How it was killed.
    pub kill: MemoryKill,
}

/// A lease grant: `operation` runs on `worker` under `lease`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseGrant {
    /// The lease.
    pub lease: LeaseId,
    /// The operation it runs.
    pub operation: OperationId,
    /// Where.
    pub worker: WorkerId,
}

/// A result the scheduler accepted from the holder of `lease`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultRecord {
    /// The lease the result came from.
    pub lease: LeaseId,
    /// The operation it answers.
    pub operation: OperationId,
    /// What happened.
    pub outcome: Outcome,
}

/// The scheduler gives up on a queued operation without running it: no live worker
/// could run it for the whole of the wait bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalRecord {
    /// The operation.
    pub operation: OperationId,
    /// Why no worker could run it, for its callers.
    pub reason: String,
}

/// An entry of the control log that the scheduler asks to commit.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlRecord {
    /// A lease grant. Its `Start` is sent only once this is committed.
    Lease(LeaseGrant),
    /// An accepted result. Its waiters are answered only once this is committed.
    Result(ResultRecord),
    /// A refusal. Its waiters are answered only once this is committed.
    Refusal(RefusalRecord),
}

/// Tells `worker` to run `operation` under a committed `lease`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartLease {
    /// The worker that runs it.
    pub worker: WorkerId,
    /// The committed lease.
    pub lease: LeaseId,
    /// The operation; the worker names it in its reports.
    pub operation: OperationId,
    /// The action to run.
    pub key: ActionKey,
    /// What the lease takes: a share of the worker, or all of it.
    pub kind: LeaseKind,
    /// What the scheduler booked for it on `worker`: the request for an action lease,
    /// the worker's whole capacity for a whole-machine one.
    pub resources: Resources,
    /// What the worker does if it loses touch.
    pub fence: FencePolicy,
}

/// Answers every waiter of a finished operation with its committed result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The operation.
    pub operation: OperationId,
    /// The lease whose result was accepted.
    pub lease: LeaseId,
    /// Every waiter attached to the operation, in attach order.
    pub waiters: Vec<WaiterId>,
    /// The result.
    pub outcome: Outcome,
    /// The operation's runs that were killed for memory, oldest first: what callers of
    /// an operation finished by a memory kill are told about.
    pub memory_runs: Vec<MemoryRun>,
}

/// Why a queued operation is waiting, when no live worker can run it now. Its callers
/// see the reason while it waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// The operation.
    pub operation: OperationId,
    /// Why no live worker can run it, or `None` once one can again.
    pub reason: Option<String>,
}

/// Answers every waiter of an operation the scheduler refused to run, with the
/// committed reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The operation.
    pub operation: OperationId,
    /// Every waiter attached to it, in attach order.
    pub waiters: Vec<WaiterId>,
    /// Why no worker could run it.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a fit test that checks one axis only, which would book memory a node
    /// does not have (or CPU it does not have).
    #[test]
    fn fits_needs_every_axis() {
        let free = Resources::new(4_000, 8 << 30);
        assert!(free.fits(&Resources::new(4_000, 8 << 30)));
        assert!(!free.fits(&Resources::new(4_001, 1)));
        assert!(!free.fits(&Resources::new(1, (8 << 30) + 1)));
    }

    /// Catches: a fit test that ignores GPUs, which would place a GPU action on a node
    /// without one, or a second GPU action on a node whose only GPU is booked.
    #[test]
    fn fits_counts_whole_gpus() {
        let one_gpu = Resources::new(4_000, 8 << 30).with_gpus(1);
        let gpu_action = Resources::new(1_000, 1 << 30).with_gpus(1);
        assert!(one_gpu.fits(&gpu_action));
        assert!(!Resources::new(4_000, 8 << 30).fits(&gpu_action));
        assert!(!one_gpu.fits(&gpu_action.with_gpus(2)));
        assert!(
            one_gpu.fits(&Resources::new(1_000, 1 << 30)),
            "a CPU action"
        );
    }

    /// Catches: arithmetic that wraps, which would turn an overbooked node into one
    /// with nearly unlimited free room.
    #[test]
    fn arithmetic_saturates() {
        let a = Resources::new(1_000, 10);
        let b = Resources::new(3_000, 5);
        assert_eq!(a.saturating_sub(b), Resources::new(0, 5));
        assert_eq!(
            Resources::new(u64::MAX, 1).saturating_add(a),
            Resources::new(u64::MAX, 11)
        );
        let gpus = Resources::default().with_gpus(2);
        let one = Resources::default().with_gpus(1);
        assert_eq!(gpus.saturating_add(one), Resources::default().with_gpus(3));
        assert_eq!(one.saturating_sub(gpus), Resources::default());
        assert_eq!(gpus.saturating_sub(one), one);
        assert_eq!(
            Resources::default()
                .with_gpus(u64::MAX)
                .saturating_add(one)
                .gpus,
            u64::MAX
        );
    }
}
