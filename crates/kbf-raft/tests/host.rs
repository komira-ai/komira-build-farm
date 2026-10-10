//! The host loop, input by input: what it does to its storage, transport and state
//! machine, in what order, for the exact inputs that need each rule. The simulation in
//! `tests/sim` crashes it at every boundary; these name the order the crashes rely on.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use kbf_raft::{
    AppendOutcome, Config, Entry, HardState, Host, HostError, LogId, LogIndex, Machine, MemStorage,
    Membership, Message, MessageKind, NotLeader, OpenError, Payload, Role, ServerId, Snapshot,
    Storage, Stored, Term, Transport,
};

const S1: ServerId = ServerId(1);
const S2: ServerId = ServerId(2);
const S3: ServerId = ServerId(3);

/// One operation the host performed on a part.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    WriteHardState(HardState),
    /// The indexes written.
    WriteEntries(Vec<u64>),
    Sync,
    /// The new base's index.
    WriteSnapshot(u64),
    Send(ServerId),
    /// The index applied.
    Apply(u64),
}

type Trace = Rc<RefCell<Vec<Op>>>;

#[derive(Debug, PartialEq, Eq)]
struct Failed;

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("injected failure")
    }
}

impl std::error::Error for Failed {}

/// A [`MemStorage`] that records each operation, and fails the ones `fail` names.
struct Disk {
    mem: MemStorage,
    trace: Trace,
    fail: fn(&Op) -> bool,
    fail_load: bool,
}

impl Disk {
    fn new(trace: &Trace) -> Self {
        Self {
            mem: MemStorage::new(),
            trace: Rc::clone(trace),
            fail: |_| false,
            fail_load: false,
        }
    }

    fn op(&mut self, op: Op) -> Result<(), Failed> {
        let failed = (self.fail)(&op);
        self.trace.borrow_mut().push(op);
        if failed { Err(Failed) } else { Ok(()) }
    }
}

impl Storage for Disk {
    type Error = Failed;

    fn load(&mut self) -> Result<Stored, Failed> {
        if self.fail_load {
            return Err(Failed);
        }
        Ok(self.mem.load().expect("infallible"))
    }

    fn write_hard_state(&mut self, hard: HardState) -> Result<(), Failed> {
        self.op(Op::WriteHardState(hard))?;
        self.mem.write_hard_state(hard).expect("infallible");
        Ok(())
    }

    fn write_entries(&mut self, entries: &[Entry]) -> Result<(), Failed> {
        self.op(Op::WriteEntries(
            entries.iter().map(|e| e.id.index.0).collect(),
        ))?;
        self.mem.write_entries(entries).expect("infallible");
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Failed> {
        self.op(Op::Sync)?;
        self.mem.sync().expect("infallible");
        Ok(())
    }

    fn write_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), Failed> {
        self.op(Op::WriteSnapshot(snapshot.base.index.0))?;
        self.mem.write_snapshot(snapshot).expect("infallible");
        Ok(())
    }
}

/// Records each message sent.
struct Net {
    trace: Trace,
    sent: Vec<(ServerId, Message)>,
}

impl Transport for Net {
    fn send(&mut self, to: ServerId, msg: Message) {
        self.trace.borrow_mut().push(Op::Send(to));
        self.sent.push((to, msg));
    }
}

/// A state machine whose state is the indexes it applied; its snapshot is them, as
/// u64 LE.
struct Indexes {
    trace: Trace,
    applied: Vec<u64>,
}

impl Machine for Indexes {
    type Error = Failed;

    fn apply(&mut self, entry: &Entry) {
        self.trace.borrow_mut().push(Op::Apply(entry.id.index.0));
        self.applied.push(entry.id.index.0);
    }

    fn snapshot(&self) -> Vec<u8> {
        self.applied.iter().flat_map(|i| i.to_le_bytes()).collect()
    }

    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), Failed> {
        let chunks = snapshot.state.chunks_exact(8);
        if !chunks.remainder().is_empty() {
            return Err(Failed);
        }
        self.applied = chunks
            .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
            .collect();
        Ok(())
    }
}

