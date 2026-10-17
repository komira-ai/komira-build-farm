//! The start-up sweep of the scratch root: every action a previous daemon recorded
//! ([`crate::record`]) and left running is killed, then every lease directory it left
//! behind is removed.
//!
//! A recorded action is killed as the daemon kills a lease's processes: SIGKILL to its
//! process group and to every process found in it or below it, again until a snapshot
//! of the process table shows none alive. Only the recorded process group is touched,
//! and only while it can still be the action's: a record of an earlier boot names
//! nothing; the record names the leader by pid and start time, and a live process under
//! that pid with another start time means the group emptied and its id may be someone
//! else's, so nothing is signalled. Processes of the daemon's own that survive SIGKILL
//! for the kill wait stop the daemon from starting, with their pids: starting would let
//! the scheduler run the same work beside them. Processes it may not signal (another
//! user's) are not its actions: they are logged, never counted as survivors, and their
//! record is dropped; the walk still goes on below them. So are processes the daemon
//! never signals ([`procs::Guards`]: its parent, a member of its own group or its
//! parent's). A record naming the daemon's own group or its parent's is read and
//! dropped, and that group is not walked at all.
//!
//! The records are trusted to be the daemon's: an entry of `runs/` that is not a
//! regular file of the daemon's user that only that user may write (a FIFO, a
//! directory, a symlink, a group- or other-writable file), and a `runs` that is not
//! such a directory, is moved aside into [`QUARANTINE`] unread (`runs/` is opened once
//! without following a link, and its records are listed, read, removed and moved through
//! that descriptor, so a swap of `runs` for a link during the sweep is never followed); a record is read
//! without blocking and at most [`MAX_RECORD_BYTES`] of it, and none of this stops the
//! start. Only the daemon writes `runs/` on macOS (the sandbox keeps actions out of
//! it); on Linux an action of the same user can write a well-formed record there,
//! naming any group, and the next start then kills every process of that user in it
//! or below it (see the crate documentation's known gaps).
//!
//! Not covered: a process of the action that left its group (`setsid`) and whose parent
//! exited before the sweep (nothing links it to the record any more; a per-lease user or
//! cgroup would), and a group id the kernel reused while the old daemon was dead, whose
//! new leader has exited too while its group lives on, in the same boot (the record
//! cannot tell that group from the action's).
//!
//! A directory the sweep cannot remove does not stop the daemon from starting: an
//! action decides what its lease directory holds, so a failure here is one build step
//! away, and refusing to start would let that step take the node out of the farm. The
//! directory is moved aside into [`QUARANTINE`] instead (out of the way of the lease
//! names), logged as an error, and tried again at every start. One that cannot even be
//! moved stays where it is, also logged; a later lease with the same name then fails
//! loudly rather than silently.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::procs::{self, Guards, Proc, Tracker};
use crate::record::{MAX_RECORD_BYTES, RUNS, Record};
use crate::runtime::KILL_PAUSE;

/// The directory under the scratch root that holds lease directories the sweep could
/// not remove.
pub const QUARANTINE: &str = "quarantine";

/// How the sweep reads the process table: a snapshot, whether a pid is one this
/// process may signal (`kill(pid, 0)`), and what the daemon never signals.
pub(crate) struct Table<'a> {
    pub snapshot: &'a dyn Fn() -> io::Result<Vec<Proc>>,
    pub ours: &'a dyn Fn(i32) -> bool,
    pub guards: &'a dyn Fn() -> Guards,
}

/// The process table as the kernel shows it.
pub(crate) const SYSTEM: Table<'static> = Table {
    snapshot: &procs::snapshot,
    ours: &signalable,
    guards: &Guards::now,
};

/// Whether this process may signal `pid`: `kill(pid, 0)` is not refused (EPERM). A pid
/// that is gone (ESRCH) is not another user's.
fn signalable(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks; it takes plain integers.
    let checked = unsafe { libc::kill(pid, 0) };
    checked == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
}

