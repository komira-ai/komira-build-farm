//! How long the roll waits for each thing: [`Timing`].

use std::time::Duration;

use crate::inventory::Role;

/// How long the roll waits for each thing. The defaults are the package-update design's
/// (node health 300 s, daemon 780 s, no answer 120 s); the safe-to-stop
/// bound and the cap on an install in progress are this crate's choices.
#[derive(Clone, Debug)]
pub struct Timing {
    /// Between two asks of the same port.
    pub poll: Duration,
    /// How long a host may stay unsafe to stop (or safe-to-stop unanswered) before the roll
    /// fails at it.
    pub safe_bound: Duration,
    /// How long a restart waits for a host that answers "install in progress".
    pub install_bound: Duration,
    /// How long a restart waits for a host that does not answer a query at all.
    pub query_bound: Duration,
    pub healthy_storage: Duration,
    pub healthy_daemon: Duration,
    /// How long a daemon host's node is given to finish its leases once it is drained. When
    /// the time is out the host is installed anyway, with the leases it still holds.
    pub drain_deadline: Duration,
    /// Between two asks of a Mac host how its apply stands.
    pub apply_poll: Duration,
    /// How long a Mac host's apply may take, from the answer `apply_started` (or from a
    /// restart that resumes the wait), to be `done` for the target.
    pub apply_deadline: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            poll: Duration::from_secs(5),
            safe_bound: Duration::from_secs(300),
            install_bound: Duration::from_secs(900),
            query_bound: Duration::from_secs(120),
            healthy_storage: Duration::from_secs(300),
            healthy_daemon: Duration::from_secs(780),
            drain_deadline: Duration::from_secs(600),
            apply_poll: Duration::from_secs(5),
            apply_deadline: Duration::from_secs(600),
        }
    }
}

impl Timing {
    pub(crate) fn healthy(&self, role: Role) -> Duration {
        match role {
            Role::Frontend | Role::Storage => self.healthy_storage,
            Role::Daemon | Role::MacDaemon => self.healthy_daemon,
        }
    }
}
