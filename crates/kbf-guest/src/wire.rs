//! The bytes on the stream between the host and `kbf-guest`.
//!
//! Every message is one frame: a 4-byte big-endian length, then that many bytes. The
//! first of them is the message kind, the rest its body. A length of 0 or above
//! [`MAX_FRAME`] is refused before anything is read into memory. Inside a body:
//!
//! - integers are fixed width and big-endian (`u8`, `u16`, `u32`, `i32`, `u64`);
//! - a string is a `u32` byte count, then that many bytes of UTF-8;
//! - a list is a `u32` item count, then the items; a count larger than the bytes left
//!   could hold is refused before any item is read;
//! - a body must be used up exactly: trailing bytes are an error.
//!
//! The host sends [`HostMsg`] kinds (`0x01`-`0x7f`), the guest [`GuestMsg`] kinds
//! (`0x81`-`0xff`). The layouts of `Hello` and `Refused` never change between
//! versions, so a guest can always read a newer host's `Hello` and refuse it with a
//! reason the host can read. Every other layout belongs to [`VERSION`]; changing one
//! means a new version. The golden bytes in `wire_tests.rs` pin each layout.

use std::io::{self, Read, Write};

/// The protocol version this build speaks. The guest refuses a `Hello` with any other.
pub const VERSION: u16 = 1;

/// The largest frame either side sends or accepts: kind plus body, in bytes.
pub const MAX_FRAME: u32 = 1 << 20;

/// The length of the per-boot token, in bytes.
pub const TOKEN_LEN: usize = 32;

const HELLO: u8 = 0x01;
const RUN: u8 = 0x02;
const KILL: u8 = 0x03;
const READY: u8 = 0x81;
const REFUSED: u8 = 0x82;
const STARTED: u8 = 0x83;
const EXITED: u8 = 0x84;

/// A message from the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostMsg {
    /// The first message on a connection: the version the host speaks and the token
    /// written for this boot.
    Hello {
        version: u16,
        token: [u8; TOKEN_LEN],
    },
    /// Run one command. The guest accepts one per boot.
    Run(RunRequest),
    /// End the running command's process group. Ignored when nothing runs.
    Kill,
}

/// A message from the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestMsg {
    /// The token matched. `session` is the guest's launchd session type (`Aqua` for a
    /// logged-in GUI session), or empty where there is none to ask (not macOS).
    Ready { version: u16, session: String },
    /// A request was refused; the connection stays open unless the reason says
    /// otherwise (see [`Refusal`]).
    Refused { reason: Refusal, detail: String },
    /// The command started; `pid` leads its process group.
    Started { pid: u32 },
    /// The command and every process left in its group are gone.
    Exited(Exit),
}

/// What a command runs as, where, and for how long.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunRequest {
    /// The program and its arguments; the program is found on `PATH` from `env`.
    pub argv: Vec<String>,
    /// The whole environment: nothing is inherited from the guest agent.
    pub env: Vec<(String, String)>,
    /// The working directory, relative to the inputs share (empty for its root).
    pub cwd: String,
    /// Milliseconds from start until the group is killed; 0 for no limit.
    pub timeout_ms: u64,
    /// Paths relative to the outputs share that the guest reports on after the exit.
    pub outputs: Vec<String>,
}

/// Why a request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Refusal {
    /// `Hello` carried another version. The guest closes the connection.
    Version = 1,
    /// `Hello` carried the wrong token. The guest closes the connection.
    Token = 2,
    /// A command already ran (or started) on this boot.
    AlreadyRan = 3,
    /// The request is malformed or names a path outside its share.
    BadRequest = 4,
    /// The first message was not `Hello`. The guest closes the connection.
    NoHello = 5,
    /// The command could not be started.
    StartFailed = 6,
}

/// How a run ended, with what it used and what it left in the outputs share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    pub end: End,
    pub usage: Usage,
    /// Every requested output, in request order.
    pub outputs: Vec<OutputEntry>,
    /// Processes of the group were still there after the kill gave up waiting.
    pub stragglers: bool,
}

/// How the group's leader ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// It exited with this status.
    Exited(i32),
    /// A signal it did not ask for ended it.
    Signaled(i32),
    /// The request's timeout passed and the group was killed.
    TimedOut,
    /// The host sent `Kill`, or the connection dropped.
    Killed,
}

/// The leader's resource usage, as `wait4` returns it, plus wall time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub user_micros: u64,
    pub system_micros: u64,
    pub peak_rss_bytes: u64,
    pub wall_micros: u64,
}

/// One requested output as the guest found it, without following a symlink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputEntry {
    pub path: String,
    pub kind: OutputKind,
    /// The byte size of a regular file; 0 for anything else.
    pub size: u64,
}

