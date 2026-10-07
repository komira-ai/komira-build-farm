//! The metadata state machine: commands in, outcomes out, queries on the side.

use std::collections::{BTreeMap, BTreeSet};
use std::iter;
use std::time::Duration;

use kbf_types::{Digest, Effect, FarmTime, StateMachine};

use crate::model::{
    ActionAnswer, ActionRecord, BlobAnswer, FindMissing, Location, Miss, ObjectId, Retention, Role,
    Touch,
};

/// One committed change to the metadata state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Advances farm time. Time never moves back: an earlier value is ignored.
    Tick(FarmTime),
    /// Records that `digest`'s bytes are durable at `location`. Counts as a touch.
    PutBlob {
        /// The blob.
        digest: Digest,
        /// Where its bytes are.
        location: Location,
    },
    /// Writes the action-cache entry for `action`, replacing any earlier one.
    PutAction {
        /// Who asks; only [`Role::Daemon`] is accepted.
        role: Role,
        /// The action digest (the key).
        action: Digest,
        /// The result and its closure.
        record: ActionRecord,
    },
    /// Touches entries a reader is about to report or serve.
    Touch(Touch),
    /// The store could not reach this object (a 404, or a store down past the hold).
    ObjectUnreachable(ObjectId),
    /// The object is reachable again.
    ObjectReachable(ObjectId),
    /// Removes every blob and action entry whose retention has run out.
    Collect,
}

/// What applying a [`Command`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// [`Command::Tick`]: the farm time after the tick.
    Ticked(FarmTime),
    /// [`Command::PutBlob`].
    Blob(BlobWrite),
    /// [`Command::PutAction`].
    Action(Result<(), ActionWriteError>),
    /// [`Command::Touch`]: the entries that were not held when the touch applied (a
    /// [`Command::Collect`] committed between the read and its touch). A reader that
    /// sees any must not answer from its earlier read; it asks again.
    Touched {
        /// Entries not touched because they are gone.
        lost: Touch,
    },
    /// [`Command::ObjectUnreachable`] or [`Command::ObjectReachable`].
    Marked,
    /// [`Command::Collect`].
    Collected(Collected),
}

/// What a [`Command::PutBlob`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobWrite {
    /// A new entry.
    Stored,
    /// The blob was already held at a reachable location, which is kept; the new copy
    /// is redundant and its space can be reclaimed. The entry was touched.
    Duplicate {
        /// The location kept.
        kept: Location,
    },
    /// The blob was held at an unreachable object; the entry now points at the new copy.
    Healed {
        /// The unreachable location replaced.
        replaced: Location,
    },
}

/// What a [`Command::Collect`] removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    /// Blobs removed from the index, with where their bytes were, so the store can
    /// account the space in each object as dead.
    pub blobs: Vec<(Digest, Location)>,
    /// Action-cache entries removed.
    pub actions: Vec<Digest>,
}

/// Why an action-cache write was refused. A refused write changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActionWriteError {
    /// Only daemons write the action cache.
    #[error("action-cache writes are accepted only from a daemon")]
    NotDaemon,
    /// A blob the entry needs is not held: the outputs must be stored first.
    #[error("action-cache entry needs blob {0}, which is not held")]
    Absent(Digest),
    /// A blob the entry needs is held at an unreachable object.
    #[error("action-cache entry needs blob {0}, whose object is unreachable")]
    Unreachable(Digest),
}

