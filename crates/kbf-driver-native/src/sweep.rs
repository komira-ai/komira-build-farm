//! The start-up sweep of the scratch root: every action a previous daemon recorded
//! ([`crate::record`]) and left running is killed, then every lease directory it left
//! behind is removed.
//!
//! A recorded action is killed as the daemon kills a lease's processes: SIGKILL to its
//! process group and to every process found in it or below it, again until a snapshot
//! of the process table shows none alive. Only the recorded process group is touched,
//! and only while it can still be the action's: the record names the leader by pid and
//! start time, and a live process under that pid with another start time means the
//! group emptied and its id may be someone else's, so nothing is signalled. Processes
//! that survive SIGKILL for the kill wait stop the daemon from starting, with their
//! pids: starting would let the scheduler run the same work beside them.
//!
//! Not covered: a process of the action that left its group (`setsid`) and whose parent
//! exited before the sweep (nothing links it to the record any more; a per-lease user or
//! cgroup would), and a group id the kernel reused while the old daemon was dead, whose
//! new leader has exited too while its group lives on (the record cannot tell that group
//! from the action's).
//!
//! A directory the sweep cannot remove does not stop the daemon from starting: an
//! action decides what its lease directory holds, so a failure here is one build step
//! away, and refusing to start would let that step take the node out of the farm. The
//! directory is moved aside into [`QUARANTINE`] instead (out of the way of the lease
//! names), logged as an error, and tried again at every start. One that cannot even be
//! moved stays where it is, also logged; a later lease with the same name then fails
//! loudly rather than silently.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::procs::{self, Proc, Tracker};
use crate::record::{RUNS, Record};
use crate::runtime::KILL_PAUSE;

/// The directory under the scratch root that holds lease directories the sweep could
/// not remove.
pub const QUARANTINE: &str = "quarantine";

