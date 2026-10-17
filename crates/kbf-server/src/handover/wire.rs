//! The handover's wire: one line per message on the control socket, and the listener
//! sockets sent with the offer as `SCM_RIGHTS`.
//!
//! ```text
//! successor -> holder   hello <version> <reapi> <worker> <api|-> <state path, hex>
//! holder -> successor   offer <generation> <api: 0|1>      (with the sockets)
//!                       refused <why>
//! successor -> holder   ready
//! holder -> successor   released                           (or: failed <why>)
//! ```
//!
//! The offer is one `sendmsg`: its line and its sockets arrive together in one
//! `recvmsg`, because the sockets are attached to its first byte and the holder sends
//! nothing else until the successor answers.

use std::ffi::OsString;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Interest};
use tokio::net::UnixStream;

/// The protocol version a hello names.
pub const PROTOCOL: u32 = 1;

/// The most sockets an offer carries: REAPI, worker, operator API.
pub const MAX_SOCKETS: usize = 3;

/// The room for the control messages of a receive: the sockets, and on Linux a peer's
/// credentials too, which the kernel attaches when the socket asks for them (it does not
/// here; a buffer too small for a message that arrives would lose the sockets with it).
#[cfg(target_os = "linux")]
const RECEIVE_SPACE: usize = rustix::cmsg_space!(ScmRights(MAX_SOCKETS), ScmCredentials(1));
#[cfg(not(target_os = "linux"))]
const RECEIVE_SPACE: usize = rustix::cmsg_space!(ScmRights(MAX_SOCKETS));

/// A new process's close on exec for the sockets it receives, where the system has it.
#[cfg(target_os = "linux")]
const RECEIVE: RecvFlags = RecvFlags::CMSG_CLOEXEC;
#[cfg(not(target_os = "linux"))]
const RECEIVE: RecvFlags = RecvFlags::empty();

/// One message of the handover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// The successor asks to take over: its protocol, the addresses its flags name and
    /// its state file.
    Hello {
        version: u32,
        reapi: SocketAddr,
        worker: SocketAddr,
        api: Option<SocketAddr>,
        state: PathBuf,
    },
    /// The holder gives its sockets (REAPI, worker, and the API if `api`) and the
    /// generation the successor must find the state store at.
    Offer { generation: u64, api: bool },
    /// The holder will not hand over.
    Refused(String),
    /// The successor holds the sockets and accepts what it was told.
    Ready,
    /// The holder has stopped, and given up the state store.
    Released,
    /// The holder could not stop cleanly.
    Failed(String),
}

impl Message {
    /// The message as a line, without its newline.
    pub fn line(&self) -> String {
        match self {
            Self::Hello {
                version,
                reapi,
                worker,
                api,
                state,
            } => {
                let api = api.map_or_else(|| "-".to_owned(), |api| api.to_string());
                let state = hex::encode(state.as_os_str().as_bytes());
                format!("hello {version} {reapi} {worker} {api} {state}")
            }
            Self::Offer { generation, api } => format!("offer {generation} {}", u8::from(*api)),
            Self::Refused(why) => format!("refused {}", one_line(why)),
            Self::Ready => "ready".to_owned(),
            Self::Released => "released".to_owned(),
            Self::Failed(why) => format!("failed {}", one_line(why)),
        }
    }

    /// The message a line is, if it is one.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        match verb {
            "hello" => hello(rest),
            "offer" => offer(rest),
            "refused" => Some(Self::Refused(rest.to_owned())),
            "ready" => Some(Self::Ready),
            "released" => Some(Self::Released),
            "failed" => Some(Self::Failed(rest.to_owned())),
            _ => None,
        }
    }
}

/// `text` with its line breaks made spaces, so it stays one line.
fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

fn hello(rest: &str) -> Option<Message> {
    let mut fields = rest.split(' ');
    let version = fields.next()?.parse().ok()?;
    let reapi = fields.next()?.parse().ok()?;
    let worker = fields.next()?.parse().ok()?;
    let api = match fields.next()? {
        "-" => None,
        api => Some(api.parse().ok()?),
    };
    let state = PathBuf::from(OsString::from_vec(hex::decode(fields.next()?).ok()?));
    Some(Message::Hello {
        version,
        reapi,
        worker,
        api,
        state,
    })
}

