//! Memory kills (failure classes, 6.1): the ladder an action climbs after it passes its
//! own memory limit, the floor it leaves for its action key, the farm reruns of a busy
//! node's kills, and the count of those kills per node.
//!
//! Two kills, told apart by the worker's report and handled apart:
//!
//! - **The action passed its own limit** ([`Failure::OutOfMemory`]). The operation runs
//!   again on the next rung of a fixed [`MemoryLadder`] (4, 16, 32 GiB by default), and
//!   after the last rung on a whole host. The first booking is the front's: the first
//!   rung for an action that names no size, else the size it names, and then the
//!   ladder climbs from there. A kill books the first rung above what the killed run
//!   booked, unless that rung is not below the **cap**, the memory of the largest node
//!   that could run it (one that is live, cordoned or not, serves its lease kind,
//!   satisfies its platform and holds its CPU and GPU request; the node the killed run
//!   used always counts): then it books the cap, a whole host: it is placed only on a
//!   node with no lease and no other lease is placed there while it runs (a booking of
//!   memory is a limit, not a share of the node's, so this is the one case where the
//!   limit keeps others out). A kill of a run that booked the cap
//!   finishes the operation with [`Failure::OutOfMemory`]: the action needs more memory
//!   than any node offers. Each raised booking is kept as the action key's memory
//!   floor, and a later submission of that key books at least the floor, so a
//!   known-big action starts at the rung that ran it.
//! - **The node killed it under pressure** ([`Failure::NodeMemoryPressure`]): a node-wide
//!   kill of a lease that never reached its own limit. The node's memory-pressure count
//!   goes up and the operation runs again with the same booking, preferring another
//!   node, as a farm fault (`super::fault`); it never raises the booking or the floor.
//!
//! **The two budgets.** A rung of the ladder is not a farm rerun, and a farm rerun is
//! not a rung: the ladder is bounded by itself (at most one rerun per rung above the
//! first booking, and one for the whole host), and farm faults (busy-node kills and
//! lost runs together) by [`FARM_RERUNS`]. Neither uses the other's count, so a long ladder never leaves a
//! busy-node kill without its reruns, and farm faults never cut a ladder short of the
//! cap. An operation therefore runs at most `1 + rungs + 1 + FARM_RERUNS`
//! times. Leases given up for silence or replacement use
//! the farm's alone, leases given up for not starting [`NOT_STARTED_RERUNS`] alone, and
//! reconnections neither.
//!
//! The floors live in this scheduler alone: a server restart forgets them, which costs
//! one more killed run per key, never a wrong answer. At most [`MEMORY_FLOORS`] are kept;
//! past that the one raised longest ago is forgotten first. A floor never falls.
//!
//! **One value.** Everything this module keeps (the floors and the order they were
//! raised in, each held operation's memory runs, each node's busy-node kills) is one
//! [`MemoryState`], which compares with `Eq`: two schedulers
//! given the same inputs hold equal ones (see [`Scheduler::memory_state`]).

use std::collections::BTreeMap;

use kbf_types::{
    ActionKey, Failure, FarmFault, MemoryKill, MemoryRun, OperationId, ResultRecord, WorkerId,
};

use super::Scheduler;
use crate::input::Request;
use crate::requeue::RequeueReason;
use crate::servable::serves;

/// One GiB, the unit of a rung and of a booking that names a size.
pub const GIB: u64 = 1 << 30;

/// How many times an operation runs again after runs that ended as the farm's fault
/// (so at most `1 + FARM_RERUNS` runs end that way): busy-node memory kills and lost
/// runs together, one budget (`fault`); the farm budget of failure classes 6.5. The
/// ladder of the action's own memory kills does not count here.
pub const FARM_RERUNS: u32 = 2;

/// How many times an operation runs again after leases given up as not started (the
/// node heartbeats but never listed the lease, `fault`): its own budget, apart from
/// [`FARM_RERUNS`], because a run that never started is always safe to run elsewhere
/// and must not spend the farm's. Small, since a node that drops one `Start` and then
/// another is not going to take the third, and each give-up costs a whole start grace;
/// the give-up past it ends the operation.
pub const NOT_STARTED_RERUNS: u32 = 3;

