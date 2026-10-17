//! The start-up consistency pass over a file store ([`Cache::consistency_pass`]): the
//! store's files checked against the index once, in the background, after the server
//! serves (`docs/deploy/storage-node.md`, "The start-up pass").

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use kbf_meta::{Applied, Command, ObjectId, UnreachableReason, Writer as ObjectWriter};
use kbf_objstore::{FsStore, ObjectStore};
use tokio::time::Instant;

use super::{Cache, CacheError, unexpected};
use crate::meta_log::{MetaLog, MetaLogError};

/// How many object files the pass opens a second, on average, while the file system
/// has room: enough to walk a million objects in under 20 minutes without taking the
/// disk from the reads it serves. Below the store's free-space floor the pass runs
/// unpaced, since every upload is refused until it finishes (and frees what it can).
pub const PASS_OPENS_PER_SEC: NonZeroU32 = NonZeroU32::new(1000).expect("not zero");

/// How many objects one [`Command::EvictObjects`] names: applying one scans the whole
/// index and action cache, about the same time for 1 object as for 256 on a million
/// of each, so the pass batches.
pub const EVICT_BATCH: usize = 256;

/// What the pass and the evictor log, once each per start, when the log cannot take
/// [`Command::EvictObjects`] and they may not ready it ([`EvictReady::NeedsV3`]).
pub const NEEDS_V3: &str = "evict/forget needs log V3";

/// Whether the metadata log takes [`Command::EvictObjects`], as the hook the pass and
/// the evictor are given answers it, called only when there is something to forget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvictReady {
    /// It does: it did already, or the hook readied it.
    Ready,
    /// It does not, and the hook may not ready it: a cell of `voters` voters, whose
    /// machine version an operator raises once every replica runs one that takes the
    /// command. Nothing is forgotten until then.
    NeedsV3 {
        /// The cell's voters.
        voters: usize,
    },
}

/// What the pass logs when the guard holds the index side back.
const NOT_THIS_DIRECTORY: &str = "start-up pass: no file of any indexed object is in the \
    store, so it is not the directory the index was written over (a missing mount?): \
    nothing is forgotten";

/// An object file the pass kept: what an evictor needs to rank it before any read has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldObject {
    /// The object's length (its file's, less the header).
    pub len: u64,
    /// When its file was written.
    pub mtime: SystemTime,
}

/// What a [`Cache::consistency_pass`] found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Object files read (each with a header naming its key).
    pub files: u64,
    /// Files with no object header; left alone.
    pub unreadable: u64,
    /// This cache's objects kept: indexed, staged, or written by this process.
    pub kept: u64,
    /// This cache's objects deleted: of an earlier epoch of this log and in no index
    /// entry.
    pub orphans_deleted: u64,
    /// Files whose key is not one of this cache's objects and that no index entry
    /// names: another log's prefix, not an object name, or another writer's object (a
    /// node writer's that is not this cache's, an earlier boot of its own member
    /// included, or an epoch writer's once this cache is a node writer); logged, kept,
    /// and not counted as held.
    pub foreign: u64,
    /// Indexed objects with no file that were forgotten ([`Command::EvictObjects`]).
    pub evicted_objects: u64,
    /// Blobs those objects held, taken out of the index.
    pub evicted_blobs: u64,
    /// Action-cache entries removed because they needed one of those blobs.
    pub evicted_actions: u64,
    /// Indexed objects with no file marked missing ([`UnreachableReason::Missing`])
    /// instead of forgotten, because the log does not take [`Command::EvictObjects`]
    /// yet ([`EvictReady::NeedsV3`]): their blobs read as missing and their action-cache
    /// entries miss; a pass under a log that takes the command forgets them.
    pub marked_missing: u64,
    /// Indexed objects with no file left in the index because the walk found no
    /// indexed file at all (the guard: a directory that is not the one the index was
    /// written over).
    pub withheld: u64,
    /// The kept objects, by id: the seed of an evictor's ranking. An object written
    /// after the pass walked its shard is not in it.
    pub held: BTreeMap<ObjectId, HeldObject>,
    /// How long the pass took.
    pub duration: Duration,
}

