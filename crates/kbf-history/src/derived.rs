//! The writer's catalog and status rows: what it keeps in memory between seeing a run
//! and the row that run changes being on the index.
//!
//! - **Shape rows** (`ui/shape/...`) are written once per key, [`Cond::Absent`]: a row
//!   from a previous life is the same row and the write conflicts harmlessly. A slot is
//!   kept until its row is known to exist, and queued again while it is not.
//! - **Status rows** (`ui/status/...`) change with every run, so each write is a
//!   read-modify-write: the stored row is read, the runs seen since are applied to it,
//!   and the result is written with the version read ([`Cond::Version`], or
//!   [`Cond::Absent`] for a key with no row), together with the by-status entry that
//!   moves with it. The runs stay in the slot until a write carrying them commits, so a
//!   write that was dropped, lost the race to another writer or whose acknowledgement
//!   was lost is made again from a fresh read, and no write is made before the read
//!   succeeded: a restart never resets a status. A run is applied once (the row's ring
//!   knows its ids), so a write that landed unacknowledged is not counted twice.
//!
//! Each map holds at most [`DerivedLimits::keys`] keys. A key past the bound is dropped
//! and counted (`stage="index"`) after the slots whose rows are known to exist have been
//! forgotten to make room: a run of a forgotten key costs one conflicting write.

use std::collections::BTreeMap;

use kbf_state::{Cond, Versioned};
use prost::Message;

use crate::keys::IndexRoot;
use crate::pending::{Also, Row};
use crate::rows::{ShapeRow, StatusKind, StatusRow};
use crate::status::Observed;

/// Bounds of [`Derived`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DerivedLimits {
    /// Shape keys, and test keys, held at most (each).
    pub keys: usize,
    /// Stored status rows read per step at most.
    pub reads_per_step: usize,
    /// Runs one test key holds before its row is written, at most.
    pub waiting: usize,
}

impl Default for DerivedLimits {
    /// 100,000 keys (the design's bound on status rows), 256 reads a step, 64 runs.
    fn default() -> DerivedLimits {
        DerivedLimits {
            keys: 100_000,
            reads_per_step: 256,
            waiting: 64,
        }
    }
}

/// The key a test's rows are named by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestId {
    /// The instance.
    pub instance: String,
    /// The shape key.
    pub shape: u64,
    /// The configuration.
    pub cfg: String,
}

#[derive(Debug)]
struct ShapeSlot {
    value: Vec<u8>,
    /// The row is on the index: committed, or found there.
    saved: bool,
}

#[derive(Debug)]
struct TestSlot {
    id: TestId,
    waiting: Vec<Observed>,
    /// A write carrying the first `n` runs is queued or in flight; `None`: the stored
    /// row is to be read, and the runs applied to it.
    queued: Option<usize>,
}

/// The catalog and status rows the writer owes the index.
#[derive(Debug)]
pub struct Derived {
    limits: DerivedLimits,
    shapes: BTreeMap<String, ShapeSlot>,
    /// Shape slots whose row is not known to exist.
    unsaved: usize,
    tests: BTreeMap<String, TestSlot>,
}

impl Derived {
    /// Nothing owed.
    #[must_use]
    pub fn new(limits: DerivedLimits) -> Derived {
        Derived {
            limits,
            shapes: BTreeMap::new(),
            unsaved: 0,
            tests: BTreeMap::new(),
        }
    }

    /// Notes that a run of the shape was seen. Returns false when the key was refused
    /// for want of room.
    pub fn shape(&mut self, root: &IndexRoot, id: &TestId, row: &ShapeRow) -> bool {
        let key = root.shape(&id.instance, id.shape, &id.cfg);
        if self.shapes.contains_key(&key) {
            return true;
        }
        if self.shapes.len() >= self.limits.keys {
            self.shapes.retain(|_, s| !s.saved);
            self.unsaved = self.shapes.len();
            if self.shapes.len() >= self.limits.keys {
                return false;
            }
        }
        self.unsaved += 1;
        self.shapes.insert(
            key,
            ShapeSlot {
                value: row.encode_to_vec(),
                saved: false,
            },
        );
        true
    }

    /// Notes a test run. Returns false when it was refused: no room for its key, or its
    /// key already holds [`DerivedLimits::waiting`] runs.
    pub fn test(&mut self, root: &IndexRoot, id: &TestId, run: Observed) -> bool {
        let key = root.status(&id.instance, id.shape, &id.cfg);
        if let Some(slot) = self.tests.get_mut(&key) {
            if slot.waiting.len() >= self.limits.waiting {
                return false;
            }
            slot.waiting.push(run);
            return true;
        }
        if self.tests.len() >= self.limits.keys {
            return false;
        }
        self.tests.insert(
            key,
            TestSlot {
                id: id.clone(),
                waiting: vec![run],
                queued: None,
            },
        );
        true
    }

