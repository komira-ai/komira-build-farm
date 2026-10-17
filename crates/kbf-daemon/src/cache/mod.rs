//! The worker's input cache (`docs/design/worker-cache.md#6-the-cache`): a
//! content-addressed directory of read-only files on the node's disk, which the
//! container driver hard-links into each lease's input root (the native driver on
//! macOS clones them, [`LinkMode::Clone`]), kept across restarts.
//!
//! - **One owner.** [`Cache::open`] takes an exclusive `flock` on `<dir>/lock` before
//!   it touches anything else; a second daemon is refused ([`Disabled::Locked`]) and
//!   runs uncached. The lock file is never unlinked.
//! - **Inserts** are fetched and hashed into `tmp/` (a file there is removed whoever
//!   drops it before the rename: a failed fill, a lease killed during its fetch), made read-only (0444, 0555 for an
//!   executable) with the fixed mtime [`FIXED_MTIME`], and renamed into `blobs/` with
//!   `RENAME_NOREPLACE` (`RENAME_EXCL` on macOS; `fill`). Nothing under `blobs/` is ever opened for writing.
//! - **The index** of keys, sizes and last uses is in memory and saved whole to
//!   `index.snap` (`index`): right after load, every [`CacheConfig::snapshot_every`],
//!   after each adopt pass, and at [`Cache::stop`]. A bad snapshot is discarded and
//!   costs LRU order only: the adopt pass (`pass`) and the fills take the files back.
//! - **Every key's slot** in the fill map serialises what changes that key's name and
//!   index entry: fills, the evictor (`evict`) and the adopt pass.
//! - **Every entry is re-hashed** the first time a lease uses it after a start, and
//!   stat-checked on every hit (`verify`).
//!
//! - **One cache with local storage** ([`CacheConfig::local`]): on a node whose local
//!   storage shares the disk (`all-in-one`), this directory and index are the node's
//!   only local copies of blobs (`kbf_storage::local_cache`), and the budget counts
//!   local storage's bytes beside the entries: eviction makes room for both, after
//!   each insert and, since local storage grows without the cache's knowing, at least
//!   every [`CacheConfig::room_every`] on the background thread.
//!
//! The cache never fails a lease that would run without it: an I/O error in the cache
//! leaves the lease to write the blob itself, as without a cache.

#[cfg(target_os = "macos")]
mod clone;
mod evict;
mod fill;
mod index;
mod layout;
pub(crate) mod lease;
mod pass;
mod verify;

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub use fill::InputStats;
pub use index::Discarded;
pub use kbf_storage::local_cache::LocalCache;
pub use layout::{FIXED_MTIME, Key};

use crate::cas::CasError;
use index::Index;

/// Unix seconds now: the clock the cache dates last uses with. Tests give a fake one.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The system clock as a [`Clock`].
#[must_use]
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    })
}

