//! What the input cache's tests share: a CAS that counts its reads, input trees, a fake
//! clock, hooks that hold the cache's work at a chosen point, and looks at the cache's
//! directory from outside.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::cache::{Cache, CacheConfig, Clock, Hooks, InputStats, Key, Point};
use kbf_daemon::cas::{Cas, CasError, digest_of, label};
use kbf_daemon::tree::{TreeError, materialize_cached};
use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode};
use prost::Message;

use super::memory::MemoryCas;

/// A CAS in memory that counts the reads of each digest and the bytes it returned.
/// File blobs only are counted by [`CountingCas::file_reads`]: Directory messages are
/// read by every lease, cache or not.
#[derive(Default)]
pub struct CountingCas {
    pub blobs: MemoryCas,
    reads: Mutex<HashMap<String, u64>>,
    bytes: AtomicU64,
    directories: Mutex<BTreeSet<String>>,
    /// While set, reads of file blobs wait.
    stalled: std::sync::atomic::AtomicBool,
    unstalled: tokio::sync::Notify,
}

impl CountingCas {
    /// Makes reads of file blobs wait until [`CountingCas::unstall`].
    pub fn stall(&self) {
        self.stalled.store(true, Ordering::SeqCst);
    }

    pub fn unstall(&self) {
        self.stalled.store(false, Ordering::SeqCst);
        self.unstalled.notify_waiters();
    }

    pub fn reads_of(&self, digest: &Digest) -> u64 {
        lock(&self.reads).get(&label(digest)).copied().unwrap_or(0)
    }

    /// Reads of file blobs, all digests together.
    pub fn file_reads(&self) -> u64 {
        let directories = lock(&self.directories);
        lock(&self.reads)
            .iter()
            .filter(|(d, _)| !directories.contains(*d))
            .map(|(_, n)| n)
            .sum()
    }

    /// Reads of Directory messages, all digests together.
    pub fn directory_reads(&self) -> u64 {
        let directories = lock(&self.directories);
        lock(&self.reads)
            .iter()
            .filter(|(d, _)| directories.contains(*d))
            .map(|(_, n)| n)
            .sum()
    }

    /// Bytes of file blobs returned.
    pub fn file_bytes(&self) -> u64 {
        self.bytes.load(Ordering::SeqCst)
    }

    /// The most reads any one file blob had.
    pub fn most_reads_of_one_file(&self) -> u64 {
        let directories = lock(&self.directories);
        lock(&self.reads)
            .iter()
            .filter(|(d, _)| !directories.contains(*d))
            .map(|(_, n)| *n)
            .max()
            .unwrap_or(0)
    }

    /// Stores `tree` and returns its root.
    pub fn tree(&self, tree: &[(String, Vec<u8>, bool)]) -> Digest {
        let mut node = Node::default();
        for (path, bytes, executable) in tree {
            let (dir, leaf) = node.at(path);
            dir.files.insert(leaf, (bytes.clone(), *executable));
        }
        node.store(&mut |bytes, directory| {
            let digest = self.blobs.insert(bytes);
            if directory {
                lock(&self.directories).insert(label(&digest));
            }
            digest
        })
    }
}

impl Cas for CountingCas {
    type Lease = std::sync::Arc<Self>;

    fn lease(self: &std::sync::Arc<Self>, _: &kbf_types::LeaseId) -> std::sync::Arc<Self> {
        std::sync::Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        *lock(&self.reads).entry(label(digest)).or_default() += 1;
        let file = !lock(&self.directories).contains(&label(digest));
        while file && self.stalled.load(Ordering::SeqCst) {
            let unstalled = self.unstalled.notified();
            if !self.stalled.load(Ordering::SeqCst) {
                break;
            }
            unstalled.await;
        }
        // Yields, so reads started together are in flight together.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let bytes = self.blobs.get(digest).await?;
        if !lock(&self.directories).contains(&label(digest)) {
            self.bytes.fetch_add(bytes.len() as u64, Ordering::SeqCst);
        }
        Ok(bytes)
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.blobs.put(bytes).await
    }
}

#[derive(Default)]
struct Node {
    files: BTreeMap<String, (Vec<u8>, bool)>,
    dirs: BTreeMap<String, Node>,
}

impl Node {
    fn at(&mut self, path: &str) -> (&mut Node, String) {
        let mut parts: Vec<&str> = path.split('/').collect();
        let leaf = parts.pop().expect("a name").to_owned();
        let mut node = self;
        for part in parts {
            node = node.dirs.entry(part.to_owned()).or_default();
        }
        (node, leaf)
    }

