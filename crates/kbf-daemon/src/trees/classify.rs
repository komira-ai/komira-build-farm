//! Which Directory nodes of an input root become shared trees
//! (`docs/design/fast-sandbox.md#41-which-directory-nodes-qualify`).
//!
//! A node qualifies when it holds at least `min_files` files and symlinks below it,
//! its bytes fit the tree store's budget, nothing the Command names lies at, under or
//! (for an output) above it, its path can be a mount destination, and no node above it
//! qualifies: the highest node on a path is taken, never its children too. The root
//! never qualifies. At most `max_mounts` are taken, the largest first; the rest are
//! linked per action as without trees.
//!
//! [`load`] reads the Directory messages (a node whose summary is already known to
//! qualify is not read: that is what spares a warm node the walk below a toolchain),
//! [`summarize`] counts files bottom-up, and [`select`] picks the nodes. All walks are
//! loops over a `Vec`, so a client's deep tree cannot overflow a stack.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use futures::{StreamExt as _, TryStreamExt as _, stream};
use kbf_proto::reapi::{Digest, Directory};

use crate::cas::{Fetch, label};
use crate::tree::{Loaded, TreeError, check_names, fetch_message_dyn};

/// The most directory occurrences one input root may have for the walks here: a
/// client's tree can name one Directory a million times over, and a walk visits each
/// place. Far above any toolchain (thousands).
pub(crate) const MAX_DIRECTORIES: usize = 1 << 22;

/// The most summaries kept by default.
const MAX_SUMMARIES: usize = 1 << 20;

/// What a Directory node holds below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    /// Regular files and symlinks, at any depth.
    pub files: u64,
    /// The bytes of the regular files.
    pub bytes: u64,
}

/// The summaries of Directory nodes seen, by digest label.
#[derive(Debug)]
pub(crate) struct Summaries {
    map: Mutex<HashMap<String, Summary>>,
    /// The most kept; past it the map is emptied (they are recomputed from the next
    /// walk of the Directory messages).
    cap: usize,
}

impl Default for Summaries {
    fn default() -> Self {
        Self::with_cap(MAX_SUMMARIES)
    }
}

impl Summaries {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            map: Mutex::default(),
            cap,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Summary>> {
        // Each update is one map operation, so the map is consistent after a panic.
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get(&self, label: &str) -> Option<Summary> {
        self.lock().get(label).copied()
    }

    pub fn extend(&self, found: &HashMap<String, Summary>) {
        let mut map = self.lock();
        if map.len().saturating_add(found.len()) > self.cap {
            map.clear();
        }
        map.extend(found.iter().map(|(k, v)| (k.clone(), *v)));
    }
}

/// What a Command reserves in the input root, as paths from the input root.
pub(crate) struct Reserved<'a> {
    /// Each output path (the working directory joined on).
    pub outputs: &'a [String],
    pub working_directory: &'a str,
}

/// Whether `inner` is `outer` or below it.
fn at_or_under(outer: &str, inner: &str) -> bool {
    outer.is_empty()
        || inner == outer
        || inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
}

impl Reserved<'_> {
    /// Whether a tree mounted at `path` would hold the working directory or an
    /// output, or sit under an output directory (the driver moves an output, whole,
    /// into the overlay's upper directory, mount point and all).
    pub fn conflicts(&self, path: &str) -> bool {
        let working_directory = self.working_directory.trim_matches('/');
        at_or_under(path, working_directory)
            || self
                .outputs
                .iter()
                .any(|output| at_or_under(path, output) || at_or_under(output, path))
    }
}

/// The limits of a classification (`TreeConfig`'s).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub min_files: u64,
    pub max_bytes: u64,
    pub max_mounts: usize,
}

/// Whether the node at `path` with `summary` would be a shared tree.
pub(crate) fn qualifies(
    path: &str,
    summary: Summary,
    reserved: &Reserved<'_>,
    limits: Limits,
) -> bool {
    // A destination with ':' or ',' is never a shared tree: mount paths stay free of
    // separators.
    summary.files >= limits.min_files
        && summary.bytes <= limits.max_bytes
        && !path.contains([':', ','])
        && !reserved.conflicts(path)
}

