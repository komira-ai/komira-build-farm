//! Removing a directory tree an action shaped, without recursion and without
//! following a symlink.
//!
//! An action can leave a tree deeper than any path the kernel accepts, directories it
//! made unreadable or unwritable, and symlinks to host paths. So the tree is removed by
//! descriptor: each directory is opened relative to its parent with `O_NOFOLLOW`, its
//! entries are listed whole before any is unlinked (deleting while reading a directory
//! skips entries on some filesystems), a directory missing any of its owner's `rwx`
//! bits gets them back first (never through a symlink), on macOS every entry first
//! loses its ACL (one can deny even the owner deletion) and the immutable and
//! append-only flags (`chflags uchg`, `uappnd`) if the action set them, and a symlink
//! is unlinked, never followed. The stack of
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
/// its owner's `rwx` bits if it lacks any. `None` when it is absent. On macOS it first
/// loses the user flags that forbid removing it or its entries ([`unlock`]).
#[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
fn prepare(dir: &OwnedFd, name: &OsStr) -> std::io::Result<Option<bool>> {
    let stat = match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    #[cfg(target_os = "macos")]
    {
        unlock(dir, name, stat.st_flags)?;
        clear_acl(dir, name)?;
    }
    let mode = stat.st_mode as u32;
    if FileType::from_raw_mode(stat.st_mode as _) != FileType::Directory {
        return Ok(Some(false));
    }
    if mode & 0o700 != 0o700 {
        let mode = Mode::from_raw_mode(((mode & 0o7777) | 0o700) as _);
        chmod_dir(dir, name, mode)?;
    }
    Ok(Some(true))
}

/// Gives `name` in `dir` the permission bits `mode`, never through a symlink: an
/// action that swapped the directory for a link since it was examined must not get
/// the permissions changed on what the link names. Linux has no `fchmodat` flag that
/// refuses a link (rustix reports `AT_SYMLINK_NOFOLLOW` unsupported), and `fchmod`
/// refuses an `O_PATH` descriptor; so the directory is opened `O_PATH` without
/// following a link (a link is then `ENOTDIR`) and changed through its
/// `/proc/self/fd` entry, which names that directory and nothing else.
#[cfg(target_os = "linux")]
fn chmod_dir(dir: &OwnedFd, name: &OsStr, mode: Mode) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let only = OFlags::PATH
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    let opened = rustix::fs::openat(dir, name, only, Mode::empty())?;
    let path = format!("/proc/self/fd/{}", opened.as_raw_fd());
    Ok(rustix::fs::chmodat(
        CWD,
        path.as_str(),
        mode,
        AtFlags::empty(),
    )?)
}

/// Gives `name` in `dir` the permission bits `mode`, never through a symlink (a link
/// swapped in since is changed itself, which harms nothing).
#[cfg(not(target_os = "linux"))]
fn chmod_dir(dir: &OwnedFd, name: &OsStr, mode: Mode) -> std::io::Result<()> {
    Ok(rustix::fs::chmodat(
        dir,
        name,
        mode,
        AtFlags::SYMLINK_NOFOLLOW,
    )?)
}

/// The user flags that stop an entry, or the entries of a directory, being removed
/// or changed: `uchg` and `uappnd`. A file's owner sets them without privilege
/// (`chflags uchg`), so an action can, and its lease directory would then never go.
/// Linux's equivalents (`chattr +i`, `+a`) need `CAP_LINUX_IMMUTABLE`, which an
/// action run as an unprivileged user does not have, so Linux has nothing to undo.
#[cfg(target_os = "macos")]
const LOCKING_FLAGS: u32 = libc::UF_IMMUTABLE | libc::UF_APPEND;

