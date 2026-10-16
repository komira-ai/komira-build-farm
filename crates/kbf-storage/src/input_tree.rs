//! The check of an Execute's input tree: [`Cache::missing_inputs`].
//!
//! An Execute names an input root `Directory`; every blob under it must be held. The
//! check used to read each Directory blob in turn, then ask FindMissingBlobs about every
//! file, for every request, remembering nothing. It now keeps two things in memory,
//! beside the metadata index and derived from it (neither is in the log; a restart
//! starts both empty):
//!
//! - **Decoded directories**, by digest: the file and subdirectory digests of each
//!   Directory read, so a tree walked again costs no store read. A digest names its
//!   content, so an entry is never stale. The cache is an LRU bounded in entries and
//!   bytes ([`InputTreeConfig`]).
//! - **A verified-until farm time per directory.** When a check finds a directory and
//!   everything under it present, having touched every blob it relied on, it records
//!   `until = now + min_ttl`, where `now` is the farm time read before the check's
//!   first report. A touched blob is held for at least `min_ttl` of farm time from the
//!   report (`docs/design/storage.md#retention-and-touches`; reads record their touch
//!   under the gate [`Cache::collect`] takes, so the guarantee holds from the report,
//!   whether or not the batched commit has gone out yet, and a touch skipped within the
//!   quantum is covered by the extra quantum an entry is held). A later check whose
//!   farm time is before `until` skips the directory without reading or asking. At or
//!   after `until` it checks again, and touches again. A directory resting on a skipped
//!   subtree gets the earlier of its own `until` and the subtree's.
//! - **The loss count it was verified at.** The guarantee above is about collection.
//!   It does not cover an object marked unreachable (a read found it gone or damaged):
//!   its blobs are held and reachable until the mark, and FindMissingBlobs lists them
//!   after it, so an Execute scheduled on a skipped subtree would fail to read an input
//!   and the client would not be told to upload it. [`kbf_meta::MetaState::losses`]
//!   counts the objects that newly became unreachable; a verification records the count
//!   its check began at, and a skip needs the count to be the same when the request begins. Any
//!   new mark therefore sends the next check of every subtree through the full walk
//!   once (from the decoded directories, touching and asking as usual, which records
//!   the new count). A mark is rare, so one object marked costs one re-walk of each
//!   tree in use, not a read of the store; no reverse index from objects to subtrees is
//!   kept, because it would be a second index of the blob locations to keep in step
//!   with the first. A single flight is shared only between requests that began at the
//!   same count, for the same reason.
//!
//! The guarantee is the process's own: the pending touches that pin a blob live in this
//! cache, and a restart forgets both memories together.
//!
//! The check is a recursion over subdirectories. Each directory is checked once at a
//! time: a request that reaches a directory another request is checking waits for that
//! answer (single flight), so a toolchain shared by a hundred Executes is walked once.
//! A check runs on its own task, so a request dropped mid-check (a client cancel) leaves
//! neither a held read slot nor a stale flight. Within a request every directory is
//! checked once. Store reads are bounded by
//! [`InputTreeConfig::reads_in_flight`] across all requests. A directory whose blob is
//! gone is reported, and nothing under it is looked at, as before; the report lists
//! absent directories (sorted) before absent files (sorted).

use std::collections::{BTreeSet, HashMap};
use std::mem::size_of;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared, join, join_all};
use kbf_objstore::ObjectStore;
use kbf_proto::reapi::Directory;
use kbf_types::{Digest, FarmTime};
use prost::Message;
use tokio::sync::Semaphore;

use crate::cache::{Cache, CacheError};
use crate::meta_log::MetaLog;
use crate::wire;

/// What the input check remembers and how hard it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputTreeConfig {
    /// The most directories kept decoded. At least 1.
    pub max_entries: NonZeroUsize,
    /// The most bytes of decoded directories kept (each costs its digests plus
    /// [`ENTRY_OVERHEAD`]). A directory larger than this is checked but not kept.
    pub max_bytes: u64,
    /// The most Directory blobs read from the store at once, across all requests.
    pub reads_in_flight: NonZeroUsize,
}

impl InputTreeConfig {
    /// A million directories, 256 MiB (a toolchain tree of 100k files is about 4 MiB),
    /// 64 reads at a time.
    pub const DEFAULT: Self = Self {
        max_entries: NonZeroUsize::MIN.saturating_add((1 << 20) - 1),
        max_bytes: 256 << 20,
        reads_in_flight: NonZeroUsize::MIN.saturating_add(63),
    };
}

/// What a kept directory costs besides its digests: its map slot, order slot and boxes.
pub const ENTRY_OVERHEAD: u64 = 160;