/// A place in the cache's work where a test may look, or hold the work until it lets
/// it go. Every point is reached on a blocking thread, never on an async one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point<'a> {
    /// A fill's temporary file is written, read-only and dated, and not yet renamed
    /// into `blobs/`.
    Filled(&'a Key),
    /// The evictor took the key's slot and removed it from the index; its name is not
    /// unlinked yet.
    Evicting(&'a Key),
    /// The background thread wrote the snapshot of what it loaded and drained the
    /// index to the budget; the first adopt pass is next.
    PassStarts,
    /// The adopt pass listed the directory `blobs/<prefix>`.
    Listed(u8),
    /// The adopt pass compared the key with the index and is about to take its slot.
    Compared(&'a Key),
    /// A snapshot's temporary file holds the first half of its bytes.
    SnapshotHalf,
}

/// Where a test watches the cache's work: called at each [`Point`].
pub trait Hooks: Send + Sync {
    fn at(&self, point: Point<'_>);
}

/// How the cache places an entry at a lease's name: a hard link (`std::fs::hard_link`)
/// on Linux, a `clonefile` on macOS ([`LinkMode::Clone`]). A test gives one that fails
/// as a full link count or another filesystem would, or (in clone mode) one that copies.
pub type LinkFn = dyn Fn(&Path, &Path) -> std::io::Result<()> + Send + Sync;

/// The hooks of a daemon: none.
#[derive(Debug)]
pub struct NoHooks;

impl Hooks for NoHooks {
    fn at(&self, _: Point<'_>) {}
}

/// How a cache is opened and run.
#[derive(Clone)]
pub struct CacheConfig {
    /// `<--data-dir>/cache`: the directory the cache owns. It must exist.
    pub dir: PathBuf,
    /// The lease scratch directory, whose filesystem the cache should share.
    pub scratch: PathBuf,
    /// The most bytes of entries kept (sized from the data directory's filesystem).
    pub max_bytes: u64,
    /// Below this much free space on the filesystem,
    /// entries no lease links are evicted. 0 is no floor.
    pub min_free_bytes: u64,
    /// How many files of a lease are linked or filled at
    /// once.
    pub link_concurrency: usize,
    /// How often the index is saved.
    pub snapshot_every: Duration,
    /// How often an adopt pass runs after the first, which runs at start.
    pub pass_every: Duration,
    /// The pause between two directories of a pass, lifted under free-space pressure.
    pub pass_pace: Duration,
    pub clock: Clock,
    pub hooks: Arc<dyn Hooks>,
    /// Places a lease's name from an entry (a hard link, or a clone in
    /// [`LinkMode::Clone`]); in hard-link mode also links one name of the lease to
    /// another.
    pub link: Arc<LinkFn>,
    /// How leases get cached files; `None` asks the platform ([`LinkMode::Clone`] on
    /// macOS, [`LinkMode::Hardlink`] elsewhere). A cache on another filesystem than the
    /// scratch directory is [`LinkMode::Copy`] whatever this says.
    pub link_mode: Option<LinkMode>,
    /// The most links an entry may have; `None` asks the filesystem
    /// (`pathconf(_PC_LINK_MAX)`).
    pub link_max: Option<u64>,
    /// The node's one cache, when its local storage shares this disk (`all-in-one`):
    /// the cache reports its entries' bytes to it, and its budget counts local
    /// storage's bytes too ([`CacheConfig::max_bytes`] is then what both may hold).
    /// `None` elsewhere: the budget is the entries'.
    pub local: Option<Arc<LocalCache>>,
    /// With [`CacheConfig::local`], how often the background thread looks whether
    /// local storage grew past what the budget leaves it, and evicts if so: local
    /// storage grows without the cache's knowing (uploads), so the evictions after each
    /// insert are not enough on a node that only uploads.
    pub room_every: Duration,
}

impl CacheConfig {
    /// A cache in `dir` for leases under `scratch`, at most `max_bytes`: no free-space
    /// floor, 64 links at once, a snapshot every 5 minutes, a pass a day at one
    /// directory a second, room for local storage made within a second, the system
    /// clock, no hooks.
    #[must_use]
    pub fn new(dir: PathBuf, scratch: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            scratch,
            max_bytes,
            min_free_bytes: 0,
            link_concurrency: 64,
            snapshot_every: Duration::from_secs(300),
            pass_every: Duration::from_secs(86_400),
            pass_pace: Duration::from_secs(1),
            clock: system_clock(),
            hooks: Arc::new(NoHooks),
            link: Arc::new(place_link),
            link_mode: None,
            link_max: None,
            local: None,
            room_every: Duration::from_secs(1),
        }
    }
}

/// What a second input cache on one node's one cache warns: the first is the one counted.
const UNCOUNTED: &str =
    "cache: the node's one cache has another input cache; this one is not counted";

/// How leases get cached files unless a test says otherwise.
#[cfg(target_os = "macos")]
const PLATFORM_LINK_MODE: LinkMode = LinkMode::Clone;
#[cfg(not(target_os = "macos"))]
const PLATFORM_LINK_MODE: LinkMode = LinkMode::Hardlink;

/// The platform's way to place an entry at a lease's name: a clone on macOS (APFS
/// copy-on-write: the new file is its own inode, so a write to it never reaches the
/// entry), a hard link elsewhere.
#[cfg(target_os = "macos")]
fn place_link(from: &Path, to: &Path) -> std::io::Result<()> {
    clone::clone_file(from, to)
}

#[cfg(not(target_os = "macos"))]
fn place_link(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::hard_link(from, to)
}

/// Why the daemon runs uncached (`cache_disabled_reason`). The daemon warns and serves
/// either way.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Disabled {
    /// Another daemon holds the directory's lock.
    #[error("locked: another daemon owns the cache directory")]
    Locked,
    /// The directory is missing, cannot be written, or holds no usable layout.
    #[error("unusable: {0}")]
    Unusable(String),
    /// The directory's name is one a lease sweep removes.
    #[error(
        "lease-name: a cache directory named kbf-lease-* or lease-* is removed by lease sweeps"
    )]
    LeaseName,
    /// The data directory's filesystem has no room for a cache once it keeps its floor
    /// free.
    #[error("no-budget: the data directory's filesystem is too small for a cache")]
    NoBudget,
    /// The Linux native driver: actions run as the cache's owner and could write it.
    #[error("native-same-uid: native actions run as the daemon's user and could write the cache")]
    NativeSameUid,
}

