//! The run loop: a virtual clock, an event queue and the faulty bus.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::FarmTime;

use crate::trace::Trace;
use crate::{Event, Faults, Node, NodeId, NodeInput, Output, Partition, SimRng, TraceHash};

/// Something due at a point in virtual time.
#[derive(Debug)]
enum Scheduled<M> {
    Start {
        node: NodeId,
    },
    Deliver {
        id: u64,
        from: NodeId,
        to: NodeId,
        msg: M,
    },
    Timer {
        node: NodeId,
        tag: u64,
    },
    Partition(Partition),
}

/// A deterministic discrete-event simulation of nodes of type `N`.
///
/// Events wait in a queue ordered by (virtual time, order of scheduling); [`Sim::step`]
/// takes the first, moves the clock to its time and applies it. Every random choice
/// comes from one [`SimRng`] seeded at construction, every collection is ordered, and
/// nothing reads a real clock, so a seed and a scenario replay to the same
/// [`TraceHash`].
pub struct Sim<N: Node> {
    now: FarmTime,
    rng: SimRng,
    faults: Faults,
    nodes: BTreeMap<NodeId, N>,
    queue: BTreeMap<(FarmTime, u64), Scheduled<N::Msg>>,
    next_seq: u64,
    next_msg: u64,
    partition: Partition,
    /// Latest delivery time scheduled on each (sender, receiver) link, for FIFO order.
    link_last: BTreeMap<(NodeId, NodeId), FarmTime>,
    trace: Trace,
}

impl<N: Node> Sim<N> {
    /// An empty simulation at farm time zero, with its random stream seeded by `seed`.
    ///
    /// # Panics
    ///
    /// If `faults.min_delay` exceeds `faults.max_delay`.
    #[must_use]
    pub fn new(seed: u64, faults: Faults) -> Self {
        assert!(
            faults.min_delay <= faults.max_delay,
            "min_delay {:?} exceeds max_delay {:?}",
            faults.min_delay,
            faults.max_delay
        );
        Self {
            now: FarmTime::default(),
            rng: SimRng::from_seed(seed),
            faults,
            nodes: BTreeMap::new(),
            queue: BTreeMap::new(),
            next_seq: 0,
            next_msg: 0,
            partition: Partition::none(),
            link_last: BTreeMap::new(),
            trace: Trace::new(),
        }
    }

    /// Keeps the trace as text as well as hashing it, for [`Sim::trace_lines`].
    #[must_use]
    pub fn with_trace_lines(mut self) -> Self {
        self.trace.keep_lines();
        self
    }

    /// Adds `node` as `id`. It receives [`Event::Start`] at the current time.
    ///
    /// # Panics
    ///
    /// If a node called `id` already exists.
    pub fn add_node(&mut self, id: impl Into<NodeId>, node: N) {
        let id = id.into();
        assert!(!self.nodes.contains_key(&id), "node {id} added twice");
        self.nodes.insert(id.clone(), node);
        self.schedule(self.now, Scheduled::Start { node: id });
    }

    /// Puts `partition` in force at `at` (or now, if `at` has passed). Messages already
    /// in flight between nodes it separates are lost when they would arrive.
    /// [`Partition::none`] heals.
    pub fn partition_at(&mut self, at: FarmTime, partition: Partition) {
        self.schedule(at.max(self.now), Scheduled::Partition(partition));
    }

    /// A random split of the current nodes into two non-empty groups, drawn from the
    /// run's seed. With fewer than two nodes, [`Partition::none`].
    pub fn random_split(&mut self) -> Partition {
        let mut ids: Vec<NodeId> = self.nodes.keys().cloned().collect();
        if ids.len() < 2 {
            return Partition::none();
        }
        self.rng.shuffle(&mut ids);
        let cut =
            usize::try_from(self.rng.between(1, ids.len() as u64 - 1)).expect("cut fits in usize");
        let right = ids.split_off(cut);
        Partition::new([ids, right])
    }

    /// The run's random stream, for scenario choices (when to partition, how long) that
    /// should follow the seed too.
    pub fn rng(&mut self) -> &mut SimRng {
        &mut self.rng
    }

    /// The current virtual time.
    #[must_use]
    pub fn now(&self) -> FarmTime {
        self.now
    }

