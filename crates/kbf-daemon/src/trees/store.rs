//! The tree store: one directory per shared tree, named by its Directory digest, built
//! once per node and pinned while a lease mounts it
//! (`docs/design/fast-sandbox.md#43-eviction-and-accounting`).
//!
//! ```text
//! <dir>/<hash>-<size>      a finished tree (its name appears by one rename)
//! <dir>/.tmp-<n>           a tree being built; removed at start
//! <dir>/.dead-<n>          an evicted tree being removed; removed at start
//! ```
//!
//! **One build per digest.** The first [`TreeStore::acquire`] of a digest builds it in
//! its own task; every other acquire of that digest waits for the builder's slot to
//! close and looks again (no file lock). A builder that fails, or is dropped, closes
//! its slot, and one waiter builds in its place.
//!
//! **Pins.** A [`TreeLease`] pins its tree. Only an unpinned tree is evicted: renamed
//! to `.dead-<n>` (a new acquire builds afresh under the digest's name) and removed in
//! the background. The budget counts every tree's file bytes, linked or not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kbf_proto::reapi::Digest;
use tokio::sync::watch;

use super::build::{build, bytes_under, publish_later, remove_tree};
use super::classify::{Candidate, tree_name};
use crate::cache::{Cache, InputStats};
use crate::cas::Fill;
use crate::tree::{Loaded, TreeError, io as io_error};

/// The mode of the store directory: owner everything, others may only traverse. The
/// OCI backend runs a rootless runtime whose container root is a subordinate id, not
/// the daemon's user, an "other" to this directory: it must search it to bind a tree
/// (0700 failed every lease with a tree: `cannot stat <tree>: Permission denied`), and
/// has no reason to list it (names are digests it was not told) or write in it (no
/// group or other write bit: trees are reached for reading only, through a read-only
/// mount). The trees' own directories are 0755, their files the cache's 0444 or 0555
/// (see `build::verify`); the directories above the store are the operator's, and must
/// be searchable too.
const STORE_MODE: u32 = 0o711;

/// What a node's tree store has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeStats {
    /// Trees in the store, and their file bytes.
    pub trees: u64,
    pub bytes: u64,
    /// Trees a lease holds (not evictable).
    pub pinned: u64,
    /// Trees built, and acquires that found the tree built (a hit) or waited for
    /// another's build.
    pub builds: u64,
    pub hits: u64,
    pub waits: u64,
    /// Trees evicted.
    pub evictions: u64,
}

#[derive(Debug)]
enum Tree {
    /// A task is building it; the receiver closes when that task is done.
    Building(watch::Receiver<()>),
    Ready {
        bytes: u64,
        /// Leases holding it; shared with each [`TreeLease`], which lets go on drop.
        pins: Arc<AtomicU32>,
        /// The store's tick at the last acquire: the LRU order.
        used: u64,
    },
}

#[derive(Debug, Default)]
struct State {
    trees: HashMap<String, Tree>,
    /// File bytes of the `Ready` trees.
    bytes: u64,
    tick: u64,
}

#[derive(Debug, Default)]
struct Counters {
    builds: AtomicU64,
    hits: AtomicU64,
    waits: AtomicU64,
    evictions: AtomicU64,
}

#[derive(Debug)]
pub(super) struct Inner {
    dir: PathBuf,
    max_bytes: u64,
    state: Mutex<State>,
    counters: Counters,
    next: AtomicU64,
}

/// The node's tree store.
#[derive(Debug)]
pub struct TreeStore {
    inner: Arc<Inner>,
}

/// A tree pinned for a lease: its path is mounted read-only. Dropping it unpins the
/// tree, and a store over budget then evicts.
#[derive(Debug)]
pub struct TreeLease {
    inner: Arc<Inner>,
    pins: Arc<AtomicU32>,
    path: PathBuf,
}

