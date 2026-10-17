//! A lease's input files through the cache (`docs/design/worker-cache.md#62-what-a-lease-sees-behaviour-changes`).
//!
//! For each blob of the input tree, each of its two keys (plain and executable) that
//! the tree names:
//!
//! - an **empty** blob is created directly at each of its names, never looked up or
//!   linked;
//! - a name an output path covers gets a **private copy** (writable, as without the
//!   cache): the driver moves it into the overlay's upper directory, where the action
//!   may change it, so it must never be the cache's inode;
//! - the other names all get **one inode**: hard links to the entry, or, when linking
//!   is not possible (copy mode, the link limit near, `EMLINK`, `EXDEV`), one private
//!   copy for the lease, dated and moded as the entry, with every other name linked to
//!   it. A fallback is per key per lease, never per name: names already linked to the
//!   entry are unlinked first.
//!
//! In [`LinkMode::Clone`] (macOS) there are no links: every name, private or not, is
//! its own **clone** of the entry (`clonefile`: the data blocks are shared until a
//! write, the inode is not), moded as an input is without the cache (0644, 0755
//! executable) and dated as the entry. An action may write any input, as without the
//! cache, and the entry stays as it was. A clone the filesystem refuses (`ENOTSUP`,
//! `EXDEV`) is a private copy of that name, counted as a fallback once per key.
//!
//! A link or clone that finds the entry gone (`ENOENT`) settles the key again and
//! retries.

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::{StreamExt as _, stream};
use rustix::fs::{Timespec, Timestamps};

use super::fill::{Fill, InputCounts, InputStats, Source, blocking};
use super::layout::{FIXED_MTIME, Key};
use super::{Cache, LinkMode, bump};
use crate::cas::{CasError, label};
use crate::tree::{Copies, TreeError};

/// How many links short of the filesystem's limit an entry stops being linked.
const LINK_HEADROOM: u64 = 1000;

/// A key's names in one lease: those linked to one inode, and the private ones.
struct Names {
    key: Key,
    shared: Vec<PathBuf>,
    private: Vec<PathBuf>,
}

/// Writes every copy of each blob in `files` through `cache`, at most
/// [`Cache::link_concurrency`] keys at once, as the module docs say. A name under one of `private` (absolute paths) gets a private
/// copy. The keys are pinned until it returns. After the first error no key is
/// started, and those started are waited for.
pub(crate) async fn write_files(
    cas: &dyn Fill,
    cache: &Cache,
    files: Vec<Copies>,
    private: &[PathBuf],
) -> Result<InputStats, TreeError> {
    write_files_as(cas, cache, files, private, false).await
}

/// [`write_files`] with the files' mode in clone mode chosen: a tree's files are
/// `read_only` (0444, 0555 as the entry), an input's 0644 and 0755.
pub(crate) async fn write_files_as(
    cas: &dyn Fill,
    cache: &Cache,
    files: Vec<Copies>,
    private: &[PathBuf],
    read_only: bool,
) -> Result<InputStats, TreeError> {
    // Only clone mode has a say: a hard link is the entry's mode already.
    let want = read_only && cache.link_mode() == LinkMode::Clone;
    let counts = InputCounts::default();
    let mut blobs = Vec::new();
    for blob in files {
        blobs.push(plan(blob, private, &counts, want)?);
    }
    let _pins = cache.pin(blobs.iter().flatten().map(|n| n.key).collect());
    let failed = AtomicBool::new(false);
    let (failed, counts_ref) = (&failed, &counts);
    // A blob's two keys one after the other: the second is then a local copy of the
    // first, not a second fetch.
    let mut placed = stream::iter(blobs)
        .map(|keys| async move {
            for names in keys {
                if failed.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let placed = place(cas, cache, names, counts_ref, want).await;
                if placed.is_err() {
                    failed.store(true, Ordering::SeqCst);
                    return placed;
                }
            }
            Ok(())
        })
        .buffer_unordered(cache.link_concurrency());
    let mut first = Ok(());
    while let Some(outcome) = placed.next().await {
        if first.is_ok() {
            first = outcome;
        }
    }
    drop(placed);
    first.map(|()| counts.stats())
}

