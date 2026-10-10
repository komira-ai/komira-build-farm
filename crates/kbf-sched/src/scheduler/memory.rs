//! Memory kills (failure classes, 6.1): the ladder an action climbs after it passes its
//! own memory limit, the floor it leaves for its action key, the farm reruns of a busy
//! node's kills, and the count of those kills per node.
//!
//! Two kills, told apart by the worker's report and handled apart:
//!
//! - **The action passed its own limit** ([`Failure::OutOfMemory`]). The operation runs
//!   again with its memory booking doubled, rounded up to whole GiB, and never past the
//!   **cap**: the memory of the largest node that could run it (one that is live,
//!   cordoned or not, serves its lease kind, satisfies its platform and holds its CPU
//!   and GPU request; the node the killed run used always counts). A doubling past the
//!   cap books the cap. A kill of a run that booked the cap finishes the operation with
//!   [`Failure::OutOfMemory`]: the action needs more memory than any node offers. Each
//!   raised booking is kept as the action key's memory floor, and a later submission
//!   of that key books at least the floor.
//! - **The node killed it under pressure** ([`Failure::NodeMemoryPressure`]): a node-wide
//!   kill of a lease that never reached its own limit. The node's memory-pressure count
//!   goes up and the operation runs again with the same booking; it never raises the
//!   booking or the floor.
//!
//! **The two budgets.** A rung of the ladder is not a farm rerun, and a farm rerun is
//! not a rung: the ladder is bounded by itself (from booking `b` to cap `c`, at most
//! ceil(log2(c / b)) reruns), and busy-node kills by [`FARM_RERUNS`]. Neither uses the
//! other's count, so a long ladder never leaves a busy-node kill without its reruns,
//! and busy-node kills never cut a ladder short of the cap. An operation therefore runs
//! at most `1 + ceil(log2(c / b)) + FARM_RERUNS` times. Leases given up for silence,
//! replacement, reconnection or not starting use neither.
//!
//! The floors live in this scheduler alone: a server restart forgets them, which costs
//! one more killed run per key, never a wrong answer. At most [`MEMORY_FLOORS`] are kept;
//! past that the one raised longest ago is forgotten first. A floor never falls.

use std::collections::BTreeMap;

use kbf_types::{ActionKey, Failure, MemoryKill, MemoryRun, OperationId, ResultRecord, WorkerId};

use super::Scheduler;
use crate::input::Request;
use crate::requeue::RequeueReason;
use crate::servable::serves;

/// One GiB, the step a raised booking is rounded up to.
pub const GIB: u64 = 1 << 30;

/// How many times an operation runs again after busy-node memory kills (so at most
/// `1 + FARM_RERUNS` runs end that way); the farm budget of failure classes 6.5. The
/// ladder of the action's own memory kills does not count here.
pub const FARM_RERUNS: u32 = 2;

/// The most memory floors a scheduler keeps; the default of [`Scheduler::new`].
pub const MEMORY_FLOORS: usize = 65_536;

/// The booking after a run that booked `booked` bytes passed its own limit, when the
/// largest node that could run it has `cap`: twice `booked` (at least 1 GiB), rounded
/// up to whole GiB, and at most `cap`. `None` when `booked` is already the cap or more:
/// no node could give it more.
#[must_use]
pub fn raised(booked: u64, cap: u64) -> Option<u64> {
    (booked < cap).then(|| {
        let doubled = booked.saturating_mul(2).max(GIB);
        doubled.div_ceil(GIB).saturating_mul(GIB).min(cap)
    })
}

/// The memory floors by action key, each with the order it was raised in, and the keys
/// in that order, so the oldest is forgotten first once there are too many.
#[derive(Clone, Debug)]
pub(super) struct Floors {
    asks: BTreeMap<ActionKey, (u64, u64)>,
    order: BTreeMap<u64, ActionKey>,
    next: u64,
    bound: usize,
}

impl Floors {
    pub(super) const fn new(bound: usize) -> Self {
        Self {
            asks: BTreeMap::new(),
            order: BTreeMap::new(),
            next: 0,
            bound,
        }
    }

