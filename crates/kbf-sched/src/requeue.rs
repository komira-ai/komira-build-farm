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

use kbf_types::{LeaseId, LostWhy, OperationId, WorkerId};

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
    /// Its worker switched the GUI session for it and was silent past the session
    /// switch grace.
    SessionSwitch,
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
    /// Another daemon process registered as its worker, naming the process its `Start`
    /// went to as its predecessor, which the restarted daemon's drivers killed every
    /// run of before it registered; the worker does not list the lease as running.
    Restarted,
    /// The worker reported that the farm lost the run (`why`); it runs again.
    Lost(LostWhy),
    /// The worker reported that the farm could not run the action
    /// ([`kbf_types::Failure::Infra`]); it runs again.
    Infra,
}

impl RequeueReason {
    /// Whether the give-up follows a kill for memory, which changes what the scheduler
    /// books and holds for the operation (a raised booking, a memory run).
    #[must_use]
    pub const fn is_memory_kill(&self) -> bool {
        matches!(self, Self::OutOfMemory { .. } | Self::NodeMemoryPressure)
    }

    /// Whether the give-up is a lost lease: the node was silent, the daemon was
    /// replaced, restarted or reconnected without the lease. They count toward the cap
    /// on lost leases ([`crate::LOSS_REQUEUES`]). The ones a worker reports, the memory
    /// ladder's, a switch of the GUI session and a `Start` that never arrived have
    /// budgets of their own.
    #[must_use]
    pub const fn is_loss(&self) -> bool {
        matches!(
            self,
            Self::Silent | Self::Replaced | Self::Reconnected | Self::Restarted
        )
    }
}

impl fmt::Display for RequeueReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Silent => "the node was silent past the lease grace",
            Self::Replaced => {
                "another daemon process registered as the node and does not run it \
                 (handover grace over)"
            }
            Self::SessionSwitch => {
                "the node was silent past the session switch grace while it switched the \
                 GUI session for the lease"
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
            Self::Restarted => {
                "the daemon restarted, and its successor killed the run before registering"
            }
            Self::Lost(why) => return write!(f, "the farm lost the run ({why}); it runs again"),
            Self::Infra => "the farm could not run the action (infra failure); it runs again",
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
            RequeueReason::SessionSwitch,
            RequeueReason::Restarted,
            RequeueReason::Lost(LostWhy::Fenced),
            RequeueReason::Lost(LostWhy::DaemonRestarted),
            RequeueReason::Infra,
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
        assert!(text[6].contains("session switch grace"), "{}", text[6]);
        assert!(
            text[7].contains("killed the run before registering"),
            "{}",
            text[7]
        );
        assert!(text[8].contains("fenced"), "{}", text[8]);
        assert!(text[9].contains("daemon restarted while"), "{}", text[9]);
    }

    /// Catches: a reason that counts toward the cap on lost leases though its own
    /// budget already bounds it (a worker's report, the memory ladder, a GUI session
    /// switch, a `Start` that never arrived), and a lost lease left out of the count.
    #[test]
    fn only_lost_leases_count_toward_the_cap() {
        let lost = [
            RequeueReason::Silent,
            RequeueReason::Replaced,
            RequeueReason::Reconnected,
            RequeueReason::Restarted,
        ];
        let budgeted = [
            RequeueReason::NotStarted,
            RequeueReason::SessionSwitch,
            RequeueReason::OutOfMemory {
                booked: 1 << 30,
                raised: 2 << 30,
            },
            RequeueReason::NodeMemoryPressure,
            RequeueReason::Lost(LostWhy::Fenced),
            RequeueReason::Infra,
        ];
        assert!(lost.iter().all(RequeueReason::is_loss));
        assert!(!budgeted.iter().any(RequeueReason::is_loss));
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
