//! The F1 scenarios (catalog section 5, F1.1 to F1.10). Each is a generator over the
//! base world; the checker holds every one to I1 to I14 after every input and to L1 at
//! the end. What a scenario checks beyond that, and the situations it must reach, are
//! in its own `finish` and in the sweep test that runs it.

use std::collections::{BTreeMap, BTreeSet};

use kbf_sim::{Chance, SimRng};
use kbf_types::{OperationId, Qos, Resources, WaiterId};

mod dedup;

pub use dedup::{Dedup, JoinAfterFinish};

use crate::world::{ANY, ARM64, DARWIN, GIB, LINUX, Node, Scenario, Spec, World, levels, request};

pub(crate) fn pick<T: Clone>(rng: &mut SimRng, items: &[T]) -> T {
    items[usize::try_from(rng.below(items.len() as u64)).unwrap()].clone()
}

/// Cuts `total` into `parts` positive pieces at random.
fn cut(rng: &mut SimRng, total: u64, parts: u64) -> Vec<u64> {
    let mut cuts: BTreeSet<u64> = BTreeSet::new();
    while (cuts.len() as u64) < parts - 1 {
        cuts.insert(rng.between(1, total - 1));
    }
    let mut pieces = Vec::new();
    let mut last = 0;
    for c in cuts.into_iter().chain([total]) {
        pieces.push(c - last);
        last = c;
    }
    pieces
}

// F1.1 ---------------------------------------------------------------------------------

/// F1.1 exact packing: one to three equal workers, each filled at second 0 by requests
/// whose sizes sum to its capacity exactly on every axis, and one request of one more
/// unit on one axis (a millicore, a byte, or a GPU). Every piece is granted in the
/// first round and fills its worker exactly; the extra unit waits and is granted at
/// the first release that frees its axis, not before.
#[derive(Debug, Default)]
pub struct ExactPacking {
    workers: u64,
    extra: Option<(WaiterId, usize)>,
    /// Per piece: its worker index, its axes and when it ends.
    pieces: Vec<(WaiterId, usize, [u64; 3], u64)>,
    pub full_at_start: u64,
    pub extra_on_axis: [u64; 3],
}

impl Scenario for ExactPacking {
    fn name(&self) -> &'static str {
        "F1.1"
    }

    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec> {
        self.workers = rng.between(1, 3);
        let cores = pick(rng, &[4, 8, 16, 32, 64]);
        let gib = pick(rng, &[8, 16, 64, 256]);
        let gpus = pick(rng, &[0, 1, 2, 4]);
        (0..self.workers)
            .map(|i| Spec::new(&format!("w{i}"), cores, gib, gpus, Node::LinuxX86))
            .collect()
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        if w.t != 0 {
            return;
        }
        let mut key = 0;
        for i in 0..self.workers {
            let cap = w.fleet[usize::try_from(i).unwrap()].capacity;
            let parts = rng.between(2, 6);
            let cpu = cut(rng, cap.cpu_millis, parts);
            let mem = cut(rng, cap.memory_bytes, parts);
            let mut gpus = vec![0; usize::try_from(parts).unwrap()];
            for _ in 0..cap.gpus {
                gpus[usize::try_from(rng.below(parts)).unwrap()] += 1;
            }
            for p in 0..usize::try_from(parts).unwrap() {
                let res = Resources::new(cpu[p], mem[p]).with_gpus(gpus[p]);
                let secs = rng.between(100, 200);
                let waiter = w.submit(request(key, Qos::Ci, res, ANY), secs);
                key += 1;
                let axes = [cpu[p], mem[p], gpus[p]];
                self.pieces
                    .push((waiter, usize::try_from(i).unwrap(), axes, secs));
            }
        }
        let gpus = w.fleet[0].capacity.gpus;
        let axis = usize::try_from(rng.below(if gpus > 0 { 3 } else { 2 })).unwrap();
        let mut one = [0; 3];
        one[axis] = 1;
        let res = Resources::new(one[0], one[1]).with_gpus(one[2]);
        let waiter = w.submit(request(key, Qos::Ci, res, ANY), 5);
        self.extra = Some((waiter, axis));
        self.extra_on_axis[axis] += 1;
    }

    fn after_tick(&mut self, w: &mut World, _rng: &mut SimRng) {
        if w.t != 0 {
            return;
        }
        for (waiter, ..) in &self.pieces {
            let op = w.check.op_of(*waiter).unwrap();
            if !w.check.granted_at.contains_key(&op) {
                w.check.fail(
                    "F1.1",
                    &format!("piece {op} not granted in the first round"),
                );
            }
        }
        for spec in &w.fleet {
            let (booked, cap) = w.check.room(&spec.name);
            if booked != cap {
                let what = format!("{} booked {booked:?} of {cap:?} after the round", spec.name);
                w.check.fail("F1.1", &what);
            }
        }
        let (extra, _) = self.extra.unwrap();
        let op = w.check.op_of(extra).unwrap();
        if !w.check.is_queued(op) {
            w.check
                .fail("F1.1", "one unit more than the capacity was granted");
        }
        self.full_at_start += 1;
    }

    fn quiet_after(&self) -> u64 {
        0
    }

    fn horizon(&self) -> u64 {
        // The extra unit waits for the first release, at 200 s at most, and runs 5 s.
        220
    }

    fn finish(&mut self, w: &World) {
        let (extra, axis) = self.extra.unwrap();
        let op = w.check.op_of(extra).unwrap();
        // The first piece, on any worker, to free the extra unit's axis.
        let first = self
            .pieces
            .iter()
            .filter(|(_, _, axes, _)| axes[axis] > 0)
            .map(|(_, _, _, ends)| *ends)
            .min()
            .expect("every axis of the fleet is booked by some piece");
        let at = w.check.granted_at.get(&op).copied();
        if at != Some(first * 1_000) {
            let what = format!(
                "the extra unit on axis {axis} was granted at {at:?} ms, the first release \
                 that frees that axis is at {first} s"
            );
            w.check.fail("F1.1", &what);
        }
    }
}