impl Disabled {
    /// The word reported as `cache_disabled_reason`.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::Unusable(_) => "unusable",
            Self::LeaseName => "lease-name",
            Self::NoBudget => "no-budget",
            Self::NativeSameUid => "native-same-uid",
        }
    }
}

/// How a lease gets a cached file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkMode {
    /// A hard link to the entry.
    Hardlink,
    /// A copy-on-write clone of the entry (`clonefile` on macOS): every name of the
    /// lease is its own inode, writable as an input is without a cache, and costs no
    /// bytes until it is written. Never a link, so an action that writes an input
    /// cannot change the entry.
    Clone,
    /// A copy of the entry: the cache is on another filesystem than the scratch
    /// directory. It still saves the network fetch.
    Copy,
}

impl std::fmt::Display for LinkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hardlink => "hardlink",
            Self::Clone => "clone",
            Self::Copy => "copy",
        })
    }
}

/// The node's cache counters (section 7 of the design, the ones C1 keeps).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Bytes and entries in the index.
    pub bytes: u64,
    /// Local storage's bytes the budget counts beside the entries (one cache,
    /// [`CacheConfig::local`]); 0 without one.
    pub storage_bytes: u64,
    pub entries: u64,
    pub evictions: u64,
    /// Entries that failed their lazy re-hash or their stat check.
    pub verify_failures: u64,
    /// Free-space rounds that ran out of entries no lease links.
    pub floor_unreachable: u64,
    pub adopted_entries: u64,
    pub dropped_entries: u64,
    /// Files under `blobs/` that are not entries (a name that is not a key, a
    /// directory, a size that is not the key's); never counted, never removed.
    pub foreign_files: u64,
    /// What the last completed pass added plus what it dropped, in bytes.
    pub index_drift_bytes: u64,
    /// Snapshots written, and snapshot writes that failed.
    pub snapshots: u64,
    pub snapshot_failures: u64,
    /// Why the snapshot at start was not loaded, if it was not.
    pub index_discarded: Option<Discarded>,
    pub index_load_ms: u64,
}