    fn store(&self, put: &mut impl FnMut(Vec<u8>, bool) -> Digest) -> Digest {
        let mut directory = Directory::default();
        for (name, (bytes, executable)) in &self.files {
            directory.files.push(FileNode {
                name: name.clone(),
                digest: Some(put(bytes.clone(), false)),
                is_executable: *executable,
                ..FileNode::default()
            });
        }
        for (name, node) in &self.dirs {
            directory.directories.push(DirectoryNode {
                name: name.clone(),
                digest: Some(node.store(put)),
            });
        }
        put(directory.encode_to_vec(), true)
    }
}

/// A file of an input tree: its path, bytes, and whether it is executable.
pub fn file(path: &str, bytes: &[u8], executable: bool) -> (String, Vec<u8>, bool) {
    (path.to_owned(), bytes.to_vec(), executable)
}

/// A tree shaped like an unpacked toolchain: `files` files in nested directories, every
/// 50th empty, every 20th executable, all others with bytes of their own.
pub fn toolchain(files: usize) -> Vec<(String, Vec<u8>, bool)> {
    (0..files)
        .map(|i| {
            let path = format!("lib/std/d{}/e{}/f{i}.zig", i % 37, i % 11);
            let bytes = if i % 50 == 0 {
                Vec::new()
            } else {
                format!("pub const n = {i};\n").into_bytes()
            };
            (path, bytes, i % 20 == 0)
        })
        .collect()
}

/// The digest of `bytes`.
pub fn digest(bytes: &[u8]) -> Digest {
    digest_of(bytes)
}

/// A fake [`Clock`] a test sets.
#[derive(Clone)]
pub struct FakeClock(Arc<AtomicU64>);

impl FakeClock {
    pub fn at(seconds: u64) -> Self {
        Self(Arc::new(AtomicU64::new(seconds)))
    }

    pub fn advance(&self, seconds: u64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }

    pub fn clock(&self) -> Clock {
        let now = Arc::clone(&self.0);
        Arc::new(move || now.load(Ordering::SeqCst))
    }
}

/// Hooks that record every point reached and hold the work at the points a test
/// chose, until it lets them go.
#[derive(Default)]
pub struct Gates {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    /// Each hold: what it waits for, whether it was reached, whether it was let go.
    holds: Vec<Hold>,
    reached: Vec<String>,
}

struct Hold {
    matches: Box<dyn Fn(&Point<'_>) -> bool + Send>,
    reached: bool,
    released: bool,
}

/// One hold, by its place in [`Gates`].
#[derive(Clone, Copy, Debug)]
pub struct HoldId(usize);

impl Gates {
    /// Gates that hold the cache's background thread before its first adopt pass
    /// ([`Point::PassStarts`]) until the cache is stopped: a pass running beside a test
    /// would adopt or drop what the test put on disk, on its own time.
    pub fn new() -> Arc<Self> {
        let gates = Self::running();
        gates.hold(|p| *p == Point::PassStarts);
        gates
    }

    /// Gates with no hold: the background thread runs as in a daemon.
    pub fn running() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Holds the first point `matches` accepts until [`Gates::release`].
    pub fn hold(&self, matches: impl Fn(&Point<'_>) -> bool + Send + 'static) -> HoldId {
        let mut state = lock(&self.state);
        state.holds.push(Hold {
            matches: Box::new(matches),
            reached: false,
            released: false,
        });
        HoldId(state.holds.len() - 1)
    }

    /// Waits, at most 30 seconds, until the work reached the hold.
    pub fn wait_reached(&self, id: HoldId) {
        let state = lock(&self.state);
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(30), |s| !s.holds[id.0].reached)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            !timeout.timed_out(),
            "hold {id:?} not reached; points: {:?}",
            state.reached
        );
    }

    pub fn release(&self, id: HoldId) {
        lock(&self.state).holds[id.0].released = true;
        self.changed.notify_all();
    }

    /// Whether the hold was reached.
    pub fn reached(&self, id: HoldId) -> bool {
        lock(&self.state).holds[id.0].reached
    }

    /// Every point reached, in order, as `Debug` text.
    pub fn points(&self) -> Vec<String> {
        lock(&self.state).reached.clone()
    }
}