impl PassReport {
    /// The bytes of the objects kept ([`PassReport::held`]), headers not counted.
    #[must_use]
    pub fn held_bytes(&self) -> u64 {
        self.held.values().map(|h| h.len).sum()
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Checks `files`, which must be the store this cache writes to, against the
    /// index, once:
    ///
    /// - Each object file that the index names (staged objects too), or that is of this
    ///   cache's writer or a later one ([`ObjectWriter::at_or_after`]), is kept, and its length and modification time
    ///   go into the report's [`PassReport::held`].
    /// - One of an earlier epoch, while this cache writes as an epoch writer, that
    ///   nothing names is an orphan (an upload that never committed, an object whose
    ///   blobs were all healed elsewhere) and is deleted.
    /// - Any other file is foreign, counted and kept: under another prefix, or of
    ///   another writer. The pass deletes nothing outside its own log's epochs: another
    ///   node writer's object, an earlier boot of this node's own member included, may
    ///   be one only a rebuild of the index knows (its metadata directory was wiped).
    /// - Then every indexed object that is not staged and whose file the walk did not
    ///   find, and still is not there, is forgotten with [`Command::EvictObjects`] in
    ///   batches of [`EVICT_BATCH`], with the action-cache entries that need its
    ///   blobs, after `ready_to_evict` readies the log for the command. If it answers
    ///   [`EvictReady::NeedsV3`], each is marked missing instead
    ///   ([`Command::ObjectUnreachable`], which every log takes), and the pass logs
    ///   [`NEEDS_V3`] once, with their number. Only if the walk found the file of at
    ///   least one indexed object: otherwise this is not the directory the index was
    ///   written over (a missing mount, which `FsStore::open` would have created
    ///   empty), and nothing is forgotten or marked.
    ///
    /// The shards are walked at `opens_per_sec` ([`PASS_OPENS_PER_SEC`]), unpaced while
    /// the store is under its free-space floor. The index is read once before the walk:
    /// only objects of the current writer can be named after that, and those are kept.
    ///
    /// # Errors
    /// A shard cannot be read, an orphan cannot be deleted, or the metadata log fails
    /// or refuses to be readied. What was done before stays done.
    pub async fn consistency_pass(
        &self,
        files: &FsStore,
        opens_per_sec: NonZeroU32,
        ready_to_evict: fn(&M) -> Result<EvictReady, MetaLogError>,
    ) -> Result<PassReport, CacheError> {
        let started = Instant::now();
        let writer = self.writer();
        let indexed = self.writer.query(|s| s.indexed_objects()).await?;
        let mut report = PassReport::default();
        let mut pace = Pace::new(opens_per_sec);
        let mut seen = BTreeSet::new();
        let mut indexed_present = 0_u64;
        for shard in 0..=u8::MAX {
            let scan = files.scan(shard).await?;
            report.unreadable += scan.unreadable as u64;
            report.files += scan.objects.len() as u64;
            let mut unnamed = Vec::new();
            for file in &scan.objects {
                let Some(id) = self.writer.keys.object(&file.key) else {
                    tracing::debug!(key = %file.key, "start-up pass: a foreign object file, kept");
                    report.foreign += 1;
                    continue;
                };
                seen.insert(id);
                let held = HeldObject {
                    len: file.len,
                    mtime: file.mtime,
                };
                if indexed.contains(&id) {
                    indexed_present += 1;
                    report.held.insert(id, held);
                } else if id.writer().at_or_after(writer) {
                    report.held.insert(id, held);
                } else if both_epochs(id.writer(), writer) {
                    // An earlier epoch: a later or equal one is held above.
                    unnamed.push((id, &file.key, held));
                } else {
                    tracing::debug!(key = %file.key, "start-up pass: another writer's object, kept");
                    report.foreign += 1;
                }
            }
            // A staged object whose blobs were all collected names nothing, but its
            // file may be the flusher's write before it commits the flush.
            let staged: Vec<bool> = self
                .writer
                .query(|s| {
                    unnamed
                        .iter()
                        .map(|(id, ..)| s.staged(*id).is_some())
                        .collect()
                })
                .await?;
            for ((id, key, held), staged) in unnamed.into_iter().zip(staged) {
                if staged {
                    report.held.insert(id, held);
                } else {
                    files.delete(key).await?;
                    tracing::debug!(%key, "start-up pass: an orphan object deleted");
                    report.orphans_deleted += 1;
                }
            }
            pace.after(scan.opened(), files).await;
        }
        report.kept = report.held.len() as u64;
        self.evict_missing(files, &seen, indexed_present, ready_to_evict, &mut report)
            .await?;
        report.duration = started.elapsed();
        Ok(report)
    }

    /// The index side of [`Cache::consistency_pass`]: forgets (or marks missing) the
    /// indexed objects that are not staged, were not `seen`, and have no file now, if
    /// `indexed_present` (the guard) is not zero.
    async fn evict_missing(
        &self,
        files: &FsStore,
        seen: &BTreeSet<ObjectId>,
        indexed_present: u64,
        ready_to_evict: fn(&M) -> Result<EvictReady, MetaLogError>,
        report: &mut PassReport,
    ) -> Result<(), CacheError> {
        let unseen: Vec<ObjectId> = self
            .writer
            .query(|s| {
                let indexed = s.indexed_objects().into_iter();
                indexed
                    .filter(|o| s.staged(*o).is_none() && !seen.contains(o))
                    .collect()
            })
            .await?;
        // Written after the walk passed its shard, or flushed from a tier since.
        let mut gone = BTreeSet::new();
        for id in unseen {
            if !files.may_hold(&self.object_key(id)?).await? {
                gone.insert(id);
            }
        }
        if gone.is_empty() {
            return Ok(());
        }
        if indexed_present == 0 {
            report.withheld = gone.len() as u64;
            tracing::error!(objects = report.withheld, "{NOT_THIS_DIRECTORY}");
            return Ok(());
        }
        if let EvictReady::NeedsV3 { voters } = ready_to_evict(&self.writer.meta)? {
            for &id in &gone {
                self.mark_unreachable(id, UnreachableReason::Missing)
                    .await?;
            }
            report.marked_missing = gone.len() as u64;
            let held = report.marked_missing;
            tracing::warn!(voters, "{NEEDS_V3}: {held} objects held");
            return Ok(());
        }
        let gone: Vec<ObjectId> = gone.into_iter().collect();
        for batch in gone.chunks(EVICT_BATCH) {
            let objects = batch.iter().copied().collect();
            match self
                .writer
                .commit(Command::EvictObjects { objects })
                .await?
            {
                Applied::Evicted(evicted) => {
                    report.evicted_blobs += evicted.blobs.len() as u64;
                    report.evicted_actions += evicted.actions.len() as u64;
                }
                other => return Err(unexpected(&other)),
            }
            report.evicted_objects += batch.len() as u64;
        }
        Ok(())
    }
}

/// Whether an object of `of` can be an orphan for a cache writing as `current`: both
/// are epoch writers of this log. Nothing of another kind of writer is ever deleted.
fn both_epochs(of: ObjectWriter, current: ObjectWriter) -> bool {
    matches!(
        (of, current),
        (ObjectWriter::Epoch(_), ObjectWriter::Epoch(_))
    )
}

/// Spaces the pass's file opens to a rate, on average over each shard.
struct Pace {
    per_sec: NonZeroU32,
    next: Instant,
}

impl Pace {
    fn new(per_sec: NonZeroU32) -> Self {
        Self {
            per_sec,
            next: Instant::now(),
        }
    }

    /// Waits out `opened` opens at the rate, unless the store is under its floor.
    async fn after(&mut self, opened: usize, files: &FsStore) {
        let now = Instant::now();
        // A file system that cannot be asked is taken to have room.
        if files.under_floor().unwrap_or(false) {
            self.next = now;
            return;
        }
        let cost = Duration::from_secs(opened as u64) / self.per_sec.get();
        self.next = self.next.max(now) + cost;
        tokio::time::sleep_until(self.next).await;
    }
}
