//! The sweep's tests.

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
    // The daemon's alone, whatever the umask ("made 0755" mutant).
    let mode = std::fs::symlink_metadata(dir.join(QUARANTINE)).expect("quarantine");
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o700
    );

    // The next start tries again; what still fails (an entry of a quarantine the
    // daemon may not write) stays in quarantine, and the start goes on.
    chmod(&dir.join(QUARANTINE), 0o500);
    let swept = sweep(&dir, &kbf_outputs::remove_tree, WAIT, &SYSTEM);
    chmod(&dir.join(QUARANTINE), 0o700);
    swept.expect("a stuck quarantine does not stop a start");
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

/// The quarantine of `dir`, not opened yet, as [`sweep`] starts with it when absent.
fn aside(dir: &Path) -> Aside {
    Aside {
        path: dir.join(QUARANTINE),
        dir: None,
    }
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
    let why = end_recorded(&dir, Duration::from_millis(30), &ours, &mut aside(&dir))
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
    let why = end_recorded(&dir, WAIT, &unreadable, &mut aside(&dir)).expect_err("no table");
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
    end_recorded(&dir, WAIT, &theirs, &mut aside(&dir)).expect("not the daemon's");
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
/// removed at the next start ("quarantine followed" mutant). The target is a
/// directory the daemon would take for its own (0700, whatever the umask), so only
/// the link check can refuse it.
#[test]
fn a_quarantine_that_is_a_link_is_removed_not_followed() {
    let dir = scratch("qlink");
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("mkdir");
    chmod(&elsewhere, 0o700);
    std::fs::write(elsewhere.join("keep"), b"the user's").expect("write");
    std::os::unix::fs::symlink(&elsewhere, dir.join(QUARANTINE)).expect("symlink");
    sweep_within(&dir).expect("the start goes on");
    assert!(elsewhere.join("keep").exists(), "emptied through the link");
    assert!(
        std::fs::symlink_metadata(dir.join(QUARANTINE)).is_err(),
        "the link removed"
    );
}

/// Catches a quarantine emptied before the kill (an action left behind is still
/// alive then), and one emptied by path after the check: an action that swaps it
/// for a symlink to a directory of the user's while the sweep kills ("emptied by
/// path" mutant) would get that directory's entries removed. The swap is done from
/// the process table read, as the dying action would.
#[test]
fn the_quarantine_is_emptied_after_the_kill_through_what_was_checked() {
    let dir = scratch("qswap");
    let quarantine = dir.join(QUARANTINE);
    let (moved, elsewhere) = (dir.join("moved"), dir.join("elsewhere"));
    for held in [&quarantine, &elsewhere] {
        std::fs::create_dir(held).expect("mkdir");
        chmod(held, 0o700);
        std::fs::write(held.join("old"), b"x").expect("write");
    }
    let boot = crate::record::boot_id().expect("boot");
    write_record(&dir, "lease-8-1", &format!("2000000051 5 {boot}\n"));
    let at_kill = std::cell::Cell::new(false);
    let swap = || {
        at_kill.set(quarantine.join("old").exists());
        std::fs::rename(&quarantine, &moved).expect("rename");
        std::os::unix::fs::symlink(&elsewhere, &quarantine).expect("symlink");
        Ok(Vec::new())
    };
    let table = Table {
        snapshot: &swap,
        ours: &|_| true,
        guards: &Guards::now,
    };
    sweep(&dir, &kbf_outputs::remove_tree, WAIT, &table).expect("sweep");
    assert!(at_kill.get(), "emptied before the kill");
    assert!(elsewhere.join("old").exists(), "emptied through the swap");
    assert!(!moved.join("old").exists(), "the checked directory emptied");
}

/// Catches what the sweep moves aside after the kill moved into the quarantine by
/// path: an action that swaps the quarantine for a symlink to a directory of the
/// user's while the sweep kills ("moved aside by path" mutant) would get its lease
/// directory, a tree it shaped, written into that directory and left there. The stuck lease goes into the directory that was
/// checked, through its descriptor, wherever the swap put it. An entry of `runs/`
/// that is not a record is set aside the same way, whether before the kill or
/// after it. The swap is done from the process table read, as the dying action
/// would.
#[test]
fn what_is_moved_aside_after_the_kill_goes_into_what_was_checked() {
    let dir = scratch("qswapaside");
    let quarantine = dir.join(QUARANTINE);
    let (moved, elsewhere) = (dir.join("moved"), dir.join("elsewhere"));
    for held in [&quarantine, &elsewhere] {
        std::fs::create_dir(held).expect("mkdir");
        chmod(held, 0o700);
    }
    std::fs::create_dir_all(dir.join("lease-stuck/root")).expect("mkdir");
    let boot = crate::record::boot_id().expect("boot");
    write_record(&dir, "lease-9-1", &format!("2000000061 5 {boot}\n"));
    for bad in 2..6 {
        std::fs::create_dir(dir.join(RUNS).join(format!("lease-9-{bad}"))).expect("mkdir");
    }
    let swapped = std::cell::Cell::new(false);
    let swap = || {
        if !swapped.replace(true) {
            std::fs::rename(&quarantine, &moved).expect("rename");
            std::os::unix::fs::symlink(&elsewhere, &quarantine).expect("symlink");
        }
        Ok(Vec::new())
    };
    let table = Table {
        snapshot: &swap,
        ours: &|_| true,
        guards: &Guards::now,
    };
    sweep(&dir, &stuck, WAIT, &table).expect("sweep");
    assert!(swapped.get(), "the kill read the table");
    let landed: Vec<_> = std::fs::read_dir(&elsewhere)
        .expect("elsewhere")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(
        landed.is_empty(),
        "moved aside through the swap: {landed:?}"
    );
    let mut kept: Vec<String> = std::fs::read_dir(&moved)
        .expect("moved")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    kept.sort();
    assert_eq!(kept.len(), 5, "{kept:?}");
    assert!(kept[0].starts_with("lease-stuck."), "{kept:?}");
    assert!(!dir.join("lease-stuck").exists(), "moved out of the way");
}

/// Catches a quarantine the sweep makes after the kill followed when it is a link:
/// absent at the start, an action can plant a symlink under its name while the
/// sweep kills ("made by path" mutant). Nothing is moved through the link; the
/// stuck lease stays where it is, logged.
#[test]
fn a_quarantine_planted_during_the_kill_is_not_moved_into() {
    let dir = scratch("qplant");
    let quarantine = dir.join(QUARANTINE);
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("mkdir");
    chmod(&elsewhere, 0o700);
    std::fs::create_dir_all(dir.join("lease-stuck/root")).expect("mkdir");
    let boot = crate::record::boot_id().expect("boot");
    write_record(&dir, "lease-9-7", &format!("2000000062 5 {boot}\n"));
    let plant = || {
        if std::fs::symlink_metadata(&quarantine).is_err() {
            std::os::unix::fs::symlink(&elsewhere, &quarantine).expect("symlink");
        }
        Ok(Vec::new())
    };
    let table = Table {
        snapshot: &plant,
        ours: &|_| true,
        guards: &Guards::now,
    };
    sweep(&dir, &stuck, WAIT, &table).expect("sweep");
    let landed = std::fs::read_dir(&elsewhere).expect("elsewhere").count();
    assert_eq!(landed, 0, "moved aside through the planted link");
    assert!(dir.join("lease-stuck/root").is_dir(), "left in place");
}

/// Catches a quarantine that cannot be made reported as the open's error instead of
/// the mkdir's ("mkdir error dropped" mutant: a scratch root the daemon may not write
/// would read as a quarantine that is not there), and the entry moved anyway.
#[test]
fn a_quarantine_that_cannot_be_made_says_why() {
    // SAFETY: geteuid takes nothing and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = scratch("qmkdir");
    std::fs::create_dir(dir.join("lease-stuck")).expect("mkdir");
    chmod(&dir, 0o500);
    let taken = aside(&dir).take(&dir.join("lease-stuck"), OsStr::new("lease-stuck"));
    chmod(&dir, 0o700);
    let why = taken.expect_err("moved into no quarantine");
    assert_eq!(why.kind(), io::ErrorKind::PermissionDenied, "{why}");
    assert!(dir.join("lease-stuck").is_dir(), "left in place");
}

/// Catches a quarantine that another user could write
/// (group- or other-writable) emptied and kept ("owner and mode unchecked" mutant):
/// what the sweep later moves aside would sit where others can reach it. It is
/// removed itself, never emptied through.
#[test]
fn a_quarantine_others_may_write_is_removed_itself() {
    for (mode, name) in [(0o770, "qgroup"), (0o703, "qother")] {
        let dir = scratch(name);
        let quarantine = dir.join(QUARANTINE);
        std::fs::create_dir(&quarantine).expect("mkdir");
        std::fs::write(quarantine.join("old"), b"x").expect("write");
        chmod(&quarantine, mode);
        sweep_within(&dir).expect("the start goes on");
        assert!(
            std::fs::symlink_metadata(&quarantine).is_err(),
            "a quarantine of mode {mode:o} emptied through, not removed"
        );
    }
}

/// Catches a quarantine of another user's held as the daemon's ("owner unchecked"
/// mutant): `/` (root's, for a test run as anyone else) stands in for it. It is
/// handed to `remove` (a fake one here, which removes nothing), never listed.
#[test]
fn a_quarantine_of_another_user_is_not_held() {
    // SAFETY: geteuid takes nothing and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let asked = std::cell::RefCell::new(Vec::new());
    let remove = |path: &Path| {
        asked.borrow_mut().push(path.to_owned());
        Ok(())
    };
    assert!(hold(Path::new("/"), &remove).is_none(), "root's held");
    assert_eq!(*asked.borrow(), [PathBuf::from("/")]);
}

/// Catches the macOS boot id read from anything but the boot session UUID (the boot
/// time, which can move within a boot): it has the UUID's shape.
#[cfg(target_os = "macos")]
#[test]
fn the_macos_boot_id_is_the_session_uuid() {
    let id = crate::record::boot_id().expect("boot id");
    assert_eq!(id.len(), 36, "{id}");
    for (i, c) in id.char_indices() {
        if [8, 13, 18, 23].contains(&i) {
            assert_eq!(c, '-', "{id}");
        } else {
            assert!(c.is_ascii_hexdigit(), "{id}");
        }
    }
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
    end_recorded(&dir, Duration::from_millis(30), &table, &mut aside(&dir))
        .expect("the start goes on");
    assert_eq!(reads.get(), 0, "a guarded group was walked");
    assert_eq!(runs_left(&dir), 0, "the records are dropped");

    // Below the action's group, neither the parent nor the member of the daemon's
    // own group is killed or a survivor.
    write_record(&dir, "lease-6-3", &format!("2000000031 5 {boot}\n"));
    end_recorded(&dir, Duration::from_millis(30), &table, &mut aside(&dir)).expect("not survivors");
    assert_eq!(reads.get(), 1, "the action's group was walked once");
    assert_eq!(runs_left(&dir), 0, "the record is dropped");
}
