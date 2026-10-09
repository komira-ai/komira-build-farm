//! One command: its paths checked against the shares, started as the leader of its own
//! process group, reaped with its resource usage, and its whole group killed.
//!
//! The run ends when the leader ends, however that happens. Anything left in the group
//! at that point is killed, and the run is reported only once the group is empty, so
//! nothing the command started can write to the outputs share after the host has read
//! the exit. A process that leaves the group (`setsid`, `setpgid`) is not reached; in a
//! VM the guest is destroyed after the lease, which ends it.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::wire::{OutputEntry, OutputKind, RunRequest, Usage};

/// The names of the command's stdout and stderr in the outputs share. A requested
/// output may not be either, or lie under either.
pub const STDOUT: &str = "stdout";
pub const STDERR: &str = "stderr";

/// How often the leader is polled and the group's emptiness checked.
pub const POLL: Duration = Duration::from_millis(5);

/// How long a killed group may take to empty before the run is reported with
/// stragglers.
pub const DRAIN: Duration = Duration::from_secs(5);

/// A request the shares do not allow, or that cannot be expressed to the OS.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestError {
    #[error("argv is empty")]
    NoProgram,
    #[error("{0:?} holds a NUL byte")]
    Nul(String),
    #[error("environment name {0:?} is empty or holds '='")]
    EnvName(String),
    #[error("{what} {path:?} is not a plain relative path")]
    NotRelative { what: &'static str, path: String },
    #[error("output {0:?} is reserved for the command's stdout or stderr")]
    Reserved(String),
    #[error("cwd {0:?} is not a directory inside the inputs share")]
    Cwd(String),
}

/// Checks `req` and returns the working directory it names.
///
/// The working directory is resolved through symlinks and must still be inside the
/// inputs share: a symlink in the share that points out of it is refused.
///
/// # Errors
/// The first rule `req` breaks.
pub fn check(req: &RunRequest, inputs: &Path) -> Result<PathBuf, RequestError> {
    if req.argv.is_empty() {
        return Err(RequestError::NoProgram);
    }
    let strings = req
        .argv
        .iter()
        .chain(req.env.iter().flat_map(|(k, v)| [k, v]))
        .chain(std::iter::once(&req.cwd))
        .chain(&req.outputs);
    if let Some(s) = strings.into_iter().find(|s| s.contains('\0')) {
        return Err(RequestError::Nul(s.clone()));
    }
    if let Some((k, _)) = req
        .env
        .iter()
        .find(|(k, _)| k.is_empty() || k.contains('='))
    {
        return Err(RequestError::EnvName(k.clone()));
    }
    for o in &req.outputs {
        match plain_relative(o) {
            None | Some(None) => {
                return Err(RequestError::NotRelative {
                    what: "output",
                    path: o.clone(),
                });
            }
            Some(Some(first)) if first == STDOUT || first == STDERR => {
                return Err(RequestError::Reserved(o.clone()));
            }
            Some(Some(_)) => {}
        }
    }
    if plain_relative(&req.cwd).is_none() {
        return Err(RequestError::NotRelative {
            what: "cwd",
            path: req.cwd.clone(),
        });
    }
    let cwd_err = || RequestError::Cwd(req.cwd.clone());
    let root = inputs.canonicalize().map_err(|_| cwd_err())?;
    let cwd = root.join(&req.cwd).canonicalize().map_err(|_| cwd_err())?;
    if !cwd.starts_with(&root) || !cwd.is_dir() {
        return Err(cwd_err());
    }
    Ok(cwd)
}

/// `Some(first component)` when `p` is made only of normal components (`Some(None)`
/// for the empty path), `None` when it is absolute or holds `.` or `..`.
fn plain_relative(p: &str) -> Option<Option<&str>> {
    let mut first = None;
    for c in Path::new(p).components() {
        match c {
            Component::Normal(n) => {
                first = first.or(n.to_str());
            }
            _ => return None,
        }
    }
    // `a/./b` loses its `.` in `components()`; refuse it from the text.
    if p.split('/').any(|c| c == "." || c == "..") {
        return None;
    }
    Some(first)
}

/// A started command: the leader of its own process group, not yet reaped. Its pid,
/// which is also the group id, cannot be reused before [`Running::wait`] or
/// [`Running::try_reap`] reaps it.
#[derive(Debug)]
pub struct Running {
    pid: libc::pid_t,
    started: Instant,
}

/// How the leader ended, as `wait4` reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reaped {
    /// `Ok(code)` for an exit, `Err(signal)` for a signal.
    pub status: Result<i32, i32>,
    pub usage: Usage,
}

impl Running {
    /// Creates `stdout` and `stderr` in `outputs` (both must be new: a second agent on
    /// the same share cannot run a second command) and starts `req` in `cwd` with
    /// exactly `req.env`, stdin closed, as the leader of a new process group.
    ///
    /// # Errors
    /// A file already exists or cannot be created, or the program cannot be started.
    pub fn start(req: &RunRequest, cwd: &Path, outputs: &Path) -> io::Result<Self> {
        let stdout = create_new(&outputs.join(STDOUT))?;
        let stderr = create_new(&outputs.join(STDERR))?;
        let mut command = Command::new(&req.argv[0]);
        command
            .args(&req.argv[1..])
            .env_clear()
            .envs(req.env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0);
        let started = Instant::now();
        let child = command.spawn()?;
        // Dropping a `std::process::Child` neither waits for it nor kills it: the
        // process is this type's to reap. A pid always fits `pid_t`.
        let pid = libc::pid_t::try_from(child.id()).map_err(io::Error::other)?;
        Ok(Self { pid, started })
    }