type TestHost = Host<Disk, Net, Indexes>;

fn config(id: ServerId, voters: &[ServerId]) -> Config {
    Config {
        id,
        membership: Membership::new(voters.iter().copied(), []).unwrap(),
        election_ticks: 10,
        heartbeat_ticks: 3,
        max_entries_per_append: 8,
    }
}

fn open(config: Config, disk: Disk) -> TestHost {
    let trace = Rc::clone(&disk.trace);
    let net = Net {
        trace: Rc::clone(&trace),
        sent: Vec::new(),
    };
    let machine = Indexes {
        trace,
        applied: Vec::new(),
    };
    Host::open(config, disk, net, machine, 0).unwrap()
}

/// The operations recorded since the last call.
fn take(trace: &Trace) -> Vec<Op> {
    std::mem::take(&mut trace.borrow_mut())
}

fn hard(term: u64, voted_for: Option<ServerId>) -> HardState {
    HardState {
        term: Term(term),
        voted_for,
    }
}

fn cmd(term: u64, index: u64) -> Entry {
    Entry {
        id: LogId::new(Term(term), LogIndex(index)),
        payload: Payload::Command(vec![1]),
    }
}

fn append(term: u64, prev: LogId, entries: Vec<Entry>, commit: u64) -> Message {
    Message {
        term: Term(term),
        kind: MessageKind::AppendRequest {
            prev,
            entries,
            commit: LogIndex(commit),
        },
    }
}

/// Ticks a single voter until it leads.
fn elect(host: &mut TestHost) {
    for _ in 0..100 {
        host.tick(0).unwrap();
        if host.core().role() == Role::Leader {
            return;
        }
    }
    panic!("never elected");
}

/// Catches: a host that sends a granted vote before syncing it (a crash after the
/// send lets the voter grant a second vote in the term), and one that syncs once per
/// write instead of once per run of writes.
#[test]
fn a_vote_is_synced_before_it_is_sent() {
    let trace = Trace::default();
    let mut host = open(config(S1, &[S1, S2, S3]), Disk::new(&trace));
    let request = Message {
        term: Term(1),
        kind: MessageKind::VoteRequest {
            last_log: LogId::default(),
        },
    };
    host.receive(S2, request, 0).unwrap();
    assert_eq!(
        take(&trace),
        [
            Op::WriteHardState(hard(1, None)),
            Op::WriteHardState(hard(1, Some(S2))),
            Op::Sync,
            Op::Send(S2),
        ]
    );
    assert_eq!(host.storage().mem.durable().hard, hard(1, Some(S2)));
    let (to, msg) = host.transport().sent.last().unwrap();
    assert_eq!(*to, S2);
    assert_eq!(msg.kind, MessageKind::VoteResponse { granted: true });
}

/// Catches: a follower that acknowledges entries before syncing them, one that
/// applies entries when it writes them rather than when the leader says they are
/// committed (a later leader may replace them), and one that applies a batch out of
/// index order.
#[test]
fn a_follower_syncs_before_it_acknowledges_and_applies_only_committed_entries_in_order() {
    let trace = Trace::default();
    let mut host = open(config(S1, &[S1, S2, S3]), Disk::new(&trace));
    let entries = vec![cmd(1, 1), cmd(1, 2), cmd(1, 3)];
    host.receive(S2, append(1, LogId::default(), entries, 0), 0)
        .unwrap();
    assert_eq!(
        take(&trace),
        [
            Op::WriteHardState(hard(1, None)),
            Op::WriteEntries(vec![1, 2, 3]),
            Op::Sync,
            Op::Send(S2),
        ]
    );
    assert!(host.machine().applied.is_empty(), "applied before commit");
    let (_, ack) = host.transport().sent.last().unwrap();
    assert_eq!(
        ack.kind,
        MessageKind::AppendResponse {
            outcome: AppendOutcome::Accepted {
                matched: LogIndex(3)
            }
        }
    );
    let last = LogId::new(Term(1), LogIndex(3));
    host.receive(S2, append(1, last, Vec::new(), 2), 0).unwrap();
    assert_eq!(take(&trace), [Op::Send(S2), Op::Apply(1), Op::Apply(2)]);
    host.receive(S2, append(1, last, Vec::new(), 3), 0).unwrap();
    assert_eq!(take(&trace), [Op::Send(S2), Op::Apply(3)]);
    assert_eq!(host.machine().applied, [1, 2, 3]);
}

