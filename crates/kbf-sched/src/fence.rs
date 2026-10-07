//! Lease timing: the re-dispatch grace G on the scheduler's side and the self-fence T
//! on the worker's side.
//!
//! G also bounds how long the scheduler keeps a committed lease that a worker it still
//! hears from does not list as running ([`START_GRACE`]), and with T it bounds how late
//! a worker may act on a `Start` ([`START_VALIDITY`]).
//!
//! The scheduler re-dispatches a worker's leases once it has heard nothing from the
//! worker for G. A worker holding a [`FencePolicy::SelfFence`] lease stops it once its
//! newest acknowledged heartbeat was *sent* more than T ago. A heartbeat's send time is
//! no later than the moment the scheduler heard it, so the worker stops at most T after
//! the scheduler last heard it, and the scheduler re-dispatches no sooner than G after
//! that. `T + LEADER_LEASE_MARGIN < G` keeps the two copies apart even across a leader
//! change (leaders acknowledge only inside their leader lease).
//!
//! [`FencePolicy::SelfFence`]: kbf_types::FencePolicy::SelfFence

use std::time::Duration;

use kbf_types::FarmTime;

/// G: how long the scheduler waits after last hearing a worker before it re-dispatches
/// that worker's leases.
pub const LEASE_GRACE: Duration = Duration::from_secs(60);

/// How long after sending a lease's `Start` the scheduler keeps the lease while the
/// worker's heartbeats leave it out of their running set. Until then the `Start` may
/// still be on its way, and a heartbeat sent before it arrived rightly omits it; after
/// that the lease is taken as lost and its operation requeued. RFC section 5.8 has one
/// wait before re-dispatch, G; this is G, counted from the `Start`. It assumes a `Start`
/// that arrives later than that is not run: a worker that refuses a `Start` older than
/// [`START_VALIDITY`] makes it hold.
pub const START_GRACE: Duration = LEASE_GRACE;

/// T: how long after sending its newest acknowledged heartbeat a worker keeps running a
/// self-fenced lease.
pub const SELF_FENCE: Duration = Duration::from_secs(40);

/// The slack the safety argument keeps between T and G.
pub const LEADER_LEASE_MARGIN: Duration = Duration::from_secs(5);

// The safety condition of RFC section 5.8. Changing a constant so that it fails is a
// compile error, not a silent overlap of two runs.
const _: () =
    assert!(SELF_FENCE.as_millis() + LEADER_LEASE_MARGIN.as_millis() < LEASE_GRACE.as_millis());

/// W: how late a worker may act on a `Start`, counted from when it sent the newest
/// heartbeat the scheduler had heard when the `Start` went out. The `Start` left after
/// that heartbeat arrived, so a `Start` acted on inside W was in flight for less than W,
/// whatever the delay, and the worker measures it on its own clock.
///
/// Why `W + T + LEADER_LEASE_MARGIN < START_GRACE` keeps a late `Start` from running
/// beside its retry. A lease given up to silence is covered by the self-fence, and one
/// given up to a new session by the old stream having ended. That leaves a heartbeat of
/// the same session, taken [`START_GRACE`] or more after the `Start` was sent, that
/// leaves the lease out. A heartbeat sent after the worker started the lease
/// lists it, so that heartbeat was sent earlier, less than W after the `Start` left.
/// Heartbeats on a stream are taken in order, so no heartbeat sent after it is
/// acknowledged before the scheduler gives the lease up, and the worker's self-fence
/// stops the run less than `W + T` after the `Start` left: before the retry exists.
pub const START_VALIDITY: Duration = Duration::from_secs(14);

const _: () = assert!(
    START_VALIDITY.as_millis() + SELF_FENCE.as_millis() + LEADER_LEASE_MARGIN.as_millis()
        < START_GRACE.as_millis()
);

/// A worker's self-fence clock, kept by the daemon.
///
/// The worker records the send time of each heartbeat the scheduler acknowledges. It
/// may start or keep running a self-fenced lease only while the newest of those is
/// less than [`SELF_FENCE`] old. A worker that has never been acknowledged may not run
/// one at all. Lease kinds with [`FencePolicy::RunOn`] ignore the fence.
///
/// [`FencePolicy::RunOn`]: kbf_types::FencePolicy::RunOn
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelfFence {
    newest_acked_send: Option<FarmTime>,
}

impl SelfFence {
    /// A fence that has seen no acknowledgement: it allows nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            newest_acked_send: None,
        }
    }

    /// The scheduler acknowledged the heartbeat this worker sent at `sent_at`. An older
    /// acknowledgement arriving late never moves the deadline back.
    pub fn acknowledged(&mut self, sent_at: FarmTime) {
        self.newest_acked_send = Some(self.newest_acked_send.map_or(sent_at, |t| t.max(sent_at)));
    }

    /// The time at which self-fenced leases must stop, if any heartbeat was acknowledged.
    #[must_use]
    pub fn deadline(&self) -> Option<FarmTime> {
        self.newest_acked_send.map(|t| t.saturating_add(SELF_FENCE))
    }

    /// Whether a self-fenced lease may run at `now`.
    #[must_use]
    pub fn allows(&self, now: FarmTime) -> bool {
        self.deadline().is_some_and(|d| now < d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> FarmTime {
        FarmTime::from_millis(secs * 1_000)
    }

    /// Catches: a fence that allows work before any acknowledgement, measures T from
    /// the acknowledgement's arrival instead of the heartbeat's send time, or lets a
    /// late, older acknowledgement shorten the deadline.
    #[test]
    fn fence_runs_t_from_newest_acked_send() {
        let mut fence = SelfFence::new();
        assert!(!fence.allows(at(0)));
        fence.acknowledged(at(10));
        assert_eq!(fence.deadline(), Some(at(50)));
        assert!(fence.allows(at(49)));
        assert!(!fence.allows(at(50)));
        fence.acknowledged(at(20));
        fence.acknowledged(at(15));
        assert_eq!(fence.deadline(), Some(at(60)));
    }
}