// F1.2 ---------------------------------------------------------------------------------

/// F1.2 each axis full on its own: a pool of equal workers (16 cores, 32 GiB) fed a
/// random mix of CPU-heavy (6 cores, 2 GiB) and memory-heavy (1 core, 12 GiB) requests
/// faster than it runs them. The sweep must see a worker full on memory with CPU free
/// while a memory-heavy request waits, and the other way round; I6 and I11 hold that
/// neither goes there.
#[derive(Debug, Default)]
pub struct EachAxis {
    pub memory_full_cpu_free: u64,
    pub cpu_full_memory_free: u64,
}

const CPU_HEAVY: Resources = Resources::new(6_000, 2 * GIB);
const MEMORY_HEAVY: Resources = Resources::new(1_000, 12 * GIB);

impl Scenario for EachAxis {
    fn name(&self) -> &'static str {
        "F1.2"
    }

    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec> {
        (0..rng.between(2, 4))
            .map(|i| Spec::new(&format!("w{i}"), 16, 32, 0, Node::LinuxX86))
            .collect()
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        if w.t >= 240 {
            return;
        }
        for _ in 0..rng.below(3) {
            let res = if rng.below(2) == 0 {
                CPU_HEAVY
            } else {
                MEMORY_HEAVY
            };
            let key = w.check.op_count() + 10_000;
            w.submit(request(key, Qos::Ci, res, ANY), rng.between(10, 40));
        }
    }

    fn after_tick(&mut self, w: &mut World, _rng: &mut SimRng) {
        let queued: Vec<[u64; 3]> = w.check.queue().map(|id| w.check.resources_of(id)).collect();
        let waits = |r: Resources| queued.contains(&[r.cpu_millis, r.memory_bytes, r.gpus]);
        for spec in &w.fleet {
            let (booked, cap) = w.check.room(&spec.name);
            let free = [cap[0] - booked[0], cap[1] - booked[1]];
            if free[1] < MEMORY_HEAVY.memory_bytes
                && free[0] >= MEMORY_HEAVY.cpu_millis
                && waits(MEMORY_HEAVY)
            {
                self.memory_full_cpu_free += 1;
            }
            if free[0] < CPU_HEAVY.cpu_millis
                && free[1] >= CPU_HEAVY.memory_bytes
                && waits(CPU_HEAVY)
            {
                self.cpu_full_memory_free += 1;
            }
        }
    }

    fn quiet_after(&self) -> u64 {
        240
    }

    fn horizon(&self) -> u64 {
        // At most 2 arrivals a second for 240 s, at most 40 s each, at least one at a
        // time per worker on two workers: 480 * 40 / 2 s, well inside this.
        240 + 480 * 40 / 2 + 60
    }
}