/// Catches: a single voter that applies (and so answers a client) before the entry
/// is synced: it commits alone, so a crash after the apply would lose an answered
/// entry. Also: a proposal's index that is not where the entry sits.
#[test]
fn a_single_voter_syncs_before_it_applies() {
    let trace = Trace::default();
    let mut host = open(config(S1, &[S1]), Disk::new(&trace));
    elect(&mut host);
    assert_eq!(
        take(&trace),
        [
            Op::WriteHardState(hard(1, Some(S1))),
            Op::WriteEntries(vec![1]),
            Op::Sync,
            Op::Apply(1),
        ]
    );
    assert_eq!(host.propose(vec![7]), Ok(LogIndex(2)));
    assert_eq!(
        take(&trace),
        [Op::WriteEntries(vec![2]), Op::Sync, Op::Apply(2)]
    );
    assert_eq!(host.storage().mem.pending(), 0);
}

/// Catches: a host that goes on after a storage error (it would acknowledge or apply
/// what it could not make durable), and one that sends what followed the failed
/// operation.
#[test]
fn a_storage_error_stops_the_host() {
    let trace = Trace::default();
    let mut disk = Disk::new(&trace);
    disk.fail = |op| *op == Op::Sync;
    let mut host = open(config(S1, &[S1, S2, S3]), disk);
    let got = host.receive(S2, append(1, LogId::default(), vec![cmd(1, 1)], 1), 0);
    assert_eq!(got, Err(HostError::Storage(Failed)));
    assert!(host.is_stopped());
    assert_eq!(
        take(&trace),
        [
            Op::WriteHardState(hard(1, None)),
            Op::WriteEntries(vec![1]),
            Op::Sync,
        ]
    );
    assert_eq!(host.tick(0), Err(HostError::Stopped));
    assert_eq!(
        host.receive(S2, append(1, LogId::default(), vec![], 0), 0),
        Err(HostError::Stopped)
    );
    assert_eq!(host.propose(vec![1]), Err(HostError::Stopped));
    assert_eq!(host.snapshot(), Err(HostError::Stopped));
    assert!(take(&trace).is_empty(), "a stopped host touched its parts");
    assert!(host.transport().sent.is_empty());
    assert!(host.machine().applied.is_empty());
}

/// Catches: a failed write that the host treats as done, and a failed snapshot write
/// after which it carries on.
#[test]
fn a_failed_write_or_snapshot_write_stops_the_host() {
    for fail in [
        (|op: &Op| matches!(op, Op::WriteHardState(_))) as fn(&Op) -> bool,
        |op| matches!(op, Op::WriteEntries(_)),
    ] {
        let trace = Trace::default();
        let mut disk = Disk::new(&trace);
        disk.fail = fail;
        let mut host = open(config(S1, &[S1]), disk);
        let got = (0..100).find_map(|_| host.tick(0).err());
        assert_eq!(got, Some(HostError::Storage(Failed)));
        assert!(host.is_stopped());
        assert!(!take(&trace).contains(&Op::Sync));
    }
    let trace = Trace::default();
    let mut disk = Disk::new(&trace);
    disk.fail = |op| matches!(op, Op::WriteSnapshot(_));
    let mut host = open(config(S1, &[S1]), disk);
    elect(&mut host);
    assert_eq!(host.snapshot(), Err(HostError::Storage(Failed)));
    assert_eq!(host.tick(0), Err(HostError::Stopped));
}

