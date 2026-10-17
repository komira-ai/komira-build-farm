//! The cap on how many times an operation's lease may be lost ([`LOSS_REQUEUES`]).
//!
//! A lease is lost when it is given up for a silent node, a replaced or restarted
//! daemon, or a daemon that reconnected without it ([`crate::RequeueReason::is_loss`]); a
//! restart of the scheduler itself is one of these, so an action that takes its host
//! down is counted whichever process notices. The count is separate from the farm
//! reruns a worker's own report spends ([`crate::FARM_RERUNS`]) and from the
//! not-started budget ([`crate::NOT_STARTED_RERUNS`]): a lease lost to a reconnect
//! charges neither. It is kept by the caller across restarts ([`crate::Recovered::losses`]).

use std::cmp::Reverse;

use kbf_types::{ControlRecord, Effect, OperationId, Qos, RefusalRecord};

use super::Scheduler;

/// How many leases an operation may lose before it is refused instead of run again: the
/// loss that reaches it refuses the operation, so the third loss does.
pub const LOSS_REQUEUES: u32 = 3;

impl Scheduler {
    /// This scheduler, refusing an operation whose lease has been lost `cap` times
    /// (instead of requeueing it again for ever). The refusal is committed through the
    /// log like an unservable one, and answers the callers with its reason. Off by
    /// default: [`Scheduler::new`] never refuses for losses.
    #[must_use]
    pub const fn with_loss_requeues(mut self, cap: u32) -> Self {
        self.loss_cap = cap;
        self
    }

    /// Proposes the refusal of every queued operation that has lost its lease as many
    /// times as the cap allows, and takes it out of the queue: nothing places it while
    /// the refusal is on its way to the log.
    pub(super) fn refuse_lost(&mut self, effects: &mut Vec<Effect>) {
        let cap = self.loss_cap;
        let lost: Vec<(Reverse<Qos>, OperationId)> = self
            .queue
            .iter()
            .filter(|(_, id)| self.ops[id].losses >= cap)
            .cloned()
            .collect();
        for entry in lost {
            self.queue.remove(&entry);
            let operation = entry.1;
            let times = self.ops[&operation].losses;
            let reason = format!(
                "its lease was lost {times} times (a node silent past the lease grace, or a \
                 daemon that restarted, was replaced or reconnected without it), so the farm \
                 does not run it again"
            );
            effects.push(Effect::Commit(ControlRecord::Refusal(RefusalRecord {
                operation,
                reason,
            })));
        }
    }
}
