//! A Raft server as a simulated node: the core, a disk that survives crashes, a state
//! machine that does not (except through snapshots), a client that proposes commands,
//! log compaction, and crash faults.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use kbf_raft::{
    AppendOutcome, Config, Effect, Entry, HardState, LogId, LogIndex, Membership, Message,
    MessageKind, Payload, Raft, Role, ServerId, Term,
};
use kbf_sim::{Event, Node, NodeId, NodeInput, Output};
use kbf_types::{FarmTime, StateMachine};

/// One logical tick of the Raft core.
pub const TICK: Duration = Duration::from_millis(10);

/// The prefix of the command a leader proposes once the network has healed; the
/// liveness check looks for it.
pub const FINAL: &[u8] = b"final:";

pub fn node_id(s: ServerId) -> NodeId {
    NodeId::new(s.to_string())
}

fn server_id(n: &NodeId) -> ServerId {
    ServerId(
        n.as_str()[1..]
            .parse()
            .expect("simulated nodes are named s<number>"),
    )
}

/// What the scenario decides for every node of one run.
#[derive(Clone, Debug)]
pub struct Plan {
    pub membership: Membership,
    pub max_entries_per_append: usize,
    /// Chance, in parts per million per input, that the node crashes part-way through
    /// carrying out that input's effects.
    pub crash_per_million: u64,
    /// No crashes, partitions or ordinary proposals from here on.
    pub heal: FarmTime,
    /// Ordinary proposals per leader term, at most.
    pub max_proposals: u64,
    /// Chance, in parts per million per tick, that the node tries to compact its log.
    pub compact_per_million: u64,
    /// The highest index every node's disk holds a committed entry at, set by the
    /// harness after each step (see `Checker::compaction_floor`). Nodes compact no
    /// further, so a leader never folds away an entry a peer still needs: sending
    /// snapshots is not implemented, and such a peer could never catch up.
    pub floor: Rc<Cell<LogIndex>>,
}

/// Something the checker needs to see, in the order it happened on this node.
#[derive(Clone, Debug)]
pub enum Observed {
    /// `entry` reached the disk, after an entry of term `prev_term`.
    Persisted { prev_term: Term, entry: Entry },
    /// A message promising durable state left before that state was on disk: an
    /// accepted append whose entries are not all persisted, or a granted vote that is
    /// not.
    UndurablePromise { msg: Message },
    /// `entry` was applied while the node was in `term`; `in_order` is false if it did
    /// not directly follow the previously applied index of this incarnation.
    Applied {
        term: Term,
        entry: Entry,
        in_order: bool,
    },
    /// The core accepted a compaction through `base`, but this node's state machine
    /// does not hold that entry: `applied` is its last applied index, and `held` the id
    /// of the entry it applied at `base.index`, if any.
    BadSnapshot {
        base: LogId,
        applied: LogIndex,
        held: Option<LogId>,
    },
}

/// A snapshot on disk: the id of the last entry it folds in, and the state machine as
/// of that entry (here, the applied entries themselves).
#[derive(Clone, Debug, Default)]
struct Snapshot {
    base: LogId,
    state: Vec<Entry>,
}

/// What a crash keeps. `log` holds the entries after `snapshot.base`.
#[derive(Clone, Debug, Default)]
struct Disk {
    hard: HardState,
    snapshot: Snapshot,
    log: Vec<Entry>,
}

/// What a run exercised, so a sweep can show it covered compaction and restores.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Compactions that moved the snapshot base.
    pub compactions: u64,
    /// Compactions the core refused.
    pub refused: u64,
    /// Restarts from a snapshot with a base past 0.
    pub restores_from_base: u64,
}

pub struct RaftNode {
    id: ServerId,
    plan: Plan,
    core: Option<Raft>,
    down_until: FarmTime,
    disk: Disk,
    /// The state machine of the current incarnation: applied entries, in order.
    applied: Vec<Entry>,
    /// Ordinary proposals made as leader in `proposed_in`.
    proposed: u64,
    proposed_in: Term,
    history: Vec<Observed>,
    out: Vec<Output<Message>>,
    coverage: Coverage,
}