/// A blob an action-cache entry needs that the closure check did not find usable.
enum Lack {
    Absent(Digest),
    Unreachable(Digest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BlobEntry {
    location: Location,
    last_touch: FarmTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActionEntry {
    record: ActionRecord,
    last_hit: FarmTime,
}

/// The metadata state: the CAS index, the action cache, unreachable objects and farm
/// time.
///
/// Every replica applies the same committed [`Command`]s and ends in the same state.
/// Reads are `&self` queries on the leader. A read that reports or serves something
/// returns a [`Touch`]; when it is not empty the leader commits it as
/// [`Command::Touch`] and answers only after it applies with nothing lost. Together
/// with [`Retention`] that keeps the promise: nothing reported present or served is
/// collected before `min_ttl` of farm time has passed since the report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaState {
    retention: Retention,
    now: FarmTime,
    blobs: BTreeMap<Digest, BlobEntry>,
    actions: BTreeMap<Digest, ActionEntry>,
    unreachable: BTreeSet<ObjectId>,
}

impl MetaState {
    /// An empty state at farm time zero.
    #[must_use]
    pub fn new(retention: Retention) -> Self {
        Self {
            retention,
            now: FarmTime::default(),
            blobs: BTreeMap::new(),
            actions: BTreeMap::new(),
            unreachable: BTreeSet::new(),
        }
    }

    /// The retention this state was built with.
    #[must_use]
    pub const fn retention(&self) -> Retention {
        self.retention
    }

    /// The latest committed farm time.
    #[must_use]
    pub const fn now(&self) -> FarmTime {
        self.now
    }

    /// The number of blobs in the CAS index.
    #[must_use]
    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    /// The number of action-cache entries, expired ones included until collected.
    #[must_use]
    pub fn action_count(&self) -> usize {
        self.actions.len()
    }

    /// Applies one command and reports what it did.
    pub fn execute(&mut self, command: Command) -> Applied {
        match command {
            Command::Tick(t) => {
                self.now = self.now.max(t);
                Applied::Ticked(self.now)
            }
            Command::PutBlob { digest, location } => Applied::Blob(self.put_blob(digest, location)),
            Command::PutAction {
                role,
                action,
                record,
            } => Applied::Action(self.put_action(role, action, record)),
            Command::Touch(touch) => Applied::Touched {
                lost: self.touch(touch),
            },
            Command::ObjectUnreachable(object) => {
                self.unreachable.insert(object);
                Applied::Marked
            }
            Command::ObjectReachable(object) => {
                self.unreachable.remove(&object);
                Applied::Marked
            }
            Command::Collect => Applied::Collected(self.collect()),
        }
    }

    /// The answer for one blob. Touches nothing; see [`MetaState::touch_for`].
    #[must_use]
    pub fn blob(&self, digest: &Digest) -> BlobAnswer {
        match self.blobs.get(digest) {
            None => BlobAnswer::Absent,
            Some(entry) if self.unreachable.contains(&entry.location.object()) => {
                BlobAnswer::Unavailable
            }
            Some(entry) => BlobAnswer::Present(entry.location),
        }
    }

    /// The touch a reader must commit before reporting or serving `digests`: the
    /// present ones whose last touch is at least the touch quantum old.
    #[must_use]
    pub fn touch_for<'a>(&self, digests: impl IntoIterator<Item = &'a Digest>) -> Touch {
        let mut touch = Touch::default();
        for digest in digests {
            if let Some(entry) = self.blobs.get(digest)
                && !self.unreachable.contains(&entry.location.object())
                && self.touch_due(entry.last_touch)
            {
                touch.blobs.insert(*digest);
            }
        }
        touch
    }

    /// Answers `FindMissingBlobs`: every requested digest is either reported missing
    /// or present-and-touched; none is omitted.
    #[must_use]
    pub fn find_missing(&self, digests: &[Digest]) -> FindMissing {
        let missing = digests
            .iter()
            .filter(|d| !matches!(self.blob(d), BlobAnswer::Present(_)))
            .copied()
            .collect();
        FindMissing {
            missing,
            touch: self.touch_for(digests),
        }
    }

    /// Answers `GetActionResult`. A hit needs an unexpired entry whose result blob and
    /// every closure blob are present and reachable; anything else is a miss.
    #[must_use]
    pub fn action(&self, action: &Digest) -> ActionAnswer {
        let Some(entry) = self.actions.get(action) else {
            return ActionAnswer::Miss(Miss::NoEntry);
        };
        if self.expired(entry.last_hit, self.retention.action_ttl) {
            return ActionAnswer::Miss(Miss::Expired);
        }
        let needed = || iter::once(&entry.record.result).chain(entry.record.closure.iter());
        if let Err(lack) = self.closure_check(needed()) {
            return ActionAnswer::Miss(match lack {
                Lack::Absent(d) => Miss::Absent(d),
                Lack::Unreachable(d) => Miss::Unreachable(d),
            });
        }
        let mut touch = self.touch_for(needed());
        if self.touch_due(entry.last_hit) {
            touch.actions.insert(*action);
        }
        ActionAnswer::Hit {
            result: entry.record.result,
            touch,
        }
    }

    /// The closure check: the first needed blob that is not present and reachable.
    fn closure_check<'a>(&self, needed: impl Iterator<Item = &'a Digest>) -> Result<(), Lack> {
        for digest in needed {
            match self.blob(digest) {
                BlobAnswer::Present(_) => {}
                BlobAnswer::Unavailable => return Err(Lack::Unreachable(*digest)),
                BlobAnswer::Absent => return Err(Lack::Absent(*digest)),
            }
        }
        Ok(())
    }