    /// The leader's pid, which is also its process group id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid.unsigned_abs()
    }

    /// When the command was started.
    #[must_use]
    pub fn started(&self) -> Instant {
        self.started
    }

    /// Reaps the leader if it has ended.
    ///
    /// # Errors
    /// `wait4` failed (the leader is not this process's child).
    pub fn try_reap(&self) -> io::Result<Option<Reaped>> {
        self.reap(libc::WNOHANG)
    }

    /// Waits for the leader to end and reaps it.
    ///
    /// # Errors
    /// `wait4` failed.
    pub fn wait(&self) -> io::Result<Reaped> {
        loop {
            match self.reap(0) {
                Ok(Some(r)) => return Ok(r),
                Ok(None) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn reap(&self, flags: libc::c_int) -> io::Result<Option<Reaped>> {
        let mut status: libc::c_int = 0;
        // SAFETY: `rusage` is a C struct of integers; all zeroes is a valid value.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are to live locals of the right types, which `wait4`
        // writes and does not keep.
        let reaped = unsafe { libc::wait4(self.pid, &raw mut status, flags, &raw mut usage) };
        match reaped {
            0 => Ok(None),
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(Some(Reaped {
                status: if libc::WIFEXITED(status) {
                    Ok(libc::WEXITSTATUS(status))
                } else {
                    Err(libc::WTERMSIG(status))
                },
                usage: Usage {
                    user_micros: micros(usage.ru_utime),
                    system_micros: micros(usage.ru_stime),
                    peak_rss_bytes: rss_bytes(usage.ru_maxrss),
                    wall_micros: u64::try_from(self.started.elapsed().as_micros())
                        .unwrap_or(u64::MAX),
                },
            })),
        }
    }

    /// Sends SIGKILL to the whole process group.
    pub fn kill_group(&self) {
        kill_group(self.pid);
    }

    /// Kills the group until it is empty, for at most `limit`. Call it once the leader
    /// is reaped. Returns whether the group emptied.
    #[must_use]
    pub fn drain(&self, limit: Duration) -> bool {
        let give_up = Instant::now() + limit;
        loop {
            kill_group(self.pid);
            if !group_exists(self.pid) {
                return true;
            }
            if Instant::now() >= give_up {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }
}

fn kill_group(pgid: libc::pid_t) {
    // SAFETY: `kill` takes plain integers. A negative pid names a process group; one
    // that is empty makes the call fail with ESRCH, which is what a kill of finished
    // work should do: nothing.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

fn group_exists(pgid: libc::pid_t) -> bool {
    // SAFETY: signal 0 checks only that the group exists and may be signalled.
    let rc = unsafe { libc::kill(-pgid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

// `tv_usec` is an `i32` on macOS and an `i64` on Linux, where the conversion is a no-op.
#[allow(clippy::useless_conversion)]
fn micros(t: libc::timeval) -> u64 {
    let micros = i64::from(t.tv_sec) * 1_000_000 + i64::from(t.tv_usec);
    u64::try_from(micros).unwrap_or(0)
}

/// `ru_maxrss` is in KiB on Linux and in bytes on macOS.
fn rss_bytes(maxrss: libc::c_long) -> u64 {
    let n = u64::try_from(maxrss).unwrap_or(0);
    if cfg!(target_os = "macos") {
        n
    } else {
        n * 1024
    }
}

/// What each of `paths` holds under `outputs`, looking at every component without
/// following a symlink: a component on the way that is not a real directory makes the
/// output missing.
#[must_use]
pub fn report_outputs(outputs: &Path, paths: &[String]) -> Vec<OutputEntry> {
    paths
        .iter()
        .map(|p| {
            let (kind, size) = examine(outputs, p);
            OutputEntry {
                path: p.clone(),
                kind,
                size,
            }
        })
        .collect()
}

fn examine(root: &Path, rel: &str) -> (OutputKind, u64) {
    let parts: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    let Some((last, dirs)) = parts.split_last() else {
        return (OutputKind::Missing, 0);
    };
    let mut at = root.to_path_buf();
    for dir in dirs {
        at.push(dir);
        if !std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_dir()) {
            return (OutputKind::Missing, 0);
        }
    }
    at.push(last);
    let Ok(meta) = std::fs::symlink_metadata(&at) else {
        return (OutputKind::Missing, 0);
    };
    let t = meta.file_type();
    if t.is_symlink() {
        (OutputKind::Symlink, 0)
    } else if t.is_file() {
        (OutputKind::File, meta.len())
    } else if t.is_dir() {
        (OutputKind::Directory, 0)
    } else {
        (OutputKind::Other, 0)
    }
}
