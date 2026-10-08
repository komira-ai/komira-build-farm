//! What `user-delete` removes besides the user record: everything keyed by the lease
//! user's name or uid that would outlive the account (S4.2, "Sweep on delete").
//!
//! Root walks places other lease users can write while their leases run, so the walk
//! never follows a symbolic link: every directory is opened relative to its parent
//! with `O_NOFOLLOW` and checked to be the directory that was examined (device and
//! inode), every entry is examined with `AT_SYMLINK_NOFOLLOW`, and a link is unlinked,
//! never followed. On macOS the user flags that block removal (`uchg`, `uappnd`) are
//! cleared, but only on entries the departing uid owns: an entry of another owner (a
//! hard link to a system file, say) is unlinked as it is and never changed. The walk
//! stays on the filesystem of the directory it starts in. It does not descend into
//! another owner's directories nested deeper than [`MAX_DEPTH`] (so no other user's
//! deep tree can stop a deletion), and it refuses to remove a directory of the
//! departing uid nested deeper than that (so the departing user's own deep tree fails
//! its deletion closed).
//!
//! Callers must have ended every process of the uid first (`user-delete` refuses
//! otherwise), so nothing of the departing user races the walk.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// How deeply nested a directory the walk enters. Below it, another owner's
/// directories are not searched; a directory of the departing uid that reaches below
/// it is an error: the lease user built it to make its own deletion fail, which fails
/// closed (the user stays, the node is looked at), never open.
pub const MAX_DEPTH: usize = 256;

/// Where the sweep looks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepPlan {
    /// Single entries named after the user, removed whole (a directory with all it
    /// holds): `{user}` and `{uid}` stand for the user's name and uid. The parent
    /// directories are fixed, root-owned paths.
    pub named: Vec<String>,
    /// Directories in which every entry the uid owns is removed, at any depth.
    pub owned: Vec<PathBuf>,
}

impl SweepPlan {
    /// macOS's places besides the home folder (which the helper adds from where it
    /// made it): the crontab (`/usr/lib/cron` is a link to `/private/var/at`),
    /// launchd's per-uid overrides and login items, `at` jobs, the shared user folder
    /// and the temporary folders (`/private/var/folders` holds each user's
    /// `DARWIN_USER_TEMP_DIR` and cache folders).
    #[must_use]
    pub fn macos() -> Self {
        Self {
            named: [
                "/private/var/at/tabs/{user}",
                "/private/var/db/com.apple.xpc.launchd/disabled.{uid}.plist",
                "/private/var/db/com.apple.xpc.launchd/loginitems.{uid}.plist",
            ]
            .map(str::to_owned)
            .to_vec(),
            owned: vec![
                PathBuf::from("/private/var/at/jobs"),
                // The shared user folder.
                Path::new("/Users").join("Shared"),
                PathBuf::from("/private/tmp"),
                PathBuf::from("/private/var/tmp"),
                PathBuf::from("/private/var/folders"),
            ],
        }
    }
}

/// What a sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Entries removed at the top of what they held (a removed directory counts once).
    pub removed: usize,
    /// Each place that could not be swept, and why. Empty when the sweep is complete.
    pub errors: Vec<String>,
}

/// Sweeps `plan` for the user `user` with uid `uid`.
#[must_use]
pub fn sweep(plan: &SweepPlan, user: &str, uid: u32) -> Swept {
    let mut swept = Swept::default();
    for template in &plan.named {
        let path = PathBuf::from(
            template
                .replace("{user}", user)
                .replace("{uid}", &uid.to_string()),
        );
        let result = remove_named(&path, uid, &mut swept);
        note(&mut swept, &path, result);
    }
    for base in &plan.owned {
        let result = sweep_base(base, uid, &mut swept);
        note(&mut swept, base, result);
    }
    swept
}

/// Records `result`'s error, if any, against `path`. Every error counts, an entry that
/// vanished during the walk included: the sweep is then retried, never assumed done.
fn note(swept: &mut Swept, path: &Path, result: io::Result<()>) {
    if let Err(why) = result {
        swept.errors.push(format!("{}: {why}", path.display()));
    }
}

/// `result`, with "no such file or directory" as `None`: a place of the plan that does
/// not exist holds nothing to remove.
fn present<T>(result: Result<T, Errno>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Errno::NOENT) => Ok(None),
        Err(why) => Err(why.into()),
    }
}

/// Opens a fixed directory of the plan, not following its last component if that is
/// a link; `None` if it does not exist.
fn open_base(path: &Path) -> io::Result<Option<OwnedFd>> {
    present(rustix::fs::openat(CWD, path, DIRECTORY, Mode::empty()))
}