impl Hooks for Gates {
    fn at(&self, point: Point<'_>) {
        let mut state = lock(&self.state);
        state.reached.push(format!("{point:?}"));
        let Some(at) = state
            .holds
            .iter()
            .position(|h| !h.reached && (h.matches)(&point))
        else {
            return;
        };
        state.holds[at].reached = true;
        self.changed.notify_all();
        // At most a minute: a test that failed with a hold still held must not hang
        // (its runtime waits for the blocking thread the hold is on).
        let _held = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(60), |s| !s.holds[at].released)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// A test's cache configuration: the cache in `dir/cache`, leases under `dir/leases`,
/// `max_bytes`, no pause in a pass, snapshots, passes and room checks far apart,
/// `hooks`, a fake clock at `clock`.
pub fn config(dir: &Path, max_bytes: u64, hooks: Arc<Gates>, clock: &FakeClock) -> CacheConfig {
    let cache = dir.join("cache");
    std::fs::create_dir_all(&cache).expect("cache dir");
    std::fs::create_dir_all(dir.join("leases")).expect("leases dir");
    let mut config = CacheConfig::new(cache, dir.join("leases"), max_bytes);
    config.pass_pace = Duration::ZERO;
    config.snapshot_every = Duration::from_secs(3600);
    config.pass_every = Duration::from_secs(3600);
    config.room_every = Duration::from_secs(3600);
    config.hooks = hooks;
    config.clock = clock.clock();
    config
}

/// Stops `cache` while its background thread may be held by `gates` (the stop runs on
/// a thread of its own, and every hold is let go), then drops it: its lock is released
/// once the last handle is gone, as when a daemon exits.
pub fn stop(cache: Arc<Cache>, gates: &Gates) {
    let stopping = Arc::clone(&cache);
    let stopper = std::thread::spawn(move || stopping.stop());
    // The stop marks the cache stopped first; holds let go after that see it.
    std::thread::sleep(Duration::from_millis(50));
    while !stopper.is_finished() {
        let count = lock(&gates.state).holds.len();
        for id in 0..count {
            gates.release(HoldId(id));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    stopper.join().expect("stopped");
    assert_eq!(
        Arc::strong_count(&cache),
        1,
        "a handle of the stopped cache is left"
    );
}

/// A fresh lease directory under `dir/leases`.
pub fn lease_dir(dir: &Path, name: &str) -> PathBuf {
    let lease = dir.join("leases").join(name);
    std::fs::create_dir_all(&lease).expect("lease dir");
    lease
}

/// Writes the tree `root` into a fresh lease directory `name` through `cache`.
pub async fn lease(
    cas: &CountingCas,
    cache: &Cache,
    root: &Digest,
    dir: &Path,
    name: &str,
) -> Result<(PathBuf, InputStats), TreeError> {
    let lease = lease_dir(dir, name);
    let stats = materialize_cached(cas, cache, root, &lease, &[]).await?;
    Ok((lease, stats))
}

/// The bytes of every file under `v1/blobs` of the cache in `dir/cache`: the on-disk sum.
pub fn on_disk_sum(dir: &Path) -> u64 {
    entries(dir).values().map(|(size, _)| size).sum()
}

/// Every file under `v1/blobs`, by name: its size and inode.
pub fn entries(dir: &Path) -> BTreeMap<String, (u64, u64)> {
    let blobs = dir.join("cache/v1/blobs");
    let mut found = BTreeMap::new();
    for prefix in std::fs::read_dir(&blobs).expect("blobs") {
        for entry in std::fs::read_dir(prefix.expect("prefix").path()).expect("prefix dir") {
            let entry = entry.expect("entry");
            let meta = entry.metadata().expect("meta");
            found.insert(
                entry.file_name().to_string_lossy().into_owned(),
                (meta.size(), meta.ino()),
            );
        }
    }
    found
}

/// The key of `bytes`, plain or executable.
pub fn key(bytes: &[u8], executable: bool) -> Key {
    Key::of(&digest_of(bytes), executable).expect("a key")
}

/// The path of `key`'s entry in the cache in `dir/cache`.
pub fn entry_path(dir: &Path, key: &Key) -> PathBuf {
    dir.join("cache/v1/blobs")
        .join(key.prefix())
        .join(key.name())
}

/// The files in `v1/tmp` of the cache in `dir/cache`.
pub fn tmp_files(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("cache/v1/tmp"))
        .expect("tmp")
        .count()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