/// Ends every recorded action (waiting at most `wait` for each to die, reading the
/// process table with `snapshot`), then removes, with `remove`, every entry of
/// [`QUARANTINE`] and every `lease-*` entry of `scratch`, moving a lease directory that
/// will not go into [`QUARANTINE`].
///
/// # Errors
/// The scratch root or its run records cannot be read, the process table cannot be
/// read, or a recorded action's processes survived SIGKILL for `wait`. A directory that
/// cannot be removed is not an error.
pub(crate) fn sweep(
    scratch: &Path,
    remove: &dyn Fn(&Path) -> io::Result<()>,
    wait: Duration,
    snapshot: &dyn Fn() -> io::Result<Vec<Proc>>,
) -> io::Result<()> {
    end_recorded(&scratch.join(RUNS), wait, snapshot)?;
    let quarantine = scratch.join(QUARANTINE);
    // Absent until a sweep first needs it.
    if let Ok(entries) = std::fs::read_dir(&quarantine) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Err(why) = remove(&path) {
                tracing::error!(dir = %path.display(), "a quarantined lease directory still cannot be removed: {why}");
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

/// Ends the action of every run record in `runs` and removes the record. A record that
/// does not parse is removed: it is written whole before its action may start
/// ([`crate::record`]), so its action never ran.
fn end_recorded(
    runs: &Path,
    wait: Duration,
    snapshot: &dyn Fn() -> io::Result<Vec<Proc>>,
) -> io::Result<()> {
    let entries = match std::fs::read_dir(runs) {
        // Absent until a runtime first starts on this scratch root.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    let me = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    let mut survivors = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let text = std::fs::read_to_string(&path)?;
        if let Some(record) = Record::parse(&text) {
            tracing::warn!(record = %path.display(), pgid = record.pgid, "ending a run left behind");
            let left = end_group(
                Tracker::resume(record.pgid, record.start, me),
                wait,
                snapshot,
            )?;
            if !left.is_empty() {
                survivors.push(format!("{} ({left:?})", path.display()));
                continue;
            }
        } else {
            tracing::warn!(record = %path.display(), "removing a torn run record");
        }
        std::fs::remove_file(&path)?;
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

/// Kills every process of `tracker`'s action until a snapshot shows none alive, or
/// `wait` has passed; returns the pids still alive then.
fn end_group(
    mut tracker: Tracker,
    wait: Duration,
    snapshot: &dyn Fn() -> io::Result<Vec<Proc>>,
) -> io::Result<Vec<i32>> {
    let give_up = Instant::now() + wait;
    loop {
        let members = tracker.members(&snapshot()?);
        if members.is_empty() {
            return Ok(Vec::new());
        }
        procs::kill_all(&tracker, &members);
        if Instant::now() >= give_up {
            return Ok(members.iter().map(|p| p.pid).collect());
        }
        std::thread::sleep(KILL_PAUSE);
    }
}

/// Moves `path` into `quarantine` under its `name` and a suffix no earlier start used.
fn move_aside(path: &Path, quarantine: &Path, name: &OsStr) -> io::Result<PathBuf> {
    std::fs::create_dir_all(quarantine)?;
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

    /// Fails for any path whose name is `lease-stuck`, as a directory the action
    /// locked would; removes everything else.
    fn stuck(path: &Path) -> io::Result<()> {
        if path.file_name().is_some_and(|n| n == "lease-stuck") {
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

        sweep(&dir, &stuck, WAIT, &procs::snapshot).expect("the sweep goes on past a failure");
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
        sweep(&dir, &fail_all, WAIT, &procs::snapshot)
            .expect("a stuck quarantine does not stop a start");
        assert_eq!(quarantined(&dir), aside);
        sweep(&dir, &kbf_outputs::remove_tree, WAIT, &procs::snapshot).expect("sweep");
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
        sweep(&dir, &stuck, WAIT, &procs::snapshot).expect("the start goes on");
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
        (child, Record { pgid: pid, start })
    }

    fn write_record(dir: &Path, lease: &str, line: &str) {
        std::fs::create_dir_all(dir.join(RUNS)).expect("mkdir");
        std::fs::write(crate::record::path(dir, lease), line).expect("write");
    }

    /// Whether `pid` has ended: gone, or a zombie not yet reaped.
    fn ended(pid: i32) -> bool {
        procs::process(pid).is_none_or(|p| p.zombie)
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
        for (lease, record) in [("lease-1-1", running), ("lease-1-2", orphaned)] {
            write_record(&dir, lease, &record.line());
            std::fs::create_dir_all(dir.join(lease).join("root")).expect("mkdir");
        }
        let (first, second) = (running.pgid, sleep);
        let remove = |path: &Path| {
            assert!(ended(first), "{} removed first", path.display());
            assert!(ended(second), "{} removed first", path.display());
            kbf_outputs::remove_tree(path)
        };
        sweep(&dir, &remove, WAIT, &procs::snapshot).expect("sweep");
        assert_eq!(leader.wait().expect("reap").signal(), Some(libc::SIGKILL));
        exited.wait().expect("reap");
        assert!(ended(sleep), "the group's other process was killed");
        assert!(!dir.join("lease-1-1").exists());
        assert!(!dir.join("lease-1-2").exists());
        let left = std::fs::read_dir(dir.join(RUNS)).expect("runs").count();
        assert_eq!(left, 0, "the records are removed");
    }

    /// Catches a sweep that trusts a record's pid without its start time (the "kill
    /// without verifying identity" mutant): a process now under the recorded pid that
    /// started after the recorded leader is not the action, and nothing of it is
    /// signalled. A torn record, written by a daemon that died mid-write before its
    /// action could start, is removed and kills nothing.
    #[test]
    fn a_record_whose_pid_names_another_process_kills_nothing() {
        let dir = scratch("reused");
        let (mut other, record) = action("exec sleep 300");
        let stale = Record {
            start: record.start - 1,
            ..record
        };
        write_record(&dir, "lease-1-1", &stale.line());
        write_record(&dir, "lease-1-2", &record.pgid.to_string());
        sweep(&dir, &kbf_outputs::remove_tree, WAIT, &procs::snapshot).expect("sweep");
        let alive = other.try_wait().expect("try_wait").is_none();
        other.kill().expect("kill");
        other.wait().expect("reap");
        assert!(alive, "a process the record does not name was killed");
        let left = std::fs::read_dir(dir.join(RUNS)).expect("runs").count();
        assert_eq!(left, 0, "both records are removed");
    }

    /// Catches a daemon that starts while processes of a run left behind survive
    /// SIGKILL (the scheduler would run the work again beside them), one that drops
    /// their record (the next start could not find them), and a process table or record
    /// that cannot be read taken for "nothing left". The pids here are above any
    /// kernel's limit, so the kills reach nothing.
    #[test]
    fn survivors_and_unreadable_records_stop_the_start() {
        let dir = scratch("survivors");
        let ghost = 2_000_000_001;
        write_record(&dir, "lease-1-1", &format!("{ghost} 5\n"));
        let table = || {
            Ok(vec![Proc {
                pid: ghost,
                ppid: 1,
                pgid: ghost,
                start: 5,
                zombie: false,
            }])
        };
        let runs = dir.join(RUNS);
        let why = end_recorded(&runs, Duration::from_millis(30), &table)
            .expect_err("survivors")
            .to_string();
        assert!(why.contains("lease-1-1 ([2000000001])"), "{why}");
        let record = crate::record::path(&dir, "lease-1-1");
        assert!(record.exists(), "kept for the next start");

        let unreadable = || Err(io::Error::other("no process table"));
        let why = end_recorded(&runs, WAIT, &unreadable).expect_err("no table");
        assert_eq!(why.to_string(), "no process table");

        std::fs::remove_file(&record).expect("rm");
        let directory = crate::record::path(&dir, "lease-1-2");
        std::fs::create_dir(&directory).expect("mkdir");
        assert!(end_recorded(&runs, WAIT, &table).is_err(), "not a file");
        std::fs::remove_dir(&directory).expect("rmdir");
        std::fs::remove_dir(&runs).expect("rmdir");
        std::fs::write(&runs, b"not a directory").expect("write");
        assert!(sweep(&dir, &kbf_outputs::remove_tree, WAIT, &table).is_err());
    }
}