    /// The node called `id`.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&N> {
        self.nodes.get(id)
    }

    /// Every node, in name order.
    pub fn nodes(&self) -> impl Iterator<Item = (&NodeId, &N)> {
        self.nodes.iter()
    }

    /// The hash of everything that has happened so far.
    #[must_use]
    pub fn trace_hash(&self) -> TraceHash {
        self.trace.hash()
    }

    /// The trace as text, if [`Sim::with_trace_lines`] asked for it.
    #[must_use]
    pub fn trace_lines(&self) -> Option<&[String]> {
        self.trace.lines()
    }

    /// Applies the next event. Returns false when nothing is queued.
    pub fn step(&mut self) -> bool {
        let Some(((at, _), event)) = self.queue.pop_first() else {
            return false;
        };
        self.now = at;
        let t = at.as_millis();
        match event {
            Scheduled::Start { node } => {
                self.trace.record(format!("{t} start {node}"));
                self.apply(&node, Event::Start);
            }
            Scheduled::Timer { node, tag } => {
                self.trace.record(format!("{t} timer {node} tag={tag}"));
                self.apply(&node, Event::Timer { tag });
            }
            Scheduled::Partition(p) => {
                self.trace.record(format!("{t} partition {p:?}"));
                self.partition = p;
            }
            Scheduled::Deliver { id, from, to, msg } => {
                if self.partition.connected(&from, &to) {
                    self.trace
                        .record(format!("{t} deliver #{id} {from}->{to} {msg:?}"));
                    self.apply(&to, Event::Message { from, msg });
                } else {
                    self.trace
                        .record(format!("{t} lost #{id} {from}->{to} partitioned"));
                }
            }
        }
        true
    }

    /// Applies every event due at or before `until`, then sets the clock to `until` (if
    /// it is later than now).
    pub fn run_until(&mut self, until: FarmTime) {
        while self
            .queue
            .first_key_value()
            .is_some_and(|(&(at, _), _)| at <= until)
        {
            self.step();
        }
        self.now = self.now.max(until);
    }

    fn schedule(&mut self, at: FarmTime, event: Scheduled<N::Msg>) {
        self.queue.insert((at, self.next_seq), event);
        self.next_seq += 1;
    }

    /// Feeds one event to `id` and carries out the outputs it queues.
    fn apply(&mut self, id: &NodeId, event: Event<N::Msg>) {
        let entropy = self.rng.next_u64();
        let input = NodeInput {
            now: self.now,
            entropy,
            event,
        };
        let node = self
            .nodes
            .get_mut(id)
            .expect("events are only scheduled for added nodes");
        let effects = node.apply(input);
        let outputs = node.take_outputs();
        if !effects.is_empty() {
            let t = self.now.as_millis();
            self.trace.record(format!("{t} effects {id} {effects:?}"));
        }
        for out in outputs {
            match out {
                Output::Send { to, msg } => self.send(id, to, msg),
                Output::Timer { after, tag } => {
                    let at = self.now.saturating_add(after);
                    self.schedule(
                        at,
                        Scheduled::Timer {
                            node: id.clone(),
                            tag,
                        },
                    );
                }
            }
        }
    }

    /// Puts one message on the bus, applying the configured faults.
    fn send(&mut self, from: &NodeId, to: NodeId, msg: N::Msg) {
        let id = self.next_msg;
        self.next_msg += 1;
        let t = self.now.as_millis();
        if !self.nodes.contains_key(&to) {
            self.trace
                .record(format!("{t} lost #{id} {from}->{to} unknown receiver"));
            return;
        }
        if !self.partition.connected(from, &to) {
            self.trace
                .record(format!("{t} lost #{id} {from}->{to} partitioned"));
            return;
        }
        if self.rng.chance(self.faults.drop) {
            self.trace
                .record(format!("{t} lost #{id} {from}->{to} dropped"));
            return;
        }
        let copies = if self.rng.chance(self.faults.duplicate) {
            2
        } else {
            1
        };
        for copy in 0..copies {
            let delay = self.draw_delay();
            let mut at = self.now.saturating_add(delay);
            let reordered = self.rng.chance(self.faults.reorder);
            if !reordered {
                let last = self
                    .link_last
                    .entry((from.clone(), to.clone()))
                    .or_default();
                at = at.max(*last);
                *last = at;
            }
            let how = match (copy, reordered) {
                (0, false) => "",
                (0, true) => " reordered",
                (_, false) => " duplicate",
                (_, true) => " duplicate reordered",
            };
            self.trace.record(format!(
                "{t} send #{id} {from}->{to} at={}{how}",
                at.as_millis()
            ));
            let event = Scheduled::Deliver {
                id,
                from: from.clone(),
                to: to.clone(),
                msg: msg.clone(),
            };
            self.schedule(at, event);
        }
    }

    fn draw_delay(&mut self) -> Duration {
        let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(
            self.rng
                .between(millis(self.faults.min_delay), millis(self.faults.max_delay)),
        )
    }
}