/// SplitMix64's finalizer: decorrelates the bits the harness uses from the ones the
/// core uses for its election timeout.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl RaftNode {
    pub fn new(id: ServerId, plan: Plan) -> Self {
        Self {
            id,
            plan,
            core: None,
            down_until: FarmTime::default(),
            disk: Disk::default(),
            applied: Vec::new(),
            proposed: 0,
            proposed_in: Term(0),
            history: Vec::new(),
            out: Vec::new(),
            coverage: Coverage::default(),
        }
    }

    pub fn id(&self) -> ServerId {
        self.id
    }

    /// The core, while the node is up.
    pub fn core(&self) -> Option<&Raft> {
        self.core.as_ref()
    }

    pub fn applied(&self) -> &[Entry] {
        &self.applied
    }

    pub fn history(&self) -> &[Observed] {
        &self.history
    }

    pub fn coverage(&self) -> Coverage {
        self.coverage
    }

    /// The entry at `index` in the current incarnation's log, the part folded into
    /// the snapshot included.
    pub fn log_entry(&self, index: LogIndex) -> Option<&Entry> {
        let core = self.core.as_ref()?;
        let base = core.snapshot_base().index;
        if index <= base {
            let i = usize::try_from(index.0.checked_sub(1)?).ok()?;
            self.disk.snapshot.state.get(i)
        } else {
            let i = usize::try_from(index.0 - base.0 - 1).ok()?;
            core.entries().get(i)
        }
    }

    /// The highest index this node's disk holds an entry at for which `committed`
    /// holds, or its snapshot base if none. By log matching, everything before it is
    /// committed too.
    pub fn durable_committed(&self, committed: impl Fn(&Entry) -> bool) -> LogIndex {
        self.disk
            .log
            .iter()
            .rev()
            .find(|e| committed(e))
            .map_or(self.disk.snapshot.base.index, |e| e.id.index)
    }

    fn config(&self) -> Config {
        Config {
            id: self.id,
            membership: self.plan.membership.clone(),
            election_ticks: 10,
            heartbeat_ticks: 3,
            max_entries_per_append: self.plan.max_entries_per_append,
        }
    }

    /// Starts a new incarnation from the disk.
    fn boot(&mut self, entropy: u64) {
        let base = self.disk.snapshot.base;
        let core = Raft::restore(
            self.config(),
            self.disk.hard,
            base,
            self.disk.log.clone(),
            entropy,
        )
        .expect("the disk holds what the core persisted");
        self.core = Some(core);
        self.applied.clone_from(&self.disk.snapshot.state);
        if base.index > LogIndex(0) {
            self.coverage.restores_from_base += 1;
        }
    }

    /// Now and then, compacts the log through a random index between the base and
    /// the last entry. Indexes up to the floor and past the applied index are asked
    /// for; the core must refuse the latter, and a compaction it accepts must match
    /// the state machine. The snapshot write and the log trim are one atomic step
    /// here (the storage layer's crash cases are its own tests).
    fn maybe_compact(&mut self, entropy: u64) {
        let roll = mix(entropy ^ 0x636f_6d70);
        let Some(core) = &mut self.core else {
            return;
        };
        if roll % 1_000_000 >= self.plan.compact_per_million {
            return;
        }
        let base = core.snapshot_base().index.0;
        let last = core.last_log_id().index.0;
        let through = LogIndex(base + (roll >> 20) % (last - base + 1));
        if through > self.plan.floor.get() && through <= core.applied_index() {
            return; // a peer may still need it
        }
        let Ok(id) = core.compact(through) else {
            self.coverage.refused += 1;
            return;
        };
        let n = usize::try_from(id.index.0).expect("fits");
        let held = n
            .checked_sub(1)
            .and_then(|i| self.applied.get(i))
            .map(|e| e.id);
        let matches = held.unwrap_or_default() == id && n <= self.applied.len();
        if !matches {
            self.history.push(Observed::BadSnapshot {
                base: id,
                applied: LogIndex(self.applied.len() as u64),
                held,
            });
            return;
        }
        if id != self.disk.snapshot.base {
            self.coverage.compactions += 1;
        }
        self.disk.snapshot = Snapshot {
            base: id,
            state: self.applied[..n].to_vec(),
        };
        self.disk.log.retain(|e| e.id.index > id.index);
    }

    fn on_tick(&mut self, now: FarmTime, entropy: u64) -> Vec<Effect> {
        let roll = mix(entropy ^ 0x7469_636b);
        let final_in_snapshot = self
            .disk
            .snapshot
            .state
            .iter()
            .any(|e| is_final(&e.payload));
        let Some(core) = &mut self.core else {
            return Vec::new();
        };
        let mut effects = core.tick(entropy);
        if core.role() != Role::Leader {
            return effects;
        }
        let term = core.hard_state().term;
        if term != self.proposed_in {
            self.proposed_in = term;
            self.proposed = 0;
        }
        let command = if now < self.plan.heal {
            let due = roll.is_multiple_of(3) && self.proposed < self.plan.max_proposals;
            due.then(|| format!("{}:{}:{}", self.id, term.0, self.proposed).into_bytes())
        } else {
            // After the heal, one final command, until this leader's log holds one.
            let has_final =
                final_in_snapshot || core.entries().iter().any(|e| is_final(&e.payload));
            (!has_final).then(|| [FINAL, self.id.to_string().as_bytes()].concat())
        };
        if let Some(command) = command {
            let p = core.propose(command).expect("checked: this node leads");
            self.proposed += 1;
            effects.extend(p.effects);
        }
        effects
    }

    /// Carries out `effects` in order, unless the node crashes part-way.
    fn carry_out(&mut self, now: FarmTime, entropy: u64, effects: Vec<Effect>) {
        let roll = mix(entropy);
        let crash = now < self.plan.heal && roll % 1_000_000 < self.plan.crash_per_million;
        let keep = if crash {
            usize::try_from((roll >> 20) % (effects.len() as u64 + 1)).expect("fits")
        } else {
            effects.len()
        };
        for effect in effects.into_iter().take(keep) {
            self.carry_out_one(effect);
        }
        if crash {
            self.core = None;
            self.down_until = now.saturating_add(Duration::from_millis((roll >> 44) % 400));
        }
    }

    fn carry_out_one(&mut self, effect: Effect) {
        match effect {
            Effect::PersistHardState(hard) => self.disk.hard = hard,
            Effect::PersistEntries(entries) => {
                let first = entries.first().expect("never empty").id.index;
                let base = self.disk.snapshot.base;
                let keep = usize::try_from(first.0 - base.index.0 - 1).expect("after the base");
                self.disk.log.truncate(keep);
                for entry in entries {
                    let prev_term = self.disk.log.last().map_or(base.term, |e| e.id.term);
                    self.history.push(Observed::Persisted {
                        prev_term,
                        entry: entry.clone(),
                    });
                    self.disk.log.push(entry);
                }
            }
            Effect::Send { to, msg } => {
                if !self.is_durable(to, &msg) {
                    let msg = msg.clone();
                    self.history.push(Observed::UndurablePromise { msg });
                }
                self.out.push(Output::Send {
                    to: node_id(to),
                    msg,
                });
            }
            Effect::Apply(entry) => {
                let expected = LogIndex(self.applied.len() as u64 + 1);
                let term = self.core.as_ref().map_or(Term(0), |c| c.hard_state().term);
                self.history.push(Observed::Applied {
                    term,
                    entry: entry.clone(),
                    in_order: entry.id.index == expected,
                });
                self.applied.push(entry);
            }
        }
    }
}

