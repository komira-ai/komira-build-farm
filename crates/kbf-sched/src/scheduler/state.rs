//! Where an operation is: [`OpState`].

use kbf_types::{Digest, Failure, LeaseId, WorkerId};

/// Where an operation is.
///
/// `Queued -> Leased -> Running -> Completed | Failed`. A lease that expires sends its
/// operation back to `Queued`, to be granted again under a new lease, and so does a
/// memory kill that runs it again (see [`kbf_types::MemoryRun`]).
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
    pub(super) fn holding(&self) -> Option<(LeaseId, &WorkerId)> {
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