/// The keys of `blob`'s copies, with their names; an empty blob is written at once.
fn plan(
    blob: Copies,
    private: &[PathBuf],
    counts: &InputCounts,
    want: bool,
) -> Result<Vec<Names>, TreeError> {
    // A digest that names no SHA-256 blob is one no CAS holds.
    let key = |executable| {
        Key::of(&blob.digest, executable)
            .ok_or_else(|| TreeError::Cas(CasError::Missing(label(&blob.digest))))
    };
    let (mut plain, mut executable) = (Names::of(key(false)?), Names::of(key(true)?));
    for (path, is_executable) in blob.copies {
        bump(&counts.files);
        if blob.digest.size_bytes == 0 {
            bump(&counts.empty_files);
            let mode = clone_mode(key(is_executable)?, want);
            create(&path, mode, &mut |_| Ok(())).map_err(crate::tree::io(&path))?;
            continue;
        }
        let names = if is_executable {
            &mut executable
        } else {
            &mut plain
        };
        if private.iter().any(|p| path.starts_with(p)) {
            names.private.push(path);
        } else {
            names.shared.push(path);
        }
    }
    Ok([plain, executable]
        .into_iter()
        .filter(|n| !n.shared.is_empty() || !n.private.is_empty())
        .collect())
}

impl Names {
    fn of(key: Key) -> Self {
        Self {
            key,
            shared: Vec::new(),
            private: Vec::new(),
        }
    }
}

/// Places every name of one key.
async fn place(
    cas: &dyn Fill,
    cache: &Cache,
    names: Names,
    counts: &InputCounts,
    want: bool,
) -> Result<(), TreeError> {
    let names = Arc::new(names);
    let mut stale = false;
    loop {
        let got = cache
            .get(cas, names.key, stale, counts)
            .await
            .map_err(TreeError::Cas)?;
        let source = got.source;
        let (mode, link_max, link) = (
            cache.link_mode(),
            cache.link_max(),
            Arc::clone(&cache.inner.config.link),
        );
        let placing = Arc::clone(&names);
        let placed = match source {
            Source::Entry { path, nlink } => {
                blocking(move || from_entry(&path, nlink, &placing, mode, link_max, &*link, want))
                    .await
            }
            // `loose` is removed when the copy is done (dropped with the closure), or
            // whenever the closure is dropped unrun.
            Source::Loose(loose) => {
                blocking(move || from_loose(loose.path(), &placing, mode, want)).await
            }
            Source::LeaseFetch => fetch_names(cas, cache, &names, counts, want).await,
        };
        match placed {
            Ok(fell_back) => {
                let files = (names.shared.len() + names.private.len()) as u64;
                let counted = if got.hit {
                    &counts.hits
                } else {
                    &counts.misses
                };
                counted.fetch_add(files, Ordering::Relaxed);
                let (shared, private) = (names.shared.len() as u64, names.private.len() as u64);
                if fell_back {
                    bump(&counts.link_fallbacks);
                }
                // A clone is its own inode, as a copy is: nothing is a link in clone mode.
                let (links, copies) = if fell_back || mode == LinkMode::Clone {
                    (0, shared + private)
                } else {
                    (shared, private)
                };
                counts.links.fetch_add(links, Ordering::Relaxed);
                counts.copies.fetch_add(copies, Ordering::Relaxed);
                return Ok(());
            }
            Err(Placing::Gone) => stale = true,
            Err(Placing::Failed(path, source)) => return Err(TreeError::Io { path, source }),
            Err(Placing::Cas(e)) => return Err(TreeError::Cas(e)),
        }
    }
}

/// Why a key's names could not all be placed.
enum Placing {
    /// The entry was gone: settle the key again.
    Gone,
    Failed(PathBuf, io::Error),
    /// The lease's own fetch of the key failed.
    Cas(CasError),
}

/// How a file a lease gets is moded and dated.
#[derive(Clone, Copy)]
enum Mode {
    /// As the cache's entry of a key: read-only, fixed mtime.
    Entry(Key),
    /// As an input without the cache: 0644 or 0755 (executable), the write's mtime.
    Writable(bool),
}