impl TreeLease {
    /// The tree's directory on the host.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TreeLease {
    fn drop(&mut self) {
        // Under the lock, so an eviction cannot see the old count and then the tree
        // pinned again.
        let mut state = self.inner.state();
        self.pins.fetch_sub(1, Ordering::SeqCst);
        let dead = self.inner.evict(&mut state);
        drop(state);
        self.inner.remove_later(dead);
    }
}

fn parses(name: &str) -> bool {
    name.split_once('-').is_some_and(|(hash, size)| {
        let digest = Digest {
            hash: hash.to_owned(),
            size_bytes: size.parse().unwrap_or(-1),
        };
        tree_name(&digest).as_deref() == Some(name)
    })
}

impl Inner {
    /// A lease on the tree `name`, whose pin count `pins` already counts it.
    fn lease(self: &Arc<Self>, name: &str, pins: Arc<AtomicU32>) -> TreeLease {
        TreeLease {
            inner: Arc::clone(self),
            path: self.dir.join(name),
            pins,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Each update leaves the map consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Renames unpinned trees, least recently used first, out of the store until the
    /// budget holds; returns their new names for [`Inner::remove_later`]. A tree that
    /// cannot be renamed stays (and the next eviction tries again).
    fn evict(&self, state: &mut State) -> Vec<PathBuf> {
        let mut dead = Vec::new();
        while state.bytes > self.max_bytes {
            let victim = state
                .trees
                .iter()
                .filter_map(|(name, tree)| match tree {
                    Tree::Ready { pins, used, bytes } if pins.load(Ordering::SeqCst) == 0 => {
                        Some((*used, name.clone(), *bytes))
                    }
                    _ => None,
                })
                .min();
            let Some((_, name, bytes)) = victim else {
                break;
            };
            let to = self.dir.join(format!(
                ".dead-{}",
                self.next.fetch_add(1, Ordering::Relaxed)
            ));
            if let Err(e) = std::fs::rename(self.dir.join(&name), &to) {
                tracing::warn!("trees: evicting {name}: {e}");
                break;
            }
            state.trees.remove(&name);
            state.bytes -= bytes;
            self.counters.evictions.fetch_add(1, Ordering::Relaxed);
            dead.push(to);
        }
        dead
    }

    /// Removes the directories `dead` on the blocking pool, or here with no runtime. A
    /// directory that cannot be removed is left for the next start's sweep.
    fn remove_later(&self, dead: Vec<PathBuf>) {
        if dead.is_empty() {
            return;
        }
        let remove = move || {
            for dir in &dead {
                if let Err(e) = remove_tree(dir) {
                    tracing::warn!("trees: removing {}: {e}", dir.display());
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}

/// Ends a build that did not finish: removes its slot and its `.tmp-*` directory.
struct Builder {
    inner: Arc<Inner>,
    name: String,
    tmp: PathBuf,
    done: bool,
    /// Closing this wakes the waiters.
    _slot: watch::Sender<()>,
}

impl Drop for Builder {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.inner.state().trees.remove(&self.name);
        self.inner.remove_later(vec![self.tmp.clone()]);
    }
}

/// What [`TreeStore::acquire`] does next.
enum Step {
    /// The tree is built: it was pinned.
    Pinned(Arc<AtomicU32>),
    /// Another acquire is building it.
    Wait(watch::Receiver<()>),
    /// Nobody is: this one builds.
    Lead(Builder),
}

impl TreeStore {
    /// Opens the store in `dir` (made, mode [`STORE_MODE`], if it is missing; set to it if it is not), blocking: removes
    /// the `.tmp-*` and `.dead-*` directories a previous run left, and adopts every
    /// tree already named (a tree is named by one rename after it was checked, so a
    /// name present means a whole tree). Anything else in `dir` is left alone.
    ///
    /// # Errors
    /// `dir` cannot be made, listed or written.
    pub fn open(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(STORE_MODE)
            .create(&dir)?;
        // Whatever the umask was, and whatever an older daemon (0700) or an operator
        // left: the mode is set, and a change is logged.
        let before = std::fs::metadata(&dir)?.permissions().mode() & 0o7777;
        if before != STORE_MODE {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(STORE_MODE))?;
            tracing::info!("trees: {dir:?} was mode {before:04o}, now {STORE_MODE:04o}");
        }
        // A container whose root is not this user needs every ancestor searchable.
        closed_ancestor(&dir).inspect(|closed| {
            tracing::warn!("trees: {closed:?} is closed to others; chmod o+x it");
        });
        let mut state = State::default();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file = entry.file_name();
            let name = file.to_string_lossy();
            if name.starts_with(".tmp-") || name.starts_with(".dead-") {
                remove_tree(&entry.path())?;
            } else if parses(&name) && entry.file_type()?.is_dir() {
                let bytes = bytes_under(&entry.path())?;
                state.bytes = state.bytes.saturating_add(bytes);
                state.trees.insert(
                    name.into_owned(),
                    Tree::Ready {
                        bytes,
                        pins: Arc::default(),
                        used: 0,
                    },
                );
            } else {
                let shown = entry.path();
                tracing::warn!("trees: {} is not a tree; left alone", shown.display());
            }
        }
        let inner = Arc::new(Inner {
            dir,
            max_bytes,
            state: Mutex::new(state),
            counters: Counters::default(),
            next: AtomicU64::new(0),
        });
        let dead = inner.evict(&mut inner.state());
        inner.remove_later(dead);
        Ok(Self { inner })
    }

    /// What the store has done.
    #[must_use]
    pub fn stats(&self) -> TreeStats {
        let state = self.inner.state();
        let c = &self.inner.counters;
        let ready = |tree: &Tree| match tree {
            Tree::Ready { pins, .. } => Some(pins.load(Ordering::SeqCst)),
            Tree::Building(_) => None,
        };
        let pins: Vec<u32> = state.trees.values().filter_map(ready).collect();
        TreeStats {
            trees: pins.len() as u64,
            bytes: state.bytes,
            pinned: pins.iter().filter(|p| **p > 0).count() as u64,
            builds: c.builds.load(Ordering::Relaxed),
            hits: c.hits.load(Ordering::Relaxed),
            waits: c.waits.load(Ordering::Relaxed),
            evictions: c.evictions.load(Ordering::Relaxed),
        }
    }

    /// Pins the tree `name` of `digest` (`name` is `tree_name(digest)`), building it
    /// first if the store has none (or waiting for the build another acquire has under
    /// way). `known` is the Directory messages the caller already read; the build reads
    /// the rest. The returned stats are those of the build this call did, if it did
    /// one.
    ///
    /// # Errors
    /// A blob is missing or corrupt, the tree breaks a naming rule, or a write fails.
    pub(super) async fn acquire(
        &self,
        cas: &dyn Fill,
        in_flight: usize,
        cache: &Cache,
        node: &Candidate,
        known: &Loaded,
    ) -> Result<(TreeLease, Option<InputStats>), TreeError> {
        let name = node.name.as_str();
        loop {
            match self.step(name) {
                Step::Pinned(pins) => return Ok((self.inner.lease(name, pins), None)),
                Step::Lead(builder) => {
                    return self.lead(cas, in_flight, cache, node, known, builder).await;
                }
                Step::Wait(mut waiting) => {
                    self.inner.counters.waits.fetch_add(1, Ordering::Relaxed);
                    // The sender is dropped when the builder is done, with a tree or
                    // without.
                    let _ = waiting.changed().await;
                    // A slot nobody builds in answers at once: let others run.
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    /// What an acquire of `name` does next. Not async, so no lock is held across an
    /// await.
    fn step(&self, name: &str) -> Step {
        let inner = &self.inner;
        let mut state = inner.state();
        state.tick += 1;
        let tick = state.tick;
        match state.trees.get_mut(name) {
            Some(Tree::Ready { pins, used, .. }) => {
                pins.fetch_add(1, Ordering::SeqCst);
                *used = tick;
                inner.counters.hits.fetch_add(1, Ordering::Relaxed);
                Step::Pinned(Arc::clone(pins))
            }
            Some(Tree::Building(slot)) => Step::Wait(slot.clone()),
            None => {
                let (tx, rx) = watch::channel(());
                state.trees.insert(name.to_owned(), Tree::Building(rx));
                drop(state);
                let tmp = inner.dir.join(format!(
                    ".tmp-{}",
                    inner.next.fetch_add(1, Ordering::Relaxed)
                ));
                Step::Lead(Builder {
                    inner: Arc::clone(inner),
                    name: name.to_owned(),
                    tmp,
                    done: false,
                    _slot: tx,
                })
            }
        }
    }

    /// Builds the tree as `builder`, names it, and pins it.
    async fn lead(
        &self,
        cas: &dyn Fill,
        in_flight: usize,
        cache: &Cache,
        node: &Candidate,
        known: &Loaded,
        mut builder: Builder,
    ) -> Result<(TreeLease, Option<InputStats>), TreeError> {
        let inner = &self.inner;
        let tmp = builder.tmp.clone();
        tokio::fs::create_dir(&tmp).await.map_err(io_error(&tmp))?;
        let (summary, stats) = build(cas, in_flight, cache, &node.digest, known, &tmp).await?;
        let to = inner.dir.join(&node.name);
        let moved = publish_later(tmp.clone(), to.clone())
            .await
            .map_err(io_error(&to))?;
        inner.counters.builds.fetch_add(1, Ordering::Relaxed);
        let pins = Arc::new(AtomicU32::new(1));
        let mut state = inner.state();
        state.tick += 1;
        let used = state.tick;
        state.bytes = state.bytes.saturating_add(summary.bytes);
        state.trees.insert(
            node.name.clone(),
            Tree::Ready {
                bytes: summary.bytes,
                pins: Arc::clone(&pins),
                used,
            },
        );
        let mut dead = inner.evict(&mut state);
        drop(state);
        builder.done = true;
        // A tree already there was renamed in whole: ours is the spare.
        dead.extend((!moved).then_some(tmp));
        inner.remove_later(dead);
        Ok((inner.lease(&node.name, pins), Some(stats)))
    }
}

/// The nearest directory from `dir` upwards that others cannot search (no `o+x`), which
/// the store does not own and so only reports. `None` if every one can be searched, or
/// if `dir` cannot be resolved.
fn closed_ancestor(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let absolute = std::fs::canonicalize(dir).ok()?;
    let closed =
        |p: &&Path| std::fs::metadata(p).is_ok_and(|meta| meta.permissions().mode() & 0o001 == 0);
    absolute
        .ancestors()
        .skip(1)
        .find(closed)
        .map(Path::to_owned)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::closed_ancestor;

    /// Catches a closed ancestor not being named (the warning that tells the operator
    /// what to chmod), the store's own directory being named (`open` makes it 0711, it
    /// is not reported), and a farther one named before the nearest.
    #[test]
    fn the_nearest_closed_ancestor_is_named() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/kbf-daemon-unit")
            .join(format!("trees-closed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("closed/inner/store");
        std::fs::create_dir_all(&store).unwrap();
        let mode = |p: &std::path::Path, m| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        };
        let (closed, inner) = (root.join("closed"), root.join("closed/inner"));
        // The store itself being closed is not reported: `open` makes it 0711.
        mode(&store, 0o700);
        mode(&inner, 0o711);
        mode(&closed, 0o700);
        let named = std::fs::canonicalize(&closed).unwrap();
        assert_eq!(closed_ancestor(&store), Some(named));
        // Two closed ancestors: the nearest is named.
        mode(&inner, 0o700);
        let nearest = std::fs::canonicalize(&inner).unwrap();
        assert_eq!(closed_ancestor(&store), Some(nearest));
        mode(&closed, 0o755);
        mode(&inner, 0o755);
        assert!(closed_ancestor(&root.join("missing")).is_none());
    }
}
