//! A kbf-raft `Host` over the store on `FaultFs`: a single voter elects itself,
//! proposes twice, snapshots and compacts, and proposes again, crashed at every
//! filesystem operation; every disk a power cut could leave reopens into a host that
//! holds every entry it acknowledged.

use std::fmt;

use kbf_raft::{
    Config, Entry, Host, LogId, LogIndex, Machine, Membership, Message, Payload, Role, ServerId,
    Snapshot, Transport,
};
use kbf_store::{Fault, FaultFs, Options, Store};

/// Two entries per segment, so the snapshot covers more than one.
const OPTS: Options = Options { segment_bytes: 40 };

const S1: ServerId = ServerId(1);

/// A single voter has no one to send to.
struct Nowhere;

impl Transport for Nowhere {
    fn send(&mut self, to: ServerId, _msg: Message) {
        panic!("a single voter sent to {to}");
    }
}

/// The state machine: the last index applied, and each one-byte command with its
/// index. Its snapshot is the index, then (index, byte) pairs, little endian.
#[derive(Debug, Default)]
struct Applied {
    through: u64,
    commands: Vec<(u64, u8)>,
}

#[derive(Debug)]
struct Undecodable;

impl fmt::Display for Undecodable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("undecodable snapshot")
    }
}

impl std::error::Error for Undecodable {}

impl Machine for Applied {
    type Error = Undecodable;

    fn apply(&mut self, entry: &Entry) {
        assert_eq!(entry.id.index.0, self.through + 1, "applied out of order");
        self.through += 1;
        if let Payload::Command(c) = &entry.payload {
            self.commands.push((self.through, c[0]));
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        let mut out = self.through.to_le_bytes().to_vec();
        for (index, c) in &self.commands {
            out.extend_from_slice(&index.to_le_bytes());
            out.push(*c);
        }
        out
    }

    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), Undecodable> {
        let s = &snapshot.state;
        let (head, rest) = s.split_at_checked(8).ok_or(Undecodable)?;
        if rest.len() % 9 != 0 {
            return Err(Undecodable);
        }
        self.through = u64::from_le_bytes(head.try_into().map_err(|_| Undecodable)?);
        self.commands = rest
            .chunks_exact(9)
            .map(|p| (u64::from_le_bytes(p[..8].try_into().unwrap()), p[8]))
            .collect();
        Ok(())
    }
}

type TestHost = Host<Store<FaultFs>, Nowhere, Applied>;

fn config() -> Config {
    Config {
        id: S1,
        membership: Membership::new([S1], []).unwrap(),
        election_ticks: 10,
        heartbeat_ticks: 3,
        max_entries_per_append: 8,
    }
}

fn open(fs: &FaultFs) -> Result<TestHost, String> {
    let (store, _) = Store::open(fs.clone(), OPTS).map_err(|e| format!("store: {e}"))?;
    Host::open(config(), store, Nowhere, Applied::default(), 0).map_err(|e| format!("host: {e}"))
}

/// Ticks until the host leads; `false` if a tick failed.
fn elect(host: &mut TestHost) -> bool {
    for _ in 0..100 {
        if host.tick(0).is_err() {
            return false;
        }
        if host.core().role() == Role::Leader {
            return true;
        }
    }
    panic!("never elected");
}

/// What the run had acknowledged when it stopped: each command whose proposal
/// returned (it was synced and applied first), with its index, and the snapshot
/// base, once the snapshot call returned.
#[derive(Clone, Debug, Default)]
struct Acked {
    commands: Vec<(u64, u8)>,
    snapshot: Option<LogId>,
}

/// The commands the script proposes, in order. In a clean run they sit at 2, 3 and 4
/// (the leader's blank is 1).
const COMMANDS: [u8; 3] = [b'a', b'b', b'c'];

/// Elects, proposes `a` and `b`, snapshots, proposes `c`; stops at the first error.
fn run(fs: &FaultFs) -> Acked {
    let mut acked = Acked::default();
    let Ok(mut host) = open(fs) else {
        return acked;
    };
    if !elect(&mut host) {
        return acked;
    }
    for (k, c) in COMMANDS.into_iter().enumerate() {
        if k == 2 {
            let Ok(base) = host.snapshot() else {
                return acked;
            };
            acked.snapshot = Some(base);
        }
        let Ok(index) = host.propose(vec![c]) else {
            return acked;
        };
        acked.commands.push((index.0, c));
    }
    acked
}

