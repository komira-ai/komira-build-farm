//! The scheduler's read-only view: what it knows of an operation, a worker and the
//! queue, for the caller that carries out its effects.

use kbf_types::{FarmRun, LeaseId, NodePhase, OperationId, Qos, Resources, WaiterId, WorkerId};

use super::{OpState, Scheduler};
use crate::cordon::Cordon;
use crate::input::Request;

impl Scheduler {
    /// Why no live worker can run `operation` now, while it is queued and none can.
    #[must_use]
    pub fn waiting(&self, operation: OperationId) -> Option<&str> {
        let op = self.ops.get(&operation)?;
        op.unservable.as_ref().map(|u| u.reason.as_str())
    }

    /// Where `operation` is, if it exists: it was submitted, and is unfinished or
    /// finished less than the finished retention ago.
    #[must_use]
    pub fn state(&self, operation: OperationId) -> Option<&OpState> {
        self.ops.get(&operation).map(|op| &op.state)
    }

    /// The runs of `operation` that ended as the farm's fault, oldest first; `None` once
    /// the scheduler no longer holds it.
    #[must_use]
    pub fn farm_runs(&self, operation: OperationId) -> Option<&[FarmRun]> {
        self.ops.get(&operation).map(|op| op.farm_runs.as_slice())
    }

    /// The operation in flight that a submission of `request` would join: its
    /// running twin, if `request` may join one.
    #[must_use]
    pub fn twin(&self, request: &Request) -> Option<OperationId> {
        if !request.joinable() {
            return None;
        }
        self.in_flight.get(&request.key).copied()
    }

    /// The id the next operation that joins no twin gets.
    #[must_use]
    pub const fn next_operation(&self) -> OperationId {
        OperationId(self.next_op)
    }

    /// Every operation not finished, in id order: the ones a successor process takes
    /// over, and the ones whose callers are still waiting.
    pub fn unfinished(&self) -> impl Iterator<Item = OperationId> + '_ {
        self.ops
            .iter()
            .filter(|(_, op)| !op.state.is_done())
            .map(|(id, _)| *id)
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

    /// How many operations wait for a lease.
    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// How many leases are held (granted or running), on all workers.
    #[must_use]
    pub fn leases_held(&self) -> usize {
        self.held.len()
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

    /// What `worker`'s newest `Hello` says it is doing ([`crate::Event::Phase`]), if it is
    /// registered.
    #[must_use]
    pub fn phase(&self, worker: &WorkerId) -> Option<&NodePhase> {
        self.workers.get(worker).map(|w| &w.phase)
    }

    /// The worker queued whole-machine `operation` holds while it waits for it to empty,
    /// if any.
    #[must_use]
    pub fn reservation(&self, operation: OperationId) -> Option<&WorkerId> {
        self.reservations.get(&operation)
    }

    /// What is booked on `worker`, if it is registered. Its memory is what the leases'
    /// limits add up to: a record, which admission does not read (an action's memory is
    /// the limit it first runs under).
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

    /// How many of `operation`'s leases were lost so far ([`RequeueReason::is_loss`]);
    /// `None` once the scheduler no longer holds it.
    ///
    /// [`RequeueReason::is_loss`]: crate::RequeueReason::is_loss
    #[must_use]
    pub fn losses(&self, operation: OperationId) -> Option<u32> {
        self.ops.get(&operation).map(|op| op.losses)
    }
}
