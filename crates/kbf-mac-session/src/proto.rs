//! The helper's wire format on its Unix socket.
//!
//! One connection carries one request. Every message is a frame: a 4-byte big-endian
//! length, then that many bytes of JSON. The client's request is one frame; `run`
//! attaches four descriptors to it (`SCM_RIGHTS`): stdin, stdout, stderr and the lease
//! directory, in that order. The helper answers with one frame, except that a started
//! `run` answers [`Reply::Started`] and then, when the process exits,
//! [`Reply::Exited`].

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use serde::{Deserialize, Serialize};

/// The largest frame either side accepts.
pub const MAX_FRAME: usize = 64 * 1024;
/// The number of descriptors a `run` request carries.
pub const RUN_FDS: usize = 4;

/// The receive buffer for ancillary data. On Linux, room for one more descriptor than a
/// request may carry: the kernel cuts what does not fit, says so (`MSG_CTRUNC`), and
/// the cut is refused. On macOS the kernel takes at most `MCLBYTES` (2048) bytes of
/// control data with one message, so this buffer is never cut there; that matters,
/// because a cut message keeps its full length in its header on macOS, which the
/// parser would read past.
#[cfg(target_os = "linux")]
const CONTROL_BYTES: usize = rustix::cmsg_space!(ScmRights(RUN_FDS + 1));
#[cfg(not(target_os = "linux"))]
const CONTROL_BYTES: usize = 4096;

/// What the daemon asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// Create the lease's user; an administrator only with a gate grant.
    UserCreate {
        lease: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant: Option<String>,
    },
    /// Start a process as the lease's user, with the attached descriptors.
    Run {
        lease: String,
        argv: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// End every process of the lease's uid.
    KillUid { lease: String },
    /// Delete the lease's user and sweep what it leaves.
    UserDelete { lease: String },
}

/// What the helper answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum Reply {
    /// The user exists with this uid.
    Created { uid: u32 },
    /// The process started with this pid.
    Started { pid: u32 },
    /// The process ended: its exit code, or the signal that ended it.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// No process of the uid remains.
    Killed,
    /// The user is gone and its state swept; `existed` is false when there was no
    /// user record left to delete (a repeated or resumed delete).
    Deleted { existed: bool },
    /// The request was refused or failed, and why.
    Refused { reason: String },
}

/// Sends `message` as one frame, with `fds` attached.
///
/// # Errors
/// The message is larger than [`MAX_FRAME`], or the socket write failed.
pub fn send<T: Serialize>(
    socket: BorrowedFd<'_>,
    message: &T,
    fds: &[BorrowedFd<'_>],
) -> io::Result<()> {
    send_frame(socket, &serde_json::to_vec(message)?, fds)
}

/// Receives one frame as a `T`, and the descriptors attached to it; `None` at a clean
/// end of stream before any byte. More than `max_fds` descriptors is an error (they
/// are closed).
///
/// # Errors
/// The stream ended inside a frame, the frame is too large or not a `T`, or too many
/// descriptors came.
pub fn recv<T: for<'de> Deserialize<'de>>(
    socket: BorrowedFd<'_>,
    max_fds: usize,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    recv_frame(socket, max_fds)?
        .map(|(body, fds)| Ok((serde_json::from_slice(&body)?, fds)))
        .transpose()
}

fn send_frame(socket: BorrowedFd<'_>, body: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut frame = u32::try_from(body.len())
        .map_err(io::Error::other)?
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(body);
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(RUN_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if fds.len() > RUN_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors",
        ));
    }
    if !fds.is_empty() {
        // The buffer has room for RUN_FDS descriptors, so this cannot fail; were it
        // to, the request would arrive without them and be refused.
        control.push(SendAncillaryMessage::ScmRights(fds));
    }
    let sent = rustix::net::sendmsg(
        socket,
        &[IoSlice::new(&frame)],
        &mut control,
        SendFlags::empty(),
    )?;
    write_all(socket, &frame[sent..])
}

/// Writes what a short `sendmsg` left (the descriptors went with its first byte).
fn write_all(socket: BorrowedFd<'_>, mut rest: &[u8]) -> io::Result<()> {
    while !rest.is_empty() {
        let wrote = rustix::net::send(socket, rest, SendFlags::empty())?;
        rest = &rest[wrote..];
    }
    Ok(())
}

