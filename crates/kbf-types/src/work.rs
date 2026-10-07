//! The vocabulary of scheduled work: operations, workers, waiters, the dedup key, the
//! request vector, fence policies, outcomes, and the records the scheduler commits to
//! the control log.
//!
//! These are shared because the front (which answers waiters), the server (which
//! carries out scheduler effects) and the daemon (which runs leases) all speak them.

use std::fmt;

use crate::{Digest, LeaseId};

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Resources {
    /// CPU in thousandths of a core (1000 = one core).
    pub cpu_millis: u64,
    /// Memory in bytes.
    pub memory_bytes: u64,
}

impl Resources {
    /// A vector of `cpu_millis` thousandths of a core and `memory_bytes` bytes.
    #[must_use]
    pub const fn new(cpu_millis: u64, memory_bytes: u64) -> Self {
        Self {
            cpu_millis,
            memory_bytes,
        }
    }

    /// Whether `request` fits in `self` on every axis.
    #[must_use]
    pub const fn fits(&self, request: &Self) -> bool {
        request.cpu_millis <= self.cpu_millis && request.memory_bytes <= self.memory_bytes
    }

    /// `self + other` on every axis, saturating.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self::new(
            self.cpu_millis.saturating_add(other.cpu_millis),
            self.memory_bytes.saturating_add(other.memory_bytes),
        )
    }

    /// `self - other` on every axis, saturating at zero.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self::new(
            self.cpu_millis.saturating_sub(other.cpu_millis),
            self.memory_bytes.saturating_sub(other.memory_bytes),
        )
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

/// An entry of the control log that the scheduler asks to commit.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlRecord {
    /// A lease grant. Its `Start` is sent only once this is committed.
    Lease(LeaseGrant),
    /// An accepted result. Its waiters are answered only once this is committed.
    Result(ResultRecord),
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
    /// What the scheduler booked for it on `worker`.
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
    }
}
