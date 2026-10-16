//! Eviction from a file store under disk pressure (`docs/design/storage.md`,
//! "Eviction"): when the store's file system has less free than twice its floor, the
//! least recently used objects are forgotten ([`Command::EvictObjects`]) and their
//! files deleted until their lengths cover the shortfall.
//!
//! Recency is kept here, per object, in memory, not in the store: a worker's input
//! cache and the write tier's flushed copies serve repeated reads without reaching the
//! store, and a FindMissing answer never touches it, so only the cache sees every use.
//! It is bumped by FindMissing's present answers, reads, action-cache hits, locates and
//! writes, and lost at a restart: the start-up pass seeds it with the objects it kept
//! ([`Cache::seed_recency`]), which rank before every object used since, oldest
//! file first.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use kbf_meta::{Applied, Command, ObjectId};
use kbf_objstore::{FsStore, ObjectStore, Room};
use tokio::time::Instant;

use super::{Cache, CacheError, EVICT_BATCH, EvictReady, HeldObject, NEEDS_V3, unexpected};
use crate::meta_log::{MetaLog, MetaLogError};

/// How long after its last use an object is never evicted: a client told a blob is
/// present (or sent an action-cache hit) has this long to read it. Under disk pressure
/// it stands in for the retention's `min_ttl`, which eviction cuts short.
pub const EVICT_GRACE: Duration = Duration::from_secs(60);

/// How often the server's evictor looks at the free space when no write was refused.
pub const EVICT_EVERY: Duration = Duration::from_secs(10);

/// What a round that evicted something logs.
const EVICTED: &str =
    "evicted the least recently used objects: the cold store directory is short of room";

/// When each object of the store was last used. Off (and empty) until
/// [`Cache::track_recency`]. Also whether the evictor has said it is off for want of
/// log version 3 ([`EvictReady::NeedsV3`]), which it says once.
#[derive(Debug, Default)]
pub(super) struct Recency(Mutex<Option<Clock>>, AtomicBool);

#[derive(Debug, Default)]
struct Clock {
    /// The last use handed out; 0 is "not used since the start".
    tick: u64,
    objects: HashMap<ObjectId, Use>,
}

#[derive(Clone, Copy, Debug)]
struct Use {
    /// The object's length, once known (from the start-up pass or its write): an
    /// object of unknown length is not evicted.
    len: Option<u64>,
    /// When its file was written: the order of objects not used since the start.
    mtime: SystemTime,
    /// Its last use, 0 if none since the start.
    tick: u64,
    /// When that was.
    at: Option<Instant>,
}

impl Recency {
    fn lock(&self) -> MutexGuard<'_, Option<Clock>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a use of each of `objects` now, if recency is on.
    pub(super) fn used(&self, objects: impl IntoIterator<Item = ObjectId>) {
        self.record(objects, None);
    }

    /// Records that `object`, `len` bytes, was written now: a use.
    pub(super) fn written(&self, object: ObjectId, len: u64) {
        self.record([object], Some(len));
    }

    fn record(&self, objects: impl IntoIterator<Item = ObjectId>, len: Option<u64>) {
        let now = Instant::now();
        if let Some(clock) = self.lock().as_mut() {
            for object in objects {
                clock.tick += 1;
                let fresh = Use {
                    len,
                    mtime: SystemTime::now(),
                    tick: 0,
                    at: None,
                };
                let entry = clock.objects.entry(object).or_insert(fresh);
                (entry.tick, entry.at) = (clock.tick, Some(now));
                entry.len = len.or(entry.len);
            }
        }
    }

    /// The objects to evict to free `shortfall` bytes: the least recently used first
    /// (those not used since the start before all others, oldest file first), none of
    /// `staged` and none used within `grace` of `now`, until their lengths cover it.
    fn choose(
        &self,
        now: Instant,
        grace: Duration,
        shortfall: u64,
        staged: &BTreeSet<ObjectId>,
    ) -> Vec<(ObjectId, u64)> {
        let clock = self.lock();
        let mut cold: Vec<(u64, SystemTime, ObjectId, u64)> = clock
            .iter()
            .flat_map(|c| &c.objects)
            .filter(|(id, u)| {
                !staged.contains(id)
                    && u.at
                        .is_none_or(|at| now.saturating_duration_since(at) >= grace)
            })
            .filter_map(|(id, u)| Some((u.tick, u.mtime, *id, u.len?)))
            .collect();
        drop(clock);
        cold.sort_unstable();
        let mut covered = 0_u64;
        let mut chosen = Vec::new();
        for (.., id, len) in cold {
            if covered >= shortfall {
                break;
            }
            covered = covered.saturating_add(len);
            chosen.push((id, len));
        }
        chosen
    }

    /// Whether this is the first round off for want of log version 3.
    fn first_off(&self) -> bool {
        !self.1.swap(true, Ordering::Relaxed)
    }

    /// Drops `object`, evicted. Only an evictor calls it, so recency is on.
    fn forget(&self, object: ObjectId) {
        for clock in self.lock().iter_mut() {
            clock.objects.remove(&object);
        }
    }
}

/// What one [`Cache::evict_round`] found and did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EvictRound {
    /// The bytes free on the store's file system when the round began.
    pub available: u64,
    /// What the round frees towards: twice the store's floor.
    pub target: u64,
    /// Objects forgotten and deleted.
    pub objects: u64,
    /// Their bytes (headers not counted).
    pub bytes: u64,
    /// Blobs taken out of the index with them.
    pub blobs: u64,
    /// Action-cache entries removed because they needed one of those blobs.
    pub actions: u64,
    /// The bytes of the shortfall no object could cover: every other object was used
    /// within the grace window, staged, or of unknown length; or the whole shortfall,
    /// if eviction is off.
    pub short: u64,
    /// The voters of a cell whose log does not take [`Command::EvictObjects`] yet
    /// ([`EvictReady::NeedsV3`]): eviction is off, nothing was evicted.
    pub needs_v3: Option<usize>,
}

