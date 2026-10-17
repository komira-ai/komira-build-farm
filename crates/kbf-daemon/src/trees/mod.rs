//! Shared read-only input trees (`docs/design/fast-sandbox.md#4-shared-trees`).
//!
//! An action's input root is mostly one toolchain, thousands of files that are the same
//! for every action that uses it. Linking each file into each action's input root costs
//! the same time per file per action. Here a large Directory node of the input root is
//! built once per node, in the [`TreeStore`], and each action that has it gets the
//! tree's directory as a [`SharedMount`]: the driver mounts it read-only at the node's
//! path, and the action's own input root holds an empty directory there. What is left
//! (the skeleton above the trees and the small files beside them) is written through
//! the node's input cache as before.
//!
//! What a tree must not be: a place the action writes. A node qualifies only if it is
//! large, fits the store's budget, holds no output path and not the working directory,
//! sits under no output directory, and is the highest such node on its path (see
//! `classify`). A write into a mounted tree fails (`EROFS` from the read-only mount,
//! and `EACCES` for a file: the tree's files are the cache's, owned by the daemon's
//! user and not writable). A `rename` or `link` across the mount's boundary fails
//! `EXDEV`: a tool that hard-links an input out of a tree without a copy fallback fails.
//! An action that writes into a toolchain directory now fails where it used to succeed;
//! a node without a tree store keeps the old behaviour.
//!
//! **macOS** has no bind or overlay mounts ([`Delivery::Clone`]): the store's trees are
//! built from clones of the cache's entries (read-only, as the cache's own), and an
//! action's copy of a tree is ONE `clonefile` of the tree's directory into its input
//! root. The clone is the lease's: copy-on-write, its own inodes, moded like the other
//! inputs (files 0644 and 0755), so an action may write it and the tree stays as it
//! was; the store lies outside every lease directory, where the sandbox lets nothing
//! be written. A filesystem that refuses the clone gets the tree's files one by one.

mod build;
mod classify;
mod place;
mod store;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use kbf_proto::reapi::Digest;

use crate::cache::{Cache, InputStats};
use crate::cas::{Cas, Fetch, Fill, label};
use crate::tree::{
    Copies, CountedReads, Loaded, TreeError, WritePhases, io as io_error, make_directories, millis,
};
use classify::{Limits, Reserved, Summaries, load, select, summarize};
pub use place::{CloneDirFn, Delivery};
pub use store::{TreeLease, TreeStats, TreeStore};

/// The default for [`TreeConfig::min_files`].
pub const MIN_FILES: u64 = 256;
/// The default for [`TreeConfig::max_mounts`].
pub const MAX_MOUNTS: usize = 8;

/// How the tree store is opened and used (`--tree-*`).
#[derive(Clone, Debug)]
pub struct TreeConfig {
    /// `<--data-dir>/trees`: the store's directory, made if missing. It should be on the
    /// input cache's filesystem (otherwise the trees' files are copies, not links).
    pub dir: PathBuf,
    /// The most file bytes of trees kept; unpinned trees are
    /// evicted, least recently used first, past it. A node larger than this is never a
    /// tree.
    pub max_bytes: u64,
    /// The fewest files and symlinks a node holds to be a tree.
    pub min_files: u64,
    /// The most trees one action mounts (or clones).
    pub max_mounts: usize,
    /// How a tree reaches an action: [`Delivery::platform`], unless a test says
    /// otherwise.
    pub delivery: Delivery,
}

impl TreeConfig {
    /// A store in `dir` of at most `max_bytes`, with [`MIN_FILES`] and [`MAX_MOUNTS`].
    #[must_use]
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes,
            min_files: MIN_FILES,
            max_mounts: MAX_MOUNTS,
            delivery: Delivery::platform(),
        }
    }
}

/// A host directory a driver mounts read-only into the action's input root. The
/// driver's other half of this interface: the container driver takes these from
/// [`Inputs::mounts`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedMount {
    /// Where it appears, from the input root: `/`-separated, no leading slash, never
    /// empty, never containing `:` or `,`. The input root has an empty directory there.
    pub path: String,
    /// The tree on the host. Read-only to the action; the driver must not mount it
    /// writable. Must outlive the action: held by [`Inputs::trees`].
    pub source: PathBuf,
}