/// The directory name of the tree of `digest` in the store: `<hash>-<size>`; `None` for
/// a digest that is not a SHA-256 one (such a name could leave the store).
pub(crate) fn tree_name(digest: &Digest) -> Option<String> {
    let hex = digest.hash.len() == 64
        && digest
            .hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (hex && digest.size_bytes >= 0).then(|| format!("{}-{}", digest.hash, digest.size_bytes))
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn child_digest(directory: &Directory, index: usize) -> Result<&Digest, TreeError> {
    let node = &directory.directories[index];
    node.digest
        .as_ref()
        .ok_or_else(|| TreeError::Invalid(format!("input directory {:?} has no digest", node.name)))
}

fn too_many(max_places: usize) -> TreeError {
    TreeError::Invalid(format!(
        "the input tree names more than {max_places} directories"
    ))
}

/// Reads the Directory messages under `root`, a level at a time, each distinct one
/// once: from `base` when it holds it, else from the CAS (names checked). A child for
/// which `skip(path, digest)` says so is neither read nor walked below.
///
/// # Errors
/// A message is missing, corrupt or breaks a naming rule, or the tree names more than
/// `max_places` places (callers pass [`MAX_DIRECTORIES`]).
pub(crate) async fn load(
    cas: &dyn Fetch,
    in_flight: usize,
    root: &Digest,
    base: &Loaded,
    skip: &(dyn Fn(&str, &Digest) -> bool + Sync),
    max_places: usize,
) -> Result<Loaded, TreeError> {
    let mut loaded = Loaded::new();
    let mut level = vec![(root.clone(), String::new())];
    let mut places = 0_usize;
    while !level.is_empty() {
        places = places.saturating_add(level.len());
        if places > max_places {
            return Err(too_many(max_places));
        }
        let mut wanted = HashSet::new();
        let mut missing = Vec::new();
        for (digest, _) in &level {
            let name = label(digest);
            if loaded.contains_key(&name) || !wanted.insert(name.clone()) {
                continue;
            }
            match base.get(&name) {
                Some(known) => {
                    loaded.insert(name, known.clone());
                }
                None => missing.push(digest.clone()),
            }
        }
        let fetched: Vec<(String, Directory)> = stream::iter(missing)
            .map(|digest| async move {
                let directory: Directory = fetch_message_dyn(cas, &digest).await?;
                Ok::<_, TreeError>((label(&digest), directory))
            })
            .buffered(in_flight)
            .try_collect()
            .await?;
        for (name, directory) in fetched {
            check_names(&directory)?;
            loaded.insert(name, directory);
        }
        let mut next = Vec::new();
        for (digest, path) in &level {
            // Every digest of the level was loaded above.
            let directory = &loaded[&label(digest)];
            for (index, node) in directory.directories.iter().enumerate() {
                let child = child_digest(directory, index)?;
                let at = join(path, &node.name);
                if !skip(&at, child) {
                    next.push((child.clone(), at));
                }
            }
        }
        level = next;
    }
    Ok(loaded)
}

/// The summary of every Directory in `loaded`, bottom-up, and of every child that is
/// not in `loaded`, which takes it from `known`.
///
/// # Errors
/// A child is in neither (a walk that skipped it without a summary).
pub(crate) fn summarize(
    loaded: &Loaded,
    known: &dyn Fn(&str) -> Option<Summary>,
) -> Result<HashMap<String, Summary>, TreeError> {
    let mut done: HashMap<String, Summary> = HashMap::new();
    // In name order, not the map's: the same input is then the same walk every time.
    let mut names: Vec<&String> = loaded.keys().collect();
    names.sort();
    for start in names {
        let mut stack = vec![start.clone()];
        while let Some(name) = stack.pop() {
            if done.contains_key(&name) {
                continue;
            }
            let Some(directory) = loaded.get(&name) else {
                return Err(TreeError::Invalid(format!(
                    "directory {name} has no summary"
                )));
            };
            let mut waiting = Vec::new();
            let mut files = (directory.files.len() + directory.symlinks.len()) as u64;
            let mut bytes: u64 = directory
                .files
                .iter()
                .filter_map(|f| f.digest.as_ref())
                .fold(0, |sum, d| sum.saturating_add(d.size_bytes.max(0) as u64));
            for index in 0..directory.directories.len() {
                let child = label(child_digest(directory, index)?);
                let summary = match done.get(&child) {
                    Some(s) => *s,
                    None => match known(&child) {
                        Some(s) if !loaded.contains_key(&child) => {
                            // Kept: `select` looks a skipped node up with the rest.
                            done.insert(child.clone(), s);
                            s
                        }
                        _ => {
                            waiting.push(child);
                            continue;
                        }
                    },
                };
                files = files.saturating_add(summary.files);
                bytes = bytes.saturating_add(summary.bytes);
            }
            if waiting.is_empty() {
                done.insert(name, Summary { files, bytes });
            } else {
                stack.push(name);
                stack.extend(waiting);
            }
        }
    }
    Ok(done)
}

/// A Directory node chosen to be a shared tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// From the input root, `/`-separated.
    pub path: String,
    pub digest: Digest,
    /// The tree's name in the store ([`tree_name`]).
    pub name: String,
    pub summary: Summary,
}

/// The nodes under `root` that become shared trees, largest first (ties by path),
/// at most `limits.max_mounts`. `summaries` holds every node `loaded` holds and every
/// node a walk skipped.
///
/// # Errors
/// A node to walk into is not in `loaded`, or the tree names more than
/// `max_places` places (callers pass [`MAX_DIRECTORIES`]).
pub(crate) fn select(
    loaded: &Loaded,
    summaries: &HashMap<String, Summary>,
    root: &Digest,
    reserved: &Reserved<'_>,
    limits: Limits,
    max_places: usize,
) -> Result<Vec<Candidate>, TreeError> {
    let mut chosen = Vec::new();
    let mut stack = vec![(root.clone(), String::new())];
    let mut places = 0_usize;
    while let Some((digest, path)) = stack.pop() {
        places = places.saturating_add(1);
        if places > max_places {
            return Err(too_many(max_places));
        }
        let Some(directory) = loaded.get(&label(&digest)) else {
            return Err(TreeError::Invalid(format!(
                "directory {} was not loaded",
                label(&digest)
            )));
        };
        for (index, node) in directory.directories.iter().enumerate() {
            let child = child_digest(directory, index)?;
            let at = join(&path, &node.name);
            let summary = summaries.get(&label(child)).copied().unwrap_or_default();
            // A digest with no store name is walked into, not mounted.
            let name = tree_name(child).filter(|_| qualifies(&at, summary, reserved, limits));
            if let Some(name) = name {
                chosen.push(Candidate {
                    path: at,
                    digest: child.clone(),
                    name,
                    summary,
                });
            } else {
                stack.push((child.clone(), at));
            }
        }
    }
    chosen.sort_by(|a, b| (b.summary.files, &a.path).cmp(&(a.summary.files, &b.path)));
    chosen.truncate(limits.max_mounts);
    Ok(chosen)
}
