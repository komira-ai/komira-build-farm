//! The guest side of a connection: the handshake, then at most one command per boot.
//!
//! A connection starts with `Hello`. A `Hello` that is late (after [`Config::hello_timeout`]),
//! of another version or with the wrong token is refused and the connection closed;
//! the agent then serves the next connection. Connections are served one at a time.
//!
//! After `Ready`, the host may send one `Run`. The agent checks it, creates the
//! command's stdout and stderr in the outputs share, starts it, sends `Started`, and
//! sends `Exited` once the command's whole process group is gone. `Kill` and the
//! request's timeout end the group early. A connection that drops while the command
//! runs kills the group, and nothing is reported. Every later `Run`, on this
//! connection or another, is refused with [`Refusal::AlreadyRan`]: one boot runs one
//! command. If the agent itself restarts, the stdout file it left in the outputs share
//! makes the next start fail, so a restart cannot run a second command either.

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use subtle::ConstantTimeEq as _;

use crate::run::{self, DRAIN, POLL, Running};
use crate::wire::{
    End, Exit, GuestMsg, HostMsg, Refusal, RunRequest, TOKEN_LEN, VERSION, WireError, read_frame,
    write_frame,
};

/// How long a new connection may take to send `Hello`, unless [`Config`] says
/// otherwise.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// A byte stream the agent can serve: a Unix socket in tests and on the host side, a
/// virtio socket in the guest (planned).
pub trait Stream: Read + Write + Send + Sized + 'static {
    /// A second handle on the same stream, for a reader thread.
    ///
    /// # Errors
    /// The handle could not be duplicated.
    fn try_clone(&self) -> io::Result<Self>;
    /// Bounds each read; `None` for no bound.
    ///
    /// # Errors
    /// The stream does not take the timeout.
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    /// Ends both directions, so a reader blocked on another handle returns.
    ///
    /// # Errors
    /// The stream could not be shut down.
    fn shutdown(&self) -> io::Result<()>;
}

impl Stream for UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        UnixStream::try_clone(self)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        UnixStream::set_read_timeout(self, timeout)
    }
    fn shutdown(&self) -> io::Result<()> {
        UnixStream::shutdown(self, std::net::Shutdown::Both)
    }
}

/// What the agent was started with.
#[derive(Clone, Debug)]
pub struct Config {
    /// The token written for this boot; a host must present it.
    pub token: [u8; TOKEN_LEN],
    /// The inputs share; the command's working directory is inside it.
    pub inputs: PathBuf,
    /// The outputs share; the command's stdout and stderr are written there.
    pub outputs: PathBuf,
    /// What `Ready` reports as the session ([`crate::session::manager_name`]).
    pub session: String,
    /// How long a new connection may take to send `Hello`.
    pub hello_timeout: Duration,
}

/// The agent for one boot.
#[derive(Debug)]
pub struct Agent {
    config: Config,
    ran: bool,
}

/// Whether the connection can still be used after a run.
enum Flow {
    Open,
    Gone,
}

