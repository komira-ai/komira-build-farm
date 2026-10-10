//! A Raft server as a simulated node: a [`Host`] over a disk that survives crashes
//! ([`MemStorage`]), the simulated network and a state machine that does not survive
//! them (except through snapshots), a client that proposes commands, log compaction,
//! and crashes at effect boundaries.
//!
//! Every operation the host performs on its parts (a write, a sync, a snapshot write,
//! a send, an apply) is one *boundary*. A crash strikes just before a chosen boundary:
//! the operations before it happened (a message may already be on the wire, writes may
//! be unsynced), that one and the rest did not, and the disk keeps what was synced plus
//! a seeded prefix of the unsynced writes. The node restarts later from its disk alone.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use kbf_raft::{
    AppendOutcome, Config, Entry, HardState, Host, LogId, LogIndex, Machine, MemStorage,
    Membership, Message, MessageKind, Payload, Raft, Role, ServerId, Snapshot, Storage, Stored,
    Term, Transport,
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
    /// Chance, in parts per million per input, that the node crashes at one of the
    /// next few boundaries of that input.
    pub crash_per_million: u64,
    /// One crash, of this server just before its boundary number `k` (counted from 0
    /// over the whole run), whatever `crash_per_million` says.
    pub crash_at: Option<(ServerId, u64)>,
    /// No crashes, partitions or ordinary proposals from here on.
    pub heal: FarmTime,
    /// Ordinary proposals per leader term, at most.
    pub max_proposals: u64,
    /// Chance, in parts per million per tick, that the node snapshots and compacts.
    pub compact_per_million: u64,
    /// The highest index every node's disk holds a committed entry at, set by the
    /// harness after each step (see `Checker::compaction_floor`). Nodes compact no
    /// further, so a leader never folds away an entry a peer still needs: sending
    /// snapshots is not implemented, and such a peer could never catch up.
    pub floor: Rc<std::cell::Cell<LogIndex>>,
}

/// Something the checker needs to see, in the order it happened on this node.
#[derive(Clone, Debug)]
pub enum Observed {
    /// `entry` was written to the disk, after an entry of term `prev_term`.
    Persisted { prev_term: Term, entry: Entry },
    /// A message promising durable state left before that state was synced: an
    /// accepted append whose entries are not all durable, or a granted vote that is
    /// not.
    UndurablePromise { msg: Message },
    /// `entry` was applied while the node was in `term`; `in_order` is false if it did
    /// not directly follow the previously applied index of this incarnation.
    Applied {
        term: Term,
        entry: Entry,
        in_order: bool,
    },
    /// The host compacted through `base`, but this node's state machine does not hold
    /// that entry: `applied` is its last applied index, and `held` the id of the entry
    /// it applied at `base.index`, if any.
    BadSnapshot {
        base: LogId,
        applied: LogIndex,
        held: Option<LogId>,
    },
}

/// What a run exercised, so a sweep can show it covered compaction and restores.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Compactions that moved the snapshot base.
    pub compactions: u64,
    /// Restarts from a snapshot with a base past 0.
    pub restores_from_base: u64,
    /// Crashes.
    pub crashes: u64,
    /// Crashes that left unsynced writes, some of them lost.
    pub crashes_losing_writes: u64,
}

/// Where the next crash strikes, counted in boundaries.
#[derive(Debug, Default)]
struct Fuse {
    passed: u64,
    at: Option<u64>,
    blown: bool,
}

impl Fuse {
    /// Whether the operation at this boundary happens. Once the fuse blows, none does.
    fn pass(&mut self) -> bool {
        if self.blown {
            return false;
        }
        if self.at == Some(self.passed) {
            self.blown = true;
            self.at = None;
            return false;
        }
        self.passed += 1;
        true
    }
}

/// What the host's three parts share with the node.
#[derive(Debug, Default)]
struct Shared {
    /// What a crash keeps (the synced part and some of the rest).
    disk: MemStorage,
    /// The state machine of the current incarnation: applied entries, in order.
    applied: Vec<Entry>,
    history: Vec<Observed>,
    out: Vec<Output<Message>>,
    fuse: Fuse,
}