/// Catches: a snapshot that does not hold the state through its base, a reopen that
/// loses the entries after the base or applies the base's entries again, and a
/// snapshot that rewrites an unchanged base.
#[test]
fn a_host_reopens_from_its_snapshot_and_the_entries_after_it() {
    let trace = Trace::default();
    let mut host = open(config(S1, &[S1]), Disk::new(&trace));
    elect(&mut host);
    host.propose(vec![1]).unwrap();
    host.propose(vec![2]).unwrap();
    take(&trace);
    let base = LogId::new(Term(1), LogIndex(3));
    assert_eq!(host.snapshot(), Ok(base));
    assert_eq!(take(&trace), [Op::WriteSnapshot(3)]);
    assert_eq!(host.snapshot(), Ok(base));
    assert!(
        take(&trace).is_empty(),
        "an unchanged base was written again"
    );
    host.propose(vec![3]).unwrap();
    assert_eq!(host.core().snapshot_base(), base);
    assert_eq!(host.machine().applied, [1, 2, 3, 4]);

    let mut disk = Disk::new(&trace);
    disk.mem = host.storage().mem.clone();
    disk.mem.crash(0);
    let mut reopened = open(config(S1, &[S1]), disk);
    assert_eq!(reopened.core().snapshot_base(), base);
    assert_eq!(
        reopened.machine().applied,
        [1, 2, 3],
        "restored from the snapshot"
    );
    assert_eq!(
        reopened.core().last_log_id(),
        LogId::new(Term(1), LogIndex(4))
    );
    elect(&mut reopened);
    assert_eq!(reopened.machine().applied, [1, 2, 3, 4, 5]);
}

/// Catches: an open that ignores a storage that cannot be read, a snapshot the
/// machine refuses, or a stored log the core refuses.
#[test]
fn open_refuses_what_it_cannot_restore() {
    let trace = Trace::default();
    let parts = || {
        let net = Net {
            trace: Rc::clone(&trace),
            sent: Vec::new(),
        };
        let machine = Indexes {
            trace: Rc::clone(&trace),
            applied: Vec::new(),
        };
        (net, machine)
    };
    let mut unreadable = Disk::new(&trace);
    unreadable.fail_load = true;
    let (net, machine) = parts();
    let got = Host::open(config(S1, &[S1]), unreadable, net, machine, 0);
    assert!(matches!(got, Err(OpenError::Storage(Failed))));

    let mut garbled = Disk::new(&trace);
    let snapshot = Snapshot {
        base: LogId::new(Term(1), LogIndex(1)),
        state: vec![1, 2, 3],
    };
    garbled.mem.write_snapshot(&snapshot).unwrap();
    let (net, machine) = parts();
    let got = Host::open(config(S1, &[S1]), garbled, net, machine, 0);
    assert!(matches!(got, Err(OpenError::Machine(Failed))));

    let mut ahead = Disk::new(&trace);
    ahead.mem.write_entries(&[cmd(2, 1)]).unwrap();
    let (net, machine) = parts();
    let got = Host::open(config(S1, &[S1]), ahead, net, machine, 0);
    assert!(matches!(got, Err(OpenError::Core(_))));
}

/// Catches: a follower that accepts a proposal.
#[test]
fn a_follower_refuses_a_proposal() {
    let trace = Trace::default();
    let mut host = open(config(S1, &[S1, S2, S3]), Disk::new(&trace));
    host.receive(S2, append(1, LogId::default(), vec![], 0), 0)
        .unwrap();
    take(&trace);
    assert_eq!(
        host.propose(vec![1]),
        Err(HostError::NotLeader(NotLeader { leader: Some(S2) }))
    );
    assert!(take(&trace).is_empty());
}
