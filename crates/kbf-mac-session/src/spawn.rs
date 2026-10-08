//! Starting an action's process as the lease user (`run`).
//!
//! The daemon passes the argument vector and environment, the three standard streams
//! and the lease directory as descriptors. The child gets the lease user's uid and the
//! lease group's gid, no supplementary groups (the standard library drops them when
//! root sets a uid), its own process group, a clean environment with `HOME`, `USER`
//! and `LOGNAME` set by the helper, and the lease directory as its working directory
//! (`fchdir` after the uid change, so the lease user must be able to search it).
//!
//! Every other descriptor is closed across `exec`: the helper serves several leases at
//! once, and macOS cannot receive a descriptor already close-on-exec, so a lease
//! directory just received for one lease could otherwise leak into another lease's
//! process started at that moment.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// The highest descriptor number the child's close-on-exec pass reaches when the
/// descriptor limit is unlimited or higher.
const FD_CEILING: libc::c_int = 1 << 16;

/// The variables the helper sets itself, whatever the daemon passed.
pub const RESERVED_ENV: [&str; 3] = ["HOME", "USER", "LOGNAME"];

/// What to start, and as whom.
#[derive(Debug)]
pub struct Spawn {
    pub uid: u32,
    pub gid: u32,
    pub user: String,
    pub home: PathBuf,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: OwnedFd,
    pub stdout: OwnedFd,
    pub stderr: OwnedFd,
    /// The lease directory, made the working directory.
    pub cwd: OwnedFd,
}

/// Checks an argument vector and environment before anything starts.
///
/// # Errors
/// The vector is empty, or a name is empty or holds `=` or NUL, or a value or argument
/// holds NUL.
pub fn check(argv: &[String], env: &[(String, String)]) -> Result<(), String> {
    if argv.is_empty() {
        return Err("the argument vector is empty".to_owned());
    }
    if argv.iter().any(|arg| arg.contains('\0')) {
        return Err("an argument holds NUL".to_owned());
    }
    for (name, value) in env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err(format!("environment variable {name:?} is malformed"));
        }
    }
    Ok(())
}

/// Starts `spawn`'s process. The caller has run [`check`].
///
/// # Errors
/// The process could not be started (the program is missing, the uid change or
/// `fchdir` failed).
pub fn spawn(spawn: Spawn) -> io::Result<Child> {
    let (program, args) = spawn
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argument vector"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(
            spawn
                .env
                .iter()
                .filter(|(name, _)| !RESERVED_ENV.contains(&name.as_str()))
                .map(|(name, value)| (name, value)),
        )
        .env("HOME", &spawn.home)
        .env("USER", &spawn.user)
        .env("LOGNAME", OsString::from(&spawn.user))
        .uid(spawn.uid)
        .gid(spawn.gid)
        .process_group(0)
        .stdin(Stdio::from(spawn.stdin))
        .stdout(Stdio::from(spawn.stdout))
        .stderr(Stdio::from(spawn.stderr));
    let cwd = spawn.cwd.as_raw_fd();
    // SAFETY: the closure runs in the child between fork and exec; `in_child` calls
    // only async-signal-safe functions and allocates nothing. `cwd` stays open in the
    // parent until `spawn` returns, so the child's copy is the lease directory.
    unsafe {
        command.pre_exec(child_steps(cwd));
    }
    let child = command.spawn();
    drop(spawn.cwd);
    child
}

/// The closure `pre_exec` runs in the child: [`in_child`] with the lease directory.
fn child_steps(cwd: libc::c_int) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || in_child(cwd)
}