    fn get(&self, key: &ActionKey) -> Option<u64> {
        self.asks.get(key).map(|&(ask, _)| ask)
    }

    /// Raises `key`'s floor to `ask` (it never falls), and makes it the newest.
    fn raise(&mut self, key: &ActionKey, ask: u64) {
        let stamp = self.next;
        self.next += 1;
        let before = self.asks.remove(key).map_or(0, |(before, at)| {
            self.order.remove(&at);
            before
        });
        self.asks.insert(key.clone(), (before.max(ask), stamp));
        self.order.insert(stamp, key.clone());
        while self.asks.len() > self.bound
            && let Some((_, oldest)) = self.order.pop_first()
        {
            self.asks.remove(&oldest);
        }
    }
}

impl Scheduler {
    /// This scheduler, keeping at most `bound` memory floors (instead of
    /// [`MEMORY_FLOORS`]); past that, the one raised longest ago is forgotten first.
    #[must_use]
    pub fn with_memory_floors(mut self, bound: usize) -> Self {
        self.floors = Box::new(Floors::new(bound));
        self
    }

    /// The memory floor of `key`, in bytes: the largest booking a run of it was raised
    /// to after it passed its own memory limit. A new submission of `key` books at
    /// least this much memory.
    #[must_use]
    pub fn memory_floor(&self, key: &ActionKey) -> Option<u64> {
        self.floors.get(key)
    }

    /// The runs of `operation` killed for memory, oldest first.
    #[must_use]
    pub fn memory_runs(&self, operation: OperationId) -> Option<&[MemoryRun]> {
        self.ops.get(&operation).map(|op| op.memory_runs.as_slice())
    }

    /// How many leases `worker` has killed under memory pressure, below the action's
    /// own limit, since it first registered.
    #[must_use]
    pub fn memory_pressure(&self, worker: &WorkerId) -> u64 {
        self.workers.get(worker).map_or(0, |w| w.memory_pressure)
    }

    /// Raises `request`'s memory booking to its key's floor, if it has one.
    pub(super) fn floored(&self, mut request: Request) -> Request {
        if let Some(floor) = self.floors.get(&request.key) {
            let memory = &mut request.resources.memory_bytes;
            *memory = (*memory).max(floor);
        }
        request
    }

    /// A committed memory kill (`failure` is [`Failure::OutOfMemory`] or
    /// [`Failure::NodeMemoryPressure`]) of `record`'s operation, whose newest committed
    /// grant is the record's lease. Returns whether the operation runs again; when it
    /// does not, the caller finishes it with `failure`.
    ///
    /// It runs again, with no new memory run recorded, when it no longer holds the
    /// killed lease (the lease was given up, and the operation waits to be granted
    /// again at its booking). Otherwise the run is recorded and the kill decides, as
    /// the module says.
    pub(super) fn rerun_after_memory_kill(
        &mut self,
        record: &ResultRecord,
        failure: Failure,
    ) -> bool {
        let id = record.operation;
        let op = &self.ops[&id];
        let Some((lease, worker)) = op.state.holding().filter(|(l, _)| *l == record.lease) else {
            return true;
        };
        let worker = worker.clone();
        // A leased operation's lease is always held: no branch on it.
        let booked = self
            .held
            .get(&lease)
            .map_or(op.request.resources.memory_bytes, |h| h.booked.memory_bytes);
        if failure == Failure::OutOfMemory {
            let cap = self.memory_cap(&op.request, &worker, booked);
            let raised = raised(booked, cap);
            let op = self.ops.get_mut(&id).expect("checked above");
            op.memory_runs.push(MemoryRun {
                worker,
                booked,
                kill: MemoryKill::OwnLimit { cap },
            });
            let Some(raised) = raised else {
                return false;
            };
            op.request.resources.memory_bytes = raised;
            let key = op.request.key.clone();
            self.floors.raise(&key, raised);
            self.requeue(id, RequeueReason::OutOfMemory { booked, raised });
            return true;
        }
        // A worker that held a lease is registered: no branch on it.
        let _ = self
            .workers
            .get_mut(&worker)
            .map(|w| w.memory_pressure += 1);
        let op = self.ops.get_mut(&id).expect("checked above");
        op.memory_runs.push(MemoryRun {
            worker,
            booked,
            kill: MemoryKill::NodePressure,
        });
        if op.farm_reruns >= FARM_RERUNS {
            return false;
        }
        op.farm_reruns += 1;
        self.requeue(id, RequeueReason::NodeMemoryPressure);
        true
    }