fn offer(rest: &str) -> Option<Message> {
    let (generation, api) = rest.split_once(' ')?;
    Some(Message::Offer {
        generation: generation.parse().ok()?,
        api: api == "1",
    })
}

/// Why the control connection failed.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The socket failed.
    #[error("control socket: {0}")]
    Io(#[from] io::Error),
    /// The other end closed the connection.
    #[error("the other process closed the control connection")]
    Closed,
    /// The other end sent something that is not a message.
    #[error("not a handover message: {0:?}")]
    Malformed(String),
}

/// One end of a control connection.
pub struct Link {
    stream: BufReader<UnixStream>,
}

impl Link {
    /// The connection `stream`.
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream: BufReader::new(stream),
        }
    }

    /// Sends `message`.
    ///
    /// # Errors
    /// The socket fails.
    pub async fn say(&mut self, message: &Message) -> io::Result<()> {
        let line = format!("{}\n", message.line());
        self.stream.get_mut().write_all(line.as_bytes()).await
    }

    /// The next message.
    ///
    /// # Errors
    /// The socket fails, the other end has closed it, or what it sent is not a message.
    pub async fn hear(&mut self) -> Result<Message, WireError> {
        let mut line = String::new();
        let read = self.stream.read_line(&mut line).await?;
        if read == 0 {
            return Err(WireError::Closed);
        }
        Message::parse(&line).ok_or_else(|| WireError::Malformed(line.trim_end().to_owned()))
    }

    /// Sends `message` with `sockets` attached to it.
    ///
    /// # Errors
    /// The socket fails.
    pub async fn say_with(
        &mut self,
        message: &Message,
        sockets: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        let line = format!("{}\n", message.line());
        let stream = self.stream.get_ref();
        let sending = stream.async_io(Interest::WRITABLE, || send_once(stream, &line, sockets));
        let sent = sending.await?;
        // The sockets went with the first byte; a short send of a line this small is
        // rare, and the rest follows as plain bytes.
        let rest = &line.as_bytes()[sent..];
        self.stream.get_mut().write_all(rest).await
    }

    /// The next message, and the sockets that came with it.
    ///
    /// # Errors
    /// The socket fails, the other end has closed it, or what it sent is not a message.
    pub async fn hear_with(&mut self) -> Result<(Message, Vec<OwnedFd>), WireError> {
        let stream = self.stream.get_ref();
        let receiving = stream.async_io(Interest::READABLE, || receive_once(stream));
        let (line, sockets) = receiving.await?;
        if line.is_empty() {
            return Err(WireError::Closed);
        }
        let line = String::from_utf8_lossy(&line);
        let message = Message::parse(&line);
        let message = message.ok_or_else(|| WireError::Malformed(line.trim_end().to_owned()))?;
        Ok((message, sockets))
    }
}

/// One `sendmsg` of `line` with `sockets` attached.
fn send_once(stream: &UnixStream, line: &str, sockets: &[BorrowedFd<'_>]) -> io::Result<usize> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_SOCKETS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    control.push(SendAncillaryMessage::ScmRights(sockets));
    let lines = [IoSlice::new(line.as_bytes())];
    sendmsg(stream, &lines, &mut control, SendFlags::empty()).map_err(io::Error::from)
}

/// One `recvmsg`: the bytes that came, and the sockets with them.
fn receive_once(stream: &UnixStream) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut space = [MaybeUninit::uninit(); RECEIVE_SPACE];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut line = [0_u8; 512];
    let mut lines = [IoSliceMut::new(&mut line)];
    let got = recvmsg(stream, &mut lines, &mut control, RECEIVE);
    let got = got.map_err(io::Error::from)?;
    let sockets = control.drain().flat_map(|received| match received {
        RecvAncillaryMessage::ScmRights(sockets) => sockets.collect::<Vec<_>>(),
        _ => Vec::new(),
    });
    let sockets = sockets.collect();
    Ok((line[..got.bytes].to_vec(), sockets))
}

#[cfg(test)]
impl Link {
    /// Writes `bytes` as they are, for a test that sends what is not a message.
    pub(crate) async fn say_raw(&mut self, bytes: &[u8]) {
        self.stream.get_mut().write_all(bytes).await.expect("write");
    }
}

#[cfg(test)]
mod tests;