/// Checks the host a disk reopens into, then extends it by one command and checks
/// that a power cut after that keeps everything.
///
/// The checks are Figure 3 of the Raft paper as one voter sees it. Log Matching: the
/// core restores the stored log (numbered on from the base, terms never falling).
/// Leader Completeness: every acknowledged command is in the snapshot the machine was
/// restored from or in the log, at its index, and an acknowledged snapshot is never
/// taken back. State Machine Safety: once re-elected, the machine applies every index
/// once and in order, and any index it holds a script command at holds the one the
/// script proposed there.
fn check(disk: &FaultFs, acked: &Acked, why: &str) {
    let mut host = open(disk).unwrap_or_else(|e| panic!("{why}: reopen refused: {e}"));
    let base = host.core().snapshot_base();
    if let Some(acked_base) = acked.snapshot {
        assert!(
            base >= acked_base,
            "{why}: snapshot {acked_base:?} taken back"
        );
    }
    assert_eq!(
        host.machine().through,
        base.index.0,
        "{why}: restored machine"
    );
    for &(index, c) in &acked.commands {
        let held = if index <= base.index.0 {
            host.machine().commands.contains(&(index, c))
        } else {
            host.core()
                .entries()
                .iter()
                .any(|e| e.id.index.0 == index && e.payload == Payload::Command(vec![c]))
        };
        assert!(held, "{why}: acknowledged {index} lost; base {base:?}");
    }
    assert!(elect(&mut host), "{why}: re-election failed");
    let applied = host.machine();
    assert_eq!(applied.through, host.core().last_log_id().index.0, "{why}");
    for &(index, c) in &applied.commands {
        let proposed = index
            .checked_sub(2)
            .and_then(|k| usize::try_from(k).ok())
            .and_then(|k| COMMANDS.get(k));
        assert_eq!(proposed, Some(&c), "{why}: {c} applied at {index}");
    }
    let next = host
        .propose(vec![b'd'])
        .unwrap_or_else(|e| panic!("{why}: propose after recovery: {e}"));
    drop(host);
    let after = open(&disk.crash(0)).unwrap_or_else(|e| panic!("{why}: reopen after extend: {e}"));
    let has = |index: LogIndex| {
        index.0 <= after.core().snapshot_base().index.0
            || after.core().entries().iter().any(|e| e.id.index == index)
    };
    assert!(has(next), "{why}: the extension was lost");
}

/// Catches (each a mutant that turns this red): a snapshot write that removes the
/// segments it covers before its rename is durable (a power cut that keeps the
/// removals but not the rename loses acknowledged entries); one that skips the
/// directory sync after the rename (the same cut, once the call returned); a store
/// whose load hides the entry after the base or returns the base's own entry (the
/// core refuses the log, or an acknowledged entry is gone); and a store that cannot
/// append after a compaction removed its last segment.
///
/// The script is crashed at every operation. Each crash is materialized as a process
/// death (everything written stays visible) and as a power cut with every subset of
/// the name changes since the last directory sync and every count of torn bytes kept.
#[test]
fn a_host_over_the_store_survives_a_crash_at_every_operation() {
    let total = {
        let fs = FaultFs::new();
        let acked = run(&fs);
        assert_eq!(acked.commands, [(2, b'a'), (3, b'b'), (4, b'c')]);
        assert_eq!(acked.snapshot.map(|b| b.index.0), Some(3));
        let (_, stored) = Store::open(fs.crash(0), OPTS).unwrap();
        assert_eq!(stored.base().index.0, 3);
        let after: Vec<u64> = stored.entries.iter().map(|e| e.id.index.0).collect();
        assert_eq!(after, [4], "the log after the snapshot");
        fs.ops()
    };
    let mut cases = 0;
    for n in 0..total {
        let fs = FaultFs::new();
        fs.fail_at(n, Fault::Crash);
        let acked = run(&fs);
        check(&fs.restart(), &acked, &format!("process death at op {n}"));
        for mask in 0..1u64 << fs.pending_names() {
            for keep in 0..=fs.unsynced() {
                let why = format!("power cut at op {n}, names {mask:b}, {keep} torn bytes kept");
                check(&fs.crash_reordered(keep, mask), &acked, &why);
                cases += 1;
            }
        }
    }
    assert!(cases > 500, "only {cases} cases");
}
