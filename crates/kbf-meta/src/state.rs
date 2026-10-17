//! The metadata state machine: commands in, outcomes out, queries on the side.

use std::collections::{BTreeMap, BTreeSet};
use std::iter;
use std::num::NonZeroU64;
use std::time::Duration;

use kbf_types::{Digest, Effect, FarmTime, StateMachine};

use crate::model::{
    ActionAnswer, ActionRecord, BlobAnswer, Epoch, FindMissing, Generation, Location, LossMark,
    Miss, NodeWriter, ObjectId, Retention, Role, Touch, UnreachableReason, Writer,
};
use crate::tier::{ServerId, StagedObject, StagedPut, TierError, TierId};

mod evict;
mod lost;
mod rebuild;
mod staged;

pub use rebuild::{AdoptRefused, Adopted, Rebuild};

/// One committed change to the metadata state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Advances farm time. Time never moves back: an earlier value is ignored.
    Tick(FarmTime),
    /// Allocates a writer epoch, greater than every epoch allocated before
    /// ([`Applied::Epoch`]). A writer takes one before it names any object.
    AllocEpoch,
    /// Records that `digest`'s bytes are durable at `location`. Counts as a touch.
    /// Refused unless the location's object is one this log's writers may put
    /// ([`ForeignWriter`]): of an allocated epoch, or of the current node writer.
    PutBlob {
        /// The blob.
        digest: Digest,
        /// Where its bytes are.
        location: Location,
    },
    /// Records several blobs at once, as one [`Command::PutBlob`] each in order (a
    /// digest listed twice is a duplicate of its first copy). All or nothing: if any
    /// location is refused as [`Command::PutBlob`] refuses it, none is recorded.
    PutBlobs(Vec<(Digest, Location)>),
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
    /// A read could not use this object, for `reason`. An object already marked
    /// [`UnreachableReason::Corrupt`] stays corrupt. The mark's [`Generation`] becomes
    /// the log index of this entry, whether or not the reason rose. Refused if the
    /// object's writer is not one this log knows ([`ForeignWriter`]).
    ObjectUnreachable {
        /// The object.
        object: ObjectId,
        /// What the read found.
        reason: UnreachableReason,
    },
    /// The object is reachable again: its mark is cleared, whatever its reason, if the
    /// mark's generation is still `generation`. A mark stamped since (a newer
    /// [`Command::ObjectUnreachable`]) is kept, and so is the state: the command is then
    /// a no-op. Refused if the object's writer is not one this log knows.
    ObjectReachable {
        /// The object.
        object: ObjectId,
        /// The mark's generation the sender read before it probed the store.
        generation: Generation,
    },
    /// Removes every blob and action entry whose retention has run out, and forgets
    /// objects that left the staged table more than the blob retention ago.
    Collect,
    /// Registers `tier` as `server`'s local write tier (write-path.md, section 6.1). A
    /// server has one tier at a time: registering another retires the one before,
    /// which is refused while it holds a staged object. An id registered to another
    /// server, or retired, is refused. Registering a server's current tier again is a
    /// no-op.
    AddTier {
        /// The server.
        server: ServerId,
        /// Its tier.
        tier: TierId,
    },
    /// Records blobs synced to block files on `tier`: each object's blobs are indexed
    /// at `Location { store: CONFIGURED, object, offset }` (as [`Command::PutBlobs`]
    /// does), the object is marked staged on `tier`, and its committed length becomes
    /// the larger of its length and `end`. A digest an object already lists is indexed
    /// at the offset listed first. All or nothing: refused if the tier is not a
    /// server's current tier, an object is staged on another tier, sealed (unless the
    /// object already lists every blob and reaches `end`: a retry), has left the table,
    /// is marked unreachable, is named twice, or a record ends past `end`.
    PutStaged {
        /// The tier the blocks were synced on.
        tier: TierId,
        /// The objects.
        objects: Vec<StagedPut>,
    },
    /// Seals a staged object: after it, a [`Command::PutStaged`] that adds to it is
    /// refused. A no-op on an object already sealed or that has left the table.
    SealStaged {
        /// The object.
        object: ObjectId,
    },
    /// The store holds a sealed object: it leaves the staged table, its blobs stay
    /// where they are. A no-op on an object that has already left.
    ObjectFlushed {
        /// The object.
        object: ObjectId,
    },
    /// The store holds different bytes under a sealed object's key: the object stays
    /// staged and is read from its block, and the flusher stops retrying it.
    FlushConflict {
        /// The object.
        object: ObjectId,
    },
    /// The store no longer holds `objects`, or is about to delete them: forget what
    /// they held. Every blob whose current location is in one of them leaves the
    /// index, then every action-cache entry whose result or closure names a removed
    /// blob. A blob that healed into another object stays. The objects' unreachable
    /// marks are cleared, and [`MetaState::losses`] rises once per object that held a
    /// blob. A staged object is skipped whole: its only copy is its block, not the
    /// store. An object nothing names changes nothing, so a retry is a no-op.
    EvictObjects {
        /// The objects.
        objects: BTreeSet<ObjectId>,
    },
    /// Makes `writer` the current node writer: from now on a put of a node object
    /// ([`Command::PutBlob`], [`Command::PutBlobs`], [`Command::PutStaged`]) must name
    /// it, and every other command may name an object of it or of an earlier boot of a
    /// member set before. Refused if the log has set the same member at this boot or a
    /// later one ([`StaleWriter`]), so no two processes ever put under one writer. An
    /// epoch writer's puts are not affected.
    SetWriter(NodeWriter),
    /// Indexes the records of a segment the store holds that this log did not write
    /// (a rebuild of the index from the segments' footers): each `(digest, offset)` of
    /// `entries` is recorded at `Location { store: CONFIGURED, object, offset }`
    /// **only if the digest is not indexed**: an entry already held, at a reachable
    /// object or not, is kept as it is, so a blob written while the rebuild ran is never
    /// replaced. Adopted entries count as a touch at the farm time they are applied.
    /// Afterwards the log takes commands naming `object`'s writer
    /// ([`Command::ObjectUnreachable`] on a read that fails, [`Command::EvictObjects`]).
    /// Refused ([`AdoptRefused`]) for an object of the current node writer, whose
    /// objects its own puts index, and for an object of an epoch this log never
    /// allocated, which a later [`Command::AllocEpoch`] would name again. Format 4.
    AdoptSegment {
        /// The segment object.
        object: ObjectId,
        /// Its records: digest and offset in the segment.
        entries: Vec<(Digest, u64)>,
    },
    /// The rebuild of the index has handled every object up to and including the one
    /// whose key is `after`, in key order: a restart resumes after it. A key not after
    /// the one held, or one after the rebuild is done, changes nothing. Format 4.
    RebuildProgress {
        /// The last object key handled.
        after: String,
    },
    /// The rebuild of the index is complete: the index names every object the store
    /// held when it started. Never undone; a log that has it does not rebuild. Format 4.
    RebuildDone,
}

