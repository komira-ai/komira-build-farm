//! Requeues as the scheduler reports them for the log: which lease it gave up, on
//! which worker, and why (issue #166).
//!
//! A requeue changes no state outside the scheduler, so it is not an
//! [`kbf_types::Effect`]: nothing has to be carried out. The scheduler keeps a list of
//! them only when asked ([`crate::Scheduler::recording_requeues`]), for the caller to
//! drain and log ([`crate::Scheduler::take_requeues`]). Nothing the scheduler decides
//! reads that list, so replaying the same inputs gives the same state either way.

use std::fmt;

use kbf_types::{LeaseId, OperationId, WorkerId};

/// A lease given up, its operation sent back to the queue to be granted again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requeue {
    /// The operation, queued again.
    pub operation: OperationId,
    /// The lease given up.
    pub lease: LeaseId,
    /// The worker that held it.
    pub worker: WorkerId,
    /// Why it was given up.
    pub reason: RequeueReason,
}

/// Why a lease was given up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequeueReason {
    /// Its worker was not heard from for the lease grace G.
    Silent,
    /// Another daemon process registered as its worker, the handover grace has ended,
    /// and the worker does not list the lease as running.
    Replaced,
    /// Its `Start` went to an earlier session of the daemon process, which registered
    /// again and does not list the lease as running.
    Reconnected,
    /// Its `Start` has been out for the start grace and the worker does not list the
    /// lease as running.
    NotStarted,
}

impl fmt::Display for RequeueReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Silent => "the node was silent past the lease grace",
            Self::Replaced => {
                "another daemon process registered as the node and does not run it \
                 (handover grace over)"
            }
            Self::Reconnected => "the daemon reconnected and does not list it as running",
            Self::NotStarted => "the node has not listed it as running within the start grace",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches two reasons that read the same in the log (an operator could not tell
    /// a silent node from a restarted daemon), and an empty one.
    #[test]
    fn each_reason_reads_differently() {
        let all = [
            RequeueReason::Silent,
            RequeueReason::Replaced,
            RequeueReason::Reconnected,
            RequeueReason::NotStarted,
        ];
        let text: Vec<String> = all.iter().map(ToString::to_string).collect();
        for (i, a) in text.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &text[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert!(text[0].contains("silent"), "{}", text[0]);
        assert!(text[1].contains("another daemon process"), "{}", text[1]);
        assert!(text[2].contains("reconnected"), "{}", text[2]);
        assert!(text[3].contains("start grace"), "{}", text[3]);
    }
}