/// Ends every recorded action (waiting at most `wait` for each to die, reading the
/// process table through `table`), then removes every entry [`QUARANTINE`] held before
/// (through its descriptor), and, with `remove`, every `lease-*` entry of `scratch`,
/// moving a lease directory that will not go into [`QUARANTINE`].
///
/// # Errors
/// The scratch root cannot be read, this boot or the process table cannot be read, or
/// processes of the daemon's own in a recorded group survived SIGKILL for `wait`.
/// Nothing in `runs/` or in a lease directory is an error.
pub(crate) fn sweep(
    scratch: &Path,
    remove: &dyn Fn(&Path) -> io::Result<()>,
    wait: Duration,
    table: &Table<'_>,
) -> io::Result<()> {
    let quarantine = scratch.join(QUARANTINE);
    // What the quarantine holds now is removed after the kill, when no action left
    // behind is alive any more to swap the directory, and through the descriptor held
    // from here, so a swap is never followed in any case. What this sweep sets aside
    // goes in through that descriptor too, and stays until the next start.
    let (held, names) = hold(&quarantine, remove).unzip();
    let mut aside = Aside {
        path: quarantine,
        dir: held,
    };
    end_recorded(scratch, wait, table, &mut aside)?;
    if let Some(dir) = &aside.dir {
        for name in names.unwrap_or_default() {
            if let Err(why) = kbf_outputs::remove_tree_at(dir, &name) {
                tracing::error!(dir = %aside.path.display(), entry = ?name, "a quarantined entry still cannot be removed: {why}");
            }
        }
    }
    for entry in std::fs::read_dir(scratch)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("lease-") {
            continue;
        }
        let path = entry.path();
        tracing::warn!(dir = %path.display(), "removing a lease directory left behind");
        let Err(why) = remove(&path) else {
            continue;
        };
        match aside.take(&path, &name) {
            Ok(to) => tracing::error!(
                dir = %path.display(),
                to = %to.display(),
                "a lease directory left behind cannot be removed ({why}); moved it aside"
            ),
            Err(moved) => tracing::error!(
                dir = %path.display(),
                "a lease directory left behind cannot be removed ({why}) or moved aside ({moved}); left in place"
            ),
        }
    }
    Ok(())
}

/// The quarantine, opened without following a link, and the names in it now, listed
/// through that descriptor. `None` when it is absent. One that is not the daemon's
/// directory (a symlink an action planted, a file, a directory others may write) is
/// removed itself with `remove`, never emptied through, and is `None` too.
fn hold(
    quarantine: &Path,
    remove: &dyn Fn(&Path) -> io::Result<()>,
) -> Option<(OwnedFd, Vec<OsString>)> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(quarantine)
        .and_then(|dir| {
            owned(&dir.metadata()?, std::fs::FileType::is_dir)?;
            let dir = OwnedFd::from(dir);
            let mut names = Vec::new();
            for entry in rustix::fs::Dir::read_from(&dir)? {
                names.push(OsStr::from_bytes(entry?.file_name().to_bytes()).to_owned());
            }
            names.retain(|name| ![".", ".."].contains(&name.to_str().unwrap_or_default()));
            Ok((dir, names))
        });
    match held {
        Ok(held) => Some(held),
        // Absent until a sweep first needs it.
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(why) => {
            tracing::error!(dir = %quarantine.display(), "the quarantine is not the daemon's directory ({why}); removing it");
            if let Err(why) = remove(quarantine) {
                tracing::error!(dir = %quarantine.display(), "the quarantine cannot be removed: {why}");
            }
            None
        }
    }
}