// F1.3 ---------------------------------------------------------------------------------

/// F1.3 whole GPUs: workers with 0 and 1 GPUs, and sometimes 2 and 4; requests for 1 to
/// 4 GPUs and CPU-only ones. Booked GPUs never exceed a worker's count (I6); a request
/// for more GPUs than any worker has is refused after the wait with the size reason;
/// every other request runs.
#[derive(Debug, Default)]
pub struct Gpus {
    most: u64,
    asked: BTreeMap<WaiterId, u64>,
    pub refused: u64,
    pub gpu_grants: u64,
}

impl Scenario for Gpus {
    fn name(&self) -> &'static str {
        "F1.3"
    }

    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec> {
        let mut fleet = vec![
            Spec::new("g0", 8, 32, 0, Node::LinuxX86),
            Spec::new("g1", 8, 32, 1, Node::LinuxX86),
        ];
        self.most = 1;
        if rng.chance(Chance::percent(60)) {
            fleet.push(Spec::new("g2", 8, 32, 2, Node::LinuxX86));
            self.most = 2;
        }
        if rng.chance(Chance::percent(50)) {
            fleet.push(Spec::new("g4", 8, 32, 4, Node::LinuxX86));
            self.most = 4;
        }
        fleet
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        if w.t >= 200 || rng.below(2) != 0 {
            return;
        }
        let gpus = pick(rng, &[0, 1, 1, 2, 3, 4]);
        let res = Resources::new(1_000, 2 * GIB).with_gpus(gpus);
        let key = w.check.op_count();
        let waiter = w.submit(request(key, Qos::Ci, res, ANY), rng.between(5, 30));
        self.asked.insert(waiter, gpus);
    }

    fn after_tick(&mut self, w: &mut World, _rng: &mut SimRng) {
        for (id, worker) in &w.check.last_round {
            if w.check.resources_of(*id)[2] > 0 {
                self.gpu_grants += 1;
                let (booked, cap) = w.check.room(worker);
                if booked[2] > cap[2] {
                    w.check.fail(
                        "I6",
                        &format!("{worker} books {} of {} GPUs", booked[2], cap[2]),
                    );
                }
            }
        }
    }

    fn quiet_after(&self) -> u64 {
        200
    }

    fn horizon(&self) -> u64 {
        // 100 arrivals of 30 s at most, one GPU worker at a time: 3,000 s, plus the wait.
        200 + 3_000 + 120
    }

    fn finish(&mut self, w: &World) {
        for (waiter, gpus) in &self.asked {
            let (op, outcome) = w.check.outcome_of[waiter];
            let refused = outcome.is_none();
            if refused != (*gpus > self.most) {
                let what = format!(
                    "{op} asked for {gpus} GPU(s), the largest worker has {}; refused {refused}",
                    self.most
                );
                w.check.fail("F1.3", &what);
            }
            if refused {
                self.refused += 1;
                let reason = &w.check.refused_reason[&op];
                if !reason.contains("smaller than its request") || !reason.contains("GPU") {
                    w.check.fail(
                        "F1.3",
                        &format!("{op} refused without the size reason: {reason}"),
                    );
                }
            }
        }
    }
}

// F1.4 ---------------------------------------------------------------------------------

/// The base world's node mix: seven in ten Linux x86-64, two arm64, one Mac.
const NODES: [Node; 10] = [
    Node::LinuxX86,
    Node::LinuxX86,
    Node::LinuxX86,
    Node::LinuxX86,
    Node::LinuxX86,
    Node::LinuxX86,
    Node::LinuxX86,
    Node::LinuxArm,
    Node::LinuxArm,
    Node::Mac,
];