impl Shared {
    /// Whether what `msg` promises `to` is durable on this node's disk.
    fn is_durable(&self, to: ServerId, msg: &Message) -> bool {
        let (written, durable) = (self.disk.written(), self.disk.durable());
        match msg.kind {
            MessageKind::AppendResponse {
                outcome: AppendOutcome::Accepted { matched },
            } => {
                // Through the base, the entries are in the snapshot, durable at once.
                let base = durable.base().index;
                let Some(after) = matched.0.checked_sub(base.0) else {
                    return true;
                };
                let n = usize::try_from(after).expect("fits");
                durable
                    .entries
                    .get(..n)
                    .is_some_and(|d| written.entries.get(..n) == Some(d))
            }
            MessageKind::VoteResponse { granted: true } => {
                durable.hard.term == msg.term && durable.hard.voted_for == Some(to)
            }
            _ => true,
        }
    }
}

struct SimStorage(Rc<RefCell<Shared>>);
struct SimTransport(Rc<RefCell<Shared>>);
struct Recorder(Rc<RefCell<Shared>>);

impl Storage for SimStorage {
    type Error = Infallible;

    fn load(&mut self) -> Result<Stored, Infallible> {
        self.0.borrow_mut().disk.load()
    }

    fn write_hard_state(&mut self, hard: HardState) -> Result<(), Infallible> {
        let mut s = self.0.borrow_mut();
        if s.fuse.pass() {
            s.disk.write_hard_state(hard)?;
        }
        Ok(())
    }

    fn write_entries(&mut self, entries: &[Entry]) -> Result<(), Infallible> {
        let mut s = self.0.borrow_mut();
        if !s.fuse.pass() {
            return Ok(());
        }
        let written = s.disk.written();
        let first = entries.first().expect("never empty").id.index;
        let mut prev_term = written
            .entries
            .iter()
            .find(|e| e.id.index == first.prev())
            .map_or(written.base().term, |e| e.id.term);
        for entry in entries {
            let entry = entry.clone();
            let next = entry.id.term;
            s.history.push(Observed::Persisted { prev_term, entry });
            prev_term = next;
        }
        s.disk.write_entries(entries)
    }

    fn sync(&mut self) -> Result<(), Infallible> {
        let mut s = self.0.borrow_mut();
        if s.fuse.pass() {
            s.disk.sync()?;
        }
        Ok(())
    }

    fn write_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), Infallible> {
        let mut s = self.0.borrow_mut();
        if s.fuse.pass() {
            s.disk.write_snapshot(snapshot)?;
        }
        Ok(())
    }
}

impl Transport for SimTransport {
    fn send(&mut self, to: ServerId, msg: Message) {
        let mut s = self.0.borrow_mut();
        if !s.fuse.pass() {
            return;
        }
        if !s.is_durable(to, &msg) {
            let msg = msg.clone();
            s.history.push(Observed::UndurablePromise { msg });
        }
        s.out.push(Output::Send {
            to: node_id(to),
            msg,
        });
    }
}

/// A snapshot that does not decode.
#[derive(Debug)]
pub struct Undecodable;

impl std::fmt::Display for Undecodable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("undecodable snapshot")
    }
}

impl std::error::Error for Undecodable {}

impl Machine for Recorder {
    type Error = Undecodable;

    fn apply(&mut self, entry: &Entry) {
        let mut s = self.0.borrow_mut();
        if !s.fuse.pass() {
            return;
        }
        let expected = LogIndex(s.applied.len() as u64 + 1);
        let term = s.disk.written().hard.term;
        s.history.push(Observed::Applied {
            term,
            entry: entry.clone(),
            in_order: entry.id.index == expected,
        });
        s.applied.push(entry.clone());
    }

    fn snapshot(&self) -> Vec<u8> {
        encode(&self.0.borrow().applied)
    }

    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), Undecodable> {
        self.0.borrow_mut().applied = decode(&snapshot.state).ok_or(Undecodable)?;
        Ok(())
    }
}

/// The state machine's bytes: each entry as term, index (u64 LE), then a payload tag
/// (0 blank, 1 command) and for a command its length (u64 LE) and bytes.
fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        out.extend(e.id.term.0.to_le_bytes());
        out.extend(e.id.index.0.to_le_bytes());
        match &e.payload {
            Payload::Blank => out.push(0),
            Payload::Command(c) => {
                out.push(1);
                out.extend((c.len() as u64).to_le_bytes());
                out.extend(c);
            }
        }
    }
    out
}

