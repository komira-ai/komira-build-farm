//! The bus does what `Faults` and `Partition` say, and nothing else.

use std::time::Duration;

use kbf_sim::{Chance, Event, Faults, Node, NodeId, NodeInput, Output, Partition, Sim};
use kbf_types::{Effect, FarmTime, StateMachine};

/// Sends scripted messages at scripted times and records what it receives.
#[derive(Default)]
struct Recorder {
    /// (send time in ms, receiver, message)
    script: Vec<(u64, NodeId, u32)>,
    out: Vec<Output<u32>>,
    /// (sender, message, arrival time in ms)
    got: Vec<(NodeId, u32, u64)>,
}

impl Recorder {
    fn sending(script: impl IntoIterator<Item = (u64, &'static str, u32)>) -> Self {
        Self {
            script: script
                .into_iter()
                .map(|(t, to, m)| (t, NodeId::from(to), m))
                .collect(),
            ..Self::default()
        }
    }

    fn msgs(&self) -> Vec<u32> {
        self.got.iter().map(|g| g.1).collect()
    }
}

impl StateMachine for Recorder {
    type Input = NodeInput<u32>;

    fn apply(&mut self, input: NodeInput<u32>) -> Vec<Effect> {
        match input.event {
            Event::Start => {
                for (i, (at, _, _)) in self.script.iter().enumerate() {
                    self.out.push(Output::Timer {
                        after: Duration::from_millis(*at),
                        tag: i as u64,
                    });
                }
            }
            Event::Timer { tag } => {
                let (_, to, msg) = self.script[usize::try_from(tag).unwrap()].clone();
                self.out.push(Output::Send { to, msg });
            }
            Event::Message { from, msg } => self.got.push((from, msg, input.now.as_millis())),
        }
        Vec::new()
    }
}

impl Node for Recorder {
    type Msg = u32;

    fn take_outputs(&mut self) -> Vec<Output<u32>> {
        std::mem::take(&mut self.out)
    }
}

/// "a" sends 0..100 to "b" at time 0; returns what "b" received.
fn hundred_to_b(seed: u64, faults: Faults) -> Recorder {
    let mut sim = Sim::new(seed, faults);
    sim.add_node("a", Recorder::sending((0..100).map(|m| (0, "b", m))));
    sim.add_node("b", Recorder::default());
    sim.run_until(FarmTime::from_millis(1_000));
    let b = sim.node(&NodeId::from("b")).expect("b was added");
    Recorder {
        got: b.got.clone(),
        ..Recorder::default()
    }
}

/// Catches: a bus that loses or invents messages with no fault configured, delivers
/// outside `min_delay..=max_delay`, or lets random delays reorder a link when `reorder`
/// is off (the per-link FIFO clamp missing).
#[test]
fn no_faults_delivers_everything_in_order_within_the_delay() {
    for seed in 0..8 {
        let b = hundred_to_b(seed, Faults::default());
        assert_eq!(b.msgs(), (0..100).collect::<Vec<_>>(), "seed {seed}");
        assert!(
            b.got.iter().all(|g| (1..=10).contains(&g.2)),
            "seed {seed}: arrival outside 1..=10 ms: {:?}",
            b.got
        );
    }
}

/// Catches: `drop` ignored, or applied as all-or-nothing.
#[test]
fn drop_loses_its_share() {
    let all = hundred_to_b(
        1,
        Faults {
            drop: Chance::always(),
            ..Faults::default()
        },
    );
    assert!(all.got.is_empty());
    let half = hundred_to_b(
        1,
        Faults {
            drop: Chance::percent(50),
            ..Faults::default()
        },
    );
    let n = half.got.len();
    assert!((20..80).contains(&n), "50% drop kept {n} of 100");
    let msgs = half.msgs();
    assert!(
        msgs.windows(2).all(|w| w[0] < w[1]),
        "survivors stay in order"
    );
}

/// Catches: `duplicate` ignored, or a duplicate that is not a copy of the original.
#[test]
fn duplicate_delivers_each_message_twice() {
    let b = hundred_to_b(
        2,
        Faults {
            duplicate: Chance::always(),
            ..Faults::default()
        },
    );
    let mut msgs = b.msgs();
    msgs.sort_unstable();
    let twice: Vec<u32> = (0..100).flat_map(|m| [m, m]).collect();
    assert_eq!(msgs, twice);
}

/// Catches: `reorder` ignored (the FIFO clamp applied to every copy).
#[test]
fn reorder_lets_messages_overtake() {
    let b = hundred_to_b(
        3,
        Faults {
            reorder: Chance::always(),
            ..Faults::default()
        },
    );
    let msgs = b.msgs();
    assert_eq!(msgs.len(), 100);
    assert!(
        msgs.windows(2).any(|w| w[0] > w[1]),
        "no message overtook another: {msgs:?}"
    );
}

/// Catches: a partition checked only when a message is sent (so a message in flight
/// crosses it), checked only on arrival (so a message sent across it arrives after the
/// heal), or never lifted.
#[test]
fn partition_loses_messages_at_send_and_in_flight_and_heals() {
    let fixed = Faults {
        min_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(10),
        ..Faults::default()
    };
    let mut sim = Sim::new(0, fixed).with_trace_lines();
    // Sent at 0, arrives at 10: in flight when the partition starts at 5.
    // Sent at 20, during the partition: arrives at 30, just after the heal.
    // Sent at 40, after the heal: arrives at 50.
    sim.add_node(
        "a",
        Recorder::sending([(0, "b", 0), (20, "b", 1), (40, "b", 2)]),
    );
    sim.add_node("b", Recorder::default());
    let split = Partition::new([[NodeId::from("a")], [NodeId::from("b")]]);
    sim.partition_at(FarmTime::from_millis(5), split);
    sim.partition_at(FarmTime::from_millis(30), Partition::none());
    sim.run_until(FarmTime::from_millis(100));
    let b = sim.node(&NodeId::from("b")).unwrap();
    assert_eq!(
        b.got,
        vec![(NodeId::from("a"), 2, 50)],
        "trace:\n{}",
        sim.trace_lines().unwrap().join("\n")
    );
}

/// Catches: a send to a node that does not exist panicking the kernel instead of being
/// lost (a real network loses it), and `run_until` leaving the clock behind.
#[test]
fn unknown_receiver_is_lost_and_clock_reaches_the_bound() {
    let mut sim = Sim::new(0, Faults::default()).with_trace_lines();
    sim.add_node("a", Recorder::sending([(0, "nobody", 7)]));
    sim.run_until(FarmTime::from_millis(25));
    assert_eq!(sim.now(), FarmTime::from_millis(25));
    let lines = sim.trace_lines().unwrap();
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with("a->nobody unknown receiver")),
        "{lines:?}"
    );
    assert!(!sim.step(), "nothing left to do");
}
