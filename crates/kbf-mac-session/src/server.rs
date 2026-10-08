//! The helper's Unix socket (S4.3): who may reach it, and one thread per connection.
//!
//! The socket lives in a root-owned directory, mode 0750, whose group is the daemon's
//! dedicated group (never `staff`, which every macOS user is in); the socket itself is
//! mode 0660. That keeps every other uid out, lease users included. Each connection's
//! caller is then checked by a [`CallerCheck`] (on macOS: the caller's code signature,
//! by audit token, against the pinned `kbf-daemon` requirement) after its request is
//! read and before anything is done: a process that connects and then executes the
//! genuine daemon is refused, because the audit token names the process as it was.

use std::io;
use std::os::fd::AsFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::Arc;

use rustix::fs::{AtFlags, CWD, Gid, Mode, OFlags};

use crate::helper::{Helper, Outcome};
use crate::proto::{self, RUN_FDS, Reply, Request};

/// Decides whether the process on the other end of a connection is the genuine
/// daemon.
pub trait CallerCheck: Send + Sync {
    /// # Errors
    /// Why the caller is refused.
    fn check(&self, socket: &UnixStream) -> Result<(), String>;
}

/// Binds the helper's socket at `path`: its directory made (or kept) mode 0750 with
/// group `gid`, any stale socket removed, the new one mode 0660 with group `gid`.
///
/// # Errors
/// The directory is a link or not a directory, or a step failed.
pub fn bind(path: &Path, gid: u32) -> io::Result<UnixListener> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the socket needs a directory")
        })?;
    match rustix::fs::mkdir(dir, Mode::from_raw_mode(0o750)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(why) => return Err(why.into()),
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let opened = rustix::fs::openat(CWD, dir, flags, Mode::empty())?;
    rustix::fs::fchown(&opened, None, Some(Gid::from_raw(gid)))?;
    rustix::fs::fchmod(&opened, Mode::from_raw_mode(0o750))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the socket needs a name"))?;
    match rustix::fs::unlinkat(&opened, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(why) => return Err(why.into()),
    }
    let listener = UnixListener::bind(path)?;
    rustix::fs::chownat(
        &opened,
        name,
        None,
        Some(Gid::from_raw(gid)),
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

/// Serves connections until `accept` fails, and returns that error.
pub fn serve(
    listener: &UnixListener,
    helper: &Arc<Helper>,
    check: &Arc<dyn CallerCheck>,
) -> io::Error {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let helper = Arc::clone(helper);
                let check = Arc::clone(check);
                std::thread::spawn(move || connection(&stream, &helper, check.as_ref()));
            }
            Err(why) => return why,
        }
    }
}

/// One connection: read the request, check the caller, act, reply.
pub fn connection(stream: &UnixStream, helper: &Helper, check: &dyn CallerCheck) {
    let (request, fds) = match proto::recv::<Request>(stream.as_fd(), RUN_FDS) {
        Ok(Some(received)) => received,
        Ok(None) => return,
        Err(why) => {
            reply(
                stream,
                &Reply::Refused {
                    reason: format!("bad request: {why}"),
                },
            );
            return;
        }
    };
    if let Err(why) = check.check(stream) {
        tracing::warn!("refused a caller: {why}");
        drop(fds);
        reply(
            stream,
            &Reply::Refused {
                reason: format!("caller refused: {why}"),
            },
        );
        return;
    }
    match helper.handle(request, fds) {
        Outcome::Reply(answer) => reply(stream, &answer),
        Outcome::Running(mut child) => {
            reply(stream, &Reply::Started { pid: child.id() });
            reply(stream, &exited(child.wait()));
        }
    }
}

/// The reply to a finished `run`.
fn exited(status: io::Result<ExitStatus>) -> Reply {
    match status {
        Ok(status) => Reply::Exited {
            code: status.code(),
            signal: status.signal(),
        },
        Err(why) => Reply::Refused {
            reason: format!("waiting for the process: {why}"),
        },
    }
}

/// Sends `answer`; a caller that went away is only logged.
fn reply(stream: &UnixStream, answer: &Reply) {
    if let Err(why) = proto::send(stream.as_fd(), answer, &[]) {
        tracing::debug!("the caller went away before the reply: {why}");
    }
}

#[cfg(test)]
mod tests;