#[derive(Debug, Default)]
struct Counters {
    evictions: AtomicU64,
    verify_failures: AtomicU64,
    floor_unreachable: AtomicU64,
    adopted: AtomicU64,
    dropped: AtomicU64,
    foreign: AtomicU64,
    drift: AtomicU64,
    snapshots: AtomicU64,
    snapshot_failures: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// A key's slot, held by a fill, the evictor or the adopt pass: what its waiters learn
/// when it is released (a fill's error, if it failed). Whoever held it, a waiter then
/// looks at the index again, and the pass skips a key whose slot is held: the holder
/// leaves the key's name and index entry in agreement.
#[derive(Debug)]
struct Slot {
    done: tokio::sync::watch::Receiver<Option<Option<CasError>>>,
}

/// What the cache's mutex guards: the index, the slots, the pins, and the keys fills
/// left unindexed.
#[derive(Debug, Default)]
struct State {
    index: Index,
    slots: HashMap<Key, Slot>,
    pins: HashMap<Key, u32>,
    unindexed: BTreeSet<Key>,
    /// Set while free space is under the floor and no entry is left to evict: the
    /// adopt pass then runs without its pause.
    pressure: bool,
}

/// The cache as one daemon owns it.
pub struct Cache {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("dir", &self.inner.config.dir)
            .finish_non_exhaustive()
    }
}

struct Inner {
    config: CacheConfig,
    v1: PathBuf,
    blobs: PathBuf,
    tmp: PathBuf,
    /// Holds the exclusive flock while the cache is open.
    _lock: File,
    link_mode: LinkMode,
    link_max: u64,
    uid: u32,
    state: Mutex<State>,
    counters: Counters,
    /// Names the temporary files of fills.
    next_tmp: AtomicU64,
    /// Local storage alone fills the budget (warned once until it no longer does).
    storage_over: std::sync::atomic::AtomicBool,
    /// Held while a snapshot is written: one writer of `index.snap.tmp` at a time.
    snapshotting: Mutex<()>,
    index_discarded: Option<Discarded>,
    index_load_ms: u64,
    /// Set by [`Cache::stop`]; the background thread waits on it.
    stopped: (Mutex<bool>, Condvar),
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Cache {
    /// Opens the cache in `config.dir`, blocking: takes the lock, clears what a
    /// previous run left in `tmp/`, loads the snapshot (a bad one is discarded with a
    /// warning), then starts the background thread, which saves a snapshot at once,
    /// evicts down to the budget, and runs the first adopt pass. The cache serves from
    /// the moment this returns.
    ///
    /// # Errors
    /// Why the cache cannot be used; the daemon then runs uncached.
    pub fn open(config: CacheConfig) -> Result<Arc<Self>, Disabled> {
        if config
            .dir
            .file_name()
            .is_some_and(|name| layout::swept_name(Path::new(name)))
        {
            return Err(Disabled::LeaseName);
        }
        let opened = layout::open(&config.dir, &config.scratch, (config.clock)())?;
        let started = Instant::now();
        let (index, discarded) = match std::fs::read(opened.v1.join(index::SNAPSHOT)) {
            Ok(bytes) => match index::decode(&bytes) {
                Ok(records) => (Index::loaded(&records), None),
                Err(why) => (Index::default(), Some(why)),
            },
            Err(_) => (Index::default(), Some(Discarded::Missing)),
        };
        if let Some(why) = discarded.map(Discarded::word) {
            tracing::warn!("cache: index snapshot discarded ({why}); starting empty");
        }
        let link_mode = if opened.same_filesystem {
            config.link_mode.unwrap_or(PLATFORM_LINK_MODE)
        } else {
            let dir = config.dir.display();
            tracing::warn!("cache: {dir} is not on the scratch filesystem; leases get copies");
            LinkMode::Copy
        };
        let inner = Arc::new(Inner {
            link_max: config
                .link_max
                .unwrap_or_else(|| layout::link_max(&opened.blobs)),
            uid: rustix::process::geteuid().as_raw(),
            v1: opened.v1,
            blobs: opened.blobs,
            tmp: opened.tmp,
            _lock: opened.lock,
            link_mode,
            state: Mutex::new(State {
                index,
                ..State::default()
            }),
            counters: Counters::default(),
            next_tmp: AtomicU64::new(0),
            storage_over: std::sync::atomic::AtomicBool::new(false),
            snapshotting: Mutex::new(()),
            index_discarded: discarded,
            index_load_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            stopped: (Mutex::new(false), Condvar::new()),
            thread: Mutex::new(None),
            config,
        });
        if let Some(local) = &inner.config.local {
            let entries = Arc::downgrade(&inner);
            let read = move || {
                entries
                    .upgrade()
                    .map_or(0, |inner| inner.state().index.total())
            };
            if !local.attach_inputs(Box::new(read)) {
                tracing::warn!("{UNCOUNTED}");
            }
        }
        let background = Arc::clone(&inner);
        // Without its thread (the system refused one) the cache still serves; the
        // timed snapshots, the drain at start and the passes are what is lost.
        let thread = std::thread::Builder::new()
            .name("kbf-cache".to_owned())
            .spawn(move || background.background());
        *lock(&inner.thread) = thread.ok();
        Ok(Arc::new(Self { inner }))
    }

    /// The directory the cache owns (`<--data-dir>/cache`).
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.inner.config.dir
    }

