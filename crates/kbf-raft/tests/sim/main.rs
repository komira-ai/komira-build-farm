//! The Raft core inside the deterministic simulator: 3 voters and 1 learner on a
//! network that drops, duplicates, delays and reorders messages, under seeded
//! partitions and crashes, for many seeds.
//!
//! After every simulation step the checker tests Raft's safety properties (election
//! safety, log matching, leader completeness, state machine safety) against what every
//! node has done so far. After the faults stop, the run must make progress: a command
//! proposed after the heal is applied on every server, learner included, and every
//! server has applied the same sequence.
//!
//! A crash may strike part-way through the effects of one input: the effects before
//! it happened (a message may already be on the wire), the rest did not, and the node
//! restarts later from its disk alone. That is how a core that acknowledges before it
//! persists gets caught.
//!
//! A failure names its seed; `run(seed)` replays it exactly.

mod checks;
mod node;

use kbf_raft::{Membership, Role, ServerId};
use kbf_sim::{Chance, Faults, Partition, Sim, TraceHash};
use kbf_types::FarmTime;

use checks::Checker;
use node::{Plan, RaftNode, is_final, node_id};

/// Seeds per CI run. `many_seeds` (ignored by default) runs more.
const SEEDS: u64 = 500;
/// Faults (partitions, crashes) happen before this time, in ms; proposals stop too,
/// except the one final command a leader proposes after it.
const HEAL_MS: u64 = 3_000;
/// Progress is due by here, in ms: the run ends at the first step after this at which
/// every server has applied the final command and the same entries as every other.
const DUE_MS: u64 = 6_000;
/// The run fails if that has not happened by here, in ms.
const END_MS: u64 = 8_000;

const VOTERS: [ServerId; 3] = [ServerId(1), ServerId(2), ServerId(3)];
const LEARNER: ServerId = ServerId(4);

fn lossy() -> Faults {
    Faults {
        drop: Chance::percent(5),
        duplicate: Chance::percent(5),
        reorder: Chance::percent(5),
        ..Faults::default()
    }
}

/// Runs one seed. Returns the trace hash, or what went wrong and when.
fn run(seed: u64) -> Result<TraceHash, String> {
    let mut sim: Sim<RaftNode> = Sim::new(seed, lossy());
    let heal = FarmTime::from_millis(HEAL_MS);
    let plan = Plan {
        membership: Membership::new(VOTERS, [LEARNER]).expect("disjoint, non-empty"),
        max_entries_per_append: usize::try_from(sim.rng().between(1, 2)).expect("small"),
        crash_per_million: 10_000,
        heal,
        max_proposals: 16,
    };
    for id in VOTERS.into_iter().chain([LEARNER]) {
        sim.add_node(node_id(id), RaftNode::new(id, plan.clone()));
    }
    sim.partition_at(heal, Partition::none());

    let mut checker = Checker::default();
    let mut nemesis = Nemesis {
        next: FarmTime::from_millis(sim.rng().between(0, 300)),
    };
    let due = FarmTime::from_millis(DUE_MS);
    let end = FarmTime::from_millis(END_MS);
    loop {
        if !sim.step() {
            return Err(format!("seed {seed}: the simulation ran out of events"));
        }
        let now = sim.now();
        if now < heal {
            nemesis.act(&mut sim);
        }
        checker
            .check(sim.nodes().map(|(_, n)| n))
            .map_err(|v| format!("seed {seed} at {} ms: {v:?}", now.as_millis()))?;
        if now < due {
            continue;
        }
        match check_liveness(&sim) {
            Ok(()) => return Ok(sim.trace_hash()),
            Err(e) if now >= end => {
                return Err(format!("seed {seed}: no progress after the heal: {e}"));
            }
            Err(_) => {}
        }
    }
}

/// Changes the network's partition at seeded times before the heal: heals it, splits
/// the servers at random, or (half the time) isolates the current leader.
///
/// An isolated leader keeps appending proposals it cannot commit while the others
/// elect a new leader, and isolating that one next (without healing first) puts the
/// first back with the remaining server. That leaves logs holding different
/// uncommitted entries from different terms at one index: the setting of the paper's
/// Figure 8, where a leader that counts replicas of an earlier term's entry commits
/// something a later leader overwrites.
struct Nemesis {
    next: FarmTime,
}

impl Nemesis {
    fn act(&mut self, sim: &mut Sim<RaftNode>) {
        let now = sim.now();
        if now < self.next {
            return;
        }
        let leader = sim
            .nodes()
            .filter_map(|(id, n)| n.core().map(|c| (c, id)))
            .filter(|(c, _)| c.role() == Role::Leader)
            .max_by_key(|(c, _)| c.hard_state().term)
            .map(|(_, id)| id.clone());
        let partition = match (sim.rng().below(4), leader) {
            (0, _) => Partition::none(),
            (1 | 2, Some(leader)) => Partition::new([[leader]]),
            _ => sim.random_split(),
        };
        sim.partition_at(now, partition);
        let lasts = std::time::Duration::from_millis(sim.rng().between(20, 250));
        self.next = now.saturating_add(lasts);
    }
}

/// Every server is up, has applied a command proposed after the heal, and has applied
/// the same entries as every other.
fn check_liveness(sim: &Sim<RaftNode>) -> Result<(), String> {
    let mut reference: Option<&RaftNode> = None;
    for (_, node) in sim.nodes() {
        if node.core().is_none() {
            return Err(format!("{} is down", node.id()));
        }
        if !node.applied().iter().any(|e| is_final(&e.payload)) {
            return Err(format!(
                "{} applied {} entries, none after the heal",
                node.id(),
                node.applied().len()
            ));
        }
        match reference {
            None => reference = Some(node),
            Some(r) if r.applied() != node.applied() => {
                return Err(format!(
                    "{} and {} applied different sequences ({} and {} entries)",
                    r.id(),
                    node.id(),
                    r.applied().len(),
                    node.applied().len()
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn sweep(seeds: std::ops::Range<u64>) {
    let failures: Vec<String> = seeds.filter_map(|seed| run(seed).err()).collect();
    assert!(
        failures.is_empty(),
        "{} seeds failed; first ones:\n{}",
        failures.len(),
        failures
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Catches: any break of election safety (e.g. a server that grants two votes in one
/// term), log matching, leader completeness or state machine safety (e.g. a follower
/// that acknowledges entries before persisting them, or a leader that commits an entry
/// of an earlier term by counting its replicas), and a cluster that stops making
/// progress once the faults stop (lost retransmission, a learner left behind).
#[test]
fn raft_is_safe_and_live_over_seeds() {
    sweep(0..SEEDS);
}

/// The same check over many more seeds, for local runs:
/// `cargo test -p kbf-raft --release --test sim -- --ignored`.
#[test]
#[ignore = "long: run locally with --ignored"]
fn raft_is_safe_and_live_over_many_seeds() {
    sweep(SEEDS..20 * SEEDS);
}

/// Catches: a hidden source of nondeterminism in the core or the harness (a clock, an
/// unseeded random draw, hashed iteration), which would make a failing seed useless as
/// a bug report.
#[test]
fn a_seed_replays_exactly() {
    for seed in [3, 77] {
        assert_eq!(run(seed), run(seed), "seed {seed}");
    }
    assert_ne!(run(3), run(77));
}