/// Ends the action of every run record under `scratch`'s `runs/` and removes the
/// record. A record of an earlier boot is removed unsignalled. A record that does not
/// parse is removed: it is written whole before its action may start
/// ([`crate::record`]), so its action never ran. Anything that is not a record the
/// daemon could have written, `runs/` itself included, is moved aside into
/// [`QUARANTINE`].
fn end_recorded(
    scratch: &Path,
    wait: Duration,
    table: &Table<'_>,
    aside: &mut Aside,
) -> io::Result<()> {
    let runs = scratch.join(RUNS);
    // `runs/` is opened once, without following a link, and checked as the daemon's;
    // everything below reads, removes and moves by name relative to that descriptor,
    // so a `runs` an action swaps for a symlink after the check is never followed:
    // the records that were listed are the ones read, removed and moved.
    let (held, names) = match hold_runs(&runs) {
        Ok(held) => held,
        // Absent until a runtime first starts on this scratch root.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(why) => {
            set_aside(&runs, aside, &why);
            return Ok(());
        }
    };
    let boot = crate::record::boot_id()?;
    let me = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    let guards = (table.guards)();
    let mut survivors = Vec::new();
    for name in &names {
        let path = runs.join(name);
        let record = match read_record(&held, name) {
            Ok(record) => record,
            Err(why) => {
                set_aside_from(&held, name, &path, aside, &why);
                continue;
            }
        };
        match record {
            Some(record) if record.boot != boot => {
                tracing::warn!(record = %path.display(), "removing a run record of an earlier boot")
            }
            // No action leads the daemon's own group or its parent's; the tree below
            // either holds what the daemon never signals.
            Some(record) if !guards.may_signal_group(record.pgid) => {
                tracing::error!(record = %path.display(), pgid = record.pgid, "a run record names the daemon's own group or its parent's; not an action's, left alone")
            }
            Some(record) => {
                tracing::warn!(record = %path.display(), pgid = record.pgid, "ending a run left behind");
                let tracker = Tracker::resume(record.pgid, record.start, me);
                let (left, spared) = end_group(tracker, wait, table, guards)?;
                if !spared.is_empty() {
                    tracing::error!(record = %path.display(), ?spared, "a run record names processes the daemon may not signal (another user's, its parent, or in its own or its parent's group); not its actions, left alone");
                }
                if !left.is_empty() {
                    survivors.push(format!("{} ({left:?})", path.display()));
                    continue;
                }
            }
            None => tracing::warn!(record = %path.display(), "removing a torn run record"),
        }
        if let Err(why) = rustix::fs::unlinkat(&held, name, rustix::fs::AtFlags::empty()) {
            tracing::error!(record = %path.display(), "a run record cannot be removed: {why}");
        }
    }
    if survivors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "processes of runs left behind survived SIGKILL for {wait:?}: {}",
            survivors.join(", ")
        )))
    }
}

/// `runs/` at `runs`, opened without following a link and checked as the daemon's
/// directory ([`owned`]) once, and the names in it now, in name order, listed through
/// that descriptor.
fn hold_runs(runs: &Path) -> io::Result<(OwnedFd, Vec<OsString>)> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(runs)?;
    owned(&dir.metadata()?, std::fs::FileType::is_dir)?;
    let dir = OwnedFd::from(dir);
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(&dir)? {
        names.push(OsStr::from_bytes(entry?.file_name().to_bytes()).to_owned());
    }
    names.retain(|name| ![".", ".."].contains(&name.to_str().unwrap_or_default()));
    names.sort();
    Ok((dir, names))
}

/// Fails unless `meta` is of the kind `kind` accepts and owned by this process's user.
fn owned(meta: &std::fs::Metadata, kind: fn(&std::fs::FileType) -> bool) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: geteuid takes nothing and cannot fail.
    let me = unsafe { libc::geteuid() };
    if !kind(&meta.file_type()) {
        return Err(io::Error::other(format!(
            "not what the daemon makes there: {:?}",
            meta.file_type()
        )));
    }
    if meta.uid() != me {
        return Err(io::Error::other(format!(
            "owned by uid {}, not {me}",
            meta.uid()
        )));
    }
    // What another user could have written is not the daemon's either.
    if meta.mode() & 0o022 != 0 {
        return Err(io::Error::other(format!(
            "writable by others (mode {:o})",
            meta.mode() & 0o7777
        )));
    }
    Ok(())
}

/// Reads the run record `name` of `runs`: a regular file of this process's user,
/// opened relative to `runs` without following a link and without blocking, at most
/// [`MAX_RECORD_BYTES`] of it. `None` for a file that holds no whole record.
fn read_record(runs: &OwnedFd, name: &OsStr) -> io::Result<Option<Record>> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read as _;
    let fd = rustix::fs::openat(
        runs,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let file = std::fs::File::from(fd);
    // What was opened, not what is listed.
    owned(&file.metadata()?, std::fs::FileType::is_file)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES).read_to_end(&mut bytes)?;
    Ok(std::str::from_utf8(&bytes).ok().and_then(Record::parse))
}

/// Moves `runs` itself, which is not a directory the daemon made (`why`), into the
/// quarantine through `aside`, logged; one that cannot be moved stays, also logged.
fn set_aside(runs: &Path, aside: &mut Aside, why: &io::Error) {
    let moved = aside_name(runs.file_name().unwrap_or_default());
    let result = aside.take(runs, &moved);
    log_aside(runs, result, why);
}

