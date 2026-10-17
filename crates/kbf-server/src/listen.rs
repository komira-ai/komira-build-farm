//! The frontend's listening sockets: bound here, or inherited from the process this one
//! takes over from ([`crate::handover`]).
//!
//! A socket that a handover may pass on keeps a second handle on its file description (a
//! `dup`), the one that goes over the control socket. That handle is dropped when the
//! listener stops accepting, so the successor's is the only one left.

use std::io;
use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use futures::Stream;
use kbf_tls::{Incoming, TlsSource};
use tonic::transport::server::TcpIncoming;

use crate::serve::ServeError;

/// A bound TCP listener, not yet serving.
pub(crate) struct Socket {
    /// Where it listens.
    pub local: SocketAddr,
    /// A second handle on the same socket, for a successor process; `None` where no
    /// handover is configured.
    pub spare: Option<OwnedFd>,
    listener: std::net::TcpListener,
}

impl Socket {
    /// A listener on `addr`, with a spare handle when `spare`.
    ///
    /// # Errors
    /// The address cannot be bound.
    pub fn bind(addr: SocketAddr, spare: bool) -> Result<Self, ServeError> {
        let bound =
            std::net::TcpListener::bind(addr).and_then(|listener| Self::of(listener, spare));
        bound.map_err(bind_error(addr))
    }

    /// The listener `fd` is, which must be listening on `want`: the flags of this process
    /// decide where it listens, and a handover that passes a socket elsewhere is refused
    /// (so the checks on the flags, such as plain text off loopback, still describe what
    /// is served).
    ///
    /// # Errors
    /// `fd` is not a listening TCP socket, or is on another address than `want`.
    pub fn adopt(fd: OwnedFd, want: SocketAddr, spare: bool) -> Result<Self, ServeError> {
        let listener = std::net::TcpListener::from(fd);
        let adopted = listener.local_addr().and_then(|local| {
            let matches = local == want;
            matches.then_some(()).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("the inherited listener is on {local}, not the address of the flags"),
                )
            })?;
            Self::of(listener, spare)
        });
        adopted.map_err(bind_error(want))
    }

    fn of(listener: std::net::TcpListener, spare: bool) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let copy = spare.then(|| listener.try_clone());
        let spare = copy.transpose()?.map(OwnedFd::from);
        Ok(Self {
            local,
            spare,
            listener,
        })
    }

    /// The listener for a gRPC server, and the latch that closes it: connections it
    /// accepts have Nagle's algorithm off (see [`crate::serve::bind`]), and are served
    /// over TLS when `tls` is given ([`Incoming`]: it must be called inside a Tokio
    /// runtime).
    ///
    /// # Errors
    /// The runtime cannot register the socket.
    pub fn grpc(self, tls: Option<TlsSource>) -> Result<(Stoppable, Latch), ServeError> {
        let registered = tokio::net::TcpListener::from_std(self.listener);
        let listener = registered.map_err(bind_error(self.local))?;
        let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));
        let open = Arc::new(Mutex::new(Open {
            incoming: Some(Incoming::new(incoming, tls)),
            waker: None,
        }));
        Ok((Stoppable(Arc::clone(&open)), Latch(open)))
    }

    /// The listener for the operator API.
    ///
    /// # Errors
    /// The runtime cannot register the socket.
    pub fn http(self) -> Result<tokio::net::TcpListener, ServeError> {
        let registered = tokio::net::TcpListener::from_std(self.listener);
        registered.map_err(bind_error(self.local))
    }
}

/// The error of a socket on `addr` that could not be made or registered.
fn bind_error(addr: SocketAddr) -> impl FnOnce(io::Error) -> ServeError {
    move |source| ServeError::Bind { addr, source }
}

/// What a gRPC listener holds: the socket, until it is closed, and the waker of the task
/// waiting to accept on it.
struct Open {
    incoming: Option<Incoming>,
    waker: Option<Waker>,
}

/// A gRPC listener that can be closed from outside: [`Latch::close`] ends the stream of
/// connections and closes the socket at once, wherever the server is in accepting. The
/// transport's own shutdown keeps the socket open until the last connection is gone, and
/// stops accepting only when it next looks at the listener; a process that handed its
/// listener to a successor must not go on holding it (a successor that crashes and is
/// started again binds the address).
pub(crate) struct Stoppable(Arc<Mutex<Open>>);

impl Stream for Stoppable {
    type Item = <Incoming as Stream>::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut open = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        open.waker = Some(cx.waker().clone());
        match open.incoming.as_mut() {
            Some(incoming) => Pin::new(incoming).poll_next(cx),
            None => Poll::Ready(None),
        }
    }
}

impl Drop for Stoppable {
    fn drop(&mut self) {
        Latch(Arc::clone(&self.0)).close();
    }
}

/// Closes a [`Stoppable`].
#[derive(Clone)]
pub(crate) struct Latch(Arc<Mutex<Open>>);

impl Latch {
    /// Closes the socket, and wakes the task waiting on it so that it sees the stream end.
    pub fn close(&self) {
        let mut open = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        open.incoming = None;
        if let Some(waker) = open.waker.take() {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests;