    /// How leases get cached files.
    #[must_use]
    pub fn link_mode(&self) -> LinkMode {
        self.inner.link_mode
    }

    /// The most hard links an entry may get before leases get copies instead.
    #[must_use]
    pub fn link_max(&self) -> u64 {
        self.inner.link_max
    }

    /// How many files of a lease are linked or filled at once.
    #[must_use]
    pub fn link_concurrency(&self) -> usize {
        self.inner.config.link_concurrency.max(1)
    }

    /// The node's counters now.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let inner = &*self.inner;
        let state = inner.state();
        let entries = state.index.len() as u64;
        let c = &inner.counters;
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let bytes = state.index.total();
        drop(state);
        CacheStats {
            bytes,
            storage_bytes: inner.storage_bytes(),
            entries,
            evictions: get(&c.evictions),
            verify_failures: get(&c.verify_failures),
            floor_unreachable: get(&c.floor_unreachable),
            adopted_entries: get(&c.adopted),
            dropped_entries: get(&c.dropped),
            foreign_files: get(&c.foreign),
            index_drift_bytes: get(&c.drift),
            snapshots: get(&c.snapshots),
            snapshot_failures: get(&c.snapshot_failures),
            index_discarded: inner.index_discarded,
            index_load_ms: inner.index_load_ms,
        }
    }

    /// Saves the index now (the background thread's work, also a test's).
    ///
    /// # Errors
    /// The snapshot could not be written; the previous one is kept.
    pub fn write_snapshot(&self) -> std::io::Result<()> {
        self.inner.write_snapshot()
    }

    /// Runs one adopt pass now, then an eviction round and a snapshot, as the
    /// background thread does after each (a test's way to run one).
    pub fn adopt_pass(&self) {
        if self.inner.pass() {
            self.inner.evict();
            let _ = self.inner.write_snapshot();
        }
    }

    /// Evicts down to the budget and the free-space floor now.
    pub fn evict(&self) {
        self.inner.evict();
    }

    /// A clean stop: ends the background thread (after the pass step it is in) and
    /// saves the index; once only. Dropping the last handle stops the cache too, so a
    /// daemon that exits saves its index. Leases may still read entries they hold
    /// links to.
    pub fn stop(&self) {
        let (stopped, wake) = &self.inner.stopped;
        if std::mem::replace(&mut *lock(stopped), true) {
            return;
        }
        wake.notify_all();
        let thread = lock(&self.inner.thread).take();
        drop(thread.map(std::thread::JoinHandle::join));
        let _ = self.inner.write_snapshot();
    }

    /// Pins `keys` (each once per call) until the returned guard is dropped: the
    /// evictor skips a pinned key.
    #[must_use]
    pub fn pin(&self, keys: Vec<Key>) -> Pins {
        let mut state = self.inner.state();
        for key in &keys {
            *state.pins.entry(*key).or_default() += 1;
        }
        drop(state);
        Pins {
            inner: Arc::clone(&self.inner),
            keys,
        }
    }

    /// Whether `key` is in the index, for tests.
    #[must_use]
    pub fn holds(&self, key: &Key) -> bool {
        self.inner.state().index.get(key).is_some()
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Keys pinned by one lease.
pub struct Pins {
    inner: Arc<Inner>,
    keys: Vec<Key>,
}

impl Drop for Pins {
    fn drop(&mut self) {
        let mut state = self.inner.state();
        for key in &self.keys {
            let left = state.pins.get_mut(key).map(|count| {
                *count -= 1;
                *count
            });
            if left == Some(0) {
                state.pins.remove(key);
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every update under these locks leaves them consistent, so a panic elsewhere
    // does not make them unusable.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn now(&self) -> u64 {
        (self.config.clock)()
    }

    /// Local storage's bytes on this disk, which the budget counts ([`CacheConfig::local`]).
    fn storage_bytes(&self) -> u64 {
        self.config
            .local
            .as_ref()
            .map_or(0, |local| local.storage_bytes())
    }

    /// A new name in `tmp/` for a fill, removed when dropped.
    fn next_tmp(&self) -> fill::TmpFile {
        let n = self.next_tmp.fetch_add(1, Ordering::Relaxed);
        fill::TmpFile::new(self.tmp.join(n.to_string()))
    }

    fn path_of(&self, key: &Key) -> PathBuf {
        self.blobs.join(key.prefix()).join(key.name())
    }

    fn write_snapshot(&self) -> std::io::Result<()> {
        let _writing = lock(&self.snapshotting);
        // Copied under the state's lock, written without it.
        let records = self.state().index.records();
        let written = index::write(&self.v1, &records, &*self.config.hooks);
        match &written {
            Ok(()) => bump(&self.counters.snapshots),
            Err(e) => {
                bump(&self.counters.snapshot_failures);
                tracing::warn!("cache: index snapshot not written ({e}); the previous one is kept");
            }
        }
        written
    }

    /// The background thread: a snapshot of what was loaded, the drain to the budget,
    /// the first pass, then snapshots and passes on their intervals until stopped. A
    /// completed pass is followed by an eviction round (what it adopted counts) and a
    /// snapshot.
    fn background(&self) {
        let _ = self.write_snapshot();
        self.evict();
        self.config.hooks.at(Point::PassStarts);
        let mut next_snapshot = Instant::now() + self.config.snapshot_every;
        let mut next_pass = Instant::now();
        loop {
            if Instant::now() >= next_pass {
                if !self.pass() {
                    return;
                }
                self.evict();
                let _ = self.write_snapshot();
                next_pass = Instant::now() + self.config.pass_every;
            }
            if Instant::now() >= next_snapshot {
                let _ = self.write_snapshot();
                next_snapshot = Instant::now() + self.config.snapshot_every;
            }
            let wait = next_pass
                .min(next_snapshot)
                .saturating_duration_since(Instant::now());
            if self.wait_stopped(wait) {
                return;
            }
        }
    }

    /// Waits up to `wait`; whether the cache was stopped. With one cache
    /// ([`CacheConfig::local`]), every [`CacheConfig::room_every`] of the wait it evicts
    /// if the entries and local storage's bytes are over the budget.
    fn wait_stopped(&self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let Some(local) = &self.config.local else {
                return self.wait_for_stop(left);
            };
            if self.wait_for_stop(left.min(self.config.room_every)) {
                return true;
            }
            if left.is_zero() {
                return false;
            }
            // Local storage's bytes read before the state's lock is taken, as `evict` does.
            let budget = self.budget_of(local.storage_bytes());
            if self.state().index.total() > budget {
                self.evict();
            }
        }
    }

    /// Waits up to `wait` for a stop; whether the cache was stopped.
    fn wait_for_stop(&self, wait: Duration) -> bool {
        let (stopped, wake) = &self.stopped;
        let guard = lock(stopped);
        let (guard, _) = wake
            .wait_timeout_while(guard, wait, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }

    fn stopping(&self) -> bool {
        *lock(&self.stopped.0)
    }
}

#[cfg(test)]
mod tests;