/// The base world's request sizes.
fn size(rng: &mut SimRng, gpus: bool, heavy: bool) -> Resources {
    let kinds = if heavy { 6 } else { 3 };
    match rng.below(if gpus { kinds + 1 } else { kinds }) {
        0 => Resources::new(1_000, GIB),
        1 => Resources::new(rng.between(2, 8) * 1_000, 2 * GIB),
        2 => Resources::new(1_000, rng.between(2, 16) * GIB),
        3 => Resources::new(2_000, rng.between(32, 128) * GIB),
        4 => Resources::new(rng.between(16, 64) * 1_000, 8 * GIB),
        5 => Resources::new(rng.between(2, 4) * 1_000, 4 * GIB),
        _ => Resources::new(4_000, 16 * GIB).with_gpus(rng.between(1, 4)),
    }
}

/// F1.4 QoS order when saturated, on the base world: 4 to 12 workers of mixed size,
/// GPUs and platform; arrivals of every size and all four levels faster than the fleet
/// runs them; per seed the swarm switches GPU requests, heavy requests, dedup keys and
/// capacity changes on or off and draws the arrival rate. I11 holds every round to the
/// reference order (urgency, then submission); the sweep must see an `interactive`
/// operation granted while older, less urgent work waited, and work granted past an
/// older operation of its own level that did not fit.
#[derive(Debug, Default)]
pub struct QosOrder {
    gpus: bool,
    heavy: bool,
    dedup: bool,
    capacity_changes: bool,
    rate: u64,
    original: Vec<Resources>,
    pub interactive_passed_older: u64,
    pub same_level_skips: u64,
}

impl Scenario for QosOrder {
    fn name(&self) -> &'static str {
        "F1.4"
    }

    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec> {
        self.gpus = rng.chance(Chance::percent(50));
        self.heavy = rng.chance(Chance::percent(50));
        self.dedup = rng.chance(Chance::percent(50));
        self.capacity_changes = rng.chance(Chance::percent(50));
        self.rate = rng.between(2, 6);
        let fleet: Vec<Spec> = (0..rng.between(4, 12))
            .map(|i| {
                let node = pick(rng, &NODES);
                let cores = pick(rng, &[4, 8, 16, 32, 64]);
                let gib = pick(rng, &[8, 16, 32, 64, 128, 256]);
                let gpus = pick(rng, &[0, 0, 0, 1, 2, 4]);
                Spec::new(&format!("w{i:02}"), cores, gib, gpus, node)
            })
            .collect();
        self.original = fleet.iter().map(|s| s.capacity).collect();
        fleet
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        if w.t >= 300 {
            if w.t == 300 && self.capacity_changes {
                for (i, cap) in self.original.clone().into_iter().enumerate() {
                    w.set_capacity(i, cap);
                }
            }
            return;
        }
        for _ in 0..rng.below(self.rate) {
            let res = size(rng, self.gpus, self.heavy);
            let qos = pick(rng, &levels());
            let platform = pick(rng, &[ANY, ANY, ANY, ANY, LINUX, LINUX, ARM64, DARWIN]);
            let key = if self.dedup && rng.below(4) == 0 {
                rng.below(20)
            } else {
                1_000 + w.check.op_count()
            };
            w.submit(request(key, qos, res, platform), rng.between(5, 60));
        }
        if self.capacity_changes && rng.below(20) == 0 {
            let i = usize::try_from(rng.below(w.fleet.len() as u64)).unwrap();
            let c = self.original[i];
            let scale = |v: u64, rng: &mut SimRng| v * rng.between(2, 6) / 4;
            let cap = Resources::new(scale(c.cpu_millis, rng), scale(c.memory_bytes, rng))
                .with_gpus(scale(c.gpus, rng));
            w.set_capacity(i, cap);
        }
    }

    fn after_tick(&mut self, w: &mut World, _rng: &mut SimRng) {
        if w.check.last_round.is_empty() {
            return;
        }
        // The oldest operation still queued at each urgency.
        let mut oldest: BTreeMap<u16, OperationId> = BTreeMap::new();
        for id in w.check.queue() {
            let u = w.check.qos_of(id).urgency();
            let e = oldest.entry(u).or_insert(id);
            *e = (*e).min(id);
        }
        for (id, _) in &w.check.last_round {
            let u = w.check.qos_of(*id).urgency();
            if u == Qos::INTERACTIVE_URGENCY && oldest.range(..u).any(|(_, o)| o < id) {
                self.interactive_passed_older += 1;
            }
            if oldest.get(&u).is_some_and(|o| o < id) {
                self.same_level_skips += 1;
            }
        }
    }

    fn quiet_after(&self) -> u64 {
        300
    }

    fn horizon(&self) -> u64 {
        // At most 5 arrivals a second for 300 s, each at most 60 s, run at least one at
        // a time on the smallest fleet's workers that can: generous on purpose.
        300 + 1_500 * 60 / 4 + 400
    }
}