/// Places `names` from the checked file at `loose` the cache could not keep: private
/// copies, and one read-only copy the shared names are linked to. Whether that was a
/// fallback (always, when there are shared names).
fn from_loose(loose: &Path, names: &Names, mode: LinkMode, want: bool) -> Result<bool, Placing> {
    let mut fill =
        |file: &mut std::fs::File| io::copy(&mut std::fs::File::open(loose)?, file).map(drop);
    if mode == LinkMode::Clone {
        // Every name its own file, as clones would be.
        for path in names.private.iter().chain(&names.shared) {
            create(path, clone_mode(names.key, want), &mut fill).map_err(failed(path))?;
        }
        return Ok(!names.shared.is_empty());
    }
    for path in &names.private {
        create(path, Mode::Writable(names.key.executable), &mut fill).map_err(failed(path))?;
    }
    let Some((first, rest)) = names.shared.split_first() else {
        return Ok(false);
    };
    create(first, Mode::Entry(names.key), &mut fill).map_err(failed(first))?;
    for path in rest {
        std::fs::hard_link(first, path).map_err(failed(path))?;
    }
    Ok(true)
}

/// Places `names` with no entry and no checked file to copy (the cache could not write
/// one): the blob is fetched once, as it arrives, into the first shared name (moded as
/// an entry, which the other shared names link to) or, with none, the first private
/// name, and every other name is placed from it as from an entry. Whether that was a
/// fallback (always, when there are shared names).
async fn fetch_names(
    cas: &dyn Fill,
    cache: &Cache,
    names: &Names,
    counts: &InputCounts,
    want: bool,
) -> Result<bool, Placing> {
    let (key, clone) = (names.key, cache.link_mode() == LinkMode::Clone);
    // `plan` keeps only keys with a name. Shared names first: the others link to it.
    let shared = !names.shared.is_empty();
    let (first, rest) = if shared {
        let rest = (names.shared[1..].to_vec(), names.private.clone());
        (&names.shared[0], rest)
    } else {
        (&names.private[0], (Vec::new(), names.private[1..].to_vec()))
    };
    let mode = match (clone, shared) {
        (true, _) => clone_mode(key, want),
        (false, true) => Mode::Entry(key),
        (false, false) => Mode::Writable(key.executable),
    };
    let at = first.clone();
    let file = blocking(move || super::fill::new_file(&at, created(mode))).await;
    let mut file = tokio::fs::File::from_std(file.map_err(failed(first))?);
    cas.fetch_to(&key.digest(), &mut file)
        .await
        .map_err(Placing::Cas)?;
    counts.fetched_bytes.fetch_add(key.size, Ordering::Relaxed);
    let file = file.into_std().await;
    let rest = Names {
        key,
        shared: rest.0,
        private: rest.1,
    };
    let (from, link_max) = (first.clone(), cache.link_max());
    let link = Arc::clone(&cache.inner.config.link);
    let placed = blocking(move || {
        finish(&file, mode).map_err(failed(&from))?;
        drop(file);
        let mode = if clone {
            LinkMode::Clone
        } else {
            LinkMode::Hardlink
        };
        let link: &super::LinkFn = if clone { &*link } else { &hard_link };
        from_entry(&from, 1, &rest, mode, link_max, link, want)
    });
    placed.await.map(|_| !names.shared.is_empty())
}

/// A hard link, as a [`super::LinkFn`].
fn hard_link(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::hard_link(from, to)
}

/// Places `names` from the entry at `entry` (with `nlink` links): see the module docs.
/// Whether the shared names fell back to a private copy.
fn from_entry(
    entry: &Path,
    nlink: u64,
    names: &Names,
    mode: LinkMode,
    link_max: u64,
    link: &super::LinkFn,
    want: bool,
) -> Result<bool, Placing> {
    if mode == LinkMode::Clone {
        return clone_all(entry, names, link, want);
    }
    for path in &names.private {
        copy_new(entry, path, Mode::Writable(names.key.executable))?;
    }
    let Some((first, rest)) = names.shared.split_first() else {
        return Ok(false);
    };
    let fits = nlink + names.shared.len() as u64 <= link_max.saturating_sub(LINK_HEADROOM);
    if mode == LinkMode::Hardlink && fits {
        match link_all(entry, first, rest, link) {
            Ok(()) => return Ok(false),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Err(Placing::Gone),
            Err(e) if matches!(e.raw_os_error(), Some(libc::EMLINK | libc::EXDEV)) => {}
            Err(e) => return Err(failed(first)(e)),
        }
    }
    copy_new(entry, first, Mode::Entry(names.key))?;
    for path in rest {
        std::fs::hard_link(first, path).map_err(failed(path))?;
    }
    Ok(true)
}