/// Removes the entry at `path` (whatever it is, a whole directory included).
fn remove_named(path: &Path, uid: u32, swept: &mut Swept) -> io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a path to an entry",
        ));
    };
    let Some(dir) = open_base(parent)? else {
        return Ok(());
    };
    let Some(stat) = present(rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW))? else {
        return Ok(());
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    remove_entry(&dir, name, &stat, uid, dev, 1)?;
    swept.removed += 1;
    Ok(())
}

/// Removes every entry the uid owns below the plan directory `base`.
fn sweep_base(base: &Path, uid: u32, swept: &mut Swept) -> io::Result<()> {
    let Some(dir) = open_base(base)? else {
        return Ok(());
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    walk_owned(&dir, base, uid, dev, 0, swept)
}

/// Removes every entry of `dir` that `uid` owns, descending into directories it does
/// not own. Errors below `dir` are recorded per entry and the walk goes on, so one
/// stuck entry does not hide the others.
///
/// # Errors
/// `dir` cannot be listed.
fn walk_owned(
    dir: &OwnedFd,
    path: &Path,
    uid: u32,
    dev: StDev,
    depth: usize,
    swept: &mut Swept,
) -> io::Result<()> {
    for name in list(dir)? {
        let here = path.join(&name);
        let result = visit(dir, &name, &here, uid, dev, depth, swept);
        note(swept, &here, result);
    }
    Ok(())
}

/// One entry of [`walk_owned`]'s directory `dir`.
fn visit(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    uid: u32,
    dev: StDev,
    depth: usize,
    swept: &mut Swept,
) -> io::Result<()> {
    let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    // Another filesystem mounted here is never entered.
    match (stat.st_dev == dev, stat.st_uid == uid, is_dir(&stat)) {
        (true, true, _) => {
            remove_entry(dir, name, &stat, uid, dev, depth + 1)?;
            swept.removed += 1;
        }
        // Another owner's directory: searched for the uid's entries, down to the limit.
        (true, false, true) if depth < MAX_DEPTH => {
            let child = open_child(dir, name, &stat)?;
            walk_owned(&child, here, uid, dev, depth + 1, swept)?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(target_os = "linux")]
type StDev = u64;
#[cfg(not(target_os = "linux"))]
type StDev = i32;

/// Removes `name` in `dir`, examined as `stat`: unlinked, or a directory emptied
/// (everything in it, whoever owns it) and removed. Only entries `uid` owns are
/// unlocked first.
fn remove_entry(
    dir: &OwnedFd,
    name: &OsStr,
    stat: &Stat,
    uid: u32,
    dev: StDev,
    depth: usize,
) -> io::Result<()> {
    if stat.st_dev != dev {
        return Err(io::Error::other(
            "another filesystem is mounted here; not crossed",
        ));
    }
    if stat.st_uid == uid {
        unlock(dir, name, stat)?;
    }
    if !is_dir(stat) {
        return Ok(rustix::fs::unlinkat(dir, name, AtFlags::empty())?);
    }
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    let child = open_child(dir, name, stat)?;
    for entry in list(&child)? {
        let inner = rustix::fs::statat(&child, &entry, AtFlags::SYMLINK_NOFOLLOW)?;
        remove_entry(&child, &entry, &inner, uid, dev, depth + 1)?;
    }
    drop(child);
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?)
}

fn too_deep() -> io::Error {
    io::Error::other(format!("nested deeper than {MAX_DEPTH} directories"))
}

#[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
fn is_dir(stat: &Stat) -> bool {
    FileType::from_raw_mode(stat.st_mode as _) == FileType::Directory
}

/// Opens directory `name` in `dir` without following a link, and checks it is the
/// directory `stat` examined: one swapped in since is refused.
fn open_child(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> io::Result<OwnedFd> {
    let child = rustix::fs::openat(dir, name, DIRECTORY, Mode::empty())?;
    let opened = rustix::fs::fstat(&child)?;
    if (opened.st_dev, opened.st_ino) != (stat.st_dev, stat.st_ino) {
        return Err(io::Error::other("replaced while being examined"));
    }
    Ok(child)
}

/// The names in `dir`, read whole before any is removed.
fn list(dir: &OwnedFd) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_owned());
        }
    }
    Ok(names)
}

/// Clears the user flags that block removing an entry or the entries of a directory
/// (`uchg`, `uappnd`), without following a link. Root may remove what any ACL denies,
/// so ACLs are left alone. Linux's equivalents need a capability lease users lack.
#[cfg(target_os = "macos")]
fn unlock(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> io::Result<()> {
    crate::macos::clear_user_flags(dir, name, stat.st_flags)
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)] // the macOS variant can fail
fn unlock(_dir: &OwnedFd, _name: &OsStr, _stat: &Stat) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests;