/// Clears the [`LOCKING_FLAGS`] of `name` in `dir`, whose flags are `flags`, without
/// following a symlink (`setattrlistat` with `FSOPT_NOFOLLOW`; macOS has no
/// `chflagsat`). The other flags are kept as they are.
#[cfg(target_os = "macos")]
fn unlock(dir: &OwnedFd, name: &OsStr, flags: u32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    if flags & LOCKING_FLAGS == 0 {
        return Ok(());
    }
    let name = std::ffi::CString::new(name.as_bytes())?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_FLAGS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut value: u32 = flags & !LOCKING_FLAGS;
    // SAFETY: `name` is NUL-terminated, `list` an attrlist that asks for the common
    // flags only, and `value` the u32 that attribute is (a setattrlist buffer has no
    // length prefix); all three outlive the call.
    let set = unsafe {
        libc::setattrlistat(
            dir.as_raw_fd(),
            name.as_ptr(),
            (&raw mut list).cast(),
            (&raw mut value).cast(),
            std::mem::size_of::<u32>(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    if set != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `KAUTH_FILESEC_MAGIC` (`sys/kauth.h`): the first word of a `kauth_filesec`.
#[cfg(target_os = "macos")]
const KAUTH_FILESEC_MAGIC: u32 = 0x012c_c16d;

/// `KAUTH_FILESEC_NOACL`: an entry count that means "no ACL at all".
#[cfg(target_os = "macos")]
const KAUTH_FILESEC_NOACL: u32 = u32::MAX;

/// Removes any ACL of `name` in `dir`, without following a symlink. On macOS an
/// entry's owner can add ACL entries that deny everyone, the owner included, the
/// right to delete it (`chmod +a "everyone deny delete"`) or a directory's entries
/// (`delete_child`), which no permission bit undoes; the owner can always remove the
/// ACL (`chmod -N`). There is no `acl_set_link_np` relative to a descriptor, so this is
/// `setattrlistat` with `FSOPT_NOFOLLOW` setting `ATTR_CMN_EXTENDED_SECURITY` to a
/// `kauth_filesec` whose entry count is `KAUTH_FILESEC_NOACL`, which the kernel takes
/// as "remove the ACL". Done for every entry (one call each: nothing cheaper says
/// whether an entry has an ACL); a filesystem without ACLs (`ENOTSUP`) has none.
#[cfg(target_os = "macos")]
fn clear_acl(dir: &OwnedFd, name: &OsStr) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // An attrreference_t (data offset from itself, length), then the kauth_filesec:
    // magic, owner and group GUIDs (zero: not set), the ACL's entry count and flags.
    const FILESEC: usize = 4 + 16 + 16 + 4 + 4;
    let mut buffer = [0u8; 8 + FILESEC];
    buffer[0..4].copy_from_slice(&8_i32.to_ne_bytes());
    buffer[4..8].copy_from_slice(&(FILESEC as u32).to_ne_bytes());
    buffer[8..12].copy_from_slice(&KAUTH_FILESEC_MAGIC.to_ne_bytes());
    buffer[44..48].copy_from_slice(&KAUTH_FILESEC_NOACL.to_ne_bytes());
    let name = std::ffi::CString::new(name.as_bytes())?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_EXTENDED_SECURITY,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // SAFETY: `name` is NUL-terminated, `list` asks for the extended security only,
    // and `buffer` is that attribute as setattrlist reads it (an attrreference_t
    // whose data, a kauth_filesec of `FILESEC` bytes, follows it in the buffer); all
    // three outlive the call.
    let set = unsafe {
        libc::setattrlistat(
            dir.as_raw_fd(),
            name.as_ptr(),
            (&raw mut list).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    if set != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOTSUP) {
            return Err(error);
        }
    }
    Ok(())
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
    remove_tree_at(&top, name)
}

/// Removes `name` in the directory `top` and everything below it, as [`remove_tree`]
/// does, without a path: `name` (one path component, as a directory listing gives it)
/// may be a file or a symlink (unlinked) or absent (nothing to do), and is looked up
/// in `top` itself, so a caller that opened `top` with `O_NOFOLLOW` removes nothing
/// through a symlink at any level.
///
/// # Errors
/// As [`remove_tree`].
pub fn remove_tree_at(top: &OwnedFd, name: &OsStr) -> std::io::Result<()> {
    let path = Path::new(name);
    match prepare(top, name)? {
        None => return Ok(()),
        Some(false) => return Ok(rustix::fs::unlinkat(top, name, AtFlags::empty())?),
        Some(true) => {}
    }
    let mut here = rustix::fs::openat(top, name, DIRECTORY, Mode::empty())?;
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
    rustix::fs::unlinkat(top, name, AtFlags::REMOVEDIR)?;
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

    /// Catches the permission repair following a symlink an action swapped in for a
    /// directory between the check and the change: the directory the link names keeps
    /// its mode.
    #[test]
    fn giving_back_permissions_never_follows_a_link() {
        use std::os::unix::fs::PermissionsExt as _;
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps")
            .join("kbf-outputs-unit")
            .join(format!("nofollow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("host")).expect("mkdir");
        let host = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(dir.join("host"), host).expect("chmod");
        std::os::unix::fs::symlink(dir.join("host"), dir.join("link")).expect("symlink");
        let top = rustix::fs::openat(CWD, &dir, DIRECTORY, Mode::empty()).expect("open");
        // On Linux the change is refused; on macOS it lands on the link itself.
        let _ = chmod_dir(&top, OsStr::new("link"), Mode::RWXU);
        let mode = std::fs::metadata(dir.join("host"))
            .expect("host")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "the link's target was changed");
        // A directory itself is changed.
        chmod_dir(&top, OsStr::new("host"), Mode::RWXU).expect("chmod a directory");
        let mode = std::fs::metadata(dir.join("host"))
            .expect("host")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::remove_dir_all(&dir).expect("clean");
    }
}