/// The most memory floors a scheduler keeps; the default of [`Scheduler::new`].
pub const MEMORY_FLOORS: usize = 65_536;

/// The fixed memory rungs of an action: what an unsized action books first (the front
/// books it), and the bookings it climbs through each time it passes its own memory
/// limit, before a whole host (the cap of [`MemoryLadder::raised`]). A rung is a whole
/// number of GiB; the rungs ascend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryLadder {
    rungs: Vec<u64>,
}

/// Why a list of rungs is not a [`MemoryLadder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderError {
    /// No rung: an unsized action would book nothing.
    Empty,
    /// A rung of 0 GiB.
    Zero,
    /// A rung that does not rise above the one before it (equal or lower).
    NotAscending {
        /// The rung's position in the list, from 0.
        at: usize,
    },
    /// A rung too large to count in bytes.
    TooLarge {
        /// The rung's position in the list, from 0.
        at: usize,
    },
}

impl std::fmt::Display for LadderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("a memory ladder needs at least one rung"),
            Self::Zero => f.write_str("a memory rung must be at least 1 GiB"),
            Self::NotAscending { at } => {
                write!(f, "memory rung {at} does not rise above the one before it")
            }
            Self::TooLarge { at } => write!(f, "memory rung {at} is too large to book"),
        }
    }
}

impl std::error::Error for LadderError {}

impl MemoryLadder {
    /// The default rungs, in GiB: an unsized action books 4, then 16, then 32, then a
    /// whole host.
    pub const DEFAULT_GIB: [u64; 3] = [4, 16, 32];

    /// The ladder of `gib` rungs, each in GiB.
    ///
    /// # Errors
    /// `gib` is empty, holds a 0, does not ascend strictly, or holds a rung that
    /// overflows a count of bytes.
    pub fn from_gib(gib: &[u64]) -> Result<Self, LadderError> {
        if gib.is_empty() {
            return Err(LadderError::Empty);
        }
        let mut rungs: Vec<u64> = Vec::with_capacity(gib.len());
        for (at, &n) in gib.iter().enumerate() {
            if n == 0 {
                return Err(LadderError::Zero);
            }
            let bytes = n.checked_mul(GIB).ok_or(LadderError::TooLarge { at })?;
            if rungs.last().is_some_and(|&last| bytes <= last) {
                return Err(LadderError::NotAscending { at });
            }
            rungs.push(bytes);
        }
        Ok(Self { rungs })
    }

    /// The booking of an action that names no size, in bytes: the first rung.
    #[must_use]
    pub fn first(&self) -> u64 {
        self.rungs[0]
    }

    /// The booking after a run that booked `booked` bytes passed its own limit, when
    /// the largest node that could run it has `cap`: the first rung above `booked` and
    /// below `cap`, else `cap` (a whole host). `None` when `booked` is already the cap
    /// or more: no node could give it more.
    #[must_use]
    pub fn raised(&self, booked: u64, cap: u64) -> Option<u64> {
        (booked < cap).then(|| {
            self.rungs
                .iter()
                .copied()
                .find(|&rung| rung > booked && rung < cap)
                .unwrap_or(cap)
        })
    }
}

impl Default for MemoryLadder {
    fn default() -> Self {
        Self {
            rungs: Self::DEFAULT_GIB.iter().map(|n| n * GIB).collect(),
        }
    }
}

/// The memory state of one scheduler: its floors, with the order they were raised in,
/// each operation's memory kills while the scheduler holds the operation, and each
/// node's busy-node kills. Equal for two schedulers given the same inputs; any kill,
/// or a different order of raises (which decides the next floor forgotten), makes it
/// differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryState {
    floors: Floors,
    /// The operations with a memory kill, while the scheduler holds them.
    ops: BTreeMap<OperationId, OpKills>,
    /// How many leases each node has killed under memory pressure, for every node
    /// that has.
    pressure: BTreeMap<WorkerId, u64>,
}

impl Default for MemoryState {
    /// No kills yet, keeping at most [`MEMORY_FLOORS`] floors.
    fn default() -> Self {
        Self::new(MEMORY_FLOORS)
    }
}

