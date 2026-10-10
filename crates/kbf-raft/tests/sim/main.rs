//! The Raft core inside the deterministic simulator, driven by the host loop
//! (`kbf_raft::Host`) over an in-memory disk: 3 voters and 1 learner on a network that
//! drops, duplicates, delays and reorders messages, under seeded partitions and
//! crashes, for many seeds.
//!
//! After every simulation step the checker tests Raft's safety properties (election
//! safety, log matching, leader completeness, state machine safety) against what every
//! node has done so far, and the host's contract: no message promises state that is
//! not synced, and entries are applied in index order. After the faults stop, the run
//! must make progress: a command proposed after the heal is applied on every server,
//! learner included, and every server has applied the same sequence.
//!
//! Every node now and then snapshots its state machine and compacts its log through
//! the applied index (staying under the floor every server's disk has reached, since
//! snapshots are not sent yet). A node restarts from its latest snapshot plus the log
//! after it.
//!
//! A crash strikes just before one of the host's operations on its parts (a write, a
//! sync, a snapshot write, a send, an apply): the operations before it happened (a
//! message may already be on the wire), the rest did not, the disk keeps what was
//! synced plus a seeded prefix of the unsynced writes, and the node restarts later
//! from its disk alone. The seeded sweep crashes at random boundaries;
//! `a_crash_at_every_boundary_recovers` crashes each server at each boundary it
//! passes in a fault-free run, one boundary per run. That is how a host that sends
//! before it syncs, or applies what is not committed, gets caught.
//!
//! A failure names its seed; `run(seed)` replays it exactly.

mod checks;
mod node;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use kbf_raft::{LogIndex, Membership, Role, ServerId};
use kbf_sim::{Chance, Faults, Partition, Sim, TraceHash};
use kbf_types::FarmTime;

use checks::Checker;
use node::{Coverage, Plan, RaftNode, node_id};

/// Seeds per CI run. `many_seeds` (ignored by default) runs more.
const SEEDS: u64 = 500;
/// Faults (partitions, crashes) happen before this time, in ms; proposals stop too,
/// except the one final command a leader proposes after it.
const HEAL_MS: u64 = 3_000;
/// Progress is due this long after the heal, in ms: the run ends at the first step
/// after it at which every server has applied the final command and the same entries
/// as every other.
const DUE_AFTER_HEAL_MS: u64 = 3_000;
/// The run fails if that has not happened this long after the heal, in ms.
const END_AFTER_HEAL_MS: u64 = 5_000;
/// The heal in the runs that crash one server at one boundary: long enough for an
/// election and a few proposals, short enough to try every boundary.
const BOUNDARY_HEAL_MS: u64 = 400;

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

/// What one run does besides the seed.
#[derive(Clone, Copy, Debug)]
struct Scenario {
    /// Drops, duplicates and reorders messages, and changes partitions, before the
    /// heal.
    lossy: bool,
    heal_ms: u64,
    crash_per_million: u64,
    crash_at: Option<(ServerId, u64)>,
}

/// The seeded sweep: every fault, crashes at random boundaries.
const SWEEP: Scenario = Scenario {
    lossy: true,
    heal_ms: HEAL_MS,
    crash_per_million: 10_000,
    crash_at: None,
};

/// What a run that ended well shows.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Outcome {
    trace: TraceHash,
    coverage: Coverage,
    /// The boundaries each server had passed when the faults stopped.
    boundaries_at_heal: BTreeMap<ServerId, u64>,
}

/// Runs one seed of the sweep.
fn run(seed: u64) -> Result<Outcome, String> {
    run_scenario(seed, SWEEP)
}