impl Agent {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self { config, ran: false }
    }

    /// Whether this boot's command has started.
    #[must_use]
    pub fn has_run(&self) -> bool {
        self.ran
    }

    /// Serves connections on `listener`, one at a time, until accepting fails.
    ///
    /// # Errors
    /// `accept` failed.
    pub fn serve(&mut self, listener: &UnixListener) -> io::Result<()> {
        loop {
            let (stream, _) = listener.accept()?;
            if let Err(e) = self.serve_connection(stream) {
                eprintln!("kbf-guest: connection ended: {e}");
            }
        }
    }

    /// Serves one connection until the host closes it, sends something unreadable,
    /// or fails the handshake.
    ///
    /// # Errors
    /// The stream could not be set up, or the command could not be reaped.
    pub fn serve_connection<S: Stream>(&mut self, mut stream: S) -> io::Result<()> {
        stream.set_read_timeout(Some(self.config.hello_timeout))?;
        let refusal = match read_frame(&mut stream).and_then(|f| HostMsg::decode(&f)) {
            Ok(HostMsg::Hello { version, .. }) if version != VERSION => Some((
                Refusal::Version,
                format!("this guest speaks version {VERSION}, the host sent {version}"),
            )),
            Ok(HostMsg::Hello { token, .. }) => (!bool::from(token.ct_eq(&self.config.token)))
                .then(|| (Refusal::Token, "wrong token".to_owned())),
            Ok(_) => Some((
                Refusal::NoHello,
                "the first message must be Hello".to_owned(),
            )),
            Err(e) => Some((Refusal::NoHello, e.to_string())),
        };
        if let Some((reason, detail)) = refusal {
            send(&mut stream, &GuestMsg::Refused { reason, detail });
            // Best effort: the host may already be gone.
            let _ = stream.shutdown();
            return Ok(());
        }
        let ready = GuestMsg::Ready {
            version: VERSION,
            session: self.config.session.clone(),
        };
        if !send(&mut stream, &ready) {
            return Ok(());
        }
        stream.set_read_timeout(None)?;

        let mut reader = stream.try_clone()?;
        let (tx, rx) = mpsc::channel();
        let reading = std::thread::spawn(move || {
            loop {
                let msg = read_frame(&mut reader).and_then(|f| HostMsg::decode(&f));
                let last = msg.is_err();
                if tx.send(msg).is_err() || last {
                    return;
                }
            }
        });
        let result = self.serve_messages(&mut stream, &rx);
        let _ = stream.shutdown();
        // The reader returns once the stream is shut down; a panic in it is a bug
        // that has already been printed.
        let _ = reading.join();
        result
    }

    fn serve_messages<S: Stream>(
        &mut self,
        stream: &mut S,
        rx: &Receiver<Result<HostMsg, WireError>>,
    ) -> io::Result<()> {
        loop {
            let msg = match rx.recv() {
                Ok(Ok(msg)) => msg,
                Ok(Err(WireError::Closed)) | Err(_) => return Ok(()),
                Ok(Err(e)) => {
                    let detail = e.to_string();
                    send(stream, &refused(Refusal::BadRequest, detail));
                    return Ok(());
                }
            };
            let ok = match msg {
                HostMsg::Run(req) => match self.run(stream, rx, &req)? {
                    Flow::Open => true,
                    Flow::Gone => return Ok(()),
                },
                HostMsg::Kill => true,
                HostMsg::Hello { .. } => send(
                    stream,
                    &refused(Refusal::BadRequest, "Hello was already sent".into()),
                ),
            };
            if !ok {
                return Ok(());
            }
        }
    }

    fn run<S: Stream>(
        &mut self,
        stream: &mut S,
        rx: &Receiver<Result<HostMsg, WireError>>,
        req: &RunRequest,
    ) -> io::Result<Flow> {
        let refusal = if self.ran {
            Some(refused(
                Refusal::AlreadyRan,
                "this boot already ran a command".into(),
            ))
        } else {
            match run::check(req, &self.config.inputs) {
                Ok(cwd) => {
                    self.ran = true;
                    match Running::start(req, &cwd, &self.config.outputs) {
                        Ok(running) => return self.supervise(stream, rx, req, &running),
                        Err(e) => Some(refused(Refusal::StartFailed, e.to_string())),
                    }
                }
                Err(e) => Some(refused(Refusal::BadRequest, e.to_string())),
            }
        };
        let open = refusal.is_none_or(|msg| send(stream, &msg));
        Ok(if open { Flow::Open } else { Flow::Gone })
    }

    /// Waits for the leader, ending the group on `Kill`, the timeout or a dropped
    /// connection, then drains the group and reports the exit.
    fn supervise<S: Stream>(
        &self,
        stream: &mut S,
        rx: &Receiver<Result<HostMsg, WireError>>,
        req: &RunRequest,
        running: &Running,
    ) -> io::Result<Flow> {
        let mut gone = !send(stream, &GuestMsg::Started { pid: running.pid() });
        let deadline =
            (req.timeout_ms > 0).then(|| running.started() + Duration::from_millis(req.timeout_ms));
        let mut ended_by = gone.then_some(End::Killed);
        let reaped = loop {
            if ended_by.is_some() {
                running.kill_group();
                break running.wait()?;
            }
            if let Some(reaped) = running.try_reap()? {
                break reaped;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                ended_by = Some(End::TimedOut);
                continue;
            }
            match rx.recv_timeout(POLL) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(Ok(HostMsg::Kill)) => ended_by = Some(End::Killed),
                Ok(Ok(HostMsg::Run(_))) => {
                    let msg = refused(Refusal::AlreadyRan, "a command is running".into());
                    if !send(stream, &msg) {
                        gone = true;
                        ended_by = Some(End::Killed);
                    }
                }
                Ok(Ok(HostMsg::Hello { .. })) => {
                    let msg = refused(Refusal::BadRequest, "Hello was already sent".into());
                    if !send(stream, &msg) {
                        gone = true;
                        ended_by = Some(End::Killed);
                    }
                }
                Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                    gone = true;
                    ended_by = Some(End::Killed);
                }
            }
        };
        let drained = running.drain(DRAIN);
        if gone {
            return Ok(Flow::Gone);
        }
        let end = ended_by.unwrap_or(match reaped.status {
            Ok(code) => End::Exited(code),
            Err(signal) => End::Signaled(signal),
        });
        let exit = GuestMsg::Exited(Exit {
            end,
            usage: reaped.usage,
            outputs: run::report_outputs(&self.config.outputs, &req.outputs),
            stragglers: !drained,
        });
        Ok(if send(stream, &exit) {
            Flow::Open
        } else {
            Flow::Gone
        })
    }
}

fn refused(reason: Refusal, detail: String) -> GuestMsg {
    GuestMsg::Refused { reason, detail }
}

/// Sends `msg`; `false` when the host is gone.
fn send(w: &mut impl Write, msg: &GuestMsg) -> bool {
    write_frame(w, &msg.encode()).is_ok()
}
