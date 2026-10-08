//! The caller check against real processes: this test binary plays the daemon (its own
//! executable is the "installed daemon") and re-runs itself as a child to connect from
//! another process, with or without `--driver native` on its command line.

use std::io::Read as _;
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};

use super::*;
use crate::testkit;

const CHILD_ENV: &str = "KBF_UPDATER_TEST_CHILD";

/// Not a test of its own: the body of a child process. With `KBF_UPDATER_TEST_CHILD`
/// unset it does nothing. `connect:<path>` connects to the socket and waits for the
/// server to close it; `connect-exit:<path>` connects and exits at once; `idle` waits for
/// its standard input to close.
#[test]
fn child_helper() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    if let Some(path) = mode.strip_prefix("connect:") {
        let mut s = UnixStream::connect(path).unwrap();
        let _ = s.read(&mut [0u8; 1]);
    } else if let Some(path) = mode.strip_prefix("connect-exit:") {
        let _s = UnixStream::connect(path).unwrap();
        std::process::exit(0);
    } else {
        let _ = std::io::stdin().read(&mut [0u8; 1]);
    }
}

/// Runs this test binary as a child in `mode`, with `extra` on its command line.
fn child(mode: &str, extra: &[&str]) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "caller::tests::child_helper",
            "--nocapture",
            "--",
        ])
        .args(extra)
        .env(CHILD_ENV, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

fn me() -> DaemonPin {
    DaemonPin {
        uid: rustix::process::geteuid().as_raw(),
        path: std::env::current_exe().unwrap(),
    }
}

/// A listener at `<scratch>/s` and its child caller, accepted.
fn connect_from_child(name: &str, mode: &str, extra: &[&str]) -> (UnixStream, Child) {
    let dir = testkit::short_dir(name);
    let path = dir.path.join("s");
    let listener = UnixListener::bind(&path).unwrap();
    let c = child(&format!("{mode}:{}", path.display()), extra);
    let (stream, _) = listener.accept().unwrap();
    (stream, c)
}

/// Catches: a kernel before 6.5 let through (no `SO_PEERPIDFD`), a newer one refused,
/// or an unparsable release read as new enough.
#[test]
fn the_kernel_floor_is_6_5() {
    for (release, ok) in [
        ("6.5.0", true),
        ("6.17.0-41-generic", true),
        ("7.0", true),
        ("6.4.99", false),
        ("5.15.0-1-azure", false),
        ("6", false),
        ("x.y", false),
        ("6.x", false),
        ("", false),
    ] {
        assert_eq!(kernel_supports_peer_pidfd(release), ok, "{release}");
    }
}

/// Catches: either spelling of the native driver missed, or another flag's value or a
/// lone `native` read as the driver.
#[test]
fn native_driver_command_lines_are_recognised() {
    for (cmdline, native) in [
        (&b"kbf-daemon\0--driver\0native\0"[..], true),
        (b"kbf-daemon\0--driver=native", true),
        (b"kbf-daemon\0--driver\0container\0", false),
        (b"kbf-daemon\0native\0--driver", false),
        (b"kbf-daemon\0--scratch\0native\0", false),
        (b"", false),
    ] {
        assert_eq!(uses_native_driver(cmdline), native, "{cmdline:?}");
    }
}

/// Catches: the start-up scan matching a native command line whose executable is not
/// the daemon, the daemon without the native driver, or failing on unreadable entries.
#[test]
fn the_start_up_scan_finds_only_a_native_daemon() {
    let dir = testkit::scratch("scan");
    let daemon = sha2::Sha256::digest(b"daemon").into();
    let proc_entry = |root: &Path, pid: &str, cmdline: Option<&[u8]>, exe: Option<&[u8]>| {
        let d = root.join(pid);
        fs::create_dir_all(&d).unwrap();
        if let Some(c) = cmdline {
            fs::write(d.join("cmdline"), c).unwrap();
        }
        if let Some(e) = exe {
            fs::write(d.join("exe"), e).unwrap();
        }
    };
    let root = dir.join("proc");
    proc_entry(
        &root,
        "self",
        Some(b"kbf-daemon\0--driver\0native"),
        Some(b"daemon"),
    );
    proc_entry(
        &root,
        "200",
        Some(b"other\0--driver\0native"),
        Some(b"other"),
    );
    proc_entry(
        &root,
        "300",
        Some(b"kbf-daemon\0--driver\0container"),
        Some(b"daemon"),
    );
    proc_entry(&root, "400", None, Some(b"daemon"));
    proc_entry(&root, "500", Some(b"kbf-daemon\0--driver=native"), None);
    assert_eq!(find_native_daemon(&root, &daemon), None);
    proc_entry(
        &root,
        "100",
        Some(b"kbf-daemon\0--driver=native"),
        Some(b"daemon"),
    );
    assert_eq!(find_native_daemon(&root, &daemon), Some(100));
    assert_eq!(find_native_daemon(&dir.join("missing"), &daemon), None);
}

/// Catches: the scan of the real `/proc` missing a running native daemon.
#[test]
fn the_start_up_scan_finds_a_real_native_daemon() {
    let mut c = child("idle", &["--driver", "native"]);
    let me = hash_path(&std::env::current_exe().unwrap()).unwrap();
    let found = find_native_daemon(Path::new("/proc"), &me);
    drop(c.stdin.take());
    c.wait().unwrap();
    // Another test's native child may be found first; any is a native daemon.
    assert!(found.is_some(), "the native child was not found");
}

/// Catches: the genuine daemon (right uid, right executable, no native driver) refused.
#[test]
fn the_daemon_itself_is_accepted() {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    check_caller(&ours, &me()).unwrap();
}

/// Catches: the uid check removed (any local user's process reaches the updater).
#[test]
fn a_caller_with_another_uid_is_refused() {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let pin = DaemonPin {
        uid: me().uid + 1,
        ..me()
    };
    assert!(matches!(
        check_caller(&ours, &pin),
        Err(CallerError::WrongUid(_))
    ));
}

/// Catches: the executable check removed (a stray binary of the daemon's uid reaches
/// the updater), or a missing daemon binary treated as a match.
#[test]
fn a_caller_that_is_not_the_installed_daemon_is_refused() {
    let dir = testkit::scratch("other-exe");
    let other = dir.join("kbf-daemon");
    fs::write(&other, b"another binary").unwrap();
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let pin = DaemonPin {
        path: other,
        ..me()
    };
    assert!(matches!(
        check_caller(&ours, &pin),
        Err(CallerError::WrongExecutable)
    ));
    let pin = DaemonPin {
        path: dir.join("missing"),
        ..me()
    };
    assert!(matches!(check_caller(&ours, &pin), Err(CallerError::Io(_))));
}

/// Catches: the native-driver check removed from the caller check: the genuine daemon
/// binary run with `--driver native` (whose actions share its uid) is refused.
#[test]
fn a_daemon_running_the_native_driver_is_refused() {
    let (stream, mut c) = connect_from_child("native", "connect", &["--driver", "native"]);
    let r = check_caller(&stream, &me());
    drop(stream);
    c.wait().unwrap();
    assert!(matches!(r, Err(CallerError::NativeDriver)), "{r:?}");
    let (stream, mut c) = connect_from_child("container", "connect", &["--driver", "container"]);
    let r = check_caller(&stream, &me());
    drop(stream);
    c.wait().unwrap();
    assert!(r.is_ok(), "{r:?}");
}

/// Catches: a caller that exited before it was checked accepted (its pid could already
/// name another process). Kernels before 6.16 refuse `SO_PEERPIDFD` for a reaped peer
/// (a credentials error); later ones hand out a pidfd that names pid -1.
#[test]
fn a_caller_that_exited_is_refused() {
    let (stream, mut c) = connect_from_child("exited", "connect-exit", &[]);
    c.wait().unwrap();
    let r = check_caller(&stream, &me());
    assert!(
        matches!(
            r,
            Err(CallerError::Gone { pidfd: -1, .. } | CallerError::Credentials(_))
        ),
        "{r:?}"
    );
}

/// Catches: the final pidfd check removed from `check_caller`. The caller passes every
/// `/proc` read and is then killed and reaped, so its pid is free for another process
/// and what `/proc` showed may no longer describe whoever holds it; only the recheck
/// after the reads notices (its pidfd now names pid -1).
#[test]
fn a_caller_reaped_after_the_proc_reads_is_refused() {
    let (stream, mut c) = connect_from_child("reaped", "connect", &[]);
    let r = check_caller_then(&stream, &me(), &mut || {
        c.kill().unwrap();
        c.wait().unwrap();
    });
    assert!(
        matches!(r, Err(CallerError::Gone { pidfd: -1, .. })),
        "{r:?}"
    );
}

/// Catches: the pid-reuse guard removed: a pidfd whose process was reaped still
/// passing as that pid.
#[test]
fn a_reaped_process_is_not_the_same_process() {
    let mut c = child("idle", &[]);
    let pid = i64::from(c.id());
    let pidfd = rustix::process::pidfd_open(
        rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap(),
        rustix::process::PidfdFlags::empty(),
    )
    .unwrap();
    same_process(&pidfd, pid).unwrap();
    assert!(matches!(
        same_process(&pidfd, pid + 1),
        Err(CallerError::Gone { .. })
    ));
    drop(c.stdin.take());
    c.wait().unwrap();
    assert!(matches!(
        same_process(&pidfd, pid),
        Err(CallerError::Gone { pidfd: -1, .. })
    ));
}

/// Catches: a descriptor that is not a socket passing as a caller.
#[test]
fn a_non_socket_has_no_credentials() {
    let (read, _write) = std::io::pipe().unwrap();
    let read = OwnedFd::from(read);
    assert!(peer_pidfd(read.as_fd()).is_err());
    let not_a_socket = UnixStream::from(read);
    assert!(matches!(
        check_caller(&not_a_socket, &me()),
        Err(CallerError::Credentials(_))
    ));
}