/// Runs one seed. Returns what the run showed, or what went wrong and when.
fn run_scenario(seed: u64, scenario: Scenario) -> Result<Outcome, String> {
    let faults = if scenario.lossy {
        lossy()
    } else {
        Faults::default()
    };
    let mut sim: Sim<RaftNode> = Sim::new(seed, faults);
    let heal = FarmTime::from_millis(scenario.heal_ms);
    let plan = Plan {
        membership: Membership::new(VOTERS, [LEARNER]).expect("disjoint, non-empty"),
        max_entries_per_append: usize::try_from(sim.rng().between(1, 2)).expect("small"),
        crash_per_million: scenario.crash_per_million,
        crash_at: scenario.crash_at,
        heal,
        max_proposals: 16,
        compact_per_million: 100_000,
        floor: Rc::new(Cell::new(LogIndex(0))),
    };
    let floor = Rc::clone(&plan.floor);
    for id in VOTERS.into_iter().chain([LEARNER]) {
        sim.add_node(node_id(id), RaftNode::new(id, plan.clone()));
    }
    sim.partition_at(heal, Partition::none());

    let mut checker = Checker::default();
    let mut nemesis = Nemesis {
        next: FarmTime::from_millis(sim.rng().between(0, 300)),
    };
    let mut boundaries_at_heal = BTreeMap::new();
    let due = FarmTime::from_millis(scenario.heal_ms + DUE_AFTER_HEAL_MS);
    let end = FarmTime::from_millis(scenario.heal_ms + END_AFTER_HEAL_MS);
    loop {
        if !sim.step() {
            return Err(format!("seed {seed}: the simulation ran out of events"));
        }
        let now = sim.now();
        if now < heal {
            if scenario.lossy {
                nemesis.act(&mut sim);
            }
        } else if boundaries_at_heal.is_empty() {
            boundaries_at_heal = sim.nodes().map(|(_, n)| (n.id(), n.boundaries())).collect();
        }
        checker
            .check(sim.nodes().map(|(_, n)| n))
            .map_err(|v| format!("seed {seed} at {} ms: {v:?}", now.as_millis()))?;
        floor.set(checker.compaction_floor(sim.nodes().map(|(_, n)| n)));
        if now < due {
            continue;
        }
        match check_liveness(&sim) {
            Ok(()) => {
                return Ok(Outcome {
                    trace: sim.trace_hash(),
                    coverage: coverage(&sim),
                    boundaries_at_heal,
                });
            }
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
/// the same entries as every other, and no leader reports a peer behind its snapshot
/// base.
fn check_liveness(sim: &Sim<RaftNode>) -> Result<(), String> {
    let mut reference: Option<&RaftNode> = None;
    for (_, node) in sim.nodes() {
        let Some(core) = node.core() else {
            return Err(format!("{} is down", node.id()));
        };
        if !core.behind_base().is_empty() {
            return Err(format!(
                "{} reports {:?} behind its snapshot base",
                node.id(),
                core.behind_base()
            ));
        }
        if !node.applied_final() {
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

fn coverage(sim: &Sim<RaftNode>) -> Coverage {
    sim.nodes()
        .fold(Coverage::default(), |acc, (_, n)| add(acc, n.coverage()))
}

fn add(a: Coverage, b: Coverage) -> Coverage {
    Coverage {
        compactions: a.compactions + b.compactions,
        restores_from_base: a.restores_from_base + b.restores_from_base,
        crashes: a.crashes + b.crashes,
        crashes_losing_writes: a.crashes_losing_writes + b.crashes_losing_writes,
    }
}

fn assert_no_failures(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{} runs failed; first ones:\n{}",
        failures.len(),
        failures
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn sweep(seeds: std::ops::Range<u64>) {
    let mut failures = Vec::new();
    let mut seeds_restoring = 0;
    let mut total = Coverage::default();
    let n = seeds.end - seeds.start;
    for seed in seeds {
        match run(seed) {
            Ok(o) => {
                seeds_restoring += u64::from(o.coverage.restores_from_base > 0);
                total = add(total, o.coverage);
            }
            Err(e) => failures.push(e),
        }
    }
    assert_no_failures(&failures);
    eprintln!("{n} seeds: {total:?}, {seeds_restoring} seeds restored from a snapshot");
    // The sweep must exercise what it claims to check: compactions, restarts from a
    // snapshot in a good share of the seeds, and crashes that lose unsynced writes
    // (a crash lands between a write and its sync only at a few boundaries).
    assert!(
        total.compactions >= n && seeds_restoring * 2 >= n && total.crashes_losing_writes * 10 >= n,
        "too little coverage over {n} seeds: {total:?}, {seeds_restoring} seeds restored from a snapshot"
    );
}

/// Catches: any break of election safety (e.g. a server that grants two votes in one
/// term), log matching, leader completeness or state machine safety (e.g. a host that
/// sends an acknowledgement before it syncs the entries, or a leader that commits an
/// entry of an earlier term by counting its replicas), a host that applies entries
/// out of order or before they commit, a snapshot that does not hold exactly the
/// entries through its base, one restored that loses the base's term or applies the
/// base's entries again, and a cluster that stops making progress once the faults
/// stop (lost retransmission, a learner left behind).
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

/// Seeds of the fault-free runs `a_crash_at_every_boundary_recovers` crashes.
const BOUNDARY_SEEDS: [u64; 2] = [1, 2];

/// Catches what the seeded sweep may miss by chance: a host whose crash at one
/// particular operation leaves a disk that breaks a Raft property or cannot recover
/// (a send before the sync of what it promises, an apply of an entry not committed,
/// an apply out of order, a snapshot write that loses entries). For each seed, a
/// fault-free run counts the boundaries each server passes before the heal; then, for
/// every server and every one of those boundaries, a run crashes that server just
/// before it, once, and must keep every property and recover.
#[test]
fn a_crash_at_every_boundary_recovers() {
    let quiet = Scenario {
        lossy: false,
        heal_ms: BOUNDARY_HEAL_MS,
        crash_per_million: 0,
        crash_at: None,
    };
    let mut failures = Vec::new();
    let mut runs = 0u64;
    let mut losing = 0u64;
    for seed in BOUNDARY_SEEDS {
        let reference = run_scenario(seed, quiet).expect("a fault-free run is safe and live");
        assert_eq!(reference.coverage.crashes, 0);
        for (&server, &passed) in &reference.boundaries_at_heal {
            assert!(passed > 0, "seed {seed}: {server} passed no boundary");
            for k in 0..passed {
                let scenario = Scenario {
                    crash_at: Some((server, k)),
                    ..quiet
                };
                runs += 1;
                match run_scenario(seed, scenario) {
                    Ok(o) if o.coverage.crashes == 1 => {
                        losing += o.coverage.crashes_losing_writes;
                    }
                    Ok(o) => failures.push(format!(
                        "seed {seed}, {server} at boundary {k}: {} crashes, not 1",
                        o.coverage.crashes
                    )),
                    Err(e) => failures.push(format!("{server} at boundary {k}: {e}")),
                }
            }
        }
    }
    assert_no_failures(&failures);
    eprintln!("{runs} single-crash runs, {losing} of them lost unsynced writes");
    assert!(losing > 0, "no crash lost an unsynced write");
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
