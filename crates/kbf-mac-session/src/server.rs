//! The helper's Unix socket (S4.3): who may reach it, and one thread per connection.
//!
//! The socket lives in a root-owned directory, mode 0750, whose group is the daemon's
//! dedicated group (never `staff`, which every macOS user is in); the socket itself is
//! mode 0660. That keeps every other uid out, lease users included. Each connection's
//! caller is then checked by a [`CallerCheck`] (on macOS: the caller's code signature,
//! by audit token, against the pinned `kbf-daemon` requirement):
//!
//! 1. As soon as the connection is accepted, before a byte is read, the process at the
//!    other end is identified and checked; a refused caller is told so and its
//!    connection closed unread.
//! 2. The helper then sends [`Reply::Hello`] with a fresh random nonce, and waits at
//!    most [`REQUEST_TIMEOUT`] for the request, which must carry that nonce: a request
//!    written before the check (by a process that then executed the genuine daemon)
//!    cannot know it.
//! 3. With the request read, the process at the other end is identified again and must
//!    be the one checked at step 1 (on macOS, the same audit token: same pid, same pid
//!    version), so a process that passed the check cannot hand the connection to
//!    another (a child it forked) to write the request.
//!
//! What this does not close: a process that connects and executes the genuine daemon
//! before the helper accepts, while a child it forked keeps the connection, passes step
//! 1 as the daemon, and passes step 3 too wherever the kernel's token for the
//! connection does not follow the process that last used it. S4.3's full answer is a
//! challenge the daemon answers with a key only its own code can use.

use std::io::{self, Read as _};
use std::os::fd::AsFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use rustix::fs::{AtFlags, CWD, Gid, Mode, OFlags};

use crate::helper::{Helper, Outcome};
use crate::proto::{self, Call, RUN_FDS, Reply};

/// How long a checked caller has to send its request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The process at the other end of a connection, as the kernel names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The kernel's whole record of it (on macOS, the 32-byte audit token).
    pub token: Vec<u8>,
    /// Its pid.
    pub pid: i64,
    /// Its pid version (macOS), which tells apart processes that reuse a pid.
    pub version: i64,
}

impl std::fmt::Display for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pid {} version {}", self.pid, self.version)
    }
}

/// Decides whether the process on the other end of a connection is the genuine
/// daemon.
pub trait CallerCheck: Send + Sync {
    /// The process at the other end of `socket` now.
    ///
    /// # Errors
    /// The kernel did not say.
    fn identify(&self, socket: &UnixStream) -> Result<Caller, String>;

    /// Whether `caller` is the genuine daemon.
    ///
    /// # Errors
    /// Why the caller is refused.
    fn check(&self, caller: &Caller) -> Result<(), String>;
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
                std::thread::spawn(move || {
                    connection(&stream, &helper, check.as_ref(), REQUEST_TIMEOUT);
                });
            }
            Err(why) => return why,
        }
    }
}

/// One connection: check the caller, say hello, read the request (waiting at most
/// `timeout`), check that the same process sent it, act, reply.
pub fn connection(
    stream: &UnixStream,
    helper: &Helper,
    check: &dyn CallerCheck,
    timeout: Duration,
) {
    let refuse = |why: String| {
        tracing::warn!("refused a caller: {why}");
        reply(
            stream,
            &Reply::Refused {
                reason: format!("caller refused: {why}"),
            },
        );
    };
    let first = match check
        .identify(stream)
        .and_then(|caller| check.check(&caller).map(|()| caller))
    {
        Ok(caller) => caller,
        Err(why) => return refuse(why),
    };
    tracing::info!("caller {first} checked at accept");
    let nonce = fresh_nonce();
    reply(
        stream,
        &Reply::Hello {
            nonce: nonce.clone(),
        },
    );
    if let Err(why) = stream.set_read_timeout(Some(timeout)) {
        return refuse(format!("setting a read timeout: {why}"));
    }
    let (call, fds) = match proto::recv::<Call>(stream.as_fd(), RUN_FDS) {
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
    let now = match check.identify(stream) {
        Ok(caller) => caller,
        Err(why) => return refuse(why),
    };
    tracing::info!("caller {now} at its request (checked at accept: {first})");
    if now != first {
        return refuse(format!(
            "the process at the other end changed since the check ({first}, now {now})"
        ));
    }
    if call.nonce != nonce {
        return refuse("the request does not carry this connection's nonce".to_owned());
    }
    match helper.handle(call.request, fds) {
        Outcome::Reply(answer) => reply(stream, &answer),
        Outcome::Running(mut child) => {
            reply(stream, &Reply::Started { pid: child.id() });
            reply(stream, &exited(child.wait()));
        }
    }
}

/// 16 random bytes, in hex: a value no request written before this connection's
/// `Hello` can carry.
fn fresh_nonce() -> String {
    let mut bytes = [0u8; 16];
    // The kernel's random device is there on every running system; without it this
    // connection's thread stops here, and nothing is done for the caller.
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .expect("reading /dev/urandom");
    hex::encode(bytes)
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