    /// The memory of the largest node that could run `request`, in bytes: one that is
    /// live (cordoned or not), serves its lease kind, satisfies its platform and holds
    /// its CPU and GPU request, and always `ran_on`, the node its killed run used. A
    /// node that is gone does not count; `booked` when none is registered.
    fn memory_cap(&self, request: &Request, ran_on: &WorkerId, booked: u64) -> u64 {
        let wanted = request.resources;
        self.workers
            .iter()
            .filter(|(name, w)| {
                *name == ran_on
                    || (w.alive(self.now)
                        && serves(&w.caps, request.kind)
                        && request.needs.matches(&w.caps)
                        && w.capacity.cpu_millis >= wanted.cpu_millis
                        && w.capacity.gpus >= wanted.gpus)
            })
            .map(|(_, w)| w.capacity.memory_bytes)
            .max()
            .unwrap_or(booked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a ladder that does not double (1 -> 3, or 1 -> 1), one that does not
    /// round a fraction of a GiB up, one that passes the cap, and one that offers a rung
    /// at or past the cap (a kill at the largest node would run again, unbounded).
    #[test]
    fn the_ladder_doubles_in_whole_gib_up_to_the_cap() {
        assert_eq!(raised(GIB, 64 * GIB), Some(2 * GIB));
        assert_eq!(raised(2 * GIB, 64 * GIB), Some(4 * GIB));
        assert_eq!(raised(4 * GIB, 64 * GIB), Some(8 * GIB));
        assert_eq!(raised(GIB / 2, 64 * GIB), Some(GIB), "at least 1 GiB");
        assert_eq!(raised(0, 64 * GIB), Some(GIB), "an unbooked run");
        assert_eq!(raised(3 * GIB / 2, 64 * GIB), Some(3 * GIB));
        assert_eq!(raised(GIB + 1, 64 * GIB), Some(3 * GIB), "rounded up");
        assert_eq!(raised(40 * GIB, 64 * GIB), Some(64 * GIB), "the cap");
        assert_eq!(
            raised(5 * GIB, 6 * GIB + 7),
            Some(6 * GIB + 7),
            "an odd cap"
        );
        assert_eq!(raised(64 * GIB, 64 * GIB), None, "killed at the cap");
        assert_eq!(raised(65 * GIB, 64 * GIB), None, "past the cap");
        assert_eq!(
            raised(u64::MAX - 1, u64::MAX),
            Some(u64::MAX),
            "no overflow"
        );
    }

    fn key(n: u8) -> ActionKey {
        ActionKey {
            instance: "main".to_owned(),
            action: kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [n; 32], 1),
        }
    }

    /// Catches a floor that falls when a smaller ask is raised, a bound that is not
    /// held, and eviction of the newest floor (or of one just raised again) instead of
    /// the one raised longest ago.
    #[test]
    fn floors_never_fall_and_the_oldest_goes_first() {
        let mut floors = Floors::new(2);
        floors.raise(&key(1), 4 * GIB);
        floors.raise(&key(1), 2 * GIB);
        assert_eq!(floors.get(&key(1)), Some(4 * GIB), "a floor fell");
        floors.raise(&key(2), GIB);
        // key(1) raised again: key(2) is now the oldest.
        floors.raise(&key(1), 8 * GIB);
        floors.raise(&key(3), GIB);
        assert_eq!(floors.get(&key(1)), Some(8 * GIB));
        assert_eq!(floors.get(&key(2)), None, "the oldest is kept");
        assert_eq!(floors.get(&key(3)), Some(GIB));
        assert_eq!(floors.asks.len(), 2);
        assert_eq!(floors.order.len(), 2);
    }
}
