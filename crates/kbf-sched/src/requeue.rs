//! Requeues as the scheduler reports them for the log: which lease it gave up, on
//! which worker, and why (issue #166). A lease that ended killed for memory and is run
//! again is one too (failure classes, 6.1).
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
    /// The action passed its own memory limit with `booked` bytes booked, and runs
    /// again with `raised` bytes booked.
    OutOfMemory {
        /// The memory booking of the run that was killed, in bytes.
        booked: u64,
        /// The memory booking of the next run, in bytes.
        raised: u64,
    },
    /// The node killed the lease for memory while the action was under its own limit;
    /// it runs again with the same booking.
    NodeMemoryPressure,
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
            Self::OutOfMemory { booked, raised } => {
                return write!(
                    f,
                    "the action passed its memory limit with {} booked; it runs again with \
                     {} booked",
                    Gib(*booked),
                    Gib(*raised)
                );
            }
            Self::NodeMemoryPressure => {
                "the node killed it for memory below the action's own limit (node memory \
                 pressure); it runs again with the same booking"
            }
        })
    }
}

/// A memory size in bytes, written in GiB: whole (`2 GiB`) when it is a whole number of
/// GiB, else to two decimals (`1.50 GiB`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gib(pub u64);

impl fmt::Display for Gib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const GIB: u64 = 1 << 30;
        if self.0.is_multiple_of(GIB) {
            write!(f, "{} GiB", self.0 / GIB)
        } else {
            // Rounded to the nearest hundredth, in integers (no float in a pure crate's
            // output); u128 so no size overflows.
            let hundredths = (u128::from(self.0) * 100 + u128::from(GIB / 2)) / u128::from(GIB);
            write!(f, "{}.{:02} GiB", hundredths / 100, hundredths % 100)
        }
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
            RequeueReason::OutOfMemory {
                booked: 1 << 30,
                raised: 2 << 30,
            },
            RequeueReason::NodeMemoryPressure,
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
        assert!(text[4].contains("with 1 GiB booked"), "{}", text[4]);
        assert!(text[4].contains("with 2 GiB booked"), "{}", text[4]);
        assert!(text[5].contains("same booking"), "{}", text[5]);
    }

    /// Catches: a size shown in bytes, or rounded to a whole GiB it is not (an operator
    /// would read a 1.5 GiB booking as 1 or 2).
    #[test]
    fn sizes_read_in_gib() {
        assert_eq!(Gib(1 << 30).to_string(), "1 GiB");
        assert_eq!(Gib(64 << 30).to_string(), "64 GiB");
        assert_eq!(Gib(3 << 29).to_string(), "1.50 GiB");
        assert_eq!(Gib(0).to_string(), "0 GiB");
        assert_eq!(Gib(u64::MAX).to_string(), "17179869184.00 GiB");
        assert_eq!(Gib((1 << 30) + 1).to_string(), "1.00 GiB");
    }
}
