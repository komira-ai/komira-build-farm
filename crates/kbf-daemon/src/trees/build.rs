//! Building one tree in the store's `.tmp-*` directory, and checking it before it is
//! named (`docs/design/fast-sandbox.md#42-preparation-and-the-bulk-linker`).

use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use kbf_proto::reapi::Digest;
use rustix::fs::{CWD, RenameFlags};

use super::classify::{MAX_DIRECTORIES, Summary, load};
use crate::cache::{Cache, InputStats};
use crate::cas::{Fill, label};
use crate::tree::{Loaded, TreeError, io as io_error, make_directories};

/// Writes the tree under `digest` into the empty directory `tmp`, files as hard links
/// to `cache` entries (read-only, owned by the daemon's user), or on macOS clones of
/// them left read-only, then checks what was
/// written against the Directory messages ([`verify`]).
///
/// # Errors
/// A blob is missing or corrupt, the tree breaks a naming rule, a write fails, or the
/// written tree is not the one the digest names.
pub(super) async fn build(
    cas: &dyn Fill,
    in_flight: usize,
    cache: &Cache,
    digest: &Digest,
    known: &Loaded,
    tmp: &Path,
) -> Result<(Summary, InputStats), TreeError> {
    let max = MAX_DIRECTORIES;
    let loaded = load(cas, in_flight, digest, known, &|_, _| false, max).await?;
    let walked =
        make_directories(cas, in_flight, digest, tmp, &loaded, &Default::default()).await?;
    let stats = crate::cache::lease::write_files_as(cas, cache, walked.files, &[], true).await?;
    let summary = check(loaded, digest.clone(), tmp.to_owned()).await?;
    Ok((summary, stats))
}

/// [`verify`] on the blocking pool. The runtime is never shut down under a lease, so the
/// blocking task finishes.
async fn check(loaded: Loaded, root: Digest, tmp: PathBuf) -> Result<Summary, TreeError> {
    let checked = tokio::task::spawn_blocking(move || verify(&loaded, &root, &tmp)).await;
    checked.expect("the tree check panicked")
}

/// Walks the tree written at `tmp` and compares it with the Directory messages of
/// `root` in `loaded`: the same names of the same kinds, each file's size, executable
/// bit and absence of group and other write bits, each symlink's target. Directories
/// are made 0755 (the container's root reads them as "other"). Returns what the tree
/// holds.
///
/// File contents are not hashed here: the cache's entries are hashed when first used
/// after a start (`cache::verify`), and the files are links to them.
pub(super) fn verify(loaded: &Loaded, root: &Digest, tmp: &Path) -> Result<Summary, TreeError> {
    let wrong = |path: &Path, what: &str| TreeError::Invalid(format!("{}: {what}", path.display()));
    let mut summary = Summary::default();
    let mut stack: Vec<(Digest, PathBuf)> = vec![(root.clone(), tmp.to_owned())];
    while let Some((digest, dir)) = stack.pop() {
        let directory = loaded
            .get(&label(&digest))
            .ok_or_else(|| wrong(&dir, "a directory the tree names was not loaded"))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .map_err(io_error(&dir))?;
        let mut on_disk = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(io_error(&dir))? {
            on_disk.push(entry.map_err(io_error(&dir))?.file_name());
        }
        let wanted = directory.files.len() + directory.symlinks.len() + directory.directories.len();
        if on_disk.len() != wanted {
            return Err(wrong(&dir, "does not hold the entries its Directory names"));
        }
        for file in &directory.files {
            let path = dir.join(&file.name);
            let meta = std::fs::symlink_metadata(&path).map_err(io_error(&path))?;
            let size = file.digest.as_ref().map_or(-1, |d| d.size_bytes);
            if !meta.file_type().is_file()
                || i64::try_from(meta.len()).ok() != Some(size)
                || (meta.mode() & 0o111 != 0) != file.is_executable
                || meta.mode() & 0o022 != 0
            {
                return Err(wrong(&path, "is not the file its Directory names"));
            }
            summary.files += 1;
            summary.bytes += meta.len();
        }
        for link in &directory.symlinks {
            let path = dir.join(&link.name);
            let meta = std::fs::symlink_metadata(&path).map_err(io_error(&path))?;
            if !meta.file_type().is_symlink() {
                return Err(wrong(&path, "is not the symlink its Directory names"));
            }
            let target = std::fs::read_link(&path).map_err(io_error(&path))?;
            if target.as_os_str() != link.target.as_str() {
                return Err(wrong(&path, "points elsewhere than its Directory says"));
            }
            summary.files += 1;
        }
        for sub in &directory.directories {
            let path = dir.join(&sub.name);
            let meta = std::fs::symlink_metadata(&path).map_err(io_error(&path))?;
            if !meta.file_type().is_dir() {
                return Err(wrong(&path, "is not the directory its Directory names"));
            }
            // A Directory with no digest was refused when it was read.
            stack.push((sub.digest.clone().unwrap_or_default(), path));
        }
    }
    Ok(summary)
}

/// [`publish`] on the blocking pool (same note as [`check`]).
pub(super) async fn publish_later(tmp: PathBuf, to: PathBuf) -> io::Result<bool> {
    let published = tokio::task::spawn_blocking(move || publish(&tmp, &to)).await;
    published.expect("naming the tree panicked")
}

/// Names the finished tree `to`: `RENAME_NOREPLACE`, so a tree already there (it was
/// renamed in whole) is left alone; returns whether this call named it.
pub(super) fn publish(tmp: &Path, to: &Path) -> io::Result<bool> {
    match rustix::fs::renameat_with(CWD, tmp, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(true),
        Err(e) if e == rustix::io::Errno::EXIST => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Removes `dir` and all it holds, with a loop over a `Vec`, not recursion, and
/// without following a symlink. A directory already gone is not an error.
pub(super) fn remove_tree(dir: &Path) -> io::Result<()> {
    if !dir.try_exists()? {
        return Ok(());
    }
    let mut dirs = vec![dir.to_owned()];
    let mut next = 0;
    while next < dirs.len() {
        let here = dirs[next].clone();
        next += 1;
        for entry in std::fs::read_dir(&here)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            } else {
                std::fs::remove_file(entry.path())?;
            }
        }
    }
    // Children before parents.
    for dir in dirs.iter().rev() {
        std::fs::remove_dir(dir)?;
    }
    Ok(())
}

/// The bytes of the regular files under `dir` (a tree adopted at start); symlinks hold
/// none, as in a tree's summary.
pub(super) fn bytes_under(dir: &Path) -> io::Result<u64> {
    let mut total = 0_u64;
    let mut stack = vec![dir.to_owned()];
    while let Some(here) = stack.pop() {
        for entry in std::fs::read_dir(&here)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}