/// An action's input root written with shared trees.
#[derive(Debug)]
pub struct Inputs {
    /// What the files written through the cache cost (those outside the trees).
    pub stats: InputStats,
    /// The trees to mount, by path (none with [`Delivery::Clone`]).
    pub mounts: Vec<SharedMount>,
    /// The trees' pins: hold them until the action's container is gone (none with
    /// [`Delivery::Clone`]: a clone does not need the tree once it is made).
    pub trees: Vec<TreeLease>,
    /// Trees cloned whole into the input root ([`Delivery::Clone`]); a tree the
    /// filesystem refused to clone is not counted, its files are in `stats`.
    pub trees_cloned: u64,
    /// Files the mounted trees hold, which this action did not write.
    pub tree_files: u64,
    /// Trees this call built (the rest were there, or another call built them).
    pub trees_built: u64,
    /// Where the time went. `dir_reads` counts the classifier's reads and the walk's;
    /// a tree's build reads none (it uses the messages the classifier read), and
    /// `trees_ms` holds its time.
    pub phases: WritePhases,
}

/// The node's tree store with the classifier's settings.
#[derive(Debug)]
pub struct Trees {
    store: TreeStore,
    summaries: Summaries,
    limits: Limits,
    delivery: Delivery,
    /// Whether a refused clone has been logged (once per node).
    refusal_logged: AtomicBool,
}

impl Trees {
    /// Opens the store (blocking: see [`TreeStore::open`]).
    ///
    /// # Errors
    /// The store's directory cannot be used.
    pub fn open(config: TreeConfig) -> std::io::Result<Self> {
        Ok(Self {
            store: TreeStore::open(config.dir, config.max_bytes)?,
            summaries: Summaries::default(),
            limits: Limits {
                min_files: config.min_files,
                max_bytes: config.max_bytes,
                max_mounts: config.max_mounts,
            },
            delivery: config.delivery,
            refusal_logged: AtomicBool::new(false),
        })
    }

    /// The store.
    #[must_use]
    pub fn store(&self) -> &TreeStore {
        &self.store
    }

    /// Writes the tree under `root` into the empty directory `dir`: the large nodes as
    /// [`SharedMount`]s (each an empty directory in `dir`), everything else through
    /// `cache` as `materialize_cached` does. `outputs` are the output paths as seen
    /// from the input root, and `working_directory` is the Command's.
    ///
    /// # Errors
    /// As `materialize_cached`, or a tree cannot be built.
    pub async fn materialize(
        &self,
        cas: &impl Cas,
        cache: &Cache,
        root: &Digest,
        dir: &std::path::Path,
        outputs: &[String],
        working_directory: &str,
    ) -> Result<Inputs, TreeError> {
        let reserved = Reserved {
            outputs,
            working_directory,
        };
        self.materialize_dyn(cas, cas.in_flight(), cache, root, dir, &reserved)
            .await
    }