// F1.5 ---------------------------------------------------------------------------------

/// F1.5 a large request behind small ones: two or three 64-core workers filled with
/// one-core requests, then a 64-core request, then a steady stream of one-core requests
/// at about the rate the fleet frees cores. Today the small ones take each freed core
/// and the large one waits while they keep coming (first fit holds nothing back; a wait
/// bound is planned); it runs once they stop (L1).
#[derive(Debug, Default)]
pub struct LargeBehindSmall {
    workers: u64,
    stop: u64,
    large: Option<WaiterId>,
    pub passed_over: u64,
    pub waited_whole_stream: u64,
}

impl Scenario for LargeBehindSmall {
    fn name(&self) -> &'static str {
        "F1.5"
    }

    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec> {
        self.workers = rng.between(2, 3);
        self.stop = rng.between(200, 300);
        (0..self.workers)
            .map(|i| Spec::new(&format!("w{i}"), 64, 256, 0, Node::LinuxX86))
            .collect()
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        let small = Resources::new(1_000, GIB);
        if w.t == 0 {
            for _ in 0..self.workers * 64 {
                let key = w.check.op_count();
                w.submit(request(key, Qos::Ci, small, ANY), rng.between(20, 60));
            }
        } else if w.t == 1 {
            let big = Resources::new(64_000, 8 * GIB);
            self.large = Some(w.submit(request(1 << 40, Qos::Ci, big, ANY), 10));
        } else if w.t < self.stop {
            // About 64 * workers / 40 cores free up each second.
            for _ in 0..rng.between(self.workers, self.workers * 2 + 1) {
                let key = w.check.op_count();
                w.submit(request(key, Qos::Ci, small, ANY), rng.between(20, 60));
            }
        }
    }

    fn after_tick(&mut self, w: &mut World, _rng: &mut SimRng) {
        let Some(large) = self.large.and_then(|l| w.check.op_of(l)) else {
            return;
        };
        if w.check.is_queued(large) {
            let newer = w
                .check
                .last_round
                .iter()
                .filter(|(id, _)| *id > large)
                .count();
            self.passed_over += newer as u64;
        }
    }

    fn quiet_after(&self) -> u64 {
        self.stop
    }

    fn horizon(&self) -> u64 {
        // Once the stream stops, its backlog (at most a few hundred cores of work)
        // drains and every small run ends; then the large one runs for 10 s.
        self.stop + 300
    }

    fn finish(&mut self, w: &World) {
        let large = w.check.op_of(self.large.unwrap()).unwrap();
        let at = w.check.granted_at[&large];
        if at >= (self.stop - 1) * 1_000 {
            self.waited_whole_stream += 1;
        }
    }
}

// F1.6 ---------------------------------------------------------------------------------

/// F1.6 `batch` under steady `interactive`: `batch` requests of two to four cores queued
/// at the start, and one-core `interactive` requests arriving faster than the fleet runs
/// them. Every interactive request is no larger than any batch one, so first fit gives
/// every freed core to interactive work while some waits: no batch operation is granted
/// in a round that ends with interactive work queued. Batch runs once the stream stops
/// and its backlog drains (L1). Fair turns and preemption are planned.
#[derive(Debug, Default)]
pub struct BatchUnderInteractive {
    stop: u64,
    batch: Vec<WaiterId>,
    pub batch_waits: u64,
}