fn decode(mut b: &[u8]) -> Option<Vec<Entry>> {
    fn u64_at(b: &mut &[u8]) -> Option<u64> {
        let (head, rest) = b.split_at_checked(8)?;
        *b = rest;
        Some(u64::from_le_bytes(head.try_into().ok()?))
    }
    let mut entries = Vec::new();
    while !b.is_empty() {
        let term = Term(u64_at(&mut b)?);
        let index = LogIndex(u64_at(&mut b)?);
        let (&tag, rest) = b.split_first()?;
        b = rest;
        let payload = match tag {
            0 => Payload::Blank,
            1 => {
                let n = usize::try_from(u64_at(&mut b)?).ok()?;
                let (c, rest) = b.split_at_checked(n)?;
                b = rest;
                Payload::Command(c.to_vec())
            }
            _ => return None,
        };
        let id = LogId::new(term, index);
        entries.push(Entry { id, payload });
    }
    Some(entries)
}

type SimHost = Host<SimStorage, SimTransport, Recorder>;

pub struct RaftNode {
    id: ServerId,
    plan: Plan,
    host: Option<SimHost>,
    down_until: FarmTime,
    shared: Rc<RefCell<Shared>>,
    /// Ordinary proposals made as leader in `proposed_in`.
    proposed: u64,
    proposed_in: Term,
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
        let shared = Shared::default();
        let shared = Rc::new(RefCell::new(shared));
        if let Some((victim, k)) = plan.crash_at
            && victim == id
        {
            shared.borrow_mut().fuse.at = Some(k);
        }
        Self {
            id,
            plan,
            host: None,
            down_until: FarmTime::default(),
            shared,
            proposed: 0,
            proposed_in: Term(0),
            coverage: Coverage::default(),
        }
    }

    pub fn id(&self) -> ServerId {
        self.id
    }

    /// The core, while the node is up.
    pub fn core(&self) -> Option<&Raft> {
        self.host.as_ref().map(Host::core)
    }

    /// The entries the current incarnation's state machine holds, in order.
    pub fn applied(&self) -> Vec<Entry> {
        self.shared.borrow().applied.clone()
    }

    /// Whether the current incarnation has applied a command proposed after the heal.
    pub fn applied_final(&self) -> bool {
        self.shared
            .borrow()
            .applied
            .iter()
            .any(|e| is_final(&e.payload))
    }

    /// Calls `f` on everything this node has done so far, in order.
    pub fn with_history<R>(&self, f: impl FnOnce(&[Observed]) -> R) -> R {
        f(&self.shared.borrow().history)
    }

    pub fn coverage(&self) -> Coverage {
        self.coverage
    }

    /// The boundaries this node has passed so far.
    pub fn boundaries(&self) -> u64 {
        self.shared.borrow().fuse.passed
    }

    /// The entry at `index` in the current incarnation's log, the part folded into
    /// the snapshot included (which the state machine holds).
    pub fn log_entry(&self, index: LogIndex) -> Option<Entry> {
        let core = self.host.as_ref()?.core();
        let base = core.snapshot_base().index;
        if index <= base {
            let i = usize::try_from(index.0.checked_sub(1)?).ok()?;
            self.shared.borrow().applied.get(i).cloned()
        } else {
            let i = usize::try_from(index.0 - base.0 - 1).ok()?;
            core.entries().get(i).cloned()
        }
    }

    /// The highest index this node's disk durably holds an entry at for which
    /// `committed` holds, or its snapshot base if none. By log matching, everything
    /// before it is committed too.
    pub fn durable_committed(&self, committed: impl Fn(&Entry) -> bool) -> LogIndex {
        let s = self.shared.borrow();
        let durable = s.disk.durable();
        durable
            .entries
            .iter()
            .rev()
            .find(|e| committed(e))
            .map_or(durable.base().index, |e| e.id.index)
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
        self.shared.borrow_mut().applied.clear();
        let parts = || Rc::clone(&self.shared);
        let host = Host::open(
            self.config(),
            SimStorage(parts()),
            SimTransport(parts()),
            Recorder(parts()),
            entropy,
        )
        .expect("the disk holds what the host persisted");
        if host.core().snapshot_base().index > LogIndex(0) {
            self.coverage.restores_from_base += 1;
        }
        self.host = Some(host);
    }

    /// Now and then, snapshots the state machine and compacts the log through the
    /// applied index, if that is not past the floor. The snapshot must hold exactly
    /// the entries through the new base.
    fn maybe_compact(&mut self, entropy: u64) {
        let roll = mix(entropy ^ 0x636f_6d70);
        if self.shared.borrow().fuse.blown {
            return; // the node is crashing: nothing of this input after the boundary ran
        }
        let Some(host) = &mut self.host else {
            return;
        };
        if roll % 1_000_000 >= self.plan.compact_per_million
            || host.core().applied_index() > self.plan.floor.get()
        {
            return;
        }
        let before = host.core().snapshot_base();
        let id = host.snapshot().expect("simulated storage never fails");
        let mut s = self.shared.borrow_mut();
        let n = usize::try_from(id.index.0).expect("fits");
        let held = n
            .checked_sub(1)
            .and_then(|i| s.applied.get(i))
            .map(|e| e.id);
        if held.unwrap_or_default() != id || n != s.applied.len() {
            let applied = LogIndex(s.applied.len() as u64);
            s.history.push(Observed::BadSnapshot {
                base: id,
                applied,
                held,
            });
            return;
        }
        if id != before {
            self.coverage.compactions += 1;
        }
    }

    fn on_tick(&mut self, now: FarmTime, entropy: u64) {
        let roll = mix(entropy ^ 0x7469_636b);
        let Some(host) = &mut self.host else {
            return;
        };
        host.tick(entropy).expect("simulated storage never fails");
        let core = host.core();
        if core.role() != Role::Leader {
            return;
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
            // After the heal, one final command, until this leader's log holds one
            // (in its entries, or folded into its snapshot).
            let s = self.shared.borrow();
            let base = usize::try_from(core.snapshot_base().index.0).expect("fits");
            let has_final = s.applied.iter().take(base).any(|e| is_final(&e.payload))
                || core.entries().iter().any(|e| is_final(&e.payload));
            (!has_final).then(|| [FINAL, self.id.to_string().as_bytes()].concat())
        };
        if let Some(command) = command {
            host.propose(command).expect("checked: this node leads");
            self.proposed += 1;
        }
    }

    /// Runs one input through the host, arming a seeded crash at one of its first
    /// boundaries now and then, and crashes the node if a crash struck.
    fn run_input(&mut self, now: FarmTime, entropy: u64, input: impl FnOnce(&mut Self)) {
        let roll = mix(entropy);
        let armed = now < self.plan.heal && roll % 1_000_000 < self.plan.crash_per_million;
        if armed {
            let mut s = self.shared.borrow_mut();
            let at = s.fuse.passed + (roll >> 20) % 8;
            s.fuse.at = Some(at);
        }
        input(self);
        let mut s = self.shared.borrow_mut();
        if !s.fuse.blown {
            if armed {
                s.fuse.at = None;
            }
            return;
        }
        s.fuse.blown = false;
        let pending = s.disk.pending() as u64;
        let keep = usize::try_from((roll >> 8) % (pending + 1)).expect("fits");
        self.coverage.crashes += 1;
        self.coverage.crashes_losing_writes += u64::from(keep < s.disk.pending());
        s.disk.crash(keep);
        drop(s);
        self.host = None;
        self.down_until = now.saturating_add(Duration::from_millis((roll >> 44) % 400));
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
        match event {
            Event::Start => {
                self.boot(entropy);
                let tick = Output::Timer {
                    after: TICK,
                    tag: 0,
                };
                self.shared.borrow_mut().out.push(tick);
            }
            Event::Timer { .. } => {
                let tick = Output::Timer {
                    after: TICK,
                    tag: 0,
                };
                self.shared.borrow_mut().out.push(tick);
                if self.host.is_none() && now >= self.down_until {
                    self.boot(entropy);
                }
                self.run_input(now, entropy, |n| {
                    n.on_tick(now, entropy);
                    n.maybe_compact(entropy);
                });
            }
            Event::Message { from, msg } => {
                self.run_input(now, entropy, |n| {
                    if let Some(host) = &mut n.host {
                        host.receive(server_id(&from), msg, entropy)
                            .expect("simulated storage never fails");
                    } // down: the message is lost
                });
            }
        }
        Vec::new()
    }
}

impl Node for RaftNode {
    type Msg = Message;

    fn take_outputs(&mut self) -> Vec<Output<Message>> {
        std::mem::take(&mut self.shared.borrow_mut().out)
    }
}