impl fmt::Display for EvictRound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            available,
            target,
            objects,
            bytes,
            blobs,
            actions,
            short,
            needs_v3: _,
        } = self;
        write!(
            f,
            "available={available} target={target} objects={objects} bytes={bytes} \
             blobs={blobs} actions={actions} short={short}"
        )
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Starts recording when each object is used ([`Cache::evict_round`] ranks by it).
    /// Call it before the cache serves; a second call changes nothing.
    pub fn track_recency(&self) {
        self.writer
            .recency
            .lock()
            .get_or_insert_with(Clock::default);
    }

    /// Adds the objects a start-up pass kept to the recency it ranks by: each not used
    /// since the start (if it was, only its length is taken).
    pub fn seed_recency(&self, held: &BTreeMap<ObjectId, HeldObject>) {
        // Seeded only once recency is on: the server turns it on before it serves.
        for clock in self.writer.recency.lock().iter_mut() {
            for (id, h) in held {
                let seeded = Use {
                    len: Some(h.len),
                    mtime: h.mtime,
                    tick: 0,
                    at: None,
                };
                let entry = clock.objects.entry(*id).or_insert(seeded);
                entry.len = Some(h.len);
            }
        }
    }

    /// Records a use of each of `objects`.
    pub(super) fn used(&self, objects: impl IntoIterator<Item = ObjectId>) {
        self.writer.recency.used(objects);
    }

    /// One round of eviction from `files`, the store this cache writes to: if the file
    /// system has less free than twice the floor, the objects [`Recency::choose`]
    /// picks for the shortfall (the least recently used, none staged, none used within
    /// `grace`) are forgotten with [`Command::EvictObjects`] in batches of
    /// [`EVICT_BATCH`], after `ready_to_evict` readies the log for the command, and
    /// only then are their files deleted: a crash between the two leaves an orphan
    /// file, which the next start-up pass deletes, never an index entry without its
    /// file. Logs what it evicted, if anything. If `ready_to_evict` answers
    /// [`EvictReady::NeedsV3`], nothing is evicted (an index entry must go before its
    /// file, which takes the command), the round says so, and the first such round of
    /// this cache logs [`NEEDS_V3`]; writes stay refused under the floor until a
    /// round under a log that takes the command frees room.
    ///
    /// # Errors
    /// The file system cannot be asked, the metadata log fails or refuses to be
    /// readied, or a file cannot be deleted. What was done before stays done.
    pub async fn evict_round(
        &self,
        files: &FsStore,
        grace: Duration,
        ready_to_evict: fn(&M) -> Result<EvictReady, MetaLogError>,
    ) -> Result<EvictRound, CacheError> {
        let Room { available, floor } = files.room()?;
        let target = floor.saturating_mul(2);
        let mut round = EvictRound {
            available,
            target,
            ..EvictRound::default()
        };
        let shortfall = target.saturating_sub(available);
        if shortfall == 0 {
            return Ok(round);
        }
        let staged = self
            .writer
            .query(|s| s.staged_objects().map(|(o, _)| o).collect())
            .await?;
        let chosen = self
            .writer
            .recency
            .choose(Instant::now(), grace, shortfall, &staged);
        round.short = shortfall.saturating_sub(chosen.iter().map(|(_, len)| len).sum());
        if !chosen.is_empty()
            && let EvictReady::NeedsV3 { voters } = ready_to_evict(&self.writer.meta)?
        {
            if self.writer.recency.first_off() {
                tracing::warn!(voters, "{NEEDS_V3}: eviction off");
            }
            (round.short, round.needs_v3) = (shortfall, Some(voters));
            return Ok(round);
        }
        for batch in chosen.chunks(EVICT_BATCH) {
            let objects = batch.iter().map(|(id, _)| *id).collect();
            let applied = self
                .writer
                .commit(Command::EvictObjects { objects })
                .await?;
            let Applied::Evicted(evicted) = &applied else {
                return Err(unexpected(&applied));
            };
            round.blobs += evicted.blobs.len() as u64;
            round.actions += evicted.actions.len() as u64;
            for (id, len) in batch {
                files.delete(&self.object_key(*id)?).await?;
                self.writer.recency.forget(*id);
                (round.objects, round.bytes) = (round.objects + 1, round.bytes + len);
            }
        }
        if round.objects > 0 {
            tracing::info!(%round, "{EVICTED}");
        }
        Ok(round)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches an object whose length its write never records (mutants: `written`
    /// records only the use; a write after a use keeps the unknown length), which an
    /// evictor then never picks; and recency recorded while it is off.
    #[test]
    fn a_write_records_the_length() {
        let recency = Recency::default();
        let none = BTreeSet::new();
        let choose = |r: &Recency| r.choose(Instant::now(), Duration::ZERO, 100, &none);
        let [a, b] = [1, 2].map(|seq| ObjectId::new(kbf_meta::Epoch::new(1), seq));
        recency.written(a, 5);
        assert!(choose(&recency).is_empty(), "recorded while off");
        *recency.lock() = Some(Clock::default());
        recency.written(a, 5);
        recency.used([b]);
        assert_eq!(choose(&recency), [(a, 5)], "b's length is unknown");
        recency.written(b, 7);
        assert_eq!(choose(&recency), [(a, 5), (b, 7)]);
        recency.forget(a);
        assert_eq!(choose(&recency), [(b, 7)]);
    }
}
