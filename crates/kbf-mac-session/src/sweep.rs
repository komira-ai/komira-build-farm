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
//! deep tree can stop a deletion). A tree of the departing uid of any depth is removed:
//! below a fixed depth the walk moves the subtree up under the directory it is
//! emptying (a rename on one filesystem, nothing followed) and removes it from there,
//! so a lease that builds a deep tree cannot leave its slot dirty for good.
//!
//! Callers must have ended every process of the uid first (`user-delete` refuses
//! otherwise; `kill-uid` sweeps the crontab and `at` jobs only after its kill), so
//! nothing of the departing user races the walk. System daemons still may: an entry
//! that is gone by the time the walk examines, opens, lists or removes it is not an
//! error (it is what the sweep wanted); every other error still is.

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

/// How deeply nested a directory the walk searches for the uid's entries in another
/// owner's directories (the flag clear looks to [`PARK_DEPTH`], fewer descriptors). A tree
/// of the departing uid is removed whatever its depth.
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
    /// macOS's places that start a process of the user with no session of its own,
    /// which `user-delete` sweeps first: the crontab (`/usr/lib/cron` is a link to
    /// `/private/var/at`) and `at` jobs.
    #[must_use]
    pub fn macos_schedules() -> Self {
        Self {
            named: vec!["/private/var/at/tabs/{user}".to_owned()],
            owned: vec![PathBuf::from("/private/var/at/jobs")],
        }
    }

    /// macOS's other places besides the home folder (which the helper adds from where
    /// it made it): launchd's per-uid overrides and login items, the shared user
    /// folder and the temporary folders (`/private/var/folders` holds each user's
    /// `DARWIN_USER_TEMP_DIR` and cache folders).
    #[must_use]
    pub fn macos() -> Self {
        Self {
            named: [
                "/private/var/db/com.apple.xpc.launchd/disabled.{uid}.plist",
                "/private/var/db/com.apple.xpc.launchd/loginitems.{uid}.plist",
            ]
            .map(str::to_owned)
            .to_vec(),
            owned: vec![
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

/// Removes every entry of the directory `dir`, whoever owns it, as [`sweep`] removes the
/// uid's own: never through a link, never past `dir`'s filesystem, with the user flags
/// cleared only on entries `uid` owns (another owner's locked entry stays, and is an
/// error). For a directory that is the uid's alone, such as its home: `dir` itself
/// stays, and a `dir` that does not exist holds nothing.
#[must_use]
pub fn empty(dir: &Path, uid: u32) -> Swept {
    empty_keeping(dir, uid, &[])
}

/// [`empty`], but a directory of `dir` named in `keep` is emptied and kept, not removed
/// (a `/var/folders` entry keeps its `0`, `C` and `T`; anything else in it, a file or a
/// directory of another name, goes whole). A `keep` name that is not a directory of
/// `dir`'s own filesystem (a file, a link, a mount point) is removed like any other
/// entry.
#[must_use]
pub fn empty_keeping(dir: &Path, uid: u32, keep: &[&str]) -> Swept {
    let mut swept = Swept::default();
    let result = empty_base(dir, uid, keep, &mut swept);
    note(&mut swept, dir, result);
    swept
}

/// [`empty_keeping`]: every entry of `path` removed, each error recorded and the rest
/// gone on with.
fn empty_base(path: &Path, uid: u32, keep: &[&str], swept: &mut Swept) -> io::Result<()> {
    let Some(dir) = open_base(path)? else {
        return Ok(());
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    let park = Park::new(&dir);
    for name in listed(&dir)? {
        let here = path.join(&name);
        // An entry gone since the listing is neither removed nor an error.
        let skipped = &park.skipped;
        let result = tolerate(examine(&dir, &name), (&dir, &name, &here), skipped).and_then(
            |stat| match stat {
                Some(stat) if is_kept(&name, keep, &stat, dev) => {
                    empty_kept(&dir, &name, &here, &stat, uid, dev, swept).map(|()| 0)
                }
                Some(stat) => {
                    unlock_own(&dir, &name, &stat, rustix::process::geteuid().as_raw());
                    tolerate(
                        remove_entry(&dir, &name, &here, &stat, (uid, dev, 1), &park).map(|()| 1),
                        (&dir, &name, &here),
                        skipped,
                    )
                }
                None => Ok(0),
            },
        );
        swept.removed += *result.as_ref().unwrap_or(&0);
        note(swept, &here, result.map(drop));
    }
    note(swept, path, park.drain(uid, dev));
    Ok(())
}

/// Whether `name`, examined as `stat` in a directory on `dev`, is a directory
/// [`empty_keeping`] keeps.
fn is_kept(name: &OsStr, keep: &[&str], stat: &Stat, dev: StDev) -> bool {
    // Without short-circuits: all three are read.
    is_dir(stat) & (stat.st_dev == dev) & keep.iter().any(|k| OsStr::new(k) == name)
}

/// Empties the kept directory `name` of `dir`, examined as `stat`: each entry removed,
/// each error recorded against its path.
fn empty_kept(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    stat: &Stat,
    uid: u32,
    dev: StDev,
    swept: &mut Swept,
) -> io::Result<()> {
    let child = match open_child(dir, name, stat) {
        Ok(child) => child,
        // Gone since it was examined: it holds nothing.
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(why) => return Err(why),
    };
    let park = Park::new(&child);
    for entry in listed(&child)? {
        let there = here.join(&entry);
        let result = tolerate(
            examine_inner(&child, &entry).and_then(|inner| match inner {
                Some(inner) => {
                    remove_entry(&child, &entry, &there, &inner, (uid, dev, 2), &park).map(|()| 1)
                }
                None => Ok(0),
            }),
            (&child, &entry, &there),
            &park.skipped,
        );
        swept.removed += *result.as_ref().unwrap_or(&0);
        note(swept, &here.join(&entry), result.map(drop));
    }
    note(swept, here, park.drain(uid, dev));
    Ok(())
}

/// Records `result`'s error, if any, against `path`. Every error counts: the sweep is
/// then retried, never assumed done. (An entry that vanished is no error by the time it
/// gets here: see [`vanished`].)
fn note(swept: &mut Swept, path: &Path, result: io::Result<()>) {
    if let Err(why) = result {
        swept.errors.push(format!("{}: {why}", path.display()));
    }
}

/// `result`, with "no such file or directory" taken as success: an entry that is gone
/// between the walk's listing of it and its removal (a macOS daemon removing its own
/// temporary file under the lease user's folder, say, kbf#652) is what
/// the sweep wanted. Only that error: every other one still leaves the sweep
/// incomplete.
fn vanished(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
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
    let park = Park::new(&dir);
    remove_entry(&dir, name, path, &stat, (uid, dev, 1), &park)?;
    park.drain(uid, dev)?;
    swept.removed += 1;
    Ok(())
}

/// Removes every entry the uid owns below the plan directory `base`.
fn sweep_base(base: &Path, uid: u32, swept: &mut Swept) -> io::Result<()> {
    let Some(dir) = open_base(base)? else {
        return Ok(());
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    let park = Park::new(&dir);
    let walked = walk_owned(&dir, base, (uid, dev), (0, &park), swept);
    note(swept, base, park.drain(uid, dev));
    walked
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
    (uid, dev): (u32, StDev),
    (depth, park): (usize, &Park),
    swept: &mut Swept,
) -> io::Result<()> {
    for name in listed(dir)? {
        let here = path.join(&name);
        let result = visit(dir, &name, &here, (uid, dev), (depth, park), swept);
        note(swept, &here, result);
    }
    Ok(())
}

/// One entry of [`walk_owned`]'s directory `dir`.
fn visit(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    (uid, dev): (u32, StDev),
    (depth, park): (usize, &Park),
    swept: &mut Swept,
) -> io::Result<()> {
    let Some(stat) = skip_protected_top(examine(dir, name), depth, here)? else {
        return Ok(());
    };
    // Another filesystem mounted here is never entered.
    match (stat.st_dev == dev, stat.st_uid == uid, is_dir(&stat)) {
        (true, true, _) => {
            remove_entry(dir, name, here, &stat, (uid, dev, depth + 1), park)?;
            swept.removed += 1;
        }
        // Another owner's directory: searched for the uid's entries, down to the limit.
        // It is another lease's or user's, which may change it under the walk (a link put
        // where the directory was, a mode that keeps us out): what cannot be searched
        // is logged and skipped, never a failure of this uid's sweep. One gone since it
        // was examined holds nothing.
        (true, false, true) if depth < MAX_DEPTH => {
            let searched =
                vanished(open_child(dir, name, &stat).and_then(|child| {
                    walk_owned(&child, here, (uid, dev), (depth + 1, park), swept)
                }));
            if let Err(why) = searched {
                tracing::debug!(folder = %here.display(), "not searched: {why}");
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether `why` is the system refusing the process ("operation not permitted" or
/// "permission denied").
fn refused(why: &io::Error) -> bool {
    matches!(
        why.raw_os_error(),
        Some(code) if code == Errno::PERM.raw_os_error() || code == Errno::ACCESS.raw_os_error()
    )
}

/// Whether the entry `name` of `dir` cannot be opened by this process, because the system
/// refuses it ("operation not permitted" or "permission denied"; a link is not followed).
/// Such an entry is a protected store of the system (an Apple daemon's data vault made for
/// the pool user): not the pool user's to read, and so not the lease's to carry.
pub(crate) fn cannot_open(dir: &OwnedFd, name: &OsStr) -> bool {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    matches!(
        rustix::fs::openat(dir, name, flags, Mode::empty()),
        Err(Errno::PERM | Errno::ACCESS)
    )
}

/// [`cannot_open`] for the path `path`.
#[must_use]
pub fn path_is_protected(path: &Path) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    match open_base(parent) {
        Ok(Some(dir)) => cannot_open(&dir, name),
        // The parent cannot be opened either: what is in it cannot be read.
        Err(why) => refused(&why),
        Ok(None) => false,
    }
}

/// Whether the error `why` of an entry is a refusal of the system, this process is not
/// root, and it cannot open the entry `name` of `dir`. Not short-circuited: the three are
/// read together.
fn skippable(why: &io::Error, dir: &OwnedFd, name: &OsStr) -> bool {
    refused(why) & !rustix::process::geteuid().is_root() & cannot_open(dir, name)
}

/// `result`, with a refusal of the system taken as a success for the entry `name` of `dir`
/// (at `here`) that this process cannot open, when the process is not root: it is skipped
/// with one warning per path, and counted in `skipped`, so that a directory it keeps is no
/// error. The next lease runs as the same user and is refused the entry as well, so it
/// carries nothing from one lease to the next. An entry that can be opened still fails.
fn tolerate<T: Default>(
    result: io::Result<T>,
    (dir, name, here): (&OwnedFd, &OsStr, &Path),
    skipped: &std::cell::Cell<usize>,
) -> io::Result<T> {
    match result {
        Err(why) if skippable(&why, dir, name) => {
            warn_once(here);
            skipped.set(skipped.get() + 1);
            Ok(T::default())
        }
        other => other,
    }
}

/// Whether `why`, from the examination of an entry at `depth` of a swept base, is the
/// system refusing root a top-level entry (an Apple data vault) and nothing the uid's own:
/// a refusal ("operation not permitted") of a top-level entry only. Deeper, and any other
/// error, still fails the sweep.
fn protected_top_entry(depth: usize, why: &io::Error) -> bool {
    depth == 0 && why.raw_os_error() == Some(Errno::PERM.raw_os_error())
}

/// `examined`, the examination of an entry at `depth` of a swept base at `here`, with the
/// system's refusal of a top-level entry (Apple's data vaults refuse root without Full Disk
/// Access even to be examined; they are not the uid's, which cannot make one) taken as an
/// entry that is not there, said once.
fn skip_protected_top(
    examined: io::Result<Option<Stat>>,
    depth: usize,
    here: &Path,
) -> io::Result<Option<Stat>> {
    match examined {
        Err(why) if protected_top_entry(depth, &why) => {
            warn_once(here);
            Ok(None)
        }
        other => other,
    }
}

/// Logs that `path` is skipped, once for each path in the process.
fn warn_once(path: &Path) {
    static SEEN: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if first_time(&mut seen, path) {
        tracing::warn!(path = %path.display(), "not swept: the system refuses this entry, and the pool user cannot open it");
    }
}

/// Whether `path` is new to `seen`; it is added.
fn first_time(seen: &mut Vec<PathBuf>, path: &Path) -> bool {
    if seen.iter().any(|p| p == path) {
        return false;
    }
    seen.push(path.to_owned());
    true
}

#[cfg(target_os = "linux")]
type StDev = u64;
#[cfg(not(target_os = "linux"))]
type StDev = i32;

/// Removes `name` in `dir`, examined as `stat`: unlinked, or a directory emptied
/// (everything in it, whoever owns it) and removed. Only entries `uid` owns are
/// unlocked first. An entry that is gone by the time a step reaches it, `name` or
/// anything below it, is removed already (see [`vanished`]).
fn remove_entry(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    stat: &Stat,
    at: (u32, StDev, usize),
    park: &Park,
) -> io::Result<()> {
    vanished(remove_present(dir, name, here, stat, at, park))
}

/// [`remove_entry`] with every step's error as it is, "no such file or directory"
/// included.
fn remove_present(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    stat: &Stat,
    (uid, dev, depth): (u32, StDev, usize),
    park: &Park,
) -> io::Result<()> {
    if stat.st_dev != dev {
        return Err(io::Error::other(
            "another filesystem is mounted here; not crossed",
        ));
    }
    let placed = is_placed(dir, name, stat);
    if (stat.st_uid == uid) & !placed {
        unlock(dir, name, stat)?;
    }
    if !is_dir(stat) {
        return Ok(rustix::fs::unlinkat(dir, name, AtFlags::empty())?);
    }
    if depth > PARK_DEPTH {
        // Too deep to go on holding a descriptor per level: moved up under the directory
        // being emptied, to be removed from there (see [`Park`]).
        return park.take(dir, name, stat);
    }
    let child = open_child(dir, name, stat)?;
    let skipped_before = park.skipped.get();
    for entry in listed(&child)? {
        let inner_path = here.join(&entry);
        tolerate(
            examine_inner(&child, &entry).and_then(|inner| match inner {
                Some(inner) => remove_entry(
                    &child,
                    &entry,
                    &inner_path,
                    &inner,
                    (uid, dev, depth + 1),
                    park,
                ),
                None => Ok(()),
            }),
            (&child, &entry, &inner_path),
            &park.skipped,
        )?;
    }
    drop(child);
    if placed | (park.skipped.get() > skipped_before) {
        // This directory was placed by the system (emptied, never removed), or something
        // the pool user may not open is inside and stays with it, and so does this one
        // (counted, for the one above it).
        park.skipped.set(park.skipped.get() + 1);
        return Ok(());
    }
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?)
}

/// How deep the removal recurses before it moves what is below up to the top: it holds one
/// descriptor per level (a Mac's default is 256 in all), so this stays well under that.
const PARK_DEPTH: usize = 64;

/// How many names [`Park::take`] tries before it gives up.
const PARK_NAMES: usize = 4096;

/// Where a removal puts a subtree that is deeper than [`PARK_DEPTH`]: `dir`, the directory
/// being emptied (same filesystem, so a rename, and the subtree is only moved, never
/// copied or followed). The moved subtree is removed from there by [`Park::drain`]; it is
/// [`PARK_DEPTH`] levels shallower, so a tree of any depth is gone after as many passes
/// as it has multiples of that. A lease that builds a deep tree cannot make its slot
/// fail, and no depth of recursion is unbounded.
struct Park<'a> {
    dir: &'a OwnedFd,
    names: std::cell::RefCell<Vec<OsString>>,
    next: std::cell::Cell<u32>,
    /// How many entries the removal left because the pool user may not open them.
    skipped: std::cell::Cell<usize>,
}

impl<'a> Park<'a> {
    fn new(dir: &'a OwnedFd) -> Self {
        Self {
            dir,
            names: std::cell::RefCell::default(),
            next: std::cell::Cell::new(0),
            skipped: std::cell::Cell::new(0),
        }
    }

    /// Moves `name` of `from`, judged as `judged`, up to a name of its own in the park.
    /// It is looked at again first: an entry replaced since it was judged is not moved
    /// (an error), one gone is removed already. The move is exclusive, so a name made
    /// ahead in the tree being emptied is never replaced, only skipped.
    fn take(&self, from: &OwnedFd, name: &OsStr, judged: &Stat) -> io::Result<()> {
        let same = examine(from, name)?
            .is_some_and(|now| (now.st_dev, now.st_ino) == (judged.st_dev, judged.st_ino));
        if !same {
            return Err(io::Error::other("replaced while being examined"));
        }
        for _ in 0..PARK_NAMES {
            let n = self.next.get();
            self.next.set(n + 1);
            let to = OsString::from(format!(".kbf-deep-{}-{n}", std::process::id()));
            match rename_excl(from, name, self.dir, to.as_os_str()) {
                Ok(()) => {
                    self.names.borrow_mut().push(to);
                    return Ok(());
                }
                // Taken: the next name.
                Err(Errno::EXIST) => {}
                Err(why) => return Err(why.into()),
            }
        }
        Err(io::Error::other(
            "no free name to move a deep tree up under",
        ))
    }

    /// Removes what was moved up, which may move more up, until nothing is left.
    fn drain(&self, uid: u32, dev: StDev) -> io::Result<()> {
        loop {
            let Some(name) = self.names.borrow_mut().pop() else {
                return Ok(());
            };
            examine(self.dir, &name)?.map_or(Ok(()), |stat| {
                remove_entry(
                    self.dir,
                    &name,
                    Path::new(&name),
                    &stat,
                    (uid, dev, 1),
                    self,
                )
            })?;
        }
    }
}

/// `renameat` that does not replace what is at the new name (`EEXIST` instead).
#[cfg(target_os = "linux")]
fn rename_excl(from: &OwnedFd, old: &OsStr, to: &OwnedFd, new: &OsStr) -> Result<(), Errno> {
    rustix::fs::renameat_with(from, old, to, new, rustix::fs::RenameFlags::NOREPLACE)
}

/// `renameatx_np` with `RENAME_EXCL`: does not replace what is at the new name.
#[cfg(target_os = "macos")]
fn rename_excl(from: &OwnedFd, old: &OsStr, to: &OwnedFd, new: &OsStr) -> Result<(), Errno> {
    use std::os::fd::AsRawFd as _;
    let (Ok(old), Ok(new)) = (
        std::ffi::CString::new(old.as_bytes()),
        std::ffi::CString::new(new.as_bytes()),
    ) else {
        return Err(Errno::INVAL);
    };
    // SAFETY: both names are NUL-terminated and outlive the call, both descriptors are
    // open directories borrowed for it.
    let renamed = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            old.as_ptr(),
            to.as_raw_fd(),
            new.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if renamed == 0 {
        return Ok(());
    }
    Err(Errno::from_raw_os_error(
        io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    ))
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

/// [`list`], and in tests the point after a listing at which a test makes entries
/// vanish. A directory removed since it was opened lists as empty (rustix's [`Dir`]
/// reads it so), never as an error.
fn listed(dir: &OwnedFd) -> io::Result<Vec<OsString>> {
    let names = list(dir)?;
    #[cfg(test)]
    tests::reached(tests::Point::Listed);
    Ok(names)
}

/// `name` in `dir`, examined without following a link; `None` if it is gone. In tests,
/// the point after an examination at which a test makes entries vanish.
fn examine(dir: &OwnedFd, name: &OsStr) -> io::Result<Option<Stat>> {
    let stat = present(rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW));
    #[cfg(test)]
    tests::reached(tests::Point::Examined(name));
    stat
}

/// [`examine`] of an entry below a folder the sweep keeps, after [`unlock_own`].
fn examine_inner(dir: &OwnedFd, name: &OsStr) -> io::Result<Option<Stat>> {
    let stat = examine(dir, name);
    if let Ok(Some(found)) = &stat {
        unlock_own(dir, name, found, rustix::process::geteuid().as_raw());
    }
    stat
}

/// Gives the directory `name` of `dir`, examined as `stat`, back the owner's `rwx` when the
/// process (`euid`, not root) owns it and the owner cannot open it (no `r` or no `x`): the owner can always do that,
/// so a lease that took every permission from a folder to keep its files past the clean
/// gains nothing, and the sweep goes on into it. A refused change leaves it as it is (a
/// protected store of the system, which is then skipped). Not done for root, nor for an
/// entry another uid owns.
fn unlock_own(dir: &OwnedFd, name: &OsStr, stat: &Stat, euid: u32) {
    #[cfg(test)]
    if !tests::unlocking() {
        return;
    }
    let at = if cfg!(target_os = "macos") {
        AtFlags::SYMLINK_NOFOLLOW
    } else {
        AtFlags::empty()
    };
    let owner_needs_it = is_dir(stat) & (stat.st_uid == euid) & (euid != 0) & !owner_can_open(stat);
    if owner_needs_it {
        let _ = rustix::fs::chmodat(dir, name, owner_rwx(stat), at);
    }
}

/// Whether the owner has `r` and `x` on `stat`: it can open and search it.
#[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
fn owner_can_open(stat: &Stat) -> bool {
    stat.st_mode as u32 & 0o500 == 0o500
}

/// The flag only the system sets (`sunlnk`, `SF_NOUNLINK`): the entry may not be unlinked.
const SF_NOUNLINK: u32 = 0x0010_0000;

/// The attribute SIP puts on what it protects.
#[cfg(target_os = "macos")]
const ROOTLESS: &std::ffi::CStr = c"com.apple.rootless";

/// Whether a directory with the flags `flags` and the SIP attribute (`rootless`, or not)
/// was placed by the system: it carries `sunlnk` or `com.apple.rootless`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn placed(flags: u32, rootless: bool) -> bool {
    flags & SF_NOUNLINK != 0 || rootless
}

/// Whether the directory `name` of `dir`, examined as `stat`, was placed by the system
/// (see [`placed`]) below a folder it keeps: neither its flags nor its ACL are the pool
/// user's to change, and it is never removed; what is in it is the lease's, and goes. Only
/// for a process that is not root, like the pool user's own sweeps.
fn is_placed(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> bool {
    let not_root = !rustix::process::geteuid().is_root();
    not_root & is_dir(stat) & system_placed(dir, name, stat)
}

#[cfg(target_os = "macos")]
fn system_placed(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> bool {
    placed(
        stat.st_flags,
        crate::macos::dir_has_xattr(dir, name, ROOTLESS),
    )
}

#[cfg(not(target_os = "macos"))]
thread_local! {
    /// The names a test says the system placed (nothing is, off macOS).
    static PRETENDED: std::cell::RefCell<Vec<OsString>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Off macOS nothing is placed by the system, but what a test names.
#[cfg(not(target_os = "macos"))]
fn system_placed(_dir: &OwnedFd, name: &OsStr, _stat: &Stat) -> bool {
    PRETENDED.with_borrow(|names| names.iter().any(|n| n == name))
}

/// For tests off macOS: the directories of these names, on this thread, count as placed by
/// the system.
#[cfg(not(target_os = "macos"))]
#[doc(hidden)]
pub fn pretend_placed(names: &[&str]) {
    PRETENDED.with_borrow_mut(|n| *n = names.iter().map(Into::into).collect());
}

/// [`is_placed`] for the path `path`.
#[must_use]
pub fn path_is_system_placed(path: &Path) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let Ok(Some(dir)) = open_base(parent) else {
        return false;
    };
    match present(rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)) {
        Ok(Some(stat)) => is_placed(&dir, name, &stat),
        _ => false,
    }
}

/// The names in `dir`, read whole before any is removed.
pub(crate) fn list(dir: &OwnedFd) -> io::Result<Vec<OsString>> {
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

/// The user flags [`clear_flags`] clears: `uchg`, `uappnd`, `nodump`, `opaque` and
/// `hidden`, as the bits of `st_flags` (`UF_IMMUTABLE`, `UF_APPEND`, `UF_NODUMP`,
/// `UF_OPAQUE`, `UF_HIDDEN`), never spelled as `chflags` words: `nodump` there is the flag
/// itself, and its opposite is `dump`. The flags only root may change (`schg`,
/// `restricted`) are not in it: they are not the pool user's to set.
pub const CLEARED_FLAGS: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x8000;

/// Clears [`CLEARED_FLAGS`] on the directory `dir` itself, whoever owns it (the caller
/// names a home or a `/var/folders` entry, a place it knows), then, with the owner's
/// mode made `u+rwx`, on every entry below it that `uid` owns, at any depth: never through
/// a link, never past `dir`'s filesystem, and nothing of another owner's is changed (a
/// hard link to a system file keeps the flags and mode of its inode). The root first, so
/// a locked root does not stop the walk below it, and a clean's removal after this
/// finds nothing locked. Each error is recorded against its path and the walk goes on.
/// Flags are macOS's: elsewhere only the modes change.
#[must_use]
pub fn clear_flags(dir: &Path, uid: u32) -> Swept {
    let mut swept = Swept::default();
    let result = clear_base(dir, uid, &mut swept);
    note(&mut swept, dir, result);
    swept
}

/// [`clear_flags`] for a `/var/folders` entry, as the uid: `dir` itself and each directory
/// of it named in `keep` keep the flags, ACLs and mode the system gave them (it protects
/// those from the user and from root alike); everything below them, and every other entry
/// of `dir`, is cleared as [`clear_flags`] does. Each error is recorded against its path.
#[must_use]
pub fn clear_flags_within(dir: &Path, uid: u32, keep: &[&str]) -> Swept {
    let mut swept = Swept::default();
    let result = clear_within(dir, uid, keep, &mut swept);
    note(&mut swept, dir, result);
    swept
}

fn clear_within(path: &Path, uid: u32, keep: &[&str], swept: &mut Swept) -> io::Result<()> {
    let Some(dir) = open_base(path)? else {
        return Ok(());
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    for name in listed(&dir)? {
        let here = path.join(&name);
        let skipped = std::cell::Cell::new(0);
        let result =
            tolerate(examine(&dir, &name), (&dir, &name, &here), &skipped).and_then(|stat| {
                match stat {
                    Some(stat) if is_kept(&name, keep, &stat, dev) => {
                        open_child(&dir, &name, &stat)
                            .and_then(|child| clear_below(&child, &here, uid, dev, 1, swept))
                    }
                    Some(_) => tolerate(
                        clear_entry(&dir, &name, &here, (uid, dev, 0), swept),
                        (&dir, &name, &here),
                        &skipped,
                    ),
                    None => Ok(()),
                }
            });
        note(swept, &here, vanished(result));
    }
    Ok(())
}

/// [`clear_flags`] from the opened `path`.
fn clear_base(path: &Path, uid: u32, swept: &mut Swept) -> io::Result<()> {
    let Some(dir) = open_base(path)? else {
        return Ok(());
    };
    clear_root(&dir)?;
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    clear_below(&dir, path, uid, dev, 0, swept)
}

/// Every entry of `dir`, cleared; errors are recorded, not returned (a listing that
/// fails is).
fn clear_below(
    dir: &OwnedFd,
    path: &Path,
    uid: u32,
    dev: StDev,
    depth: usize,
    swept: &mut Swept,
) -> io::Result<()> {
    for name in listed(dir)? {
        let here = path.join(&name);
        let result = tolerate(
            clear_entry(dir, &name, &here, (uid, dev, depth), swept),
            (dir, &name, &here),
            &std::cell::Cell::new(0),
        );
        note(swept, &here, vanished(result));
    }
    Ok(())
}

/// One entry of [`clear_below`]'s directory: its flags and mode if `uid` owns it, then
/// what is below it if it is a directory on the same filesystem.
fn clear_entry(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    (uid, dev, depth): (u32, StDev, usize),
    swept: &mut Swept,
) -> io::Result<()> {
    let Some(stat) = examine_inner(dir, name)? else {
        return Ok(());
    };
    if stat.st_dev != dev {
        return Ok(());
    }
    // A directory the system placed keeps its flags and ACL; what is in it is cleared.
    if (stat.st_uid == uid) & !is_placed(dir, name, &stat) {
        clear_marked(dir, name, &stat)?;
        swept.removed += 1;
        make_owner_rwx(dir, name, &stat)?;
    }
    if !is_dir(&stat) {
        return Ok(());
    }
    if depth >= PARK_DEPTH {
        // Not searched deeper, so that no more than [`PARK_DEPTH`] descriptors are open
        // (a Mac's soft limit can be 256 in all): the removal that follows clears the
        // locks of what the uid owns as it goes, and a root removal needs nothing else.
        return Ok(());
    }
    let child = open_child(dir, name, &stat)?;
    clear_below(&child, here, uid, dev, depth + 1, swept)
}

/// Adds the owner's `rwx` to the entry `name` of `dir`, judged as `stat`, through a
/// descriptor opened without following a link and checked to be the entry judged: a link
/// swapped in since is never followed (`chmod` has no no-follow form on Linux), and one
/// that cannot be opened (a link, a socket, a file root alone may read) is left as it is,
/// which a removal by root does not need changed.
fn make_owner_rwx(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> io::Result<()> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    // Linux has no mode of a link to set (and refuses the flag for any entry).
    let at = if cfg!(target_os = "macos") {
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW
    } else {
        rustix::fs::AtFlags::empty()
    };
    let (opened, file) = match rustix::fs::openat(dir, name, flags, Mode::empty()) {
        Ok(file) => (rustix::fs::fstat(&file)?, Some(file)),
        // A directory its owner took every permission from cannot be opened, by the
        // owner (a clean that runs as the uid): it is judged again by name, and its mode
        // set by name, never on a link.
        Err(rustix::io::Errno::ACCESS)
            if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_symlink() =>
        {
            (
                rustix::fs::statat(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?,
                None,
            )
        }
        Err(_) => return Ok(()),
    };
    if (opened.st_dev, opened.st_ino) != (stat.st_dev, stat.st_ino) {
        return Ok(());
    }
    match file {
        Some(file) => Ok(rustix::fs::fchmod(&file, owner_rwx(stat))?),
        None => Ok(rustix::fs::chmodat(dir, name, owner_rwx(stat), at)?),
    }
}

/// The permission bits of `stat` with the owner's `rwx` added.
#[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
fn owner_rwx(stat: &Stat) -> Mode {
    Mode::from_raw_mode(((stat.st_mode as u32 & 0o7777) | 0o700) as _)
}

/// Clears [`CLEARED_FLAGS`] of the directory `dir` itself.
#[cfg(target_os = "macos")]
fn clear_root(dir: &OwnedFd) -> io::Result<()> {
    crate::macos::clear_dir_flags(dir, CLEARED_FLAGS)
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)] // the macOS variant can fail
fn clear_root(_dir: &OwnedFd) -> io::Result<()> {
    Ok(())
}

/// Clears [`CLEARED_FLAGS`] of `name` in `dir`, without following a link.
#[cfg(target_os = "macos")]
fn clear_marked(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> io::Result<()> {
    crate::macos::clear_flags_of(dir, name, stat.st_flags, CLEARED_FLAGS)
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)] // the macOS variant can fail
fn clear_marked(_dir: &OwnedFd, _name: &OsStr, _stat: &Stat) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests;