/// The file and subdirectory digests of one Directory.
#[derive(Debug)]
struct Decoded {
    files: Box<[Digest]>,
    dirs: Box<[Digest]>,
}

impl Decoded {
    fn cost(&self) -> u64 {
        let digests = (self.files.len() + self.dirs.len()) as u64;
        digests
            .saturating_mul(size_of::<Digest>() as u64)
            .saturating_add(ENTRY_OVERHEAD)
    }
}

/// That a directory and everything under it was found held, and when.
#[derive(Clone, Copy, Debug)]
struct Verified {
    until: FarmTime,
    /// [`kbf_meta::MetaState::losses`] when the check began.
    losses: u64,
}

#[derive(Debug)]
struct Entry {
    decoded: Arc<Decoded>,
    cost: u64,
    verified: Option<Verified>,
    /// Position in the recency order.
    used: u64,
}

/// Decoded directories in least-recently-used order.
#[derive(Debug)]
struct Lru {
    config: InputTreeConfig,
    entries: HashMap<Digest, Entry>,
    /// Recency tick to digest; the first key is the least recently used.
    order: std::collections::BTreeMap<u64, Digest>,
    tick: u64,
    bytes: u64,
}

impl Lru {
    fn new(config: InputTreeConfig) -> Self {
        Self {
            config,
            entries: HashMap::new(),
            order: std::collections::BTreeMap::new(),
            tick: 0,
            bytes: 0,
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// The directory and its verification, now the most recently used.
    fn get(&mut self, digest: &Digest) -> Option<(Arc<Decoded>, Option<Verified>)> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(digest)?;
        self.order.remove(&entry.used);
        entry.used = tick;
        self.order.insert(tick, *digest);
        Some((Arc::clone(&entry.decoded), entry.verified))
    }

    /// Keeps `decoded` as the most recently used entry, unless it alone costs more
    /// than the byte bound. An entry already kept under `digest` is replaced.
    fn insert(&mut self, digest: Digest, decoded: Arc<Decoded>) {
        let cost = decoded.cost();
        let old = self.entries.remove(&digest);
        self.bytes -= old.as_ref().map_or(0, |e| e.cost);
        // Ticks start at 1, so tick 0 names no entry.
        self.order.remove(&old.map_or(0, |e| e.used));
        if cost > self.config.max_bytes {
            return;
        }
        let used = self.next_tick();
        self.order.insert(used, digest);
        self.bytes += cost;
        self.entries.insert(
            digest,
            Entry {
                decoded,
                cost,
                verified: None,
                used,
            },
        );
        self.evict();
    }

    /// Drops the least recently used directories until the bounds hold.
    fn evict(&mut self) {
        let (max_entries, max_bytes) = (self.config.max_entries.get(), self.config.max_bytes);
        let (mut entries, mut bytes) = (self.entries.len(), self.bytes);
        let victims: Vec<(u64, Digest)> = self
            .order
            .iter()
            .take_while(|(_, digest)| {
                let over = entries > max_entries || bytes > max_bytes;
                entries = entries.saturating_sub(1);
                bytes = bytes.saturating_sub(self.entries.get(*digest).map_or(0, |e| e.cost));
                over
            })
            .map(|(tick, digest)| (*tick, *digest))
            .collect();
        for (tick, digest) in victims {
            self.order.remove(&tick);
            self.bytes -= self.entries.remove(&digest).map_or(0, |gone| gone.cost);
        }
    }

    /// Records that `digest` was verified until `until` by a check that began at loss
    /// count `losses`. A record of the same count is never lowered; one of an older
    /// count is replaced; one of a newer count (a check that ended late) is kept. A
    /// directory no longer kept is not recorded.
    fn verified(&mut self, digest: &Digest, until: FarmTime, losses: u64) {
        let Some(entry) = self.entries.get_mut(digest) else {
            return;
        };
        entry.verified = Some(match entry.verified {
            Some(was) if was.losses > losses => was,
            Some(was) if was.losses == losses => Verified {
                until: was.until.max(until),
                losses,
            },
            _ => Verified { until, losses },
        });
    }
}

/// The memories of the input check, and the bound on its reads.
#[derive(Debug)]
pub(crate) struct InputTrees {
    lru: Mutex<Lru>,
    /// Directories being checked now, for any request, by the loss count the checking
    /// request began at.
    flights: Mutex<HashMap<(Digest, u64), Flight>>,
    reads: Semaphore,
}

impl InputTrees {
    pub(crate) fn new(config: InputTreeConfig) -> Self {
        Self {
            lru: Mutex::new(Lru::new(config)),
            flights: Mutex::new(HashMap::new()),
            reads: Semaphore::new(config.reads_in_flight.get()),
        }
    }