/// Moves the entry `name` of `runs/`, which is not a run record the daemon wrote
/// (`why`), into the quarantine through `aside`, out of the directory `held` that was
/// checked; `path` names it for the log.
fn set_aside_from(held: &OwnedFd, name: &OsStr, path: &Path, aside: &mut Aside, why: &io::Error) {
    let result = aside.take_from(held, name, &aside_name(name));
    log_aside(path, result, why);
}

/// The name an entry of `runs/` gets in the quarantine, before the suffix.
fn aside_name(name: &OsStr) -> OsString {
    let mut moved = OsString::from("runs.");
    moved.push(name);
    moved
}

fn log_aside(path: &Path, result: io::Result<PathBuf>, why: &io::Error) {
    match result {
        Ok(to) => {
            tracing::error!(entry = %path.display(), to = %to.display(), "not a run record the daemon wrote ({why}); moved aside")
        }
        Err(moved) => {
            tracing::error!(entry = %path.display(), "not a run record the daemon wrote ({why}), and it cannot be moved aside ({moved}); left in place")
        }
    }
}

/// Kills every process of `tracker`'s action this process may signal and `guards`
/// allow until a snapshot shows none alive, or `wait` has passed. Returns the pids of
/// those still alive then, and of every process it found and spared (another user's,
/// or one the guards refuse).
fn end_group(
    mut tracker: Tracker,
    wait: Duration,
    table: &Table<'_>,
    guards: Guards,
) -> io::Result<(Vec<i32>, BTreeSet<i32>)> {
    let give_up = Instant::now() + wait;
    let mut spared = BTreeSet::new();
    loop {
        let (own, other): (Vec<Proc>, Vec<Proc>) = tracker
            .members(&(table.snapshot)()?)
            .into_iter()
            .partition(|p| guards.may_kill(p) && (table.ours)(p.pid));
        spared.extend(other.iter().map(|p| p.pid));
        if own.is_empty() {
            return Ok((Vec::new(), spared));
        }
        procs::kill_all(&tracker, &own);
        if Instant::now() >= give_up {
            return Ok((own.iter().map(|p| p.pid).collect(), spared));
        }
        std::thread::sleep(KILL_PAUSE);
    }
}

/// Where the sweep moves what it sets aside: [`QUARANTINE`] at `path`, and the
/// descriptor of the directory found there, opened without following a link and
/// checked as the daemon's ([`owned`]), once. Everything moves in through that
/// descriptor, so a quarantine an action swaps for a symlink, or plants as one, after
/// it was opened is never moved into: what is moved lands in the directory that was
/// checked, wherever the swap put it.
struct Aside {
    path: PathBuf,
    dir: Option<OwnedFd>,
}

impl Aside {
    /// The quarantine's descriptor: the one held, or else the directory at `path`,
    /// made (mode 0700, the daemon's alone, as `runs/` is) when absent.
    fn dir(&mut self) -> io::Result<&OwnedFd> {
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
        if let Some(dir) = self.dir.take() {
            return Ok(self.dir.insert(dir));
        }
        match std::fs::DirBuilder::new().mode(0o700).create(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.path)?;
        owned(&dir.metadata()?, std::fs::FileType::is_dir)?;
        Ok(self.dir.insert(OwnedFd::from(dir)))
    }

    /// Moves `path` into the quarantine under its `name` and a suffix no earlier start
    /// used; returns where, as a path for the log.
    fn take(&mut self, path: &Path, name: &OsStr) -> io::Result<PathBuf> {
        self.take_from(rustix::fs::CWD, path, name)
    }

    /// [`Aside::take`] for `source`, a path relative to the directory `from` (a
    /// descriptor already checked, or the working directory for a path).
    fn take_from(
        &mut self,
        from: impl AsFd,
        source: impl rustix::path::Arg,
        name: &OsStr,
    ) -> io::Result<PathBuf> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut aside = name.to_owned();
        aside.push(format!(".{nanos}"));
        rustix::fs::renameat(from, source, self.dir()?, &aside)?;
        Ok(self.path.join(aside))
    }
}

#[cfg(test)]
mod tests;
