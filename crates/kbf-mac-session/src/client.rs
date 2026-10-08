//! The daemon's side of the helper's socket: one connection per request, which waits
//! for the helper's `Hello` and answers it with the request and its nonce.

use std::fmt;
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::grant::AdminGrant;
use crate::proto::{self, Call, Reply, Request};

/// Why a call did not succeed.
#[derive(Debug)]
pub enum ClientError {
    /// The socket failed, or the helper hung up.
    Io(io::Error),
    /// The helper refused or failed, and said why.
    Refused(String),
    /// The helper answered something the request does not expect.
    Unexpected(Reply),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(why) => write!(f, "kbf-mac-session: {why}"),
            Self::Refused(why) => write!(f, "kbf-mac-session refused: {why}"),
            Self::Unexpected(reply) => write!(f, "kbf-mac-session answered {reply:?}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(why: io::Error) -> Self {
        Self::Io(why)
    }
}

/// A connection to the helper's socket at a path.
#[derive(Clone, Debug)]
pub struct Client {
    socket: PathBuf,
}

/// A process the helper started; [`Running::wait`] reports how it ended.
#[derive(Debug)]
pub struct Running {
    stream: UnixStream,
    pub pid: u32,
}

/// How a process ended: its exit code, or the signal that ended it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl Client {
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    fn call(
        &self,
        request: &Request,
        fds: &[BorrowedFd<'_>],
    ) -> Result<(UnixStream, Reply), ClientError> {
        call_on(UnixStream::connect(&self.socket)?, request, fds)
    }

    /// `user-create`: returns the new user's uid.
    ///
    /// # Errors
    /// The call failed or was refused.
    pub fn user_create(&self, lease: &str, grant: Option<&AdminGrant>) -> Result<u32, ClientError> {
        let request = Request::UserCreate {
            lease: lease.to_owned(),
            grant: grant.cloned(),
        };
        match self.call(&request, &[])?.1 {
            Reply::Created { uid } => Ok(uid),
            other => Err(unexpected(other)),
        }
    }

    /// `run`: starts `argv` as the lease user with the four descriptors stdin,
    /// stdout, stderr and the lease directory.
    ///
    /// # Errors
    /// The call failed or was refused.
    pub fn run(
        &self,
        lease: &str,
        argv: &[String],
        env: &[(String, String)],
        fds: [BorrowedFd<'_>; 4],
    ) -> Result<Running, ClientError> {
        let request = Request::Run {
            lease: lease.to_owned(),
            argv: argv.to_vec(),
            env: env.to_vec(),
        };
        match self.call(&request, &fds)? {
            (stream, Reply::Started { pid }) => Ok(Running { stream, pid }),
            (_, other) => Err(unexpected(other)),
        }
    }

    /// `kill-uid`: returns once no process of the lease user is left.
    ///
    /// # Errors
    /// The call failed or was refused.
    pub fn kill_uid(&self, lease: &str) -> Result<(), ClientError> {
        let request = Request::KillUid {
            lease: lease.to_owned(),
        };
        match self.call(&request, &[])?.1 {
            Reply::Killed => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// `user-delete`: returns whether a user record was there to delete.
    ///
    /// # Errors
    /// The call failed or was refused.
    pub fn user_delete(&self, lease: &str) -> Result<bool, ClientError> {
        let request = Request::UserDelete {
            lease: lease.to_owned(),
        };
        match self.call(&request, &[])?.1 {
            Reply::Deleted { existed } => Ok(existed),
            other => Err(unexpected(other)),
        }
    }
}

/// Sends `request` over `stream`, a connection to the helper not yet used: waits for
/// the helper's `Hello` and sends the request with its nonce. Returns the stream and
/// the helper's first answer to the request.
///
/// # Errors
/// The helper refused the caller, answered something other than `Hello`, or the
/// connection failed.
pub fn call_on(
    stream: UnixStream,
    request: &Request,
    fds: &[BorrowedFd<'_>],
) -> Result<(UnixStream, Reply), ClientError> {
    let nonce = match next(&stream)? {
        Reply::Hello { nonce } => nonce,
        other => return Err(unexpected(other)),
    };
    let call = Call {
        nonce,
        request: request.clone(),
    };
    proto::send(stream.as_fd(), &call, fds)?;
    let reply = next(&stream)?;
    Ok((stream, reply))
}

impl Running {
    /// Waits for the process to end.
    ///
    /// # Errors
    /// The connection failed, or the helper could not wait for the process.
    pub fn wait(self) -> Result<Exit, ClientError> {
        match next(&self.stream)? {
            Reply::Exited { code, signal } => Ok(Exit { code, signal }),
            other => Err(unexpected(other)),
        }
    }
}

/// The next reply on `stream`; the end of the stream is an error.
fn next(stream: &UnixStream) -> Result<Reply, ClientError> {
    let (reply, _) = proto::recv::<Reply>(stream.as_fd(), 0)?
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    Ok(reply)
}

fn unexpected(reply: Reply) -> ClientError {
    match reply {
        Reply::Refused { reason } => ClientError::Refused(reason),
        other => ClientError::Unexpected(other),
    }
}