/// How a clone-mode file is created when it is not a clone.
fn clone_mode(key: Key, read_only: bool) -> Mode {
    if read_only {
        Mode::Entry(key)
    } else {
        Mode::Writable(key.executable)
    }
}

/// Places every name of `names` as its own clone of `entry` (`link` is the clone), made
/// writable unless `want` (read-only: a clone has the entry's mode). Whether
/// a name fell back to a private copy. When the entry is gone, the names already placed
/// are removed, so the key's next attempt starts from none.
fn clone_all(
    entry: &Path,
    names: &Names,
    link: &super::LinkFn,
    want: bool,
) -> Result<bool, Placing> {
    let executable = names.key.executable;
    let mut fell_back = false;
    let mut placed: Vec<&Path> = Vec::new();
    for path in names.private.iter().chain(&names.shared) {
        let cloned = match link(entry, path) {
            Ok(()) => {
                placed.push(path);
                if want {
                    Ok(())
                } else {
                    make_writable(path, executable).map_err(failed(path))
                }
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Err(Placing::Gone),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EXDEV)) => {
                fell_back = true;
                copy_new(entry, path, clone_mode(names.key, want)).inspect(|()| placed.push(path))
            }
            Err(e) => Err(failed(path)(e)),
        };
        if let Err(why) = cloned {
            if matches!(why, Placing::Gone) {
                for done in placed {
                    let _ = std::fs::remove_file(done);
                }
            }
            return Err(why);
        }
    }
    Ok(fell_back)
}

/// Moded as an input is without the cache: 0644, 0755 when executable.
fn make_writable(path: &Path, executable: bool) -> io::Result<()> {
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Links `first` to `entry` and each of `rest` to `first`: one inode. On an error, the
/// names already linked are unlinked again, so a fallback starts from none.
fn link_all(entry: &Path, first: &Path, rest: &[PathBuf], link: &super::LinkFn) -> io::Result<()> {
    link(entry, first)?;
    for (done, path) in rest.iter().enumerate() {
        if let Err(e) = link(first, path) {
            for linked in std::iter::once(first).chain(rest[..done].iter().map(PathBuf::as_path)) {
                std::fs::remove_file(linked)?;
            }
            return Err(e);
        }
    }
    Ok(())
}

/// Copies the entry at `entry` into a new file at `to`, moded and dated by `mode`.
fn copy_new(entry: &Path, to: &Path, mode: Mode) -> Result<(), Placing> {
    // An entry that cannot be opened is settled again: its slot's holder finds out why.
    let Ok(mut from) = std::fs::File::open(entry) else {
        return Err(Placing::Gone);
    };
    create(to, mode, &mut |file| io::copy(&mut from, file).map(drop)).map_err(failed(to))
}

/// The error placing `path` failed with.
fn failed(path: &Path) -> impl FnOnce(io::Error) -> Placing + '_ {
    move |e| Placing::Failed(path.to_owned(), e)
}

/// Creates `path` (`O_EXCL`), fills it with `fill`, then modes and dates it.
fn create(
    path: &Path,
    mode: Mode,
    fill: &mut dyn FnMut(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let mut file = super::fill::new_file(path, created(mode))?;
    fill(&mut file)?;
    finish(&file, mode)
}

/// The mode a file of `mode` is created with: the owner's alone until an entry's is
/// set; a writable one's final mode at once.
const fn created(mode: Mode) -> u32 {
    match mode {
        Mode::Entry(_) => 0o600,
        Mode::Writable(true) => 0o755,
        Mode::Writable(false) => 0o644,
    }
}

/// Modes and dates a file filled as `mode` says: an entry's mode and fixed mtime; a
/// writable one as it was created.
fn finish(file: &std::fs::File, mode: Mode) -> io::Result<()> {
    if let Mode::Entry(key) = mode {
        rustix::fs::fchmod(file, rustix::fs::Mode::from_raw_mode(key.mode()))?;
        let fixed = Timespec {
            tv_sec: FIXED_MTIME,
            tv_nsec: 0,
        };
        rustix::fs::futimens(
            file,
            &Timestamps {
                last_access: fixed,
                last_modification: fixed,
            },
        )?;
    }
    Ok(())
}
