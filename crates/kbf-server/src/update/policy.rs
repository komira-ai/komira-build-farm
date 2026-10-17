//! The numbers the node step and the picker compare against
//! (`docs/design/split-mode-updates.md` sections 2.6 to 2.10; the design is #898).

use std::time::Duration;

use super::facts::Platform;

/// Every limit of a node update. [`Policy::default`] is the design's defaults; the
/// controller overrides the ones `update/policy` names (`idle_wait`, `min_serving`,
/// `mac_force_after`).
///
/// A field marked *not in the design* has no value there: the table names the failure but
/// not how long to wait for it. Each is the controller's backstop, so that a silent
/// helper or a stuck upload holds the run instead of hanging it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// A member row's time to live. A member not heard within this counts as down
    /// (`MembershipConfig::ttl`, 15 s).
    pub ttl: Duration,
    /// How long a `Down` member stays in the map (`MembershipConfig::dwell`, 300 s).
    pub dwell: Duration,
    /// How long the picker waits for a node to go idle before it cordons the least-loaded
    /// one (D3, 2 minutes), counted from the run's start or the last node's uncordon.
    pub idle_wait: Duration,
    /// Section 2.8, the capacity floor: a host is cordoned only if at least this many
    /// other hosts of its platform (a Linux host, or a Mac of its class) stay connected and
    /// uncordoned. *Not in the design* (it names the field, not its unit or a value): a
    /// count of hosts, and 0 by default, which is no floor. A farm that must never run
    /// out of capacity sets it; the last Mac of a class has its own rule (section 2.10).
    pub min_serving: usize,
    /// Step 3 on Linux: how long to wait for leases to end by themselves (15 minutes).
    pub lease_limit: Duration,
    /// Step 3 on a Mac: a Mac lease may last an hour (60 minutes).
    pub mac_lease_limit: Duration,
    /// Step 4 (and 5): how long to wait for open write sessions and routed writes to reach
    /// zero. *Not in the design* (C31 names the limit, not the value): 60 seconds. The mark
    /// written at step 4 lives for the dwell whatever the node does, so the waits of steps 4
    /// and 5 must fit well inside [`Policy::restart_deadline`] ([`Policy::validate`]).
    pub write_limit: Duration,
    /// Step 6: how long to wait for the helper to report `ACTIVATED_UNCONFIRMED`. *Not in
    /// the design* (the helper's own revert window caps the host's side): 5 minutes.
    pub activate_limit: Duration,
    /// How long a claim, a cordon, an intact node's return to `Active` and a confirm may
    /// take to show (steps 1, 2, 7, 9 and 10; 120 s). Steps 1, 2, 9 and 10 are *not in the
    /// design*: its table names the failure of each but not the wait.
    pub health_limit: Duration,
    /// Step 7: how long a rebuilding (`CatchingUp`) node may take. The design says only
    /// "wait while it makes progress, never revert"; `None` waits without a bound until
    /// the rebuild reports its progress (#860), and the step alerts at the health limit.
    pub rebuild_limit: Option<Duration>,
    /// Step 8: how long the debts of the downtime may take to settle (5 minutes).
    pub repair_wait: Duration,
    /// Section 2.10: after this long without an idle Mac, the least-loaded Mac is
    /// cordoned as D3 does a Linux host. `None` (the default) never forces a Mac.
    pub mac_force_after: Option<Duration>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(15),
            dwell: Duration::from_secs(300),
            idle_wait: Duration::from_secs(120),
            min_serving: 0,
            lease_limit: Duration::from_secs(15 * 60),
            mac_lease_limit: Duration::from_secs(60 * 60),
            write_limit: Duration::from_secs(60),
            activate_limit: Duration::from_secs(5 * 60),
            health_limit: Duration::from_secs(120),
            rebuild_limit: None,
            repair_wait: Duration::from_secs(5 * 60),
            mac_force_after: None,
        }
    }
}

/// Why a [`Policy`] cannot be kept.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The dwell leaves no time between the mark and the restart deadline.
    #[error("the dwell {dwell:?} is not longer than two time-to-lives ({ttl:?})")]
    DwellTooShort {
        /// The dwell.
        dwell: Duration,
        /// The time to live.
        ttl: Duration,
    },
    /// Steps 4 and 5 may each wait a write limit, and the mark lives only to the deadline.
    #[error("steps 4 and 5 may wait {waits:?} but the restart deadline is {deadline:?}")]
    WritesOutlastRestartDeadline {
        /// Two write limits.
        waits: Duration,
        /// The restart deadline.
        deadline: Duration,
    },
}

impl Policy {
    /// How long step 3 waits for the leases of a host on `platform`.
    #[must_use]
    pub const fn lease_limit_for(&self, platform: &Platform) -> Duration {
        match platform {
            Platform::Linux => self.lease_limit,
            Platform::Mac(_) => self.mac_lease_limit,
        }
    }

    /// How long a planned-down node may take to be back: the dwell less two time-to-lives
    /// (section 2.6: "alerts at dwell minus two TTLs instead of waiting for the dwell to
    /// decide"), because the dwell's end drops the member and starts re-replication.
    #[must_use]
    pub fn restart_deadline(&self) -> Duration {
        self.dwell.saturating_sub(self.ttl.saturating_mul(2))
    }

    /// Checks the one invariant the limits share: the waits that happen while the node is
    /// marked down (steps 4 and 5, one write limit each) end before the restart deadline,
    /// which is measured from the mark.
    ///
    /// # Errors
    /// [`PolicyError`] naming the limit that cannot be kept.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.dwell <= self.ttl.saturating_mul(2) {
            return Err(PolicyError::DwellTooShort {
                dwell: self.dwell,
                ttl: self.ttl,
            });
        }
        let waits = self.write_limit.saturating_mul(2);
        let deadline = self.restart_deadline();
        if waits >= deadline {
            return Err(PolicyError::WritesOutlastRestartDeadline { waits, deadline });
        }
        Ok(())
    }
}
