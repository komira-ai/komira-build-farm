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
//! such a directory, is moved aside into [`QUARANTINE`] unread; a record is read
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
use std::ffi::OsStr;
use std::io;
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
/// process table through `table`), then removes, with `remove`, every entry of
/// [`QUARANTINE`] and every `lease-*` entry of `scratch`, moving a lease directory that
/// will not go into [`QUARANTINE`].
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
    // Absent until a sweep first needs it. Emptied first, so what this sweep sets aside
    // stays there until the next start. One that is not the daemon's directory (a
    // symlink an action planted) is removed itself, never emptied through.
    match ours(&quarantine, std::fs::FileType::is_dir) {
        Ok(()) => {
            for entry in std::fs::read_dir(&quarantine)
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = entry.path();
                if let Err(why) = remove(&path) {
                    tracing::error!(dir = %path.display(), "a quarantined entry still cannot be removed: {why}");
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(why) => {
            tracing::error!(dir = %quarantine.display(), "the quarantine is not the daemon's directory ({why}); removing it");
            if let Err(why) = remove(&quarantine) {
                tracing::error!(dir = %quarantine.display(), "the quarantine cannot be removed: {why}");
            }
        }
    }
    end_recorded(scratch, wait, table)?;
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
        match move_aside(&path, &quarantine, &name) {
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

/// Ends the action of every run record under `scratch`'s `runs/` and removes the
/// record. A record of an earlier boot is removed unsignalled. A record that does not
/// parse is removed: it is written whole before its action may start
/// ([`crate::record`]), so its action never ran. Anything that is not a record the
/// daemon could have written, `runs/` itself included, is moved aside into
/// [`QUARANTINE`].
fn end_recorded(scratch: &Path, wait: Duration, table: &Table<'_>) -> io::Result<()> {
    let runs = scratch.join(RUNS);
    let quarantine = scratch.join(QUARANTINE);
    let entries =
        match ours(&runs, std::fs::FileType::is_dir).and_then(|()| std::fs::read_dir(&runs)) {
            Ok(entries) => entries,
            // Absent until a runtime first starts on this scratch root.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(why) => {
                set_aside(&runs, &quarantine, &why);
                return Ok(());
            }
        };
    let boot = crate::record::boot_id()?;
    let me = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    let guards = (table.guards)();
    let mut survivors = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        let record = match read_record(&path) {
            Ok(record) => record,
            Err(why) => {
                set_aside(&path, &quarantine, &why);
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
        if let Err(why) = std::fs::remove_file(&path) {
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

/// Fails unless `path` (not followed) is of the kind `kind` accepts and owned by this
/// process's user.
fn ours(path: &Path, kind: fn(&std::fs::FileType) -> bool) -> io::Result<()> {
    owned(&std::fs::symlink_metadata(path)?, kind)
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

/// Reads the run record at `path`: a regular file of this process's user, opened
/// without following a link and without blocking, at most [`MAX_RECORD_BYTES`] of it.
/// `None` for a file that holds no whole record.
fn read_record(path: &Path) -> io::Result<Option<Record>> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    ours(path, std::fs::FileType::is_file)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    // What was opened, not what was looked at a moment before.
    owned(&file.metadata()?, std::fs::FileType::is_file)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES).read_to_end(&mut bytes)?;
    Ok(std::str::from_utf8(&bytes).ok().and_then(Record::parse))
}

/// Moves `path`, which is not a run record the daemon wrote (`why`), into `quarantine`,
/// logged; one that cannot be moved stays, also logged.
fn set_aside(path: &Path, quarantine: &Path, why: &io::Error) {
    let mut name = std::ffi::OsString::from("runs.");
    name.push(path.file_name().unwrap_or_default());
    match move_aside(path, quarantine, &name) {
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

/// Moves `path` into `quarantine` under its `name` and a suffix no earlier start used.
fn move_aside(path: &Path, quarantine: &Path, name: &OsStr) -> io::Result<PathBuf> {
    // The daemon's alone, as `runs/` is.
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
        .create(quarantine)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut aside = name.to_owned();
    aside.push(format!(".{nanos}"));
    let to = quarantine.join(aside);
    std::fs::rename(path, &to)?;
    Ok(to)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The kill wait the sweeps here get.
    const WAIT: Duration = Duration::from_secs(5);

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("sweep-{name}-{}", std::process::id()));
        // Absent unless a run with this pid left it.
        let _ = kbf_outputs::remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        // Logging on, so the sweep's messages are written (and their arguments run).
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        dir
    }

    /// Fails for any path whose name is `lease-stuck` or the quarantine's, as a
    /// directory the action locked would; removes everything else.
    fn stuck(path: &Path) -> io::Result<()> {
        if path
            .file_name()
            .is_some_and(|n| n == "lease-stuck" || n == QUARANTINE)
        {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        kbf_outputs::remove_tree(path)
    }

    fn quarantined(scratch: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(scratch.join(QUARANTINE))
            .map(|entries| {
                entries
                    .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Catches one lease directory that cannot be removed stopping the daemon from
    /// starting (one build step could brick the node), the stuck directory left where
    /// the next lease could collide with it, and the sweep giving up on the others.
    #[test]
    fn a_directory_that_cannot_be_removed_is_moved_aside() {
        let dir = scratch("aside");
        std::fs::create_dir_all(dir.join("lease-stuck/root")).expect("mkdir");
        std::fs::write(dir.join("lease-stuck/root/out"), b"x").expect("write");
        std::fs::create_dir_all(dir.join("lease-1-1/root")).expect("mkdir");
        std::fs::write(dir.join("keep"), b"not a lease").expect("write");

        sweep(&dir, &stuck, WAIT, &SYSTEM).expect("the sweep goes on past a failure");
        assert!(!dir.join("lease-stuck").exists(), "moved out of the way");
        assert!(!dir.join("lease-1-1").exists(), "the others still go");
        assert!(dir.join("keep").exists(), "not a lease directory");
        let aside = quarantined(&dir);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(aside[0].starts_with("lease-stuck."), "{aside:?}");
        let kept = dir.join(QUARANTINE).join(&aside[0]).join("root/out");
        assert_eq!(std::fs::read(kept).expect("moved whole"), b"x");

        // The next start tries again; what still fails stays in quarantine, and the
        // start goes on.
        let fail_all = |_: &Path| Err(io::Error::from_raw_os_error(libc::EPERM));
        sweep(&dir, &fail_all, WAIT, &SYSTEM).expect("a stuck quarantine does not stop a start");
        assert_eq!(quarantined(&dir), aside);
        sweep(&dir, &kbf_outputs::remove_tree, WAIT, &SYSTEM).expect("sweep");
        assert_eq!(
            quarantined(&dir),
            Vec::<String>::new(),
            "removed once it can be"
        );
    }

    /// Catches a start refused because the stuck directory could not be moved aside
    /// either (the quarantine path taken by a file): the directory stays, logged, and
    /// the start goes on.
    #[test]
    fn a_directory_that_cannot_be_moved_aside_stays() {
        let dir = scratch("stays");
        std::fs::create_dir_all(dir.join("lease-stuck")).expect("mkdir");
        std::fs::write(dir.join(QUARANTINE), b"in the way").expect("write");
        sweep(&dir, &stuck, WAIT, &SYSTEM).expect("the start goes on");
        assert!(dir.join("lease-stuck").is_dir());
    }

    /// Starts `sh -c script` leading a process group of its own, as an action does, with
    /// its stdout piped; returns it and the record the daemon would have written.
    fn action(script: &str) -> (std::process::Child, Record) {
        use std::os::unix::process::CommandExt as _;
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", script])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("spawn");
        let pid = i32::try_from(child.id()).expect("pid");
        let start = procs::process(pid).expect("in the table").start;
        let boot = crate::record::boot_id().expect("boot id");
        (
            child,
            Record {
                pgid: pid,
                start,
                boot,
            },
        )
    }

    /// Writes a record as the daemon does: mode 0600 in a `runs` of mode 0700.
    fn write_record(dir: &Path, lease: &str, line: &str) {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join(RUNS))
            .expect("mkdir");
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(crate::record::path(dir, lease))
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .expect("write");
    }

    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// Whether `pid` has ended: gone, or a zombie not yet reaped.
    fn ended(pid: i32) -> bool {
        procs::process(pid).is_none_or(|p| p.zombie)
    }

    fn runs_left(dir: &Path) -> usize {
        std::fs::read_dir(dir.join(RUNS)).expect("runs").count()
    }

    /// Kills and reaps `child`; whether it was still running first.
    fn still_ran(mut child: std::process::Child) -> bool {
        let alive = child.try_wait().expect("try_wait").is_none();
        child.kill().expect("kill");
        child.wait().expect("reap");
        alive
    }

    /// Runs the sweep on another thread; panics unless it ends within 20 s.
    fn sweep_within(dir: &Path) -> io::Result<()> {
        let dir = dir.to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(sweep(&dir, &kbf_outputs::remove_tree, WAIT, &SYSTEM));
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("the sweep blocked")
    }

    /// Catches (issue #155) a sweep that removes the lease directories a killed daemon
    /// left but leaves their actions running (the "sweep skips the kill" mutant), one
    /// that removes a directory before its action is dead, one that misses a process
    /// still in the group after its leader exited, and one that keeps the records.
    #[test]
    fn a_recorded_run_is_killed_before_its_directory_goes() {
        use std::os::unix::process::ExitStatusExt as _;

        let dir = scratch("kill");
        let (mut leader, running) = action("exec sleep 300");
        // The leader exits at once; the sleep it started stays in its group.
        let (mut exited, orphaned) = action("sleep 300 & echo $!");
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(exited.stdout.take().expect("stdout")),
            &mut line,
        )
        .expect("the sleep's pid");
        let sleep: i32 = line.trim().parse().expect("a pid");
        let first = running.pgid;
        for (lease, record) in [("lease-1-1", running), ("lease-1-2", orphaned)] {
            write_record(&dir, lease, &record.line());
            std::fs::create_dir_all(dir.join(lease).join("root")).expect("mkdir");
        }
        let remove = |path: &Path| {
            assert!(ended(first), "{} removed first", path.display());
            assert!(ended(sleep), "{} removed first", path.display());
            kbf_outputs::remove_tree(path)
        };
        sweep(&dir, &remove, WAIT, &SYSTEM).expect("sweep");
        assert_eq!(leader.wait().expect("reap").signal(), Some(libc::SIGKILL));
        exited.wait().expect("reap");
        assert!(ended(sleep), "the group's other process was killed");
        assert!(!dir.join("lease-1-1").exists());
        assert!(!dir.join("lease-1-2").exists());
        assert_eq!(runs_left(&dir), 0, "the records are removed");
    }

    /// Catches a sweep that trusts a record's pid without its start time (the "kill
    /// without verifying identity" mutant), or without its boot: a process now under
    /// the recorded pid that started after the recorded leader, or a record of an
    /// earlier boot, names no action, and nothing is signalled. A torn record, written
    /// by a daemon that died mid-write before its action could start, is removed and
    /// kills nothing.
    #[test]
    fn a_record_whose_pid_names_another_process_kills_nothing() {
        let dir = scratch("reused");
        let (other, record) = action("exec sleep 300");
        let stale = Record {
            start: record.start - 1,
            ..record.clone()
        };
        let rebooted = Record {
            boot: "an-earlier-boot".to_owned(),
            ..record.clone()
        };
        write_record(&dir, "lease-1-1", &stale.line());
        write_record(&dir, "lease-1-2", &record.pgid.to_string());
        write_record(&dir, "lease-1-3", &rebooted.line());
        sweep(&dir, &kbf_outputs::remove_tree, WAIT, &SYSTEM).expect("sweep");
        assert!(
            still_ran(other),
            "a process the record does not name was killed"
        );
        assert_eq!(runs_left(&dir), 0, "the records are removed");
    }

    /// Catches a daemon that starts while processes of its own in a run left behind
    /// survive SIGKILL (the scheduler would run the work again beside them), one that
    /// drops their record (the next start could not find them), and a process table
    /// that cannot be read taken for "nothing left". The pids here are above any
    /// kernel's limit, so the kills reach nothing.
    #[test]
    fn survivors_of_its_own_stop_the_start() {
        let dir = scratch("survivors");
        let ghost = 2_000_000_001;
        let boot = crate::record::boot_id().expect("boot");
        write_record(&dir, "lease-1-1", &format!("{ghost} 5 {boot}\n"));
        let snapshot = || {
            Ok(vec![Proc {
                pid: ghost,
                ppid: 1,
                pgid: ghost,
                start: 5,
                zombie: false,
            }])
        };
        let ours = Table {
            snapshot: &snapshot,
            ours: &|_| true,
            guards: &Guards::now,
        };
        let why = end_recorded(&dir, Duration::from_millis(30), &ours)
            .expect_err("survivors")
            .to_string();
        assert!(why.contains("lease-1-1 ([2000000001])"), "{why}");
        assert!(
            crate::record::path(&dir, "lease-1-1").exists(),
            "kept for the next start"
        );
        let unreadable = Table {
            snapshot: &|| Err(io::Error::other("no process table")),
            ours: &|_| true,
            guards: &Guards::now,
        };
        let why = end_recorded(&dir, WAIT, &unreadable).expect_err("no table");
        assert_eq!(why.to_string(), "no process table");
    }

    /// Catches a record naming another user's processes (forged, or a reused pid)
    /// stopping every start ("survived SIGKILL") or being kept: processes the daemon
    /// may not signal are not its actions. The table is a fake one: a test never points
    /// the real sweep at processes it did not start (it would kill what runs below
    /// them). Also `signalable` taking init, another user's, for the daemon's; signal
    /// 0 only checks, it delivers nothing.
    #[test]
    fn a_record_naming_another_users_group_is_dropped() {
        let dir = scratch("foreign");
        let ghost = 2_000_000_003;
        let boot = crate::record::boot_id().expect("boot");
        write_record(&dir, "lease-2-1", &format!("{ghost} 5 {boot}\n"));
        let snapshot = || {
            Ok(vec![Proc {
                pid: ghost,
                ppid: 1,
                pgid: ghost,
                start: 5,
                zombie: false,
            }])
        };
        let theirs = Table {
            snapshot: &snapshot,
            ours: &|_| false,
            guards: &Guards::now,
        };
        end_recorded(&dir, WAIT, &theirs).expect("not the daemon's");
        assert_eq!(runs_left(&dir), 0, "the record is dropped");

        // SAFETY: geteuid takes nothing and cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        assert_eq!(signalable(1), root, "init taken for the daemon's");
    }

    /// Catches entries of `runs/` that no daemon wrote stopping or hanging the start
    /// (a FIFO opened for reading blocks), a symlink followed to a record elsewhere (it
    /// would kill what that record names), and `runs` itself not a directory of the
    /// daemon's (a file) stopping the start: each is moved aside into the quarantine,
    /// unread, and the start goes on. Also a record that cannot be removed, and an
    /// entry that cannot be moved aside, which are logged.
    #[test]
    fn what_is_not_a_record_is_set_aside_and_the_start_goes_on() {
        let dir = scratch("forged");
        let (victim, record) = action("exec sleep 300");
        let elsewhere = dir.join("elsewhere");
        std::fs::write(&elsewhere, record.line()).expect("write");
        write_record(&dir, "lease-3-0", "torn");
        let runs = dir.join(RUNS);
        let fifo = std::ffi::CString::new(runs.join("lease-3-1").as_os_str().as_encoded_bytes())
            .expect("a C path");
        // SAFETY: a C string path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::fs::create_dir(runs.join("lease-3-2")).expect("mkdir");
        std::os::unix::fs::symlink(&elsewhere, runs.join("lease-3-3")).expect("symlink");

        sweep_within(&dir).expect("the start goes on");
        assert!(still_ran(victim), "a symlinked record was followed");
        assert_eq!(runs_left(&dir), 0);
        let aside = quarantined(&dir);
        assert_eq!(aside.len(), 3, "{aside:?}");
        for (name, lease) in aside.iter().zip(["lease-3-1", "lease-3-2", "lease-3-3"]) {
            assert!(name.starts_with(&format!("runs.{lease}.")), "{aside:?}");
        }
        // A start after that removes what was set aside.
        sweep_within(&dir).expect("sweep");
        assert!(quarantined(&dir).is_empty());

        // `runs` a file: set aside, and the start goes on.
        std::fs::remove_dir(&runs).expect("rmdir");
        std::fs::write(&runs, b"in the way").expect("write");
        sweep_within(&dir).expect("the start goes on");
        assert!(!runs.exists());
        assert!(quarantined(&dir)[0].starts_with("runs.runs."));

        // A torn record that cannot be removed, and a FIFO that cannot be moved aside
        // (out of a `runs` the daemon cannot write): logged, and the start goes on.
        write_record(&dir, "lease-4-0", "torn");
        // SAFETY: as above.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        chmod(&runs, 0o500);
        let swept = sweep_within(&dir);
        chmod(&runs, 0o700);
        swept.expect("the start goes on");
        assert_eq!(runs_left(&dir), 2, "both left in place");
    }

    /// Catches an entry owned by another user taken for the daemon's.
    #[test]
    fn only_the_daemons_own_entries_are_read() {
        let why = ours(Path::new("/"), std::fs::FileType::is_dir).expect_err("root's");
        assert!(why.to_string().contains("owned by uid 0"), "{why}");
    }

    /// Catches a `runs` or a record that another user could write (group or other write
    /// bits) read as the daemon's ("mode unchecked" mutant), and a `runs` that is a
    /// symlink followed to a directory of records elsewhere ("ours follows links"
    /// mutant): each is set aside unread, the action it names keeps running, and the
    /// start goes on.
    #[test]
    fn a_runs_open_to_others_or_a_link_is_set_aside_unread() {
        let dir = scratch("modes");
        let (victim, record) = action("exec sleep 300");
        let runs = dir.join(RUNS);

        write_record(&dir, "lease-7-1", &record.line());
        chmod(&runs, 0o770);
        sweep_within(&dir).expect("the start goes on");
        assert!(!runs.exists(), "a group-writable runs set aside");

        write_record(&dir, "lease-7-2", &record.line());
        chmod(&crate::record::path(&dir, "lease-7-2"), 0o602);
        sweep_within(&dir).expect("the start goes on");
        assert_eq!(runs_left(&dir), 0, "an other-writable record set aside");

        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("mkdir");
        chmod(&elsewhere, 0o700);
        std::fs::write(elsewhere.join("lease-7-3"), record.line()).expect("write");
        chmod(&elsewhere.join("lease-7-3"), 0o600);
        std::fs::remove_dir(&runs).expect("rmdir");
        std::os::unix::fs::symlink(&elsewhere, &runs).expect("symlink");
        sweep_within(&dir).expect("the start goes on");
        assert!(!runs.exists(), "the link set aside");
        assert!(
            elsewhere.join("lease-7-3").exists(),
            "not read through the link"
        );
        assert!(
            still_ran(victim),
            "a record that is not the daemon's was read"
        );
    }

    /// Catches a quarantine that is a symlink emptied through the link: an action that
    /// points it at a directory of the user's would get that directory's entries
    /// removed at the next start ("quarantine followed" mutant).
    #[test]
    fn a_quarantine_that_is_a_link_is_removed_not_followed() {
        let dir = scratch("qlink");
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("mkdir");
        std::fs::write(elsewhere.join("keep"), b"the user's").expect("write");
        std::os::unix::fs::symlink(&elsewhere, dir.join(QUARANTINE)).expect("symlink");
        sweep_within(&dir).expect("the start goes on");
        assert!(elsewhere.join("keep").exists(), "emptied through the link");
        assert!(
            std::fs::symlink_metadata(dir.join(QUARANTINE)).is_err(),
            "the link removed"
        );
    }

    /// Catches a pid that exited between the snapshot and the check taken for another
    /// user's (logged as foreign): only EPERM says a process is not the daemon's.
    #[test]
    fn a_pid_that_is_gone_is_not_another_users() {
        assert!(signalable(2_000_000_041), "ESRCH");
        let me = i32::try_from(std::process::id()).expect("pid");
        assert!(signalable(me));
    }

    /// Catches a record naming the daemon's own group or its parent's (forged: on Linux
    /// an action can read both in `/proc`) followed at all: walking that group's tree
    /// reaches the parent's other children, and its members, which the daemon never
    /// signals, would outlive the kill wait and stop the start ("the guarded group is
    /// walked" mutant). Also a process below a recorded group that the daemon never
    /// signals (its parent, a member of its own group) counted as a survivor ("guarded
    /// processes are survivors" mutant). The pids are above any kernel's limit.
    #[test]
    fn what_the_daemon_never_signals_neither_stops_the_start_nor_is_walked() {
        let dir = scratch("guarded");
        let boot = crate::record::boot_id().expect("boot");
        let guards = Guards {
            own_group: 2_000_000_021,
            parent: 2_000_000_030,
            parent_group: 2_000_000_022,
        };
        let g = |pid, ppid, pgid, zombie| Proc {
            pid,
            ppid,
            pgid,
            start: 5,
            zombie,
        };
        // Live members of both guarded groups, which a walk would find and not kill;
        // and an action's group (its leader a zombie) with the daemon's parent and a
        // member of the daemon's own group below it.
        let reads = std::cell::Cell::new(0);
        let snapshot = || {
            reads.set(reads.get() + 1);
            Ok(vec![
                g(2_000_000_021, 1, 2_000_000_021, false),
                g(2_000_000_022, 1, 2_000_000_022, false),
                g(2_000_000_031, 1, 2_000_000_031, true),
                g(2_000_000_030, 2_000_000_031, 2_000_000_032, false),
                g(2_000_000_033, 2_000_000_031, 2_000_000_021, false),
            ])
        };
        let table = Table {
            snapshot: &snapshot,
            ours: &|_| true,
            guards: &|| guards,
        };
        write_record(&dir, "lease-6-1", &format!("2000000021 5 {boot}\n"));
        write_record(&dir, "lease-6-2", &format!("2000000022 5 {boot}\n"));
        end_recorded(&dir, Duration::from_millis(30), &table).expect("the start goes on");
        assert_eq!(reads.get(), 0, "a guarded group was walked");
        assert_eq!(runs_left(&dir), 0, "the records are dropped");

        // Below the action's group, neither the parent nor the member of the daemon's
        // own group is killed or a survivor.
        write_record(&dir, "lease-6-3", &format!("2000000031 5 {boot}\n"));
        end_recorded(&dir, Duration::from_millis(30), &table).expect("not survivors");
        assert_eq!(reads.get(), 1, "the action's group was walked once");
        assert_eq!(runs_left(&dir), 0, "the record is dropped");
    }
}