impl Scenario for BatchUnderInteractive {
    fn name(&self) -> &'static str {
        "F1.6"
    }

    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec> {
        self.stop = rng.between(100, 200);
        (0..rng.between(2, 4))
            .map(|i| Spec::new(&format!("w{i}"), 16, 64, 0, Node::LinuxX86))
            .collect()
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        let n = w.fleet.len() as u64;
        if w.t == 0 {
            // Fill the fleet with interactive work first, so batch queues behind it.
            for _ in 0..16 * n {
                let key = w.check.op_count();
                let res = Resources::new(1_000, GIB);
                w.submit(
                    request(key, Qos::Interactive, res, ANY),
                    rng.between(10, 40),
                );
            }
            for _ in 0..rng.between(4, 12) {
                let res = Resources::new(rng.between(2, 4) * 1_000, 2 * GIB);
                let key = w.check.op_count();
                self.batch
                    .push(w.submit(request(key, Qos::Batch, res, ANY), rng.between(5, 20)));
            }
        }
        if w.t < self.stop {
            // About 16 * n / 25 cores free up each second; ask for more.
            for _ in 0..rng.between(n, 2 * n) {
                let key = w.check.op_count();
                let res = Resources::new(1_000, GIB);
                w.submit(
                    request(key, Qos::Interactive, res, ANY),
                    rng.between(10, 40),
                );
            }
        }
    }

    fn after_tick(&mut self, w: &mut World, _rng: &mut SimRng) {
        let interactive_waits = w
            .check
            .queue()
            .any(|id| w.check.qos_of(id) == &Qos::Interactive);
        for (id, worker) in &w.check.last_round {
            if w.check.qos_of(*id) == &Qos::Batch && interactive_waits {
                let what = format!("batch {id} granted on {worker} while interactive work waits");
                w.check.fail("F1.6", &what);
            }
        }
        if interactive_waits {
            self.batch_waits += self
                .batch
                .iter()
                .filter(|b| w.check.is_queued(w.check.op_of(**b).unwrap()))
                .count() as u64;
        }
    }

    fn quiet_after(&self) -> u64 {
        self.stop
    }

    fn horizon(&self) -> u64 {
        // The interactive backlog: at most 2n a second for `stop` seconds of 40 s on
        // 16n cores, 5 * stop seconds; then batch.
        self.stop * 6 + 100
    }
}

// F1.9 ---------------------------------------------------------------------------------

/// F1.9 capacity shrinks below bookings, then grows: busy workers whose capacity a
/// resent `Hello` changes at random to between a quarter and one and a half times its
/// size, on each axis, and back at the end. Running leases are untouched (the base
/// world's rule: none is given up); no grant goes where bookings plus the request
/// exceed the new capacity (I6); placement uses grown room at the next round (I11).
#[derive(Debug, Default)]
pub struct CapacityShrink {
    original: Vec<Resources>,
    current: Vec<Resources>,
    grown: BTreeSet<usize>,
    pub shrunk_below_bookings: u64,
    pub grants_into_grown_room: u64,
}