    fn lru(&self) -> std::sync::MutexGuard<'_, Lru> {
        // Map and order edits only; a poisoned LRU is whole.
        self.lru.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn flights(&self) -> std::sync::MutexGuard<'_, HashMap<(Digest, u64), Flight>> {
        self.flights.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Directories kept decoded.
    pub(crate) fn len(&self) -> usize {
        self.lru().entries.len()
    }

    /// Bytes the kept directories cost.
    pub(crate) fn bytes(&self) -> u64 {
        self.lru().bytes
    }
}

/// What checking one directory found.
#[derive(Debug)]
enum Verdict {
    /// Everything at and under it is held, until this farm time.
    Held { until: FarmTime },
    /// Something is not: directories at or under it that are absent, and files (not
    /// those under an absent directory).
    Absent {
        dirs: BTreeSet<Digest>,
        files: BTreeSet<Digest>,
    },
}

type Answer = Arc<Result<Verdict, CacheError>>;
type Flight = Shared<BoxFuture<'static, Answer>>;

/// One request's check: the farm time it started at, and the directories it has
/// already started, so a directory two paths reach is checked once.
struct Walk<M, O> {
    cache: Arc<Cache<M, O>>,
    /// Directory blobs this request's checks read from the store.
    dirs_read: AtomicU64,
    now: FarmTime,
    until: FarmTime,
    /// [`kbf_meta::MetaState::losses`] when the request began.
    losses: u64,
    memo: Mutex<HashMap<Digest, Flight>>,
}

/// Removes a directory's flight when its check ends, however it ends (a panic too).
struct Done<'a> {
    trees: &'a InputTrees,
    key: (Digest, u64),
}

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.trees.flights().remove(&self.key);
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Every blob of the input tree under `root` that is not held: directories that
    /// cannot be read (absent ones first, each sorted) and files FindMissingBlobs
    /// would report. Every blob found held is touched, as a read or FindMissingBlobs
    /// touches it, unless an earlier check verified its directory and the retention
    /// guarantee of that check still runs (see the module).
    ///
    /// # Errors
    /// A directory does not decode ([`CacheError::Invalid`]), or a read or the
    /// metadata log fails: the input exists or cannot be told, so it is not missing.
    pub async fn missing_inputs(self: &Arc<Self>, root: Digest) -> Result<Vec<Digest>, CacheError> {
        Ok(self.walk_inputs(root).await?.absent)
    }

    /// [`Cache::missing_inputs`], and how many Directory blobs the checks this call
    /// started read from the store (a check another request started and this one
    /// joined is counted for that request).
    pub async fn walk_inputs(self: &Arc<Self>, root: Digest) -> Result<Walked, CacheError> {
        let Horizon { now, until, losses } = self.retention_horizon().await?;
        let walk = Arc::new(Walk {
            cache: Arc::clone(self),
            dirs_read: AtomicU64::new(0),
            now,
            until,
            losses,
            memo: Mutex::new(HashMap::new()),
        });
        let answer = visit(&walk, root).await;
        let absent = match &*answer {
            Ok(Verdict::Held { .. }) => Vec::new(),
            Ok(Verdict::Absent { dirs, files }) => dirs.iter().chain(files).copied().collect(),
            Err(e) => return Err(e.clone()),
        };
        Ok(Walked {
            absent,
            dirs_read: walk.dirs_read.load(Ordering::Relaxed),
        })
    }
}

/// The metadata state a request reads before its first report.
pub(crate) struct Horizon {
    /// The farm time now.
    pub(crate) now: FarmTime,
    /// The farm time until which a blob reported held now stays held.
    pub(crate) until: FarmTime,
    /// [`kbf_meta::MetaState::losses`].
    pub(crate) losses: u64,
}

/// What one walk of an input tree found.
#[derive(Debug)]
pub struct Walked {
    /// What [`Cache::missing_inputs`] returns.
    pub absent: Vec<Digest>,
    /// Directory blobs read from the store.
    pub dirs_read: u64,
}