    /// [`Trees::materialize`] behind a pointer: compiled once whatever the CAS.
    async fn materialize_dyn(
        &self,
        cas: &dyn Fill,
        in_flight: usize,
        cache: &Cache,
        root: &Digest,
        dir: &std::path::Path,
        reserved: &Reserved<'_>,
    ) -> Result<Inputs, TreeError> {
        let limits = self.limits;
        // A node already known to be a tree is not read: that is the walk a warm node
        // does not repeat.
        let skip = |path: &str, digest: &Digest| {
            self.summaries
                .get(&label(digest))
                .is_some_and(|summary| classify::qualifies(path, summary, reserved, limits))
        };
        let max = classify::MAX_DIRECTORIES;
        let reads = CountedReads::new(cas);
        let loading = Instant::now();
        let loaded = load(&reads, in_flight, root, &Loaded::new(), &skip, max).await?;
        let mut walk_ms = millis(loading);
        let choosing = Instant::now();
        let summaries = summarize(&loaded, &|name| self.summaries.get(name))?;
        self.summaries.extend(&summaries);
        let chosen = select(&loaded, &summaries, root, reserved, limits, max)?;

        let mut leases = Vec::new();
        let mut mounts = Vec::new();
        let (mut tree_files, mut built) = (0, 0);
        for candidate in &chosen {
            let (lease, stats) = self
                .store
                .acquire(cas, in_flight, cache, candidate, &loaded)
                .await?;
            mounts.push(SharedMount {
                path: candidate.path.clone(),
                source: lease.path().to_owned(),
            });
            tree_files += candidate.summary.files;
            built += u64::from(stats.is_some());
            leases.push(lease);
        }
        let mut trees_ms = millis(choosing);
        let mount_dirs: HashSet<PathBuf> = mounts.iter().map(|m| dir.join(&m.path)).collect();
        let walking = Instant::now();
        let walked = make_directories(&reads, in_flight, root, dir, &loaded, &mount_dirs).await?;
        walk_ms += millis(walking);
        let mut files = walked.files;
        let mut dirs_made = walked.dirs_made;
        let mut cloned = 0;
        if let Delivery::Clone(clone) = &self.delivery {
            // The empty mount points become the lease's own copies of the trees. The
            // pins end with this: a clone does not need the tree.
            let cloning = Instant::now();
            let node = Node {
                reads: &reads,
                in_flight,
                dir,
                loaded: &loaded,
            };
            let mounts = std::mem::take(&mut mounts);
            let (done, made) = self
                .clone_trees(clone, &node, mounts, &chosen, &mut files)
                .await?;
            (cloned, dirs_made) = (done, dirs_made + made);
            leases.clear();
            trees_ms += millis(cloning);
        }
        let private: Vec<PathBuf> = reserved.outputs.iter().map(|p| dir.join(p)).collect();
        let writing = Instant::now();
        let stats = crate::cache::lease::write_files(cas, cache, files, &private).await?;
        let phases = WritePhases {
            walk_ms,
            dir_reads: reads.reads(),
            dirs_made,
            trees_ms,
            link_ms: millis(writing),
            links: stats.links,
            copies: stats.copies,
        };
        Ok(Inputs {
            stats,
            mounts,
            trees: leases,
            trees_cloned: cloned,
            tree_files,
            trees_built: built,
            phases,
        })
    }

    /// Replaces each of `mounts` (an empty directory in `node.dir`) with a clone of its
    /// tree; a tree the filesystem refuses is written as a plain directory instead and
    /// its files are added to `files`. Returns the trees cloned and the directories made
    /// by the plain ones.
    async fn clone_trees(
        &self,
        clone: &Arc<CloneDirFn>,
        node: &Node<'_>,
        mounts: Vec<SharedMount>,
        chosen: &[classify::Candidate],
        files: &mut Vec<Copies>,
    ) -> Result<(u64, u64), TreeError> {
        let (mut cloned, mut dirs_made) = (0, 0);
        for mount in mounts {
            let at = node.dir.join(&mount.path);
            let (from, clone, to) = (mount.source, Arc::clone(clone), at.clone());
            let placed = tokio::task::spawn_blocking(move || place::place(&*clone, &from, &to))
                .await
                .expect("cloning a tree panicked")
                .map_err(io_error(&at))?;
            let place::Placed::Refused(why) = placed else {
                cloned += 1;
                continue;
            };
            if !self.refusal_logged.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "trees: the filesystem refused to clone a tree ({why}); its files are written one by one"
                );
            }
            // The tree's node is written as a plain directory: its messages, then its
            // files through the cache.
            let tree = chosen.iter().find(|c| c.path == mount.path);
            let tree = tree.expect("a mount is a chosen node");
            let sub = make_directories(
                node.reads,
                node.in_flight,
                &tree.digest,
                &at,
                node.loaded,
                &HashSet::new(),
            )
            .await?;
            dirs_made += sub.dirs_made;
            merge(files, sub.files);
        }
        Ok((cloned, dirs_made))
    }
}

/// What a clone of a tree needs of the walk that made the lease's directories.
struct Node<'a> {
    reads: &'a dyn Fetch,
    in_flight: usize,
    dir: &'a std::path::Path,
    loaded: &'a Loaded,
}

/// Adds `more` to `files`, one entry per digest (a blob's names are placed together).
fn merge(files: &mut Vec<Copies>, more: Vec<Copies>) {
    let mut at: HashMap<String, usize> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (label(&f.digest), i))
        .collect();
    for file in more {
        match at.get(&label(&file.digest)) {
            Some(&i) => files[i].copies.extend(file.copies),
            None => {
                at.insert(label(&file.digest), files.len());
                files.push(file);
            }
        }
    }
}

#[cfg(test)]
#[path = "classify_tests.rs"]
mod classify_tests;
#[cfg(test)]
#[path = "verify_tests.rs"]
mod verify_tests;