/// One frame's bytes and descriptors; see [`recv`].
fn recv_frame(
    socket: BorrowedFd<'_>,
    max_fds: usize,
) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut fds = Vec::new();
    let mut header = [0u8; 4];
    let got = fill(socket, &mut header, &mut fds)?;
    if got == 0 {
        return Ok(None);
    }
    if got < header.len() {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    if fill(socket, &mut body, &mut fds)? < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if fds.len() > max_fds {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many descriptors",
        ));
    }
    Ok(Some((body, fds)))
}

/// Fills `buf` from the socket, collecting attached descriptors. Returns how many
/// bytes came: fewer than `buf` holds only if the stream ended.
fn fill(socket: BorrowedFd<'_>, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        let mut space = [MaybeUninit::uninit(); CONTROL_BYTES];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let message = rustix::net::recvmsg(
            socket,
            &mut [IoSliceMut::new(&mut buf[got..])],
            &mut control,
            recv_flags(),
        )?;
        for received in control.drain().filter_map(rights) {
            for fd in received {
                close_on_exec(&fd)?;
                fds.push(fd);
            }
        }
        if message.flags.contains(rustix::net::ReturnFlags::CTRUNC) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many descriptors: the kernel cut them off",
            ));
        }
        if message.bytes == 0 {
            break;
        }
        got += message.bytes;
    }
    Ok(got)
}

