//! Helpers shared by the kbf-meta tests.

#![allow(dead_code)]

use std::time::Duration;

use kbf_meta::{
    ActionRecord, Applied, BlobWrite, Closure, Command, Epoch, Location, MetaState, ObjectId,
    Retention, Role, StoreId, Touch, UnreachableReason,
};
use kbf_types::{Digest, DigestFunction, FarmTime};

pub const HOUR: Duration = Duration::from_secs(60 * 60);
pub const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// A distinct digest per `n`, of size `n`.
pub fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// The epoch [`meta`] allocates, which every test object is written under.
pub const EPOCH: Epoch = Epoch::new(1);

/// Object `seq` of [`EPOCH`].
pub fn object(seq: u64) -> ObjectId {
    ObjectId::new(EPOCH, seq)
}

/// A record at offset `n * 100` in segment `segment` (of [`EPOCH`], in the configured
/// store).
pub fn in_segment(segment: u64, n: u8) -> Location {
    Location {
        store: StoreId::CONFIGURED,
        object: object(segment),
        offset: u64::from(n) * 100,
    }
}

/// Farm time `d` after the epoch.
pub fn at(d: Duration) -> FarmTime {
    FarmTime::from_millis(0).saturating_add(d)
}

/// A state with the RFC retention (7 days, a 1-day touch quantum, 30-day actions) and
/// [`EPOCH`] allocated.
pub fn meta() -> MetaState {
    let mut m = MetaState::new(Retention::default());
    assert_eq!(m.execute(Command::AllocEpoch), Applied::Epoch(EPOCH));
    m
}

/// Marks segment `segment` unreachable for `reason`.
pub fn mark(meta: &mut MetaState, segment: u64, reason: UnreachableReason) {
    let applied = meta.execute(Command::ObjectUnreachable {
        object: object(segment),
        reason,
    });
    assert_eq!(applied, Applied::Marked(Ok(())));
}

/// Marks segment `segment` missing, as a read that got a 404 does.
pub fn mark_missing(meta: &mut MetaState, segment: u64) {
    mark(meta, segment, UnreachableReason::Missing);
}

/// Marks segment `segment` reachable again.
pub fn mark_reachable(meta: &mut MetaState, segment: u64) {
    let applied = meta.execute(Command::ObjectReachable(object(segment)));
    assert_eq!(applied, Applied::Marked(Ok(())));
}

pub fn tick(meta: &mut MetaState, d: Duration) {
    meta.execute(Command::Tick(at(d)));
}

pub fn put(meta: &mut MetaState, digest: Digest, location: Location) -> BlobWrite {
    match meta.execute(Command::PutBlob { digest, location }) {
        Applied::Blob(Ok(write)) => write,
        other => panic!("PutBlob applied as {other:?}"),
    }
}

/// Commits `touch` and returns what was lost.
pub fn commit_touch(meta: &mut MetaState, touch: Touch) -> Touch {
    match meta.execute(Command::Touch(touch)) {
        Applied::Touched { lost } => lost,
        other => panic!("Touch applied as {other:?}"),
    }
}

pub fn put_action(
    meta: &mut MetaState,
    role: Role,
    action: Digest,
    record: ActionRecord,
) -> Result<(), kbf_meta::ActionWriteError> {
    match meta.execute(Command::PutAction {
        role,
        action,
        record,
    }) {
        Applied::Action(result) => result,
        other => panic!("PutAction applied as {other:?}"),
    }
}

pub fn collect(meta: &mut MetaState) -> kbf_meta::Collected {
    match meta.execute(Command::Collect) {
        Applied::Collected(c) => c,
        other => panic!("Collect applied as {other:?}"),
    }
}

/// The blobs of one cached action.
pub struct Cached {
    pub action: Digest,
    pub result: Digest,
    pub output: Digest,
    pub tree: Digest,
    pub child: Digest,
}

impl Cached {
    pub fn all(&self) -> [Digest; 4] {
        [self.result, self.output, self.tree, self.child]
    }

    pub fn record(&self) -> ActionRecord {
        ActionRecord {
            result: self.result,
            closure: Closure::from_iter([self.output, self.tree, self.child]),
        }
    }
}

/// Stores an action's result, one output file, one output tree and the tree's one
/// child, each in its own segment (ids 1 to 4), and writes the entry as a daemon.
pub fn cache_action(meta: &mut MetaState) -> Cached {
    let c = Cached {
        action: digest(10),
        result: digest(11),
        output: digest(12),
        tree: digest(13),
        child: digest(14),
    };
    for (i, d) in c.all().into_iter().enumerate() {
        put(meta, d, in_segment(i as u64 + 1, 0));
    }
    put_action(meta, Role::Daemon, c.action, c.record()).expect("daemon write accepted");
    c
}