impl MemoryState {
    const fn new(bound: usize) -> Self {
        Self {
            floors: Floors::new(bound),
            ops: BTreeMap::new(),
            pressure: BTreeMap::new(),
        }
    }

    /// The runs of `operation` killed for memory, oldest first.
    pub(super) fn runs(&self, operation: OperationId) -> &[MemoryRun] {
        self.ops
            .get(&operation)
            .map_or(&[][..], |op| op.runs.as_slice())
    }

    /// Every held operation's memory runs, oldest first: what a successor that took the
    /// operations over must hold too.
    #[must_use]
    pub fn runs_by_operation(&self) -> BTreeMap<OperationId, Vec<MemoryRun>> {
        self.ops
            .iter()
            .map(|(id, kills)| (*id, kills.runs.clone()))
            .collect()
    }

    /// Takes up the kills `operation` had before, in a scheduler that holds it now.
    pub(super) fn restore(&mut self, operation: OperationId, runs: Vec<MemoryRun>) {
        if !runs.is_empty() {
            self.ops.insert(operation, OpKills { runs });
        }
    }

    /// Forgets the kills of `operation`, which the scheduler has dropped.
    pub(super) fn forget(&mut self, operation: OperationId) {
        self.ops.remove(&operation);
    }
}

/// One operation's memory kills.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct OpKills {
    /// Its runs killed for memory, oldest first.
    runs: Vec<MemoryRun>,
}

/// The memory floors by action key, each with the order it was raised in, and the keys
/// in that order, so the oldest is forgotten first once there are too many.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Floors {
    asks: BTreeMap<ActionKey, (u64, u64)>,
    order: BTreeMap<u64, ActionKey>,
    next: u64,
    bound: usize,
}

impl Floors {
    const fn new(bound: usize) -> Self {
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
        while self.asks.len() > self.bound {
            let (_, oldest) = self.order.pop_first().expect("every floor is in the order");
            self.asks.remove(&oldest);
        }
    }
}

impl Scheduler {
    /// This scheduler, keeping at most `bound` memory floors (instead of
    /// [`MEMORY_FLOORS`]); past that, the one raised longest ago is forgotten first.
    #[must_use]
    pub fn with_memory_floors(mut self, bound: usize) -> Self {
        self.memory = Box::new(MemoryState::new(bound));
        self
    }

    /// Climbs `ladder` after an own-limit kill, instead of [`MemoryLadder::default`].
    /// Runs already booked keep their booking.
    pub fn set_memory_ladder(&mut self, ladder: MemoryLadder) {
        self.ladder = ladder;
    }

    /// The memory floor of `key`, in bytes: the largest booking a run of it was raised
    /// to after it passed its own memory limit. A new submission of `key` books at
    /// least this much memory.
    #[must_use]
    pub fn memory_floor(&self, key: &ActionKey) -> Option<u64> {
        self.memory.floors.get(key)
    }

    /// The runs of `operation` killed for memory, oldest first; `None` once the
    /// scheduler no longer holds it.
    #[must_use]
    pub fn memory_runs(&self, operation: OperationId) -> Option<&[MemoryRun]> {
        self.ops
            .contains_key(&operation)
            .then(|| self.memory.runs(operation))
    }

    /// How many leases `worker` has killed under memory pressure, below the action's
    /// own limit, since it first registered.
    #[must_use]
    pub fn memory_pressure(&self, worker: &WorkerId) -> u64 {
        self.memory.pressure.get(worker).copied().unwrap_or(0)
    }

    /// Everything this scheduler keeps about memory kills, as one value to compare.
    #[must_use]
    pub fn memory_state(&self) -> &MemoryState {
        &self.memory
    }