impl RaftNode {
    /// Whether what `msg` promises `to` is on this node's disk.
    fn is_durable(&self, to: ServerId, msg: &Message) -> bool {
        match msg.kind {
            MessageKind::AppendResponse {
                outcome: AppendOutcome::Accepted { matched },
            } => {
                let Some(core) = &self.core else {
                    return true;
                };
                // Through the base, the entries are in the snapshot on disk.
                let base = self.disk.snapshot.base.index;
                debug_assert_eq!(base, core.snapshot_base().index);
                let Some(after) = matched.0.checked_sub(base.0) else {
                    return true;
                };
                let n = usize::try_from(after).expect("fits");
                let on_disk = self.disk.log.get(..n);
                on_disk.is_some_and(|d| core.entries().get(..n) == Some(d))
            }
            MessageKind::VoteResponse { granted: true } => {
                self.disk.hard.term == msg.term && self.disk.hard.voted_for == Some(to)
            }
            _ => true,
        }
    }
}

pub fn is_final(p: &Payload) -> bool {
    matches!(p, Payload::Command(c) if c.starts_with(FINAL))
}

impl StateMachine for RaftNode {
    type Input = NodeInput<Message>;

    fn apply(&mut self, input: NodeInput<Message>) -> Vec<kbf_types::Effect> {
        let NodeInput {
            now,
            entropy,
            event,
        } = input;
        let effects = match event {
            Event::Start => {
                self.boot(entropy);
                self.out.push(Output::Timer {
                    after: TICK,
                    tag: 0,
                });
                Vec::new()
            }
            Event::Timer { .. } => {
                self.out.push(Output::Timer {
                    after: TICK,
                    tag: 0,
                });
                if self.core.is_none() && now >= self.down_until {
                    self.boot(entropy);
                }
                let effects = self.on_tick(now, entropy);
                self.carry_out(now, entropy, effects);
                self.maybe_compact(entropy);
                return Vec::new();
            }
            Event::Message { from, msg } => match &mut self.core {
                Some(core) => core.receive(server_id(&from), msg, entropy),
                None => Vec::new(), // down: the message is lost
            },
        };
        self.carry_out(now, entropy, effects);
        Vec::new()
    }
}

impl Node for RaftNode {
    type Msg = Message;

    fn take_outputs(&mut self) -> Vec<Output<Message>> {
        std::mem::take(&mut self.out)
    }
}
