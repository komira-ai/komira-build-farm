//! The state store's outage as the scheduler is told of it: nothing is given up or
//! placed while it is down, and expiry timers restart with a grace window when it
//! returns.
//!
//! The scheduler keeps its leases' deadlines in memory, judged by the farm's clock. An
//! outage keeps the farm from hearing its workers (its writes wait on the store), so
//! when the store returns every worker looks silent and every lease expired. Without the
//! window the first tick would requeue all running work at once. Only that silence is
//! waited out: a heartbeat that comes in is fresh evidence of what its worker runs.

use kbf_types::FarmTime;

use super::Scheduler;
use crate::fence::LEASE_GRACE;

impl Scheduler {
    /// The state store refused a write: nothing is given up or placed until
    /// [`Scheduler::store_available`], because neither could be written down.
    pub const fn store_unavailable(&mut self) {
        self.store_down = true;
    }

    /// The state store took a write again at `now`. Every expiry timer restarts: for
    /// [`LEASE_GRACE`] after `now` no lease is given up for the silence of its worker, so
    /// an outage that kept the scheduler from hearing its workers does not requeue all
    /// running work at once. A worker that is not heard from by then has its leases given
    /// up as a silent worker's are. A heartbeat is not held back: the running set it lists
    /// is heard now, and a lease it leaves out is given up as ever.
    pub fn store_available(&mut self, now: FarmTime) {
        self.store_down = false;
        self.grace_until = now.saturating_add(LEASE_GRACE);
    }

    /// Whether giving a lease up for silence waits: the store is down, or has not been back
    /// for the grace.
    pub(super) fn settling(&self) -> bool {
        self.store_down || self.now < self.grace_until
    }
}
