//! What the farm tells the invocation recorder (`docs/design/invocations.md`, section
//! 3.4): each lease granted, started and given up, and each operation's end, with
//! the lease that ended it and, for a failure, the daemon's reason. Every event is
//! sent under the farm's lock in the order the scheduler made the change, and after
//! the Execute call that queued or joined the operation, which [`Dispatch::submit`]
//! sends before it queues anything.
//!
//! Only operations a recorded call waits on are told: those [`State::record_submit`]
//! noted, which a call without an invocation id, or any call with the recorder off,
//! never is. For any other operation each seam costs a set lookup, and no event or
//! listing is built. (The `action usage:` line, for every operation, is
//! [`crate::action_usage`]'s.)
//!
//! [`Dispatch::submit`]: kbf_front::Dispatch::submit

use kbf_front::{ExecuteCall, Finished, wall_now};
use kbf_invocation::{Event, LeaseEnd, Outcome as Answered, Run};
use kbf_sched::Requeue;
use kbf_types::{Answer, Digest, LeaseGrant, LeaseId, OperationId, Outcome, Refusal, StartLease};
use tonic::Code;

use super::State;

impl State {
    /// Sends the events of an Execute call answered with `name` on the scheduler's
    /// operation that a submission of `request` gets: the twin it joins, or the next
    /// new one, and notes the operation as recorded. Call it before the request is
    /// fed, so the call comes first.
    pub(super) fn record_submit(
        &mut self,
        call: ExecuteCall,
        name: &str,
        request: &kbf_sched::Request,
    ) {
        let twin = self.sched.twin(request);
        let operation = twin.unwrap_or_else(|| self.sched.next_operation());
        self.recorded.insert(operation);
        for event in call.answered(name.to_owned(), operation, twin.is_some()) {
            self.recorder.emit(event);
        }
    }

    /// Whether a recorded call waits on `operation`.
    pub(super) fn is_recorded(&self, operation: OperationId) -> bool {
        self.recorded.contains(&operation)
    }

    pub(super) fn record_granted(&self, grant: &LeaseGrant) {
        if !self.is_recorded(grant.operation) {
            return;
        }
        self.recorder.emit(Event::Granted {
            operation: grant.operation,
            lease: grant.lease,
            worker: grant.worker.clone(),
            at: wall_now(),
        });
    }

    pub(super) fn record_started(&self, start: &StartLease) {
        if !self.is_recorded(start.operation) {
            return;
        }
        self.recorder.emit(Event::Started {
            operation: start.operation,
            lease: start.lease,
            at: wall_now(),
        });
    }

    pub(super) fn record_requeue(&self, requeue: &Requeue) {
        if !self.is_recorded(requeue.operation) {
            return;
        }
        self.recorder.emit(Event::Requeued {
            operation: requeue.operation,
            lease: requeue.lease,
            reason: requeue.reason,
            at: wall_now(),
        });
    }

    /// Sends what the report of `lease` measured, once the scheduler took it. Only a
    /// recorded operation's report is measured (`Farm::report` asks
    /// [`State::is_recorded`] before it builds `run`).
    pub(super) fn record_ran(&self, operation: OperationId, lease: LeaseId, run: Run) {
        self.recorder.emit(Event::Ran {
            operation,
            lease,
            run: Box::new(run),
        });
    }

    /// Sends the end of `answer`'s operation: its callers were told `finished`; the
    /// listing of its result, stored as `stored`, if it ran, or of a timed-out run's
    /// partial result (its logs); and `reason`, the daemon's text, if its lease
    /// reported a failure.
    pub(super) fn record_finished(
        &mut self,
        answer: &Answer,
        finished: &Finished,
        stored: Option<Digest>,
        reason: Option<String>,
    ) {
        if !self.recorded.remove(&answer.operation) {
            return;
        }
        let listing = match finished {
            Finished::Ran(result) => stored.map(|digest| kbf_front::listing(Some(digest), result)),
            Finished::TimedOut { partial, .. } => Some(kbf_front::listing(None, partial)),
            Finished::Failed(_) => None,
        };
        let end = match answer.outcome {
            Outcome::Completed { .. } => LeaseEnd::Accepted,
            Outcome::Failed(failure) => LeaseEnd::Failed(failure, reason.unwrap_or_default()),
        };
        self.recorder
            .emit(Event::Finished(kbf_invocation::Finished {
                operation: answer.operation,
                lease: Some((answer.lease, end)),
                outcome: answered(finished),
                result: listing,
                at: wall_now(),
            }));
    }

    /// Sends the end of an operation the scheduler refused: no lease ended it.
    pub(super) fn record_refused(&mut self, refusal: &Refusal) {
        if !self.recorded.remove(&refusal.operation) {
            return;
        }
        self.recorder
            .emit(Event::Finished(kbf_invocation::Finished {
                operation: refusal.operation,
                lease: None,
                outcome: Answered::Refused(refusal.reason.clone()),
                result: None,
                at: wall_now(),
            }));
    }
}

/// What callers told `finished` were answered, as a row records it (section 3.2): an
/// OK status with the exit code, a timeout (DEADLINE_EXCEEDED, with or without a
/// partial result), or another status with its code and message.
fn answered(finished: &Finished) -> Answered {
    match finished {
        Finished::Ran(result) => Answered::Ok {
            exit_code: result.exit_code,
        },
        Finished::TimedOut { .. } => Answered::TimedOut,
        Finished::Failed(status) if status.code == Code::DeadlineExceeded as i32 => {
            Answered::TimedOut
        }
        Finished::Failed(status) => Answered::Failed {
            code: status.code,
            message: status.message.clone(),
        },
    }
}