/// The check of `digest` for this request: the request's own earlier start of it, a
/// subtree verified until after this request began, another request's check in flight,
/// or a new check.
fn visit<M: MetaLog, O: ObjectStore>(walk: &Arc<Walk<M, O>>, digest: Digest) -> Flight {
    let cache = &walk.cache;
    let trees = cache.input_trees();
    let metrics = cache.metrics();
    let mut memo = walk.memo.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(flight) = memo.get(&digest) {
        return flight.clone();
    }
    let verified = trees.lru().get(&digest).and_then(|(_, verified)| verified);
    let skippable = verified.filter(|v| v.until > walk.now && v.losses == walk.losses);
    if let Some(Verified { until, .. }) = skippable {
        metrics.input_dirs_skipped.add(1);
        let flight = answered(Verdict::Held { until });
        memo.insert(digest, flight.clone());
        return flight;
    }
    let mut flights = trees.flights();
    let key = (digest, walk.losses);
    if let Some(flight) = flights.get(&key) {
        metrics.input_dirs_joined.add(1);
        memo.insert(digest, flight.clone());
        return flight.clone();
    }
    // The check runs on its own task, so it finishes, frees its read slot and leaves
    // `flights` even when every request waiting on it is dropped (a client cancel drops
    // the handler future). The flight only awaits the task.
    let owned = Arc::clone(walk);
    let task = tokio::spawn(async move {
        let _done = Done {
            trees: owned.cache.input_trees(),
            key,
        };
        Arc::new(check(&owned, digest).await)
    });
    let flight: Flight = async move {
        task.await.unwrap_or_else(|e| {
            Arc::new(Err(CacheError::Invalid(format!(
                "the check of input directory {digest} did not finish: {e}"
            ))))
        })
    }
    .boxed()
    .shared();
    flights.insert(key, flight.clone());
    memo.insert(digest, flight.clone());
    flight
}

fn answered(verdict: Verdict) -> Flight {
    futures::future::ready(Arc::new(Ok(verdict)))
        .boxed()
        .shared()
}

fn absent_dir(digest: Digest) -> Verdict {
    Verdict::Absent {
        dirs: BTreeSet::from([digest]),
        files: BTreeSet::new(),
    }
}

/// Checks one directory and, concurrently, everything under it.
async fn check<M: MetaLog, O: ObjectStore>(
    walk: &Arc<Walk<M, O>>,
    digest: Digest,
) -> Result<Verdict, CacheError> {
    let cache = &walk.cache;
    let metrics = cache.metrics();
    let kept = cache.input_trees().lru().get(&digest);
    let decoded = if let Some((decoded, _)) = kept {
        // Decoded in memory, but the blob must still be held, and the report touches it.
        if !cache.find_missing(&[digest]).await?.is_empty() {
            return Ok(absent_dir(digest));
        }
        metrics.input_dirs_cached.add(1);
        decoded
    } else {
        let bytes = {
            let _slot = cache.input_trees().reads.acquire().await;
            cache.read_blob(&digest).await
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(CacheError::NotFound(_)) => return Ok(absent_dir(digest)),
            Err(e) => return Err(e),
        };
        metrics.execute_dir_reads.add(1);
        walk.dirs_read.fetch_add(1, Ordering::Relaxed);
        let decoded = Arc::new(decode(&digest, &bytes)?);
        cache
            .input_trees()
            .lru()
            .insert(digest, Arc::clone(&decoded));
        decoded
    };
    let children: Vec<Flight> = decoded.dirs.iter().map(|c| visit(walk, *c)).collect();
    let (files, children) = join(absent_files(cache, &decoded.files), join_all(children)).await;
    let mut until = walk.until;
    let mut absent_dirs = BTreeSet::new();
    let mut absent_files: BTreeSet<Digest> = files?.into_iter().collect();
    for child in children {
        match &*child {
            Err(e) => return Err(e.clone()),
            Ok(Verdict::Held { until: held }) => until = until.min(*held),
            Ok(Verdict::Absent { dirs, files }) => {
                absent_dirs.extend(dirs);
                absent_files.extend(files);
            }
        }
    }
    if absent_dirs.is_empty() && absent_files.is_empty() {
        cache
            .input_trees()
            .lru()
            .verified(&digest, until, walk.losses);
        return Ok(Verdict::Held { until });
    }
    Ok(Verdict::Absent {
        dirs: absent_dirs,
        files: absent_files,
    })
}

/// The files of one directory that are not held; those that are, are touched.
async fn absent_files<M: MetaLog, O: ObjectStore>(
    cache: &Cache<M, O>,
    files: &[Digest],
) -> Result<Vec<Digest>, CacheError> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    cache.find_missing(files).await
}

fn decode(digest: &Digest, bytes: &[u8]) -> Result<Decoded, CacheError> {
    let invalid = |e: tonic::Status| CacheError::Invalid(e.message().to_owned());
    let directory = Directory::decode(bytes).map_err(|e| {
        CacheError::Invalid(format!("input directory {digest} does not decode: {e}"))
    })?;
    let files = directory
        .files
        .iter()
        .map(|f| wire::digest(f.digest.as_ref()))
        .collect::<Result<Box<[_]>, _>>()
        .map_err(invalid)?;
    let dirs = directory
        .directories
        .iter()
        .map(|d| wire::digest(d.digest.as_ref()))
        .collect::<Result<Box<[_]>, _>>()
        .map_err(invalid)?;
    Ok(Decoded { files, dirs })
}

#[cfg(test)]
#[path = "input_tree_tests.rs"]
mod tests;
