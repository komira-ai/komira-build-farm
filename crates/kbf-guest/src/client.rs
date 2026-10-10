//! The host side of a connection, for the VM driver and for tests.

use crate::server::Stream;
use crate::wire::{
    Exit, GuestMsg, HostMsg, Refusal, RunRequest, TOKEN_LEN, VERSION, WireError, read_frame,
    write_frame,
};

/// What went wrong talking to the guest.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("the guest refused ({reason:?}): {detail}")]
    Refused { reason: Refusal, detail: String },
    #[error("the guest speaks version {0}, this host {VERSION}")]
    Version(u16),
    #[error("unexpected message from the guest: {0:?}")]
    Unexpected(GuestMsg),
}

/// A connection that has passed the handshake.
#[derive(Debug)]
pub struct Client<S: Stream> {
    stream: S,
    session: String,
}

/// Sends `Kill` on a client's connection from another thread.
#[derive(Debug)]
pub struct Killer<S: Stream>(S);

impl<S: Stream> Killer<S> {
    /// # Errors
    /// The guest is gone.
    pub fn kill(&mut self) -> Result<(), WireError> {
        write_frame(&mut self.0, &HostMsg::Kill.encode())
    }
}

impl<S: Stream> Client<S> {
    /// Sends `Hello` with `token` and waits for `Ready`.
    ///
    /// # Errors
    /// The guest refused, spoke another version, or the stream failed.
    pub fn connect(mut stream: S, token: [u8; TOKEN_LEN]) -> Result<Self, ClientError> {
        let hello = HostMsg::Hello {
            version: VERSION,
            token,
        };
        write_frame(&mut stream, &hello.encode())?;
        match GuestMsg::decode(&read_frame(&mut stream)?)? {
            GuestMsg::Ready { version, session } if version == VERSION => {
                Ok(Self { stream, session })
            }
            GuestMsg::Ready { version, .. } => Err(ClientError::Version(version)),
            GuestMsg::Refused { reason, detail } => Err(ClientError::Refused { reason, detail }),
            other => Err(ClientError::Unexpected(other)),
        }
    }

    /// The session type the guest reported in `Ready`.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// A handle that can send `Kill` while [`Client::wait`] blocks.
    ///
    /// # Errors
    /// The stream could not be duplicated.
    pub fn killer(&self) -> std::io::Result<Killer<S>> {
        self.stream.try_clone().map(Killer)
    }

    /// Sends `req` and waits for `Started`; returns the leader's pid.
    ///
    /// # Errors
    /// The guest refused the request, or the stream failed.
    pub fn run(&mut self, req: &RunRequest) -> Result<u32, ClientError> {
        self.send(&HostMsg::Run(req.clone()))?;
        match self.recv()? {
            GuestMsg::Started { pid } => Ok(pid),
            GuestMsg::Refused { reason, detail } => Err(ClientError::Refused { reason, detail }),
            other => Err(ClientError::Unexpected(other)),
        }
    }

    /// Waits for `Exited`.
    ///
    /// # Errors
    /// The stream failed or the guest sent something else.
    pub fn wait(&mut self) -> Result<Exit, ClientError> {
        match self.recv()? {
            GuestMsg::Exited(exit) => Ok(exit),
            other => Err(ClientError::Unexpected(other)),
        }
    }

    /// Sends one message.
    ///
    /// # Errors
    /// The stream failed.
    pub fn send(&mut self, msg: &HostMsg) -> Result<(), WireError> {
        write_frame(&mut self.stream, &msg.encode())
    }

    /// Reads one message.
    ///
    /// # Errors
    /// The stream failed or the frame did not decode.
    pub fn recv(&mut self) -> Result<GuestMsg, WireError> {
        GuestMsg::decode(&read_frame(&mut self.stream)?)
    }
}