impl Scenario for CapacityShrink {
    fn name(&self) -> &'static str {
        "F1.9"
    }

    fn wait_secs(&self) -> u64 {
        300
    }

    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec> {
        let fleet: Vec<Spec> = (0..rng.between(2, 4))
            .map(|i| Spec::new(&format!("w{i}"), 16, 64, pick(rng, &[0, 2]), Node::LinuxX86))
            .collect();
        self.original = fleet.iter().map(|s| s.capacity).collect();
        self.current.clone_from(&self.original);
        fleet
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        self.grown.clear();
        if w.t == 300 {
            for (i, cap) in self.original.clone().into_iter().enumerate() {
                w.set_capacity(i, cap);
            }
        }
        if w.t >= 300 {
            return;
        }
        for _ in 0..rng.below(3) {
            let gpus = u64::from(rng.below(4) == 0);
            let res =
                Resources::new(rng.between(1, 4) * 1_000, rng.between(2, 16) * GIB).with_gpus(gpus);
            let key = w.check.op_count();
            w.submit(
                request(key, pick(rng, &levels()), res, ANY),
                rng.between(20, 60),
            );
        }
        if rng.below(10) == 0 {
            let i = usize::try_from(rng.below(w.fleet.len() as u64)).unwrap();
            let c = self.original[i];
            let scale = |v: u64, rng: &mut SimRng| v * rng.between(1, 6) / 4;
            let cap = Resources::new(scale(c.cpu_millis, rng), scale(c.memory_bytes, rng))
                .with_gpus(scale(c.gpus, rng));
            let (booked, _) = w.check.room(&w.fleet[i].name);
            let new = [cap.cpu_millis, cap.memory_bytes, cap.gpus];
            if (0..3).any(|a| booked[a] > new[a]) {
                self.shrunk_below_bookings += 1;
            }
            w.set_capacity(i, cap);
            let old = self.current[i];
            if cap.cpu_millis > old.cpu_millis
                || cap.memory_bytes > old.memory_bytes
                || cap.gpus > old.gpus
            {
                self.grown.insert(i);
            }
            self.current[i] = cap;
        }
    }

    fn after_tick(&mut self, w: &mut World, _rng: &mut SimRng) {
        for &i in &self.grown {
            let name = &w.fleet[i].name;
            self.grants_into_grown_room += w
                .check
                .last_round
                .iter()
                .filter(|(_, worker)| worker == name)
                .count() as u64;
        }
    }

    fn quiet_after(&self) -> u64 {
        300
    }

    fn horizon(&self) -> u64 {
        // At most 2 arrivals a second for 300 s of 60 s each on 32 cores at least.
        300 + 600 * 60 * 4 / 32 + 400
    }
}

// F1.10 --------------------------------------------------------------------------------

/// F1.10 more than one round's worth: 513 to 767 one-core requests of mixed QoS in one
/// second on twelve 64-core workers. Each round grants `min(256, left)`, in queue
/// order (I11 holds the order and the count against a reference that limits a round to
/// 256 by its own constant, not the scheduler's).
#[derive(Debug, Default)]
pub struct Rounds {
    count: u64,
    pub per_round: Vec<usize>,
}

impl Scenario for Rounds {
    fn name(&self) -> &'static str {
        "F1.10"
    }

    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec> {
        self.count = rng.between(513, 767);
        (0..12)
            .map(|i| Spec::new(&format!("w{i:02}"), 64, 256, 0, Node::LinuxX86))
            .collect()
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        if w.t == 0 {
            for key in 0..self.count {
                let req = request(key, pick(rng, &levels()), Resources::new(1_000, GIB), ANY);
                w.submit(req, rng.between(100, 200));
            }
        }
    }

    fn after_tick(&mut self, w: &mut World, _rng: &mut SimRng) {
        if w.t < 4 {
            self.per_round.push(w.check.last_round.len());
        }
    }

    fn quiet_after(&self) -> u64 {
        0
    }

    fn horizon(&self) -> u64 {
        220
    }

    fn finish(&mut self, w: &World) {
        let mut left = usize::try_from(self.count).unwrap();
        for (t, &n) in self.per_round.iter().enumerate() {
            let want = left.min(256);
            if n != want {
                let what = format!("round {t} granted {n}, want {want} of {left} left");
                w.check.fail("F1.10", &what);
            }
            left -= want;
        }
    }
}

/// Every scenario by name, fresh.
pub fn by_name(name: &str) -> Option<Box<dyn Scenario>> {
    Some(match name {
        "F1.1" => Box::new(ExactPacking::default()),
        "F1.2" => Box::new(EachAxis::default()),
        "F1.3" => Box::new(Gpus::default()),
        "F1.4" => Box::new(QosOrder::default()),
        "F1.5" => Box::new(LargeBehindSmall::default()),
        "F1.6" => Box::new(BatchUnderInteractive::default()),
        "F1.7" => Box::new(Dedup::default()),
        "F1.8" => Box::new(JoinAfterFinish::default()),
        "F1.9" => Box::new(CapacityShrink::default()),
        "F1.10" => Box::new(Rounds::default()),
        _ => return None,
    })
}

/// The names `by_name` knows.
pub const NAMES: [&str; 10] = [
    "F1.1", "F1.2", "F1.3", "F1.4", "F1.5", "F1.6", "F1.7", "F1.8", "F1.9", "F1.10",
];