/// The descriptors of an `SCM_RIGHTS` message; no other kind is asked for.
fn rights(message: RecvAncillaryMessage<'_>) -> Option<impl Iterator<Item = OwnedFd> + '_> {
    match message {
        RecvAncillaryMessage::ScmRights(fds) => Some(fds),
        _ => None,
    }
}

/// Linux delivers received descriptors close-on-exec; macOS has no such flag, so
/// [`close_on_exec`] marks them at once (and the child's own pass covers the gap).
#[cfg(target_os = "linux")]
fn recv_flags() -> RecvFlags {
    RecvFlags::CMSG_CLOEXEC
}

#[cfg(not(target_os = "linux"))]
fn recv_flags() -> RecvFlags {
    RecvFlags::empty()
}

fn close_on_exec(fd: &OwnedFd) -> io::Result<()> {
    Ok(rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    use super::*;

    fn run_request() -> Request {
        Request::Run {
            lease: "1.2".to_owned(),
            argv: vec!["/usr/bin/true".to_owned()],
            env: vec![("A".to_owned(), "b".to_owned())],
        }
    }

    /// Catches: descriptors lost or reordered between the daemon and the helper, or
    /// a frame read short.
    #[test]
    fn a_frame_and_its_descriptors_arrive_together() {
        let (a, b) = UnixStream::pair().unwrap();
        let files: Vec<std::fs::File> = (0..RUN_FDS)
            .map(|_| std::fs::File::open("/dev/null").unwrap())
            .collect();
        let fds: Vec<BorrowedFd<'_>> = files.iter().map(AsFd::as_fd).collect();
        send(a.as_fd(), &run_request(), &fds).unwrap();
        send(a.as_fd(), &Reply::Killed, &[]).unwrap();
        drop(a);
        let (request, got) = recv::<Request>(b.as_fd(), RUN_FDS).unwrap().unwrap();
        assert_eq!(request, run_request());
        assert_eq!(got.len(), RUN_FDS);
        for fd in &got {
            let flags = rustix::io::fcntl_getfd(fd).unwrap();
            assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        }
        assert_eq!(
            recv::<Reply>(b.as_fd(), 0).unwrap().unwrap().0,
            Reply::Killed
        );
        assert!(recv::<Reply>(b.as_fd(), 0).unwrap().is_none());
    }

    /// Catches: a request carrying more descriptors than `run` takes accepted (the
    /// extra ones would stay open in the helper).
    #[test]
    fn too_many_descriptors_are_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        send(a.as_fd(), &Reply::Killed, &[file.as_fd(), file.as_fd()]).unwrap();
        let error = recv::<Reply>(b.as_fd(), 1).unwrap_err();
        assert!(error.to_string().contains("too many"), "{error}");

        let five = [file.as_fd(); RUN_FDS + 1];
        let error = send(a.as_fd(), &Reply::Killed, &five).unwrap_err();
        assert!(error.to_string().contains("too many"), "{error}");

        // Many more than a request carries, in one message: all received, then refused.
        let (a, b) = UnixStream::pair().unwrap();
        let many = [file.as_fd(); 8 * RUN_FDS];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8 * RUN_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&many)));
        let frame = [0, 0, 0, 2, b'{', b'}'];
        rustix::net::sendmsg(
            &a,
            &[IoSlice::new(&frame)],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        let error = recv::<Reply>(b.as_fd(), RUN_FDS).unwrap_err();
        assert!(error.to_string().contains("too many"), "{error}");
    }

    /// The kernel cuts ancillary data that does not fit the receive buffer, and says
    /// so; the cut is refused, not taken for the whole. (Linux: macOS's buffer is
    /// larger than any control data its kernel passes.)
    #[cfg(target_os = "linux")]
    #[test]
    fn descriptors_beyond_the_buffer_are_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let many = [file.as_fd(); 4 * RUN_FDS];
        let body = b"{}";
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(body);
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4 * RUN_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&many)));
        rustix::net::sendmsg(
            &a,
            &[IoSlice::new(&frame)],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        let error = recv::<Reply>(b.as_fd(), 4 * RUN_FDS).unwrap_err();
        assert!(error.to_string().contains("cut them off"), "{error}");
    }

    /// Catches: an unbounded frame length (the helper would allocate what any caller
    /// asks), a truncated frame taken whole, or unknown fields accepted.
    #[test]
    fn malformed_frames_are_refused() {
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&(MAX_FRAME as u32 + 1).to_be_bytes()).unwrap();
        assert!(
            recv::<Request>(b.as_fd(), 0)
                .unwrap_err()
                .to_string()
                .contains("too large")
        );

        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[0, 0, 0, 9, b'{']).unwrap();
        drop(a);
        assert_eq!(
            recv::<Request>(b.as_fd(), 0).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[0, 0]).unwrap();
        drop(a);
        assert_eq!(
            recv::<Request>(b.as_fd(), 0).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let (mut a, b) = UnixStream::pair().unwrap();
        let body = br#"{"verb":"kill-uid","lease":"1.1","uid":0}"#;
        a.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
        a.write_all(body).unwrap();
        assert!(recv::<Request>(b.as_fd(), 0).is_err());

        let (a, _b) = UnixStream::pair().unwrap();
        let huge = Reply::Refused {
            reason: "x".repeat(MAX_FRAME),
        };
        assert!(
            send(a.as_fd(), &huge, &[])
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
    }

    /// What a short `sendmsg` leaves is written after it, whole. (A blocking Unix
    /// stream socket rarely sends short, so the remainder path is driven directly.)
    #[test]
    fn the_rest_of_a_short_send_is_written() {
        let (a, mut b) = UnixStream::pair().unwrap();
        write_all(a.as_fd(), b"rest of the frame").unwrap();
        drop(a);
        let mut got = String::new();
        std::io::Read::read_to_string(&mut b, &mut got).unwrap();
        assert_eq!(got, "rest of the frame");
        let big = Reply::Refused {
            reason: "y".repeat(MAX_FRAME - 64),
        };
        let (a, b) = UnixStream::pair().unwrap();
        let reader = std::thread::spawn(move || recv::<Reply>(b.as_fd(), 0).unwrap().unwrap().0);
        send(a.as_fd(), &big, &[]).unwrap();
        assert_eq!(reader.join().unwrap(), big);
    }

    /// Only descriptors are taken from ancillary data (credentials, which the socket
    /// never asks for, are not descriptors).
    #[cfg(target_os = "linux")]
    #[test]
    fn credentials_are_not_descriptors() {
        let credentials = rustix::net::UCred {
            pid: rustix::process::getpid(),
            uid: rustix::process::getuid(),
            gid: rustix::process::getgid(),
        };
        assert!(rights(RecvAncillaryMessage::ScmCredentials(credentials)).is_none());
    }

    #[test]
    fn the_wire_names_are_kebab_case() {
        let text = serde_json::to_string(&Request::UserCreate {
            lease: "1.1".to_owned(),
            grant: None,
        })
        .unwrap();
        assert_eq!(text, r#"{"verb":"user-create","lease":"1.1"}"#);
        let text = serde_json::to_string(&Reply::Deleted { existed: true }).unwrap();
        assert_eq!(text, r#"{"deleted":{"existed":true}}"#);
    }
}
