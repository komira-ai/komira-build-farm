//! Who may reach the updater (S4.3 of `docs/design/fleet-updates-security.md`), on
//! Linux.
//!
//! Every connection's caller is checked before its request is read:
//!
//! 1. `SO_PEERPIDFD` gives a pidfd for the process that connected, so the checks below
//!    are about that process even if it exits and its pid is reused; the pidfd's pid
//!    must be the pid `SO_PEERCRED` reports.
//! 2. The caller's effective uid (as `SO_PEERCRED` recorded it at `connect`) must be the
//!    daemon's.
//! 3. The caller's executable (`/proc/<pid>/exe`, opened, so the file itself and not a
//!    path) must hash to the installed `kbf-daemon`.
//! 4. The caller must not run with `--driver native`: on Linux the native driver runs
//!    actions as the daemon's own uid, so an action could reach the socket.
//! 5. After reading `/proc`, the pidfd must still name the same pid: once a process is
//!    reaped its pidfd names -1, so if the caller exited and its pid was reused while
//!    the checks ran (what `/proc` showed was another process's), it is refused. A
//!    pid is reused only after a reap, so a caller still alive here is the one checked.
//!
//! `SO_PEERPIDFD` needs Linux 6.5 ([`kernel_supports_peer_pidfd`]); the updater does not
//! start on an older kernel. It also does not start while any process running the
//! installed daemon uses `--driver native` ([`find_native_daemon`]).

use std::fs;
use std::io::{self, Read as _};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

/// `SO_PEERPIDFD` from the kernel's `asm-generic/socket.h` (x86-64 and arm64); the libc
/// crate does not name it yet.
const SO_PEERPIDFD: libc::c_int = 77;

/// The installed daemon a caller must be.
#[derive(Clone, Debug)]
pub struct DaemonPin {
    /// The daemon's uid.
    pub uid: u32,
    /// The installed `kbf-daemon` binary.
    pub path: PathBuf,
}

/// Why a caller was refused.
#[derive(Debug, thiserror::Error)]
pub enum CallerError {
    /// The kernel could not say who the caller is.
    #[error("peer credentials: {0}")]
    Credentials(io::Error),
    /// The caller is no longer the process that connected: it exited (its pidfd names
    /// pid -1), or its pidfd names another pid than its credentials.
    #[error("the caller is gone: its pidfd names pid {pidfd}, its credentials pid {cred}")]
    Gone {
        /// The pid the pidfd names.
        pidfd: i64,
        /// The pid `SO_PEERCRED` reported.
        cred: i64,
    },
    /// The caller's effective uid is not the daemon's.
    #[error("caller uid {0} is not the daemon's")]
    WrongUid(u32),
    /// The caller's executable is not the installed daemon.
    #[error("the caller's executable is not the installed kbf-daemon")]
    WrongExecutable,
    /// The caller runs the native driver.
    #[error("the caller runs --driver native")]
    NativeDriver,
    /// Reading the caller's or the daemon's files failed.
    #[error("{0}")]
    Io(io::Error),
}

/// Whether a kernel release string (`uname -r`) is 6.5 or later.
#[must_use]
pub fn kernel_supports_peer_pidfd(release: &str) -> bool {
    let mut parts = release.split(['.', '-']).map(str::parse::<u32>);
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= (6, 5),
        _ => false,
    }
}

/// Whether a NUL-separated command line (`/proc/<pid>/cmdline`) selects the native
/// driver, as `--driver native` or `--driver=native`.
#[must_use]
pub fn uses_native_driver(cmdline: &[u8]) -> bool {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    args.windows(2)
        .any(|w| w[0] == b"--driver" && w[1] == b"native")
        || args.contains(&b"--driver=native".as_slice())
}

/// SHA-256 of everything `file` reads.
fn hash(mut file: fs::File) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buf[..n]);
    }
}

/// SHA-256 of the file at `path`.
///
/// # Errors
/// It cannot be opened or read.
pub fn hash_path(path: &Path) -> io::Result<[u8; 32]> {
    hash(fs::File::open(path)?)
}

/// The first process under `proc_root` (normally `/proc`) whose command line selects the
/// native driver and whose executable hashes to `daemon` (the installed daemon's hash).
/// Processes that cannot be read (another user's, or exited meanwhile) are skipped.
#[must_use]
pub fn find_native_daemon(proc_root: &Path, daemon: &[u8; 32]) -> Option<u32> {
    let entries = fs::read_dir(proc_root).ok()?;
    entries.filter_map(Result::ok).find_map(|entry| {
        let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
        let cmdline = fs::read(entry.path().join("cmdline")).ok()?;
        let native =
            uses_native_driver(&cmdline) && hash_path(&entry.path().join("exe")).ok()? == *daemon;
        native.then_some(pid)
    })
}

/// The caller's pidfd, from `SO_PEERPIDFD`.
///
/// # Errors
/// `fd` is not a connected Unix socket, or the kernel is older than 6.5.
pub fn peer_pidfd(fd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let mut pidfd: libc::c_int = -1;
    let mut len = libc::socklen_t::try_from(size_of::<libc::c_int>()).expect("4 fits");
    // SAFETY: `pidfd` and `len` are live locals the kernel writes at most `len` bytes
    // into; the socket fd is borrowed for the call.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            SO_PEERPIDFD,
            (&raw mut pidfd).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success the kernel installed a new pidfd, which nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(pidfd) })
}

/// The pid a pidfd names (its `/proc/self/fdinfo` `Pid:` line): -1 once the process has
/// exited.
fn pidfd_pid(pidfd: &OwnedFd) -> io::Result<i64> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?;
    let pid = info
        .lines()
        .find_map(|l| l.strip_prefix("Pid:")?.trim().parse().ok());
    pid.ok_or(io::ErrorKind::InvalidData.into())
}

/// Checks that `pidfd` still names the live process `pid`: a reaped process's pidfd
/// names pid -1, and a process outside this pid namespace names 0.
fn same_process(pidfd: &OwnedFd, pid: i64) -> Result<(), CallerError> {
    let now = pidfd_pid(pidfd).map_err(CallerError::Io)?;
    if now == pid {
        Ok(())
    } else {
        Err(CallerError::Gone {
            pidfd: now,
            cred: pid,
        })
    }
}

/// Checks that the process at the other end of `stream` is the installed daemon (the
/// module documentation lists the checks, in order).
///
/// # Errors
/// The first check that fails.
pub fn check_caller(stream: &UnixStream, daemon: &DaemonPin) -> Result<(), CallerError> {
    let cred = rustix::net::sockopt::socket_peercred(stream)
        .map_err(|e| CallerError::Credentials(e.into()))?;
    let pidfd = peer_pidfd(stream.as_fd()).map_err(CallerError::Credentials)?;
    let pid = i64::from(cred.pid.as_raw_nonzero().get());
    same_process(&pidfd, pid)?;
    if cred.uid.as_raw() != daemon.uid {
        return Err(CallerError::WrongUid(cred.uid.as_raw()));
    }
    let exe = fs::File::open(format!("/proc/{pid}/exe")).and_then(hash);
    if exe.map_err(CallerError::Io)? != hash_path(&daemon.path).map_err(CallerError::Io)? {
        return Err(CallerError::WrongExecutable);
    }
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).map_err(CallerError::Io)?;
    if uses_native_driver(&cmdline) {
        return Err(CallerError::NativeDriver);
    }
    same_process(&pidfd, pid)
}

#[cfg(test)]
#[path = "caller_tests.rs"]
mod tests;
