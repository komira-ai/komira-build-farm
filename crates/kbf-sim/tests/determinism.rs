//! A seed names exactly one run.
//!
//! The workload is gossip among five nodes with every fault switched on and a seeded
//! partition, so many messages are due at the same millisecond and every kernel choice
//! (delay, drop, duplicate, reorder, partition, entropy, order of equal-time events)
//! shows in the trace.

use std::collections::BTreeSet;
use std::time::Duration;

use kbf_sim::{Chance, Event, Faults, Node, NodeId, NodeInput, Output, Sim, TraceHash};
use kbf_types::{Effect, FarmTime, StateMachine};

const NODES: usize = 5;
const ROUNDS: u64 = 20;
const MAX_HOPS: u8 = 3;

#[derive(Clone, Debug)]
#[expect(
    dead_code,
    reason = "origin and round are read only through Debug, in the trace"
)]
struct Rumour {
    origin: usize,
    round: u64,
    hops: u8,
}

struct Gossiper {
    me: usize,
    rounds: u64,
    heard: u64,
    out: Vec<Output<Rumour>>,
}

fn name(i: usize) -> NodeId {
    NodeId::new(format!("n{i}"))
}

impl Gossiper {
    fn tell_random_peer(&mut self, entropy: u64, msg: Rumour) {
        let peer = usize::try_from(entropy % NODES as u64).unwrap();
        self.out.push(Output::Send {
            to: name(peer),
            msg,
        });
    }
}

impl StateMachine for Gossiper {
    type Input = NodeInput<Rumour>;

    fn apply(&mut self, input: NodeInput<Rumour>) -> Vec<Effect> {
        let tick = Output::Timer {
            after: Duration::from_millis(7),
            tag: 0,
        };
        match input.event {
            Event::Start => self.out.push(tick),
            Event::Timer { .. } => {
                self.rounds += 1;
                let msg = Rumour {
                    origin: self.me,
                    round: self.rounds,
                    hops: 0,
                };
                self.tell_random_peer(input.entropy, msg);
                if self.rounds < ROUNDS {
                    self.out.push(tick);
                }
            }
            Event::Message { msg, .. } => {
                self.heard += 1;
                if msg.hops < MAX_HOPS {
                    let next = Rumour {
                        hops: msg.hops + 1,
                        ..msg
                    };
                    self.tell_random_peer(input.entropy, next);
                }
            }
        }
        Vec::new()
    }
}

impl Node for Gossiper {
    type Msg = Rumour;

    fn take_outputs(&mut self) -> Vec<Output<Rumour>> {
        std::mem::take(&mut self.out)
    }
}

/// One gossip run; returns its trace hash and what each node heard.
fn run(seed: u64) -> (TraceHash, Vec<u64>) {
    let faults = Faults {
        min_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
        drop: Chance::percent(5),
        duplicate: Chance::percent(5),
        reorder: Chance::percent(10),
    };
    let mut sim = Sim::new(seed, faults);
    for me in 0..NODES {
        let node = Gossiper {
            me,
            rounds: 0,
            heard: 0,
            out: Vec::new(),
        };
        sim.add_node(name(me), node);
    }
    let start = sim.rng().between(20, 60);
    let len = sim.rng().between(10, 50);
    let split = sim.random_split();
    sim.partition_at(FarmTime::from_millis(start), split);
    sim.partition_at(
        FarmTime::from_millis(start + len),
        kbf_sim::Partition::none(),
    );
    sim.run_until(FarmTime::from_millis(400));
    let heard = sim.nodes().map(|(_, n)| n.heard).collect();
    (sim.trace_hash(), heard)
}

/// Catches: anything in the kernel that varies between runs of one seed: a random
/// stream seeded from the clock or the OS, delivery order taken from a hashed
/// collection, a real clock read. The replay runs on a fresh thread, so a per-thread
/// hasher seed or thread-local state also differs between the two runs.
#[test]
fn the_same_seed_gives_the_same_trace() {
    for seed in 0..4 {
        let here = run(seed);
        let there = std::thread::spawn(move || run(seed)).join().unwrap();
        assert_eq!(here, there, "seed {seed} replayed differently");
    }
}

/// Catches: a seed that does not reach the run (a fixed stream, or a scenario that
/// makes no random choice), which would make every seed test the same thing.
#[test]
fn different_seeds_give_different_traces() {
    let hashes: BTreeSet<TraceHash> = (0..8).map(|seed| run(seed).0).collect();
    assert_eq!(hashes.len(), 8);
}

/// Catches: a change in what a seed means across processes, platforms or releases: the
/// random algorithm or its seed expansion, the order the kernel draws in, the trace
/// format. CI runs this on x86 and arm64. A deliberate kernel change moves this hash in
/// the same commit, and old failing seeds must be re-found.
#[test]
fn seed_one_is_pinned() {
    let (hash, _) = run(1);
    assert_eq!(
        hash.to_string(),
        "d8c5ad04d37349dbdf54dd72ad721f94fb56afd02a8a6ff4036a66e4f09b2657"
    );
}