/// What applying a [`Command`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// [`Command::Tick`]: the farm time after the tick.
    Ticked(FarmTime),
    /// [`Command::AllocEpoch`]: the new epoch.
    Epoch(Epoch),
    /// [`Command::PutBlob`].
    Blob(Result<BlobWrite, ForeignWriter>),
    /// [`Command::PutBlobs`]: one outcome per blob, in order.
    Blobs(Result<Vec<BlobWrite>, ForeignWriter>),
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
    Marked(Result<(), ForeignWriter>),
    /// [`Command::Collect`].
    Collected(Collected),
    /// [`Command::PutStaged`]: one outcome per blob, object by object, in order.
    Staged(Result<Vec<BlobWrite>, TierError>),
    /// [`Command::AddTier`], [`Command::SealStaged`], [`Command::ObjectFlushed`] or
    /// [`Command::FlushConflict`].
    Tier(Result<(), TierError>),
    /// [`Command::EvictObjects`].
    Evicted(Evicted),
    /// [`Command::SetWriter`].
    Writer(Result<(), StaleWriter>),
    /// [`Command::AdoptSegment`].
    Adopted(Result<Adopted, AdoptRefused>),
    /// [`Command::RebuildProgress`] or [`Command::RebuildDone`]: the rebuild's state after
    /// the command.
    Rebuild(Rebuild),
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

