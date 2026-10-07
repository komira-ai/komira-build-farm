//! The simulator finds a replication bug, and its seed reproduces it.
//!
//! A primary counts to `TARGET`, one increment every 10 ms, and replicates each
//! increment to two replicas. `FireAndForget` sends each increment once, without
//! acknowledgement: a deliberately buggy protocol, correct only on a perfect network.
//! `Acked` numbers increments, has replicas apply them in order and acknowledge, and
//! resends everything unacknowledged on every tick. The invariant, checked well after
//! the partition heals: every replica's count equals the primary's.

use std::time::Duration;

use kbf_sim::{Chance, Event, Faults, Node, NodeId, NodeInput, Output, Partition, Sim, TraceHash};
use kbf_types::{Effect, FarmTime, StateMachine};

const TARGET: u64 = 20;
const TICK: Duration = Duration::from_millis(10);
const SEEDS: u64 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    FireAndForget,
    Acked,
}

#[derive(Clone, Debug)]
enum Msg {
    /// Increment number `seq` (1-based).
    Inc { seq: u64 },
    /// The replica has applied increments up to `seq`.
    Ack { seq: u64 },
}

struct Counter {
    protocol: Protocol,
    /// The replicas, on the primary; empty on a replica.
    replicas: Vec<NodeId>,
    /// Increments applied.
    value: u64,
    /// On the primary: the highest increment each replica has acknowledged.
    acked: Vec<u64>,
    out: Vec<Output<Msg>>,
}

impl Counter {
    fn primary(protocol: Protocol) -> Self {
        Self {
            protocol,
            replicas: vec![NodeId::from("replica-1"), NodeId::from("replica-2")],
            value: 0,
            acked: vec![0, 0],
            out: Vec::new(),
        }
    }

    fn replica(protocol: Protocol) -> Self {
        Self {
            replicas: Vec::new(),
            acked: Vec::new(),
            ..Self::primary(protocol)
        }
    }

    fn send(&mut self, to: NodeId, msg: Msg) {
        self.out.push(Output::Send { to, msg });
    }

    fn on_tick(&mut self) {
        if self.value < TARGET {
            self.value += 1;
            if self.protocol == Protocol::FireAndForget {
                for r in self.replicas.clone() {
                    self.send(r, Msg::Inc { seq: self.value });
                }
            }
        }
        if self.protocol == Protocol::Acked {
            for (r, acked) in self.replicas.clone().into_iter().zip(self.acked.clone()) {
                for seq in acked + 1..=self.value {
                    self.send(r.clone(), Msg::Inc { seq });
                }
            }
        }
        self.out.push(Output::Timer {
            after: TICK,
            tag: 0,
        });
    }

    fn on_message(&mut self, from: NodeId, msg: Msg) {
        match (self.protocol, msg) {
            (Protocol::FireAndForget, Msg::Inc { .. }) => self.value += 1,
            (Protocol::Acked, Msg::Inc { seq }) => {
                if seq == self.value + 1 {
                    self.value = seq;
                }
                self.send(from, Msg::Ack { seq: self.value });
            }
            (_, Msg::Ack { seq }) => {
                if let Some(i) = self.replicas.iter().position(|r| *r == from) {
                    self.acked[i] = self.acked[i].max(seq);
                }
            }
        }
    }
}

impl StateMachine for Counter {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        match input.event {
            Event::Start if !self.replicas.is_empty() => self.out.push(Output::Timer {
                after: TICK,
                tag: 0,
            }),
            Event::Start => {}
            Event::Timer { .. } => self.on_tick(),
            Event::Message { from, msg } => self.on_message(from, msg),
        }
        Vec::new()
    }
}

impl Node for Counter {
    type Msg = Msg;

    fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }
}

/// How a run ended: each node's count (in name order) and the trace hash.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    counts: Vec<(String, u64)>,
    hash: TraceHash,
}

impl Outcome {
    fn diverged(&self) -> bool {
        self.counts.iter().any(|(_, c)| *c != TARGET)
    }
}

/// Runs the counter under `faults`; with `partitioned`, a seeded split of the three
/// nodes starts somewhere in the first 200 ms and lasts 20 to 100 ms.
fn run(seed: u64, protocol: Protocol, faults: Faults, partitioned: bool) -> Outcome {
    let mut sim = Sim::new(seed, faults);
    sim.add_node("primary", Counter::primary(protocol));
    sim.add_node("replica-1", Counter::replica(protocol));
    sim.add_node("replica-2", Counter::replica(protocol));
    if partitioned {
        let start = sim.rng().between(0, 200);
        let len = sim.rng().between(20, 100);
        let split = sim.random_split();
        sim.partition_at(FarmTime::from_millis(start), split);
        sim.partition_at(FarmTime::from_millis(start + len), Partition::none());
    }
    // The last increment is at 200 ms; the partition is over by 300 ms.
    sim.run_until(FarmTime::from_millis(1_000));
    Outcome {
        counts: sim
            .nodes()
            .map(|(id, n)| (id.as_str().to_owned(), n.value))
            .collect(),
        hash: sim.trace_hash(),
    }
}

fn lossy() -> Faults {
    Faults {
        drop: Chance::percent(10),
        duplicate: Chance::percent(10),
        reorder: Chance::percent(10),
        ..Faults::default()
    }
}

/// Catches: an invariant check that cannot pass (so the failures below would prove
/// nothing), and a kernel that loses messages with no fault configured.
#[test]
fn fire_and_forget_is_correct_on_a_perfect_network() {
    for seed in 0..SEEDS {
        let out = run(seed, Protocol::FireAndForget, Faults::default(), false);
        assert!(!out.diverged(), "seed {seed}: {out:?}");
    }
}

/// Catches: a partition that does not cut traffic, or a seed sweep that never places
/// the partition where it matters; either way the simulator would not find this bug.
/// Then the property that makes a failure actionable: rerunning the failing seed gives
/// the same divergent counts and the same trace, so the seed is the bug report.
#[test]
fn fire_and_forget_fails_under_a_seeded_partition_and_the_seed_reproduces() {
    let failing = (0..SEEDS).find_map(|seed| {
        let out = run(seed, Protocol::FireAndForget, Faults::default(), true);
        out.diverged().then_some((seed, out))
    });
    let Some((seed, first)) = failing else {
        panic!("no seed in 0..{SEEDS} broke the fire-and-forget counter");
    };
    let again = run(seed, Protocol::FireAndForget, Faults::default(), true);
    assert_eq!(first, again, "failing seed {seed} did not reproduce");
    assert!(again.diverged());
}

/// Catches: a kernel whose faults break even a correct protocol (lost messages after
/// the heal, timers that stop), and a correct protocol's retransmit or ordering
/// removed. Three replicas survive every seeded partition, with drop, duplicate and
/// reorder on as well.
#[test]
fn acked_counter_survives_seeded_partitions_and_lossy_links() {
    for seed in 0..SEEDS {
        let out = run(seed, Protocol::Acked, lossy(), true);
        assert!(!out.diverged(), "seed {seed}: {out:?}");
    }
}