/// What a requested output path holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OutputKind {
    /// Nothing, or a component on the way is not a directory.
    Missing = 0,
    File = 1,
    Directory = 2,
    /// A symlink, recorded and never followed.
    Symlink = 3,
    /// A socket, fifo or device.
    Other = 4,
}

/// A frame that could not be read or decoded.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("the stream ended")]
    Closed,
    #[error("frame length {0} is outside 1..={MAX_FRAME}")]
    Length(u32),
    #[error("the body ended early")]
    Truncated,
    #[error("{0} bytes left after the body")]
    Trailing(usize),
    #[error("unknown message kind {0:#04x}")]
    Kind(u8),
    #[error("unknown {field} tag {tag}")]
    Tag { field: &'static str, tag: u8 },
    #[error("a string is not UTF-8")]
    Utf8,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Reads one frame and returns its bytes (kind and body).
///
/// # Errors
/// [`WireError::Closed`] at a clean end of stream before a frame starts; a bad length;
/// an I/O error, including an end of stream inside a frame.
pub fn read_frame(r: &mut impl Read) -> Result<Vec<u8>, WireError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Err(WireError::Closed),
            Ok(0) => return Err(WireError::Truncated),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_be_bytes(len);
    if len == 0 || len > MAX_FRAME {
        return Err(WireError::Length(len));
    }
    let mut frame = vec![0u8; len as usize];
    r.read_exact(&mut frame).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => WireError::Truncated,
        _ => WireError::Io(e),
    })?;
    Ok(frame)
}

/// Writes `frame` (kind and body) with its length in front.
///
/// # Errors
/// The frame is empty or larger than [`MAX_FRAME`], or the write failed.
pub fn write_frame(w: &mut impl Write, frame: &[u8]) -> Result<(), WireError> {
    let len = u32::try_from(frame.len())
        .ok()
        .filter(|&n| n > 0 && n <= MAX_FRAME)
        .ok_or(WireError::Length(
            u32::try_from(frame.len()).unwrap_or(u32::MAX),
        ))?;
    let mut out = Vec::with_capacity(frame.len() + 4);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(frame);
    w.write_all(&out)?;
    w.flush()?;
    Ok(())
}

impl HostMsg {
    /// The frame bytes (kind and body) of this message.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        match self {
            HostMsg::Hello { version, token } => {
                e.u8(HELLO).u16(*version).raw(token);
            }
            HostMsg::Run(r) => {
                e.u8(RUN).u32(len32(r.argv.len()));
                for a in &r.argv {
                    e.str(a);
                }
                e.u32(len32(r.env.len()));
                for (k, v) in &r.env {
                    e.str(k).str(v);
                }
                e.str(&r.cwd).u64(r.timeout_ms).u32(len32(r.outputs.len()));
                for o in &r.outputs {
                    e.str(o);
                }
            }
            HostMsg::Kill => {
                e.u8(KILL);
            }
        }
        e.0
    }

    /// Decodes one frame's bytes.
    ///
    /// # Errors
    /// A guest kind, an unknown kind, or a malformed body.
    pub fn decode(frame: &[u8]) -> Result<Self, WireError> {
        let mut d = Dec(frame);
        let msg = match d.u8()? {
            HELLO => {
                let version = d.u16()?;
                let mut token = [0u8; TOKEN_LEN];
                token.copy_from_slice(d.take(TOKEN_LEN)?);
                HostMsg::Hello { version, token }
            }
            RUN => {
                let argv = d.list(4, Dec::str)?;
                let env = d.list(8, |d| Ok((d.str()?, d.str()?)))?;
                let cwd = d.str()?;
                let timeout_ms = d.u64()?;
                let outputs = d.list(4, Dec::str)?;
                HostMsg::Run(RunRequest {
                    argv,
                    env,
                    cwd,
                    timeout_ms,
                    outputs,
                })
            }
            KILL => HostMsg::Kill,
            kind => return Err(WireError::Kind(kind)),
        };
        d.end()?;
        Ok(msg)
    }
}