/// What a [`Command::EvictObjects`] removed, each list in digest order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evicted {
    /// Blobs removed from the index.
    pub blobs: Vec<Digest>,
    /// Action-cache entries removed, because they needed a removed blob.
    pub actions: Vec<Digest>,
}

/// A command named an object of a writer this log does not take for it, and changes
/// nothing: an epoch never allocated (a writer that skipped [`Command::AllocEpoch`], or
/// one whose epoch came from another log), a node writer never set
/// ([`Command::SetWriter`]), or, for a put, a node writer that is not the current one
/// (an earlier boot, or another member). Another writer's objects enter the index only
/// by a command that adopts them, never by a put.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("object {0} is not of a writer this log takes for the command")]
pub struct ForeignWriter(pub ObjectId);

/// A [`Command::SetWriter`] named a member at a boot the log has set already or one
/// before it: a process registered earlier, or a state store that handed out a boot
/// again. The command changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("member {} boot {} is not after boot {current}, the latest set", .writer.member(), .writer.boot())]
pub struct StaleWriter {
    /// The writer refused.
    pub writer: NodeWriter,
    /// The member's latest boot set.
    pub current: NonZeroU64,
}

/// Why an action-cache write was refused. A refused write changes nothing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActionWriteError {
    /// Only daemons write the action cache.
    #[error("action-cache writes are accepted only from a daemon")]
    NotDaemon,
    /// Every blob the entry needs that is not held, the result's first, then the
    /// closure's in digest order; never empty. The outputs must be stored first.
    #[error("action-cache entry needs blobs that are not held: {}", listed(.0))]
    Absent(Vec<Digest>),
    /// Every blob the entry needs that is held at an unreachable object, in the same
    /// order; never empty. Only when none is absent.
    #[error("action-cache entry needs blobs whose objects are unreachable: {}", listed(.0))]
    Unreachable(Vec<Digest>),
}

