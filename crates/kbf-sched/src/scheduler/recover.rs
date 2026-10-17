//! Taking up again the operations a state store holds: a scheduler rebuilt after its
//! process ended, or taken over by another.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use kbf_types::{FarmRun, LeaseId, LeaseKind, MemoryRun, OperationId, WaiterId, WorkerId};

use super::{Held, OpState, Operation, Scheduler, StartSent, Worker};
use crate::fence::LEASE_GRACE;
use crate::input::Request;

/// An unfinished operation as the state store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// The operation's id, as it was granted.
    pub operation: OperationId,
    /// What it runs.
    pub request: Request,
    /// The one caller the scheduler holds it for: the callers it had are gone with the
    /// process that held them, and a client that asks for the action again joins it.
    pub waiter: WaiterId,
    /// The committed lease it holds, if one was granted and not given up.
    pub lease: Option<RecoveredLease>,
    /// How many of its leases were lost before (counted by
    /// [`Scheduler::with_loss_requeues`]), as the store holds it.
    pub losses: u32,
    /// What the operation's faults changed (kills for memory, farm runs, not-started
    /// give-ups), as the store holds it; `None` for an operation that has none (or a
    /// store written before the row existed).
    pub fate: Option<Fate>,
}

/// What an operation's faults changed in the scheduler, which a scheduler that takes the
/// operation over must hold too: the memory it books now (raised by a kill), its runs
/// killed for memory, and the budgets its other faults spent (farm runs, the nodes
/// placement avoids, leases given up as not started).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fate {
    /// The memory booking the next run takes, in bytes: [`Request::resources`]'s
    /// `memory_bytes` after the raise. The request in the store is the submitted one.
    pub booked_bytes: u64,
    /// The operation's runs killed for memory, oldest first.
    pub memory_runs: Vec<MemoryRun>,
    /// The operation's runs that ended as the farm's fault, oldest first
    /// ([`crate::FARM_RERUNS`] bounds them).
    pub farm_runs: Vec<FarmRun>,
    /// The workers placement skips for the operation while another could run it.
    pub avoid: BTreeSet<WorkerId>,
    /// How many of its leases were given up as not started
    /// ([`crate::NOT_STARTED_RERUNS`] bounds them).
    pub not_started: u32,
}

impl Fate {
    /// Whether the operation's faults changed nothing, so there is nothing to write down.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.memory_runs.is_empty()
            && self.farm_runs.is_empty()
            && self.avoid.is_empty()
            && self.not_started == 0
    }
}

/// A committed lease of a [`Recovered`] operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredLease {
    /// The lease.
    pub lease: LeaseId,
    /// The worker it was granted to.
    pub worker: WorkerId,
}

impl Scheduler {
    /// What the faults of `operation` changed, to write down for a successor; `None` when
    /// they changed nothing, or the scheduler no longer holds it.
    #[must_use]
    pub fn fate(&self, operation: OperationId) -> Option<Fate> {
        let op = self.ops.get(&operation)?;
        let fate = Fate {
            booked_bytes: op.request.resources.memory_bytes,
            memory_runs: self.memory.runs(operation).to_vec(),
            farm_runs: op.farm_runs.clone(),
            avoid: op.avoid.clone(),
            not_started: op.not_started,
        };
        (!fate.is_empty()).then_some(fate)
    }

    /// Takes `operations` up in an empty scheduler, which grants leases under `term`
    /// from `next_seq` and operation ids from `next_operation` on (both past every id
    /// the store ever named, finished operations' included, so no id names two).
    ///
    /// An operation with no lease is queued. One with a lease holds it on its worker,
    /// which has not registered with this scheduler yet: for [`LEASE_GRACE`] the lease
    /// is neither given up for silence nor counted against the worker, and when the
    /// worker registers it is booked there as a lease whose `Start` went to an earlier
    /// session of the same daemon process. The worker's first heartbeat then decides: a
    /// lease it lists is kept (the run is re-bound), one it leaves out, which includes a
    /// grant that was committed but whose `Start` never reached it, goes back to the
    /// queue at once, charged to no budget. A worker that does not register in the grace
    /// has its leases given up as a silent worker's are.
    pub fn recover(
        &mut self,
        term: u64,
        next_operation: OperationId,
        next_seq: u64,
        operations: Vec<Recovered>,
    ) {
        self.term = term;
        self.next_op = next_operation.0;
        self.next_seq = next_seq;
        let until = self.now.saturating_add(LEASE_GRACE);
        for recovered in operations {
            let Recovered {
                operation: id,
                request,
                waiter,
                lease,
                losses,
                fate,
            } = recovered;
            let mut request = request;
            let Fate {
                booked_bytes,
                memory_runs,
                farm_runs,
                avoid,
                not_started,
            } = fate.unwrap_or_default();
            if booked_bytes > 0 {
                // A booking never falls: the store's request may carry a larger one than
                // the row (a floor applied at submit).
                let memory = &mut request.resources.memory_bytes;
                *memory = (*memory).max(booked_bytes);
            }
            self.memory.restore(id, memory_runs);
            if request.joinable() {
                self.in_flight.insert(request.key.clone(), id);
            }
            let committed_lease = lease.as_ref().map(|l| l.lease);
            let state = match lease {
                Some(RecoveredLease { lease, worker }) => {
                    // A session no worker has: the heartbeat reconcile takes the lease
                    // for one that went to an earlier session of the process.
                    let sent = StartSent {
                        at: self.now,
                        session: u64::MAX,
                        process: 0,
                    };
                    let held = Held {
                        operation: id,
                        booked: request.resources,
                        device: None,
                        start_sent: Some(sent),
                    };
                    self.held.insert(lease, held);
                    self.awaited.entry(worker.clone()).or_insert(until);
                    OpState::Leased {
                        lease,
                        worker,
                        committed: true,
                    }
                }
                None => {
                    self.queue.insert((Reverse(request.qos.clone()), id));
                    OpState::Queued
                }
            };
            self.ops.insert(
                id,
                Operation {
                    request,
                    waiters: vec![waiter],
                    state,
                    committed_lease,
                    result_proposed: false,
                    gave_up: false,
                    unservable: None,
                    farm_runs,
                    avoid,
                    not_started,
                    losses,
                },
            );
        }
    }

    /// Books the recovered leases of `id`, which is registering as `worker`, on it.
    pub(super) fn adopt(&mut self, id: &WorkerId, worker: &mut Worker) {
        if self.awaited.remove(id).is_none() {
            return;
        }
        for held in self.held.values_mut() {
            let op = &self.ops[&held.operation];
            if op.state.holding().is_none_or(|(_, holder)| holder != id) {
                continue;
            }
            held.booked = match op.request.kind {
                LeaseKind::WholeMachine => {
                    worker.whole = true;
                    worker.capacity
                }
                LeaseKind::Action | LeaseKind::Vm => {
                    worker.whole |= worker.exclusive(&op.request);
                    op.request.resources
                }
            };
            worker.booked = worker.booked.saturating_add(held.booked);
            worker.leases += 1;
        }
    }
}