    /// Raises `request`'s memory booking to its key's floor, if it has one.
    pub(super) fn floored(&self, mut request: Request) -> Request {
        if let Some(floor) = self.memory.floors.get(&request.key) {
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
            let raised = self.ladder.raised(booked, cap);
            self.memory.ops.entry(id).or_default().runs.push(MemoryRun {
                worker,
                booked,
                kill: MemoryKill::OwnLimit { cap },
            });
            let Some(raised) = raised else {
                return false;
            };
            let op = self.ops.get_mut(&id).expect("checked above");
            op.request.resources.memory_bytes = raised;
            let key = op.request.key.clone();
            self.memory.floors.raise(&key, raised);
            self.requeue(id, RequeueReason::OutOfMemory { booked, raised });
            return true;
        }
        *self.memory.pressure.entry(worker.clone()).or_default() += 1;
        let kills = self.memory.ops.entry(id).or_default();
        kills.runs.push(MemoryRun {
            worker: worker.clone(),
            booked,
            kill: MemoryKill::NodePressure,
        });
        let fault = FarmFault::NodeMemoryPressure;
        self.farm_fault(id, worker, fault, RequeueReason::NodeMemoryPressure)
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
    use std::time::Duration;

    use kbf_caps::NodeCaps;
    use kbf_sim::SimRng;
    use kbf_types::{
        Digest, DigestFunction, Effect, FarmTime, LeaseKind, Outcome, Qos, Resources, StartLease,
        StateMachine, WaiterId,
    };

    use super::*;
    use crate::input::{DaemonInstance, Event, Input};

    /// Catches a ladder that skips a rung, climbs by doubling, passes the cap, books a
    /// rung at or past the cap instead of the whole host (a node cannot give it), or
    /// offers a rung at the cap (a kill at the largest node would run again).
    #[test]
    fn the_ladder_climbs_its_rungs_then_books_the_whole_host() {
        let ladder = MemoryLadder::default();
        let cap = 64 * GIB;
        assert_eq!(ladder.first(), 4 * GIB);
        assert_eq!(ladder.raised(4 * GIB, cap), Some(16 * GIB));
        assert_eq!(ladder.raised(16 * GIB, cap), Some(32 * GIB));
        assert_eq!(ladder.raised(32 * GIB, cap), Some(cap), "the whole host");
        assert_eq!(ladder.raised(cap, cap), None, "killed at the cap");
        assert_eq!(ladder.raised(65 * GIB, cap), None, "past the cap");
        // A size the platform named climbs from where it is.
        assert_eq!(ladder.raised(GIB, cap), Some(4 * GIB));
        assert_eq!(ladder.raised(5 * GIB, cap), Some(16 * GIB));
        assert_eq!(ladder.raised(16 * GIB + 1, cap), Some(32 * GIB));
        assert_eq!(ladder.raised(0, cap), Some(4 * GIB));
        // A rung not below the largest node skips to the whole host.
        assert_eq!(ladder.raised(4 * GIB, 16 * GIB), Some(16 * GIB));
        assert_eq!(ladder.raised(4 * GIB, 10 * GIB), Some(10 * GIB));
        assert_eq!(ladder.raised(4 * GIB, 5 * GIB), Some(5 * GIB));
        assert_eq!(ladder.raised(16 * GIB, 24 * GIB), Some(24 * GIB));
        assert_eq!(ladder.raised(32 * GIB, 32 * GIB + 1), Some(32 * GIB + 1));
    }

    /// Catches a default that is not 4, 16, 32 GiB, and one that differs from what the
    /// flag's default builds.
    #[test]
    fn the_default_ladder_is_4_16_32_gib() {
        assert_eq!(
            MemoryLadder::default(),
            MemoryLadder::from_gib(&[4, 16, 32]).unwrap()
        );
        assert_eq!(MemoryLadder::DEFAULT_GIB, [4, 16, 32]);
    }

    /// Catches a ladder that accepts an empty list, a 0, a rung equal to or below the
    /// one before it, or a rung that wraps in bytes; and one that refuses a good list.
    #[test]
    fn a_ladder_must_ascend_and_be_positive() {
        assert_eq!(MemoryLadder::from_gib(&[]), Err(LadderError::Empty));
        assert_eq!(MemoryLadder::from_gib(&[0, 4]), Err(LadderError::Zero));
        assert_eq!(MemoryLadder::from_gib(&[4, 0]), Err(LadderError::Zero));
        assert_eq!(
            MemoryLadder::from_gib(&[4, 4]),
            Err(LadderError::NotAscending { at: 1 })
        );
        assert_eq!(
            MemoryLadder::from_gib(&[4, 16, 8]),
            Err(LadderError::NotAscending { at: 2 })
        );
        assert_eq!(
            MemoryLadder::from_gib(&[4, 1 << 40]),
            Err(LadderError::TooLarge { at: 1 })
        );
        assert!(MemoryLadder::from_gib(&[1]).is_ok(), "one rung is a ladder");
        let two = MemoryLadder::from_gib(&[2, 8]).unwrap();
        assert_eq!(
            (two.first(), two.raised(2 * GIB, 64 * GIB)),
            (2 * GIB, Some(8 * GIB))
        );
        for error in [
            LadderError::Empty,
            LadderError::Zero,
            LadderError::NotAscending { at: 1 },
            LadderError::TooLarge { at: 1 },
        ] {
            assert!(!error.to_string().is_empty());
        }
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

    /// A scheduler fed one input at a time at farm time `now`, every commit fed
    /// straight back, as the single-node server does; the `Start`s are kept.
    struct Sim {
        s: Scheduler,
        now: FarmTime,
        running: Vec<StartLease>,
        next_waiter: u64,
    }

    impl Sim {
        /// Nodes `a` (4 GiB) and `b` (16 GiB), and at most `bound` floors. Farm time
        /// does not move, so no operation is dropped.
        fn new(bound: usize) -> Self {
            Self::with(Scheduler::new(1).with_memory_floors(bound))
        }

        /// As [`Sim::new`], but each finished operation is dropped at the next input.
        fn dropping(bound: usize) -> Self {
            let s = Scheduler::new(1).with_memory_floors(bound);
            Self::with(s.with_finished_retention(Duration::ZERO))
        }

        fn with(s: Scheduler) -> Self {
            let mut sim = Self {
                s,
                now: FarmTime::default(),
                running: Vec::new(),
                next_waiter: 0,
            };
            for (name, gib) in [("a", 4), ("b", 16)] {
                let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
                    .expect("a valid report")
                    .with_drivers(["container"]);
                sim.feed(Event::WorkerUp {
                    worker: WorkerId::new(name),
                    instance: DaemonInstance::new(name),
                    predecessor_stopped: false,
                    capacity: Resources::new(8_000, gib * GIB),
                    caps,
                });
            }
            sim
        }

        fn feed(&mut self, event: Event) {
            let mut pending = self.s.apply(Input::new(self.now, event));
            while !pending.is_empty() {
                let mut more = Vec::new();
                for effect in pending {
                    match effect {
                        Effect::Commit(record) => {
                            let committed = Input::new(self.now, Event::Committed(record));
                            more.extend(self.s.apply(committed));
                        }
                        Effect::Start(start) => self.running.push(start),
                        _ => {}
                    }
                }
                pending = more;
            }
        }

        /// Submits action `n` at 1 GiB, and runs a placement round.
        fn submit(&mut self, n: u8) {
            let request = Request {
                key: key(n),
                qos: Qos::Ci,
                kind: LeaseKind::Action,
                resources: Resources::new(1_000, GIB),
                hermetic: true,
                do_not_cache: false,
                needs: kbf_caps::Request::from_platform([("OSFamily", "linux")])
                    .expect("a Linux platform"),
            };
            let waiter = WaiterId(self.next_waiter);
            self.next_waiter += 1;
            self.feed(Event::Submit { waiter, request });
            self.feed(Event::Tick);
        }

        /// The running lease of action `n` ends with `outcome`; then a placement round.
        fn end(&mut self, n: u8, outcome: Outcome) {
            let at = self
                .running
                .iter()
                .position(|s| s.key == key(n))
                .expect("action n runs");
            self.end_at(at, outcome);
        }

        /// The `at`th running lease ends with `outcome`; then a placement round.
        fn end_at(&mut self, at: usize, outcome: Outcome) {
            let start = self.running.remove(at);
            self.feed(Event::Report {
                operation: start.operation,
                lease: start.lease,
                outcome,
            });
            self.feed(Event::Tick);
        }
    }

    const OOM: Outcome = Outcome::Failed(Failure::OutOfMemory);
    const PRESSURE: Outcome = Outcome::Failed(Failure::NodeMemoryPressure);

    fn done() -> Outcome {
        Outcome::Completed {
            action_result: Digest::new(DigestFunction::Sha256, [7; 32], 1),
        }
    }

    /// One step of a generated sequence: a submission of an action, or the end of the
    /// `pick`th running lease (modulo how many run) with an outcome.
    #[derive(Clone, Debug)]
    enum Step {
        Submit(u8),
        End(u64, Outcome),
    }

    /// The sequence `seed` names: submissions of 6 actions, and ends that are mostly
    /// memory kills of both kinds, with 3 floors kept.
    fn sequence(seed: u64) -> Vec<Step> {
        let mut rng = SimRng::from_seed(seed);
        (0..120)
            .map(|_| match rng.below(5) {
                0 | 1 => Step::Submit(u8::try_from(rng.below(6)).unwrap_or_default()),
                2 => Step::End(rng.below(64), OOM),
                3 => Step::End(rng.below(64), PRESSURE),
                _ => Step::End(rng.below(64), done()),
            })
            .collect()
    }

    /// Applies `step`; an end with nothing running does nothing. Returns whether a
    /// lease ended.
    fn apply(sim: &mut Sim, step: &Step) -> bool {
        match step {
            Step::Submit(n) => {
                sim.submit(*n);
                false
            }
            Step::End(pick, outcome) => {
                let running = u64::try_from(sim.running.len()).unwrap_or(u64::MAX);
                if running == 0 {
                    return false;
                }
                let at = usize::try_from(pick % running).unwrap_or_default();
                sim.end_at(at, *outcome);
                true
            }
        }
    }

    /// (a) Catches a memory state that is not a function of the scheduler's inputs
    /// (a value drawn from outside them, such as a process-wide counter or a random
    /// source; a hashed map is refused by clippy, as its equality ignores order): over
    /// a sweep of seeds, two
    /// schedulers fed the same generated sequence of submissions and memory kills
    /// hold equal memory states after every step. The sweep must reach full floors,
    /// busy-node counts and operations with kills, so the check is never vacuous.
    #[test]
    fn schedulers_fed_one_oom_sequence_hold_equal_memory_states() {
        let (mut evicted, mut pressure, mut runs) = (false, false, false);
        for seed in 0..32 {
            let (mut a, mut b) = (Sim::new(3), Sim::new(3));
            // The actions that held a floor at some step: more than 3 means one was
            // forgotten.
            let mut floored = std::collections::BTreeSet::new();
            for step in sequence(seed) {
                apply(&mut a, &step);
                apply(&mut b, &step);
                assert_eq!(a.s.memory_state(), b.s.memory_state(), "seed {seed}");
                floored.extend((0..6).filter(|&n| a.s.memory_floor(&key(n)).is_some()));
            }
            let memory = a.s.memory_state();
            evicted |= floored.len() > 3;
            pressure |= !memory.pressure.is_empty();
            runs |= memory.ops.values().any(|op| op.runs.len() > 1);
        }
        assert!(evicted, "no seed raised floors past the bound");
        assert!(pressure, "no seed had a busy-node kill");
        assert!(runs, "no seed killed one operation twice");
    }

    /// (b) Catches a kill that leaves no trace in the memory state's equality (an
    /// equality that ignores the whole state, or a kill recorded nowhere): for each
    /// seed, the sequence with its first memory kill turned into a completed run
    /// leaves a different memory state from the sequence itself. (No operation is
    /// dropped, so the killed one keeps that run to the end in one and never gets it
    /// in the other.) Which part of the state must differ is pinned by the tests
    /// below, one part each.
    #[test]
    fn sequences_that_differ_in_one_kill_leave_unequal_memory_states() {
        for seed in 0..32 {
            let steps = sequence(seed);
            let (mut original, mut changed) = (Sim::new(3), Sim::new(3));
            let mut flipped = false;
            for step in &steps {
                apply(&mut original, step);
                let first_kill = matches!(step, Step::End(_, o) if *o != done());
                let step = match step {
                    Step::End(pick, _) if first_kill && !flipped => {
                        flipped = !changed.running.is_empty();
                        Step::End(*pick, done())
                    }
                    other => other.clone(),
                };
                apply(&mut changed, &step);
            }
            assert!(flipped, "seed {seed} has no kill");
            assert_ne!(
                original.s.memory_state(),
                changed.s.memory_state(),
                "seed {seed}"
            );
        }
    }

    /// Catches floors or busy-node counts left out of the memory state's equality, and
    /// an operation's kills kept after the scheduler drops it: once the operations are
    /// dropped, a past own-limit kill leaves only its floor and a past busy-node kill
    /// only its count, and each still makes the state differ from one without it.
    #[test]
    fn kills_of_dropped_operations_still_differ() {
        let quiet = Sim::dropping(3);
        for kill in [OOM, PRESSURE] {
            let mut killed = Sim::dropping(3);
            killed.submit(1);
            killed.end(1, kill);
            killed.end(1, done());
            // The finished retention is zero: the next input drops the operation.
            killed.feed(Event::Tick);
            let memory = killed.s.memory_state();
            assert_eq!(
                memory.ops,
                BTreeMap::new(),
                "{kill:?}: kills of a dropped operation kept"
            );
            assert_eq!(memory.floors.asks.is_empty(), kill == PRESSURE, "{kill:?}");
            assert_eq!(memory.pressure.is_empty(), kill == OOM, "{kill:?}");
            assert_ne!(memory, quiet.s.memory_state(), "{kill:?}");
        }
    }

    /// Catches each operation's memory runs left out of the memory state's equality:
    /// a busy-node kill of one of two operations running on one node, or of the other,
    /// leaves equal floors (none) and equal busy-node counts, and differs only in which
    /// operation holds the run.
    #[test]
    fn the_same_kill_of_another_operation_differs() {
        let killed = |pick: usize| {
            let mut sim = Sim::new(3);
            sim.submit(1);
            sim.submit(2);
            assert_eq!(sim.running[0].worker, sim.running[1].worker, "one node");
            sim.end_at(pick, PRESSURE);
            sim
        };
        let (first, second) = (killed(0), killed(1));
        let (a, b) = (first.s.memory_state(), second.s.memory_state());
        assert_eq!(a.floors, b.floors);
        assert_eq!(a.pressure, b.pressure);
        assert_ne!(a, b);
    }

    /// (c) Catches the order floors were raised in left out of the memory state's
    /// equality: with 2 floors kept, raising those of actions 1, 2, 3 or of 1, 3, 2
    /// keeps the same floors (2 and 3, at 4 GiB) with the same runs, and differs only
    /// in which of them is forgotten next. A fourth raise then keeps a different pair.
    #[test]
    fn sequences_that_differ_only_in_raise_order_past_the_bound_are_unequal() {
        let raised_in = |order: [u8; 3]| {
            let mut sim = Sim::new(2);
            for n in 1..=3 {
                sim.submit(n);
            }
            for n in order {
                sim.end(n, OOM);
            }
            sim
        };
        let (mut first, mut second) = (raised_in([1, 2, 3]), raised_in([1, 3, 2]));
        for n in 1..=3 {
            let floor = first.s.memory_floor(&key(n));
            assert_eq!(floor, second.s.memory_floor(&key(n)), "action {n}");
            assert_eq!(floor, (n > 1).then_some(4 * GIB), "action {n}");
        }
        assert_eq!(first.s.memory_state().ops, second.s.memory_state().ops);
        assert_ne!(first.s.memory_state(), second.s.memory_state());
        for sim in [&mut first, &mut second] {
            sim.submit(4);
            sim.end(4, OOM);
        }
        assert_eq!(first.s.memory_floor(&key(2)), None);
        assert_eq!(first.s.memory_floor(&key(3)), Some(4 * GIB));
        assert_eq!(second.s.memory_floor(&key(2)), Some(4 * GIB));
        assert_eq!(second.s.memory_floor(&key(3)), None);
    }
}