/// `digests`, comma-separated, for an error message.
fn listed(digests: &[Digest]) -> String {
    let named: Vec<String> = digests.iter().map(ToString::to_string).collect();
    named.join(", ")
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

/// The metadata state: the CAS index, the action cache, unreachable objects, farm
/// time, the next writer epoch, and the write tiers with their staged objects.
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
    unreachable: BTreeMap<ObjectId, LossMark>,
    /// How many times an object went from reachable to marked unreachable.
    losses: u64,
    /// Objects marked unreachable while they were staged, each with how many index
    /// entries are at it: those blobs' only copy was the object's block on a write
    /// tier, so they are lost ([`MetaState::lost_blobs`]). An object leaves when its
    /// last entry does, or when its mark is cleared.
    lost: BTreeMap<ObjectId, u64>,
    /// How many blobs were ever marked lost ([`MetaState::blob_losses`]).
    blob_losses: u64,
    next_epoch: u64,
    /// Each member [`Command::SetWriter`] named, with the latest boot it set.
    node_boots: BTreeMap<NonZeroU64, NonZeroU64>,
    /// Writers whose objects [`Command::AdoptSegment`] indexed: known to the log from
    /// then on, though it never allocated or set them.
    adopted_writers: BTreeSet<Writer>,
    /// How far the rebuild of the index is ([`MetaState::rebuild`]).
    rebuild: Rebuild,
    /// The node writer set last: the only one a put of a node object may name.
    current_writer: Option<NodeWriter>,
    /// Each server's current tier.
    tiers: BTreeMap<ServerId, TierId>,
    /// Tiers a server replaced; never registered again.
    retired_tiers: BTreeSet<TierId>,
    /// Objects whose only copy is a block on a tier.
    staged: BTreeMap<ObjectId, StagedObject>,
    /// Objects that left `staged` (flushed, or marked unreachable), with the farm time
    /// they left, so a late `PutStaged` cannot stage one again. Forgotten by `Collect`
    /// after the blob retention (`min_ttl` plus the touch quantum).
    left: BTreeMap<ObjectId, FarmTime>,
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
            unreachable: BTreeMap::new(),
            losses: 0,
            lost: BTreeMap::new(),
            blob_losses: 0,
            next_epoch: 1,
            node_boots: BTreeMap::new(),
            adopted_writers: BTreeSet::new(),
            rebuild: Rebuild::default(),
            current_writer: None,
            tiers: BTreeMap::new(),
            retired_tiers: BTreeSet::new(),
            staged: BTreeMap::new(),
            left: BTreeMap::new(),
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

    /// Why `object` is marked unreachable, or `None` if it is not.
    #[must_use]
    pub fn unreachable(&self, object: ObjectId) -> Option<UnreachableReason> {
        self.unreachable.get(&object).map(|mark| mark.reason)
    }

    /// How many times an object has newly become unreachable: a
    /// [`Command::ObjectUnreachable`] on an object with no mark counts, and so does one
    /// after a [`Command::ObjectReachable`] cleared it. A mark that only raises the
    /// reason or restamps the generation does not count, nor does a clear. It never
    /// falls.
    ///
    /// A blob held in the object answered present until the mark and answers
    /// unreachable after it, so anything remembered about "these blobs are held" that
    /// was recorded when this count was lower may be wrong. Whoever adds a command that
    /// takes held blobs away before their retention ends must count it here:
    /// [`Command::EvictObjects`] counts each object it took a blob from.
    #[must_use]
    pub const fn losses(&self) -> u64 {
        self.losses
    }

    /// `object`'s loss mark, or `None` if it is not marked. A prober reads the
    /// generation here before it probes the store, and names it in the
    /// [`Command::ObjectReachable`] it sends.
    #[must_use]
    pub fn loss_mark(&self, object: ObjectId) -> Option<LossMark> {
        self.unreachable.get(&object).copied()
    }

    /// Applies one command, the payload of the log entry at `index`, and reports what
    /// it did. The index is an input only to [`Command::ObjectUnreachable`], which
    /// stamps it as the mark's [`Generation`]; every caller passes the index of the
    /// entry it applies.
    pub fn execute(&mut self, index: u64, command: Command) -> Applied {
        match command {
            Command::Tick(t) => {
                self.now = self.now.max(t);
                Applied::Ticked(self.now)
            }
            Command::AllocEpoch => {
                let epoch = Epoch::new(self.next_epoch);
                self.next_epoch += 1;
                Applied::Epoch(epoch)
            }
            Command::PutBlob { digest, location } => Applied::Blob(
                self.writable(location.object)
                    .map(|()| self.put_blob(digest, location)),
            ),
            Command::PutBlobs(blobs) => Applied::Blobs(self.put_blobs(blobs)),
            Command::PutAction {
                role,
                action,
                record,
            } => Applied::Action(self.put_action(role, action, record)),
            Command::Touch(touch) => Applied::Touched {
                lost: self.touch(touch),
            },
            Command::ObjectUnreachable { object, reason } => {
                Applied::Marked(self.known(object).map(|()| {
                    let generation = Generation::new(index);
                    if !self.unreachable.contains_key(&object) {
                        self.losses += 1;
                    }
                    let mark = self
                        .unreachable
                        .entry(object)
                        .or_insert(LossMark { reason, generation });
                    mark.reason = mark.reason.max(reason);
                    mark.generation = generation;
                    self.mark_lost(object);
                    self.unstage(object);
                }))
            }
            Command::ObjectReachable { object, generation } => {
                Applied::Marked(self.known(object).map(|()| {
                    if self
                        .unreachable
                        .get(&object)
                        .is_some_and(|mark| mark.generation == generation)
                    {
                        self.unreachable.remove(&object);
                        self.lost.remove(&object);
                    }
                }))
            }
            Command::Collect => Applied::Collected(self.collect()),
            Command::AddTier { server, tier } => Applied::Tier(self.add_tier(server, tier)),
            Command::PutStaged { tier, objects } => Applied::Staged(self.put_staged(tier, objects)),
            Command::SealStaged { object } => Applied::Tier(self.seal_staged(object)),
            Command::ObjectFlushed { object } => Applied::Tier(self.object_flushed(object)),
            Command::FlushConflict { object } => Applied::Tier(self.flush_conflict(object)),
            Command::EvictObjects { objects } => Applied::Evicted(self.evict_objects(&objects)),
            Command::SetWriter(writer) => Applied::Writer(self.set_writer(writer)),
            Command::AdoptSegment { object, entries } => {
                Applied::Adopted(self.adopt_segment(object, entries))
            }
            Command::RebuildProgress { after } => Applied::Rebuild(self.rebuild_progress(after)),
            Command::RebuildDone => Applied::Rebuild(self.rebuild_done()),
        }
    }

    /// The answer for one blob. Touches nothing; see [`MetaState::touch_for`].
    #[must_use]
    pub fn blob(&self, digest: &Digest) -> BlobAnswer {
        match self.blobs.get(digest) {
            None => BlobAnswer::Absent,
            Some(entry) if self.unreachable.contains_key(&entry.location.object) => {
                BlobAnswer::Unavailable
            }
            Some(entry) => BlobAnswer::Present {
                location: entry.location,
                staged_on: self.staged.get(&entry.location.object).map(|s| s.tier),
            },
        }
    }

    /// The touch a reader must commit before reporting or serving `digests`: the
    /// present ones whose last touch is at least the touch quantum old.
    #[must_use]
    pub fn touch_for<'a>(&self, digests: impl IntoIterator<Item = &'a Digest>) -> Touch {
        let mut touch = Touch::default();
        for digest in digests {
            if let Some(entry) = self.blobs.get(digest)
                && !self.unreachable.contains_key(&entry.location.object)
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
        let mut missing = Vec::new();
        let mut objects = BTreeSet::new();
        for digest in digests {
            match self.blob(digest) {
                BlobAnswer::Present { location, .. } => {
                    objects.insert(location.object);
                }
                BlobAnswer::Unavailable | BlobAnswer::Absent => missing.push(*digest),
            }
        }
        FindMissing {
            missing,
            touch: self.touch_for(digests),
            objects,
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

    /// The objects in the store that hold the blobs `action`'s entry needs (its result
    /// blob and closure), each once: a reader checks the store still has them before it
    /// serves a hit. A blob staged on a tier is left out (the store does not hold it
    /// yet), as is one not present. Empty without an entry.
    #[must_use]
    pub fn stored_objects(&self, action: &Digest) -> BTreeSet<ObjectId> {
        self.actions
            .get(action)
            .into_iter()
            .flat_map(|e| iter::once(&e.record.result).chain(e.record.closure.iter()))
            .filter_map(|d| match self.blob(d) {
                BlobAnswer::Present {
                    location,
                    staged_on: None,
                } => Some(location.object),
                _ => None,
            })
            .collect()
    }

    /// The closure check: the first needed blob that is not present and reachable.
    fn closure_check<'a>(&self, needed: impl Iterator<Item = &'a Digest>) -> Result<(), Lack> {
        for digest in needed {
            match self.blob(digest) {
                BlobAnswer::Present { .. } => {}
                BlobAnswer::Unavailable => return Err(Lack::Unreachable(*digest)),
                BlobAnswer::Absent => return Err(Lack::Absent(*digest)),
            }
        }
        Ok(())
    }

    /// The write-time closure check: every blob of `needed` that is not present and
    /// reachable, as the refusal of an action-cache write names them
    /// ([`ActionWriteError::Absent`] if any is absent, else
    /// [`ActionWriteError::Unreachable`]); `None` if every one is.
    #[must_use]
    pub fn lacking<'a>(
        &self,
        needed: impl Iterator<Item = &'a Digest>,
    ) -> Option<ActionWriteError> {
        let (mut absent, mut unreachable) = (Vec::new(), Vec::new());
        for digest in needed {
            match self.blob(digest) {
                BlobAnswer::Present { .. } => {}
                BlobAnswer::Unavailable => unreachable.push(*digest),
                BlobAnswer::Absent => absent.push(*digest),
            }
        }
        if !absent.is_empty() {
            Some(ActionWriteError::Absent(absent))
        } else if !unreachable.is_empty() {
            Some(ActionWriteError::Unreachable(unreachable))
        } else {
            None
        }
    }

    /// The node writer every put of a node object must name, once one is set.
    #[must_use]
    pub const fn current_writer(&self) -> Option<NodeWriter> {
        self.current_writer
    }

    /// Whether this log knows `object`'s writer: an epoch it allocated, a member it
    /// set at the object's boot or a later one, or a writer whose objects it adopted.
    fn known(&self, object: ObjectId) -> Result<(), ForeignWriter> {
        let by_rule = match object.writer() {
            Writer::Epoch(epoch) => epoch.get() != 0 && epoch.get() < self.next_epoch,
            Writer::Node(node) => self
                .node_boots
                .get(&node.member())
                .is_some_and(|latest| node.boot() <= *latest),
            Writer::Legacy(_) => false,
        };
        let known = by_rule || self.adopted_writers.contains(&object.writer());
        if known {
            Ok(())
        } else {
            Err(ForeignWriter(object))
        }
    }

    /// Whether a put may name `object`: one of an allocated epoch, or of the current
    /// node writer.
    fn writable(&self, object: ObjectId) -> Result<(), ForeignWriter> {
        match object.writer() {
            Writer::Epoch(_) => self.known(object),
            Writer::Node(node) if self.current_writer == Some(node) => Ok(()),
            Writer::Node(_) | Writer::Legacy(_) => Err(ForeignWriter(object)),
        }
    }

    fn set_writer(&mut self, writer: NodeWriter) -> Result<(), StaleWriter> {
        if let Some(current) = self.node_boots.get(&writer.member()).copied()
            && writer.boot() <= current
        {
            return Err(StaleWriter { writer, current });
        }
        self.node_boots.insert(writer.member(), writer.boot());
        self.current_writer = Some(writer);
        Ok(())
    }

    fn put_blobs(
        &mut self,
        blobs: Vec<(Digest, Location)>,
    ) -> Result<Vec<BlobWrite>, ForeignWriter> {
        for (_, location) in &blobs {
            self.writable(location.object)?;
        }
        Ok(blobs
            .into_iter()
            .map(|(digest, location)| self.put_blob(digest, location))
            .collect())
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
            BlobAnswer::Present { location, .. } => BlobWrite::Duplicate { kept: location },
            BlobAnswer::Unavailable => {
                let replaced = entry.location;
                entry.location = location;
                self.entry_left(replaced.object);
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
        if let Some(lacking) = self.lacking(iter::once(&record.result).chain(record.closure.iter()))
        {
            return Err(lacking);
        }
        self.order_flush(&record.result, record.closure.iter());
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
                self.entry_left(entry.location.object);
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
        let forgotten: Vec<ObjectId> = self
            .left
            .iter()
            .filter(|(_, at)| self.expired(**at, min_ttl))
            .map(|(o, _)| *o)
            .collect();
        for object in forgotten {
            self.left.remove(&object);
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

#[cfg(test)]
mod idempotence;

impl StateMachine for MetaState {
    /// A log entry's index and its command.
    type Input = (u64, Command);

    /// Applies the command at its index; its outcome goes to the caller through
    /// [`MetaState::execute`], and the metadata core asks for no effects.
    fn apply(&mut self, (index, command): (u64, Command)) -> Vec<Effect> {
        self.execute(index, command);
        Vec::new()
    }
}
