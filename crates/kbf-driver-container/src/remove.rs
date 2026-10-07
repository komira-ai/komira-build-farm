//! Removing a lease's scratch directory: without recursion and without following a
//! symlink.
//!
//! The action decides what its scratch directory holds, outputs or not, and how deep.
//! `std::fs::remove_dir_all` recurses once per directory level, so a tree a few
//! thousand levels deep that the action left anywhere in its upper directory ran the
//! clean step off its thread's stack and took the daemon down. This walk is a loop
//! over a `Vec` of frames on the heap, in the style of the output walk (`outputs`):
//! only the directory being emptied holds a descriptor, every directory is opened
//! relative to its parent with `O_NOFOLLOW`, and the walk returns to a parent through
//! the child's `..`, refusing one that is not the directory it came from (device and
//! inode).
//!
//! Every entry is unlinked without being looked at first: `unlinkat` removes a file, a
//! symlink (never its target), a socket or a device, and refuses a directory with
//! `EISDIR`, which the walk then empties and removes.

#![allow(dead_code)] // MUTANT

use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, Dir, Mode};
use rustix::io::Errno;

use crate::outputs::{DIRECTORY, Identity, back_to_parent, identity};

/// A directory being emptied.
struct Frame {
    /// Its name in its parent (unused for the top directory).
    name: OsString,
    id: Identity,
    /// The subdirectories still to empty and remove.
    subdirectories: Vec<OsString>,
}

/// Removes the directory `dir` and everything in it, at any depth. An error (a
/// directory the daemon's user may not read or write) stops the walk and leaves the
/// rest; `NotFound` means `dir` is already gone.
pub(crate) fn remove_tree(dir: &Path) -> std::io::Result<()> {
    let mut here = rustix::fs::openat(CWD, dir, DIRECTORY, Mode::empty())?;
    let mut stack = vec![Frame {
        name: OsString::new(),
        id: identity(&here)?,
        subdirectories: unlink_all_but_directories(&here)?,
    }];
    while let Some(mut top) = stack.pop() {
        if let Some(name) = top.subdirectories.pop() {
            let child = rustix::fs::openat(&here, name.as_os_str(), DIRECTORY, Mode::empty())?;
            let frame = Frame {
                name,
                id: identity(&child)?,
                subdirectories: unlink_all_but_directories(&child)?,
            };
            stack.extend([top, frame]);
            here = child;
            continue;
        }
        // `here` is empty: remove it from its parent, the next frame down.
        if let Some(parent) = stack.last() {
            here = back_to_parent(&here, parent.id)?;
            rustix::fs::unlinkat(&here, top.name.as_os_str(), AtFlags::REMOVEDIR)?;
        }
    }
    drop(here);
    std::fs::remove_dir(dir)
}

/// Unlinks every entry of `dir` that is not a directory, and returns the names of
/// those that are. The names are read first, then unlinked: no entry is removed while
/// the directory stream is being read.
fn unlink_all_but_directories(dir: &OwnedFd) -> std::io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name));
        }
    }
    let mut subdirectories = Vec::new();
    for name in names {
        match rustix::fs::unlinkat(dir, name.as_os_str(), AtFlags::empty()) {
            Err(Errno::ISDIR) => subdirectories.push(name),
            other => other?,
        }
    }
    Ok(subdirectories)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    /// A fresh scratch directory for one unit test, beside the test binary.
    fn scratch(name: &str) -> PathBuf {
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit")
            .join(name);
        let _ = remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Makes `levels` directories nested in `dir` (`d/d/.../d`), by descriptor (the
    /// deepest path is longer than `PATH_MAX`), with a file, a symlink and a FIFO at
    /// the bottom.
    fn deep(dir: &Path, levels: usize) {
        let flags = DIRECTORY;
        let mut here = rustix::fs::openat(CWD, dir, flags, Mode::empty()).expect("open");
        for _ in 0..levels {
            rustix::fs::mkdirat(&here, "d", Mode::from_raw_mode(0o755)).expect("mkdir");
            here = rustix::fs::openat(&here, "d", flags, Mode::empty()).expect("open");
        }
        rustix::fs::symlinkat("/", &here, "link").expect("symlink");
        let fifo = rustix::fs::FileType::Fifo;
        rustix::fs::mknodat(&here, "fifo", fifo, Mode::RUSR, 0).expect("mkfifo");
        let file = rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE;
        rustix::fs::openat(&here, "f", file, Mode::RUSR).expect("create");
    }

    /// Catches a recursive removal: 20,000 levels on a 256 KiB stack, an eighth of a
    /// tokio blocking thread's, where `std::fs::remove_dir_all` overflows (seen red:
    /// the test binary aborts). Also catches a removal that leaves anything behind,
    /// at the bottom or beside the tree, and one that follows a symlink (`link` names
    /// `/`).
    #[test]
    fn a_tree_of_any_depth_is_removed_without_recursion() {
        let dir = scratch("remove-deep");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("wide/a")).expect("mkdir");
        std::fs::write(top.join("wide/b"), b"b").expect("write");
        deep(&top, 20_000);
        let removed = top.clone();
        std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || remove_tree(&removed))
            .expect("spawn")
            .join()
            .expect("the removal finished")
            .expect("removed");
        assert!(std::fs::symlink_metadata(&top).is_err(), "the tree is left");
        assert!(Path::new("/").exists());
        let gone = remove_tree(&top).expect_err("already gone");
        assert_eq!(gone.kind(), std::io::ErrorKind::NotFound);
    }

    /// Catches an entry the daemon's user cannot remove (its directory is not
    /// writable) being skipped, or reported as success: the removal fails, so the
    /// clean step falls back to `podman unshare`.
    #[test]
    fn an_entry_that_cannot_be_unlinked_fails_the_removal() {
        let dir = scratch("remove-readonly");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("ro")).expect("mkdir");
        std::fs::write(top.join("ro/f"), b"f").expect("write");
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(top.join("ro"), mode(0o500)).expect("chmod");
        let why = remove_tree(&top).expect_err("ro/f stays");
        std::fs::set_permissions(top.join("ro"), mode(0o755)).expect("chmod back");
        assert_eq!(why.kind(), std::io::ErrorKind::PermissionDenied, "{why}");
        remove_tree(&top).expect("removed once writable");
    }
}
