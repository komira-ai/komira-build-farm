//! Removing a directory tree an action shaped, without recursion and without
//! following a symlink.
//!
//! An action can leave a tree deeper than any path the kernel accepts, directories it
//! made unreadable or unwritable, and symlinks to host paths. So the tree is removed by
//! descriptor: each directory is opened relative to its parent with `O_NOFOLLOW`, its
//! entries are listed whole before any is unlinked (deleting while reading a directory
//! skips entries on some filesystems), a directory missing any of its owner's `rwx`
//! bits gets them back first, and a symlink is unlinked, never followed. The stack of
//! directories still to finish is a `Vec` on the heap, and only the directory being
//! cleared holds a descriptor: the walk returns to a parent through `..` and refuses a
//! `..` that is not the directory it came from.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A directory's device and inode.
type Identity = (u64, u64);

#[allow(clippy::unnecessary_cast)] // the field types differ between Linux and macOS
fn identity(fd: &OwnedFd) -> std::io::Result<Identity> {
    let stat = rustix::fs::fstat(fd)?;
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

/// Whether `name` in `dir` is a directory (not following a symlink), giving it back
/// its owner's `rwx` bits if it lacks any. `None` when it is absent.
#[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
fn prepare(dir: &OwnedFd, name: &OsStr) -> std::io::Result<Option<bool>> {
    let stat = match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mode = stat.st_mode as u32;
    if FileType::from_raw_mode(stat.st_mode as _) != FileType::Directory {
        return Ok(Some(false));
    }
    if mode & 0o700 != 0o700 {
        let mode = Mode::from_raw_mode(((mode & 0o7777) | 0o700) as _);
        rustix::fs::chmodat(dir, name, mode, AtFlags::empty())?;
    }
    Ok(Some(true))
}

/// Unlinks every entry of `dir` that is not a directory and returns the names of those
/// that are.
fn clear(dir: &OwnedFd) -> std::io::Result<Vec<Vec<u8>>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(name);
        }
    }
    let mut subdirs = Vec::new();
    for name in names {
        let os = OsStr::from_bytes(&name);
        if prepare(dir, os)? == Some(true) {
            subdirs.push(name);
        } else {
            rustix::fs::unlinkat(dir, os, AtFlags::empty())?;
        }
    }
    Ok(subdirs)
}

/// A directory being removed: its identity, its name in its parent, and the
/// subdirectories not yet removed.
struct Frame {
    id: Identity,
    name: Vec<u8>,
    subdirs: Vec<Vec<u8>>,
}

/// Removes `path` and everything below it. `path` itself may be a file or a symlink
/// (unlinked) or absent (nothing to do); its parent directories are the caller's and
/// are opened as given.
///
/// # Errors
/// A directory could not be listed, made writable or removed; a parent was replaced
/// while the walk was below it.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("{} has no parent to remove it from", path.display()),
        ));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let top = rustix::fs::openat(
        CWD,
        parent,
        DIRECTORY.difference(OFlags::NOFOLLOW),
        Mode::empty(),
    )?;
    match prepare(&top, name)? {
        None => return Ok(()),
        Some(false) => return Ok(rustix::fs::unlinkat(&top, name, AtFlags::empty())?),
        Some(true) => {}
    }
    let mut here = rustix::fs::openat(&top, name, DIRECTORY, Mode::empty())?;
    let mut stack = vec![Frame {
        id: identity(&here)?,
        name: Vec::new(),
        subdirs: clear(&here)?,
    }];
    while let Some(mut frame) = stack.pop() {
        if let Some(sub) = frame.subdirs.pop() {
            stack.push(frame);
            let os = OsStr::from_bytes(&sub);
            // Made accessible by `clear`. Opened with O_NOFOLLOW all the same: a
            // directory swapped for a symlink since is an error, never followed.
            let child = rustix::fs::openat(&here, os, DIRECTORY, Mode::empty())?;
            let id = identity(&child)?;
            let subdirs = clear(&child)?;
            stack.push(Frame {
                id,
                name: sub,
                subdirs,
            });
            here = child;
            continue;
        }
        // `frame` is empty now. The root's is removed below, from `top`.
        if let Some(parent) = stack.last() {
            here = back_to(&here, parent.id, path)?;
            rustix::fs::unlinkat(&here, OsStr::from_bytes(&frame.name), AtFlags::REMOVEDIR)?;
        }
    }
    drop(here);
    rustix::fs::unlinkat(&top, name, AtFlags::REMOVEDIR)?;
    Ok(())
}

/// The parent of `dir` through its `..`, refusing any directory but `expected`.
fn back_to(dir: &OwnedFd, expected: Identity, path: &Path) -> std::io::Result<OwnedFd> {
    let up = rustix::fs::openat(dir, "..", DIRECTORY, Mode::empty())?;
    if identity(&up)? != expected {
        return Err(std::io::Error::other(format!(
            "{}: a directory was replaced while it was being removed",
            path.display()
        )));
    }
    Ok(up)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches the removal climbing back into a directory other than the one it came
    /// from: `..` must be the parent it left, by device and inode.
    #[test]
    fn the_walk_goes_back_only_where_it_came_from() {
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps")
            .join("kbf-outputs-unit")
            .join("back-to");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("child")).expect("mkdir");
        let top = rustix::fs::openat(CWD, &dir, DIRECTORY, Mode::empty()).expect("open");
        let child = rustix::fs::openat(&top, "child", DIRECTORY, Mode::empty()).expect("open");
        let id = identity(&top).expect("identity");
        let back = back_to(&child, id, &dir).expect("the same parent");
        assert_eq!(identity(&back).expect("identity"), id);
        let why = back_to(&child, (id.0, id.1 ^ 1), &dir).expect_err("another directory");
        assert!(why.to_string().contains("was replaced"), "{why}");
    }
}