/// The child's last steps before `exec`, after the uid change: enter the lease
/// directory and mark every descriptor above the standard streams close-on-exec.
/// Async-signal-safe (fchdir, getrlimit, fcntl).
fn in_child(cwd: libc::c_int) -> io::Result<()> {
    // SAFETY: fchdir takes any number; one that is not a directory fails.
    if unsafe { libc::fchdir(cwd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    close_on_exec_from(3);
    Ok(())
}

/// Marks every descriptor from `first` up to the descriptor limit (at most
/// [`FD_CEILING`]) close-on-exec. Async-signal-safe.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // bounded by FD_CEILING
fn close_on_exec_from(first: libc::c_int) {
    // Left at the ceiling if getrlimit fails, so the pass never shrinks to nothing.
    let mut limit = libc::rlimit {
        rlim_cur: FD_CEILING as libc::rlim_t,
        rlim_max: FD_CEILING as libc::rlim_t,
    };
    // SAFETY: `limit` is a writable rlimit for the call to fill.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) };
    let ceiling = limit.rlim_cur.min(FD_CEILING as libc::rlim_t) as libc::c_int;
    for fd in first..ceiling {
        // SAFETY: F_SETFD on a number that is not an open descriptor fails with EBADF
        // and changes nothing.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::os::fd::FromRawFd as _;

    use super::*;

    fn pipe() -> (std::io::PipeReader, OwnedFd) {
        let (read, write) = std::io::pipe().unwrap();
        (read, write.into())
    }

    fn null() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    fn me() -> (u32, u32) {
        (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        )
    }

    fn base(argv: &[&str], cwd: &std::path::Path) -> (Spawn, std::io::PipeReader) {
        let (out, write) = pipe();
        let (uid, gid) = me();
        let spawn = Spawn {
            uid,
            gid,
            user: "kbf-lease-1-1".to_owned(),
            home: PathBuf::from("/var/empty/kbf-lease-1-1"),
            argv: argv.iter().map(|&a| a.to_owned()).collect(),
            env: vec![
                ("KEEP".to_owned(), "kept".to_owned()),
                ("HOME".to_owned(), "/root".to_owned()),
            ],
            stdin: null(),
            stdout: write,
            stderr: null(),
            cwd: std::fs::File::open(cwd).unwrap().into(),
        };
        (spawn, out)
    }

    /// Catches: the lease directory not made the working directory, the daemon's
    /// environment (or the helper's) passed through unchecked, the reserved names
    /// left to the daemon, or a process left in the helper's process group.
    #[test]
    fn the_process_runs_in_the_lease_directory_with_a_clean_environment() {
        let dir =
            std::env::temp_dir().join(format!("kbf-mac-session-spawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // CARGO_MANIFEST_DIR is in the test's own environment, and must not leak.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let script = "pwd; echo \"$HOME $USER $LOGNAME $KEEP ${CARGO_MANIFEST_DIR:-clean}\"; /bin/ps -o pgid= -p $$; echo $$";
        let (spawn, mut out) = base(&["/bin/sh", "-c", script], &dir);
        let mut child = super::spawn(spawn).unwrap();
        assert!(child.wait().unwrap().success());
        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        assert_eq!(
            lines[0],
            std::fs::canonicalize(&dir).unwrap().to_str().unwrap()
        );
        assert_eq!(
            lines[1],
            "/var/empty/kbf-lease-1-1 kbf-lease-1-1 kbf-lease-1-1 kept clean"
        );
        assert_eq!(
            lines[2], lines[3],
            "not the leader of its own process group"
        );
    }

    /// Catches: a descriptor of the helper (another lease's directory, say) inherited
    /// by the action, which the close-on-exec pass exists to prevent.
    #[test]
    fn no_other_descriptor_reaches_the_process() {
        let dir = std::env::temp_dir();
        // An inheritable descriptor, as a lease directory just received is on macOS.
        let raw = rustix::fs::open(
            "/dev/null",
            rustix::fs::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let leaked = std::os::fd::IntoRawFd::into_raw_fd(raw);
        let (spawn, mut out) = base(
            &[
                "/bin/sh",
                "-c",
                &format!("test -e /dev/fd/{leaked} && echo leaked || echo closed"),
            ],
            &dir,
        );
        let mut child = super::spawn(spawn).unwrap();
        child.wait().unwrap();
        // SAFETY: `leaked` is the descriptor opened above, owned by nothing else.
        drop(unsafe { OwnedFd::from_raw_fd(leaked) });
        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        assert_eq!(text.trim(), "closed");
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let (spawn, _out) = base(&["/no/such/program"], &std::env::temp_dir());
        assert_eq!(
            super::spawn(spawn).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    /// Catches: a working directory the lease user cannot enter silently replaced by
    /// another (the helper's own).
    #[test]
    fn a_lease_directory_that_cannot_be_entered_is_an_error() {
        let (mut spawn, _out) = base(&["/bin/true"], &std::env::temp_dir());
        // A descriptor that is not a directory: fchdir fails in the child.
        spawn.cwd = null();
        assert_eq!(
            super::spawn(spawn).unwrap_err().raw_os_error(),
            Some(libc::ENOTDIR)
        );
        let (mut spawn, _out) = base(&["/bin/true"], &std::env::temp_dir());
        spawn.argv.clear();
        assert_eq!(
            super::spawn(spawn).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn malformed_vectors_are_refused() {
        let ok = |argv: &[&str], env: &[(&str, &str)]| {
            let argv: Vec<String> = argv.iter().map(|&a| a.to_owned()).collect();
            let env: Vec<(String, String)> = env
                .iter()
                .map(|&(n, v)| (n.to_owned(), v.to_owned()))
                .collect();
            check(&argv, &env)
        };
        assert_eq!(ok(&["/bin/true"], &[("A", "b=c")]), Ok(()));
        assert!(ok(&[], &[]).unwrap_err().contains("empty"));
        assert!(ok(&["a\0b"], &[]).unwrap_err().contains("NUL"));
        for env in [("", "x"), ("A=B", "x"), ("A\0", "x"), ("A", "x\0")] {
            assert!(
                ok(&["/bin/true"], &[env])
                    .unwrap_err()
                    .contains("malformed"),
                "{env:?}"
            );
        }
    }

    /// The child's steps, run in the test process (the child's own run leaves no
    /// coverage record: it ends in `exec`). Entering the current directory leaves the
    /// process where it was; a descriptor that is not a directory is an error.
    #[test]
    fn the_child_steps_enter_the_directory_or_fail() {
        let here = std::fs::File::open(".").unwrap();
        child_steps(here.as_raw_fd())().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(
            in_child(file.as_raw_fd()).unwrap_err().raw_os_error(),
            Some(libc::ENOTDIR)
        );
    }
}
