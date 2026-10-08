//! F1.7 and F1.8: dedup, promotion, and joins after a twin finished.

use std::collections::BTreeMap;

use kbf_sim::SimRng;
use kbf_types::{ActionKey, OperationId, Qos, Resources};

use super::pick;
use crate::world::{ANY, DARWIN, GIB, LINUX, Node, Scenario, Spec, World, digest, levels, request};

// F1.7 ---------------------------------------------------------------------------------

/// F1.7 dedup and promotion: a small fleet kept saturated by filler, and a handful of
/// keys each submitted by many waiters at QoS levels that mostly rise, some lower. Twins
/// that may not join are mixed in: the same digest under another instance, networked
/// and `do_not_cache` requests. One operation per joinable key at a time; a join raises
/// a queued twin's level and moves it in the queue (I13, I14, I11); a join never lowers
/// it; every waiter is answered once, with its operation's outcome.
#[derive(Debug, Default)]
pub struct Dedup {
    ladder: BTreeMap<u64, usize>,
    pub other_instance: u64,
    pub networked: u64,
    pub do_not_cache: u64,
}

impl Scenario for Dedup {
    fn name(&self) -> &'static str {
        "F1.7"
    }

    fn fleet(&mut self, rng: &mut SimRng) -> Vec<Spec> {
        (0..rng.between(2, 3))
            .map(|i| Spec::new(&format!("w{i}"), 8, 32, 0, Node::LinuxX86))
            .collect()
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        if w.t >= 300 {
            return;
        }
        // Filler keeps the fleet full, so twins are queued when they are joined.
        if rng.below(2) == 0 {
            let key = 1_000_000 + w.check.op_count();
            let res = Resources::new(rng.between(1, 4) * 1_000, 2 * GIB);
            w.submit(
                request(key, pick(rng, &levels()), res, ANY),
                rng.between(10, 40),
            );
        }
        for _ in 0..rng.below(3) {
            let key = rng.below(12);
            let step = self.ladder.entry(key).or_default();
            // Mostly rising; one in five drops two levels.
            *step = if rng.below(5) == 0 {
                step.saturating_sub(2)
            } else {
                (*step + 1).min(3)
            };
            let qos = levels()[*step].clone();
            let mut req = request(key, qos, Resources::new(2_000, 4 * GIB), ANY);
            match rng.below(8) {
                0 => {
                    req.key = ActionKey {
                        instance: "other".to_owned(),
                        action: digest(key),
                    };
                    self.other_instance += 1;
                }
                1 => {
                    req.hermetic = false;
                    self.networked += 1;
                }
                2 => {
                    req.do_not_cache = true;
                    self.do_not_cache += 1;
                }
                _ => {}
            }
            w.submit(req, rng.between(10, 40));
        }
    }

    fn quiet_after(&self) -> u64 {
        300
    }

    fn horizon(&self) -> u64 {
        // At most 3.5 new operations a second for 300 s, 40 s each at 1 core or more,
        // on 16 cores at least: generous.
        300 + 1_050 * 40 * 4 / 16 + 100
    }

    fn finish(&mut self, w: &World) {
        // Every waiter of an operation carries that operation's outcome.
        let mut by_op: BTreeMap<OperationId, _> = BTreeMap::new();
        for (waiter, (op, outcome)) in &w.check.outcome_of {
            let first = by_op.entry(*op).or_insert(*outcome);
            if first != outcome {
                w.check.fail(
                    "F1.7",
                    &format!("waiter {waiter:?} of {op} got another outcome"),
                );
            }
        }
    }
}

// F1.8 ---------------------------------------------------------------------------------

/// F1.8 a join after the twin finished or was refused: keys resubmitted at the very
/// farm time their operation is answered (after the result, before the tick) or
/// refused (right after the tick that refused it). Each resubmission queues a new
/// operation instead of joining the finished one; every waiter is answered once.
#[derive(Debug, Default)]
pub struct JoinAfterFinish {
    keys: BTreeMap<OperationId, (u64, usize, u32)>,
    pub after_answer: u64,
    pub after_refusal: u64,
}

const RESUBMITS: u32 = 3;

impl JoinAfterFinish {
    fn resubmit(&mut self, w: &mut World, rng: &mut SimRng, refusal: bool) {
        for op in std::mem::take(&mut w.finished) {
            let Some((key, platform, round)) = self.keys.remove(&op) else {
                continue;
            };
            if round >= RESUBMITS {
                continue;
            }
            let qos = pick(rng, &levels());
            let waiter = w.submit(
                request(key, qos, Resources::new(1_000, GIB), platform),
                rng.between(5, 20),
            );
            let new = w.check.op_of(waiter).unwrap();
            if new == op {
                w.check
                    .fail("I13", &format!("a resubmission joined finished {op}"));
            }
            self.keys.insert(new, (key, platform, round + 1));
            if refusal {
                self.after_refusal += 1;
            } else {
                self.after_answer += 1;
            }
        }
    }
}

impl Scenario for JoinAfterFinish {
    fn name(&self) -> &'static str {
        "F1.8"
    }

    fn fleet(&mut self, _rng: &mut SimRng) -> Vec<Spec> {
        vec![
            Spec::new("w0", 8, 16, 0, Node::LinuxX86),
            Spec::new("w1", 8, 16, 0, Node::LinuxX86),
        ]
    }

    fn second(&mut self, w: &mut World, rng: &mut SimRng) {
        if w.t == 0 {
            for key in 0..rng.between(4, 10) {
                // Some keys ask for a platform no worker has: they are refused.
                let platform = if rng.below(3) == 0 { DARWIN } else { LINUX };
                let waiter = w.submit(
                    request(key, Qos::Ci, Resources::new(1_000, GIB), platform),
                    rng.between(5, 20),
                );
                self.keys
                    .insert(w.check.op_of(waiter).unwrap(), (key, platform, 0));
            }
        }
        self.resubmit(w, rng, false);
    }

    fn after_tick(&mut self, w: &mut World, rng: &mut SimRng) {
        self.resubmit(w, rng, true);
    }

    fn quiet_after(&self) -> u64 {
        // Three refusals in a row, a wait apiece.
        4 * 61
    }

    fn horizon(&self) -> u64 {
        4 * 61 + 30
    }
}