impl GuestMsg {
    /// The frame bytes (kind and body) of this message.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        match self {
            GuestMsg::Ready { version, session } => {
                e.u8(READY).u16(*version).str(session);
            }
            GuestMsg::Refused { reason, detail } => {
                e.u8(REFUSED).u8(*reason as u8).str(detail);
            }
            GuestMsg::Started { pid } => {
                e.u8(STARTED).u32(*pid);
            }
            GuestMsg::Exited(x) => {
                let (tag, value) = match x.end {
                    End::Exited(c) => (0, c),
                    End::Signaled(s) => (1, s),
                    End::TimedOut => (2, 0),
                    End::Killed => (3, 0),
                };
                e.u8(EXITED).u8(tag).i32(value);
                let u = &x.usage;
                e.u64(u.user_micros)
                    .u64(u.system_micros)
                    .u64(u.peak_rss_bytes)
                    .u64(u.wall_micros);
                e.u32(len32(x.outputs.len()));
                for o in &x.outputs {
                    e.str(&o.path).u8(o.kind as u8).u64(o.size);
                }
                e.u8(u8::from(x.stragglers));
            }
        }
        e.0
    }

    /// Decodes one frame's bytes.
    ///
    /// # Errors
    /// A host kind, an unknown kind or tag, or a malformed body.
    pub fn decode(frame: &[u8]) -> Result<Self, WireError> {
        let mut d = Dec(frame);
        let msg = match d.u8()? {
            READY => GuestMsg::Ready {
                version: d.u16()?,
                session: d.str()?,
            },
            REFUSED => GuestMsg::Refused {
                reason: refusal(d.u8()?)?,
                detail: d.str()?,
            },
            STARTED => GuestMsg::Started { pid: d.u32()? },
            EXITED => {
                let tag = d.u8()?;
                let value = d.i32()?;
                let end = match tag {
                    0 => End::Exited(value),
                    1 => End::Signaled(value),
                    2 => End::TimedOut,
                    3 => End::Killed,
                    tag => return Err(WireError::Tag { field: "end", tag }),
                };
                let usage = Usage {
                    user_micros: d.u64()?,
                    system_micros: d.u64()?,
                    peak_rss_bytes: d.u64()?,
                    wall_micros: d.u64()?,
                };
                let outputs = d.list(13, |d| {
                    Ok(OutputEntry {
                        path: d.str()?,
                        kind: output_kind(d.u8()?)?,
                        size: d.u64()?,
                    })
                })?;
                let stragglers = match d.u8()? {
                    0 => false,
                    1 => true,
                    tag => {
                        return Err(WireError::Tag {
                            field: "stragglers",
                            tag,
                        });
                    }
                };
                GuestMsg::Exited(Exit {
                    end,
                    usage,
                    outputs,
                    stragglers,
                })
            }
            kind => return Err(WireError::Kind(kind)),
        };
        d.end()?;
        Ok(msg)
    }
}

fn refusal(tag: u8) -> Result<Refusal, WireError> {
    Ok(match tag {
        1 => Refusal::Version,
        2 => Refusal::Token,
        3 => Refusal::AlreadyRan,
        4 => Refusal::BadRequest,
        5 => Refusal::NoHello,
        6 => Refusal::StartFailed,
        tag => {
            return Err(WireError::Tag {
                field: "refusal",
                tag,
            });
        }
    })
}

fn output_kind(tag: u8) -> Result<OutputKind, WireError> {
    Ok(match tag {
        0 => OutputKind::Missing,
        1 => OutputKind::File,
        2 => OutputKind::Directory,
        3 => OutputKind::Symlink,
        4 => OutputKind::Other,
        tag => {
            return Err(WireError::Tag {
                field: "output kind",
                tag,
            });
        }
    })
}

/// A count or length as the `u32` the wire carries. Anything a frame can hold fits:
/// [`MAX_FRAME`] is far below `u32::MAX`, and a larger message fails in
/// [`write_frame`].
fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[derive(Default)]
struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.raw(&v.to_be_bytes())
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.raw(&v.to_be_bytes())
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.raw(&v.to_be_bytes())
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.raw(&v.to_be_bytes())
    }
    fn str(&mut self, s: &str) -> &mut Self {
        self.u32(len32(s.len())).raw(s.as_bytes())
    }
    fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
}

struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.0.len() < n {
            return Err(WireError::Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn i32(&mut self) -> Result<i32, WireError> {
        Ok(i32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn str(&mut self) -> Result<String, WireError> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| WireError::Utf8)
    }
    /// A list whose items take at least `min_item` bytes each, so a count the rest of
    /// the body cannot hold is refused before anything is allocated for it.
    fn list<T>(
        &mut self,
        min_item: usize,
        mut item: impl FnMut(&mut Self) -> Result<T, WireError>,
    ) -> Result<Vec<T>, WireError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_item) > self.0.len() {
            return Err(WireError::Truncated);
        }
        (0..n).map(|_| item(self)).collect()
    }
    fn end(&self) -> Result<(), WireError> {
        match self.0.len() {
            0 => Ok(()),
            n => Err(WireError::Trailing(n)),
        }
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