    fn put_blob(&mut self, digest: Digest, location: Location) -> BlobWrite {
        let now = self.now;
        let answer = self.blob(&digest);
        let entry = self.blobs.entry(digest).or_insert(BlobEntry {
            location,
            last_touch: now,
        });
        entry.last_touch = now;
        match answer {
            BlobAnswer::Absent => BlobWrite::Stored,
            BlobAnswer::Present(kept) => BlobWrite::Duplicate { kept },
            BlobAnswer::Unavailable => {
                let replaced = entry.location;
                entry.location = location;
                BlobWrite::Healed { replaced }
            }
        }
    }

    fn put_action(
        &mut self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), ActionWriteError> {
        if role != Role::Daemon {
            return Err(ActionWriteError::NotDaemon);
        }
        self.closure_check(iter::once(&record.result).chain(record.closure.iter()))
            .map_err(|lack| match lack {
                Lack::Absent(d) => ActionWriteError::Absent(d),
                Lack::Unreachable(d) => ActionWriteError::Unreachable(d),
            })?;
        self.actions.insert(
            action,
            ActionEntry {
                record,
                last_hit: self.now,
            },
        );
        Ok(())
    }

    fn touch(&mut self, touch: Touch) -> Touch {
        let now = self.now;
        let mut lost = Touch::default();
        for digest in touch.blobs {
            match self.blobs.get_mut(&digest) {
                Some(entry) => entry.last_touch = now,
                None => {
                    lost.blobs.insert(digest);
                }
            }
        }
        for action in touch.actions {
            match self.actions.get_mut(&action) {
                Some(entry) => entry.last_hit = now,
                None => {
                    lost.actions.insert(action);
                }
            }
        }
        lost
    }

    fn collect(&mut self) -> Collected {
        let mut collected = Collected::default();
        let (min_ttl, action_ttl) = (self.retention.min_ttl, self.retention.action_ttl);
        let dead: Vec<Digest> = self
            .blobs
            .iter()
            .filter(|(_, e)| self.expired(e.last_touch, min_ttl))
            .map(|(d, _)| *d)
            .collect();
        for digest in dead {
            if let Some(entry) = self.blobs.remove(&digest) {
                collected.blobs.push((digest, entry.location));
            }
        }
        let dead: Vec<Digest> = self
            .actions
            .iter()
            .filter(|(_, e)| self.expired(e.last_hit, action_ttl))
            .map(|(d, _)| *d)
            .collect();
        for action in dead {
            self.actions.remove(&action);
            collected.actions.push(action);
        }
        collected
    }

    /// Whether a touch at `last` is old enough that a reader must touch again.
    fn touch_due(&self, last: FarmTime) -> bool {
        self.now >= last.saturating_add(self.retention.touch_quantum)
    }

    /// Whether an entry last touched at `last` has outlived `ttl`. Entries are held for
    /// `ttl` plus the touch quantum: a read inside the quantum commits no touch, and the
    /// extra quantum still covers it for the full `ttl`.
    fn expired(&self, last: FarmTime, ttl: Duration) -> bool {
        self.now
            > last
                .saturating_add(ttl)
                .saturating_add(self.retention.touch_quantum)
    }
}

impl StateMachine for MetaState {
    type Input = Command;

    /// Applies the command; its outcome goes to the caller through
    /// [`MetaState::execute`], and the metadata core asks for no effects.
    fn apply(&mut self, input: Command) -> Vec<Effect> {
        self.execute(input);
        Vec::new()
    }
}