    /// The shape rows not known to exist, for the caller to queue where none is pending.
    pub fn unsaved_shapes(&self) -> impl Iterator<Item = (&str, Row)> {
        let some = self.unsaved > 0;
        self.shapes
            .iter()
            .filter(move |(_, s)| some && !s.saved)
            .map(|(key, s)| (key.as_str(), Row::new(s.value.clone(), Cond::Absent, None)))
    }

    /// The status keys whose stored row is to be read, at most
    /// [`DerivedLimits::reads_per_step`].
    #[must_use]
    pub fn to_read(&self) -> Vec<String> {
        self.tests
            .iter()
            .filter(|(_, s)| s.queued.is_none())
            .map(|(key, _)| key.clone())
            .take(self.limits.reads_per_step)
            .collect()
    }

    /// Takes the stored row of status key `key` (`None`: no row), applies the runs
    /// waiting to it and returns the write: the new row with the by-status entry that
    /// moves with it. `None` when there is nothing to write (the stored row already
    /// holds the runs).
    pub fn read(&mut self, root: &IndexRoot, key: &str, stored: Option<Versioned>) -> Option<Row> {
        let write = self
            .tests
            .get_mut(key)
            .and_then(|slot| plan(slot, root, stored));
        if write.is_none() {
            self.tests.remove(key);
        }
        write
    }

    /// The row of `key` committed.
    pub fn committed(&mut self, key: &str) {
        self.saved(key);
        let Some(slot) = self.tests.get_mut(key) else {
            return;
        };
        let taken = slot.queued.take().unwrap_or(0);
        slot.waiting.drain(..taken.min(slot.waiting.len()));
        if slot.waiting.is_empty() {
            self.tests.remove(key);
        }
    }

    /// The write of `key` failed its precondition: a shape row exists already; a status
    /// row changed under the reader, so it is read again.
    pub fn conflicted(&mut self, key: &str) {
        self.saved(key);
        if let Some(slot) = self.tests.get_mut(key) {
            slot.queued = None;
        }
    }

    /// Notes that the shape row of `key`, if it is one, exists.
    fn saved(&mut self, key: &str) {
        if let Some(s) = self.shapes.get_mut(key) {
            self.unsaved -= usize::from(!s.saved);
            s.saved = true;
        }
    }

    /// The write of `key` was dropped: a status row is read and written again; a shape
    /// row is queued again from its slot.
    pub fn dropped(&mut self, key: &str) {
        if let Some(slot) = self.tests.get_mut(key) {
            slot.queued = None;
        }
    }

    /// Shape keys and test keys held.
    #[must_use]
    pub fn keys(&self) -> (usize, usize) {
        (self.shapes.len(), self.tests.len())
    }

    /// Rows owed: shape rows not known to exist, and test keys with runs waiting.
    #[must_use]
    pub fn owed(&self) -> usize {
        self.unsaved + self.tests.len()
    }
}

/// The write that applies `slot`'s waiting runs to the `stored` row of its key; `None`
/// when the stored row holds them already. A stored row that does not decode is
/// replaced, as an unreadable header is.
fn plan(slot: &mut TestSlot, root: &IndexRoot, stored: Option<Versioned>) -> Option<Row> {
    let before = stored
        .as_ref()
        .and_then(|v| StatusRow::decode(v.value.as_slice()).ok());
    let mut row = before.clone().unwrap_or_default();
    for run in &slot.waiting {
        row.apply(run);
    }
    let value = row.encode_to_vec();
    if stored.as_ref().is_some_and(|v| v.value == value) {
        return None;
    }
    let cond = stored.map_or(Cond::Absent, |v| Cond::Version(v.version));
    let id = &slot.id;
    let entry = |kind: StatusKind| root.statusx(kind.name(), &id.instance, id.shape, &id.cfg);
    let (was, now) = (before.and_then(|r| r.kind()), row.kind());
    let mut also = Vec::new();
    if was != now {
        also.extend(was.map(|k| Also::Delete(entry(k))));
        also.extend(now.map(|k| Also::Put(entry(k), Vec::new())));
    }
    slot.queued = Some(slot.waiting.len());
    Some(Row {
        value,
        cond,
        origin: None,
        also,
    })
}
