//! The frontend's listening sockets, bound here, and the latch that closes one.

use std::cell::Cell;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures::Stream;
use kbf_tls::{Incoming, TlsSource};
use tonic::transport::server::TcpIncoming;

use crate::serve::ServeError;

/// A bound TCP listener, not yet serving.
pub(crate) struct Socket {
    /// Where it listens.
    pub local: SocketAddr,
    listener: std::net::TcpListener,
}

impl Socket {
    /// A listener on `addr`, bound through `binder`.
    ///
    /// # Errors
    /// The address cannot be bound.
    pub fn bind(addr: SocketAddr, binder: &Binder) -> Result<Self, ServeError> {
        let bound = binder.bind(addr).and_then(|listener| {
            listener.set_nonblocking(true)?;
            let local = listener.local_addr()?;
            Ok(Self { local, listener })
        });
        bound.map_err(bind_error(addr))
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

    /// The listener for the operator API twice over: two handles on the one socket, one for
    /// the server that is sent GOAWAY and one for the server that goes on accepting when it
    /// is ([`crate::serve::serve_api_goaway`]). Dropping a handle leaves the socket open
    /// while the other is.
    ///
    /// # Errors
    /// The socket cannot be duplicated, or the runtime cannot register it.
    pub fn http_pair(
        self,
    ) -> Result<(tokio::net::TcpListener, tokio::net::TcpListener), ServeError> {
        let registered = self.listener.try_clone().and_then(|twin| {
            let first = tokio::net::TcpListener::from_std(self.listener)?;
            Ok((first, tokio::net::TcpListener::from_std(twin)?))
        });
        registered.map_err(bind_error(self.local))
    }
}

/// How long binds that fail with `AddrNotAvailable` are retried, all of them together,
/// before the start is fatal.
///
/// A listen address on a VPN or overlay interface can appear seconds after
/// `network-online.target`, which only waits for the ordinary links. The bound is below
/// the `TimeoutStartSec=120` of the rendered frontend and storage units (systemd would
/// kill a start that waits longer, with a worse message), and a later start-up step still
/// has its share. It is one budget for the whole start ([`Binder`]), not one per bind.
pub(crate) const BIND_BUDGET: Duration = Duration::from_secs(90);

/// The first wait between two bind attempts; each wait doubles, up to [`BIND_WAIT_MAX`].
const BIND_WAIT_FIRST: Duration = Duration::from_millis(250);
const BIND_WAIT_MAX: Duration = Duration::from_secs(2);

/// Binds the listeners of one process start, and holds the one retry budget they share.
///
/// A bind whose address is not on this host yet (`AddrNotAvailable`) is retried, every
/// other error is at once the result. The waits of every bind made through the same
/// `Binder` are taken from the same [`BIND_BUDGET`], so several listeners on a late
/// interface wait for it together, not one after the other (three sequential waits of
/// the whole budget would outlast the unit's start timeout). Each mode makes one
/// `Binder` when it starts and passes it to every bind. Blocks the thread while it
/// waits: it runs at start-up, before serving.
pub(crate) struct Binder {
    left: Cell<Duration>,
    attempt: Box<dyn Fn(SocketAddr) -> io::Result<std::net::TcpListener>>,
    sleep: Box<dyn Fn(Duration)>,
}

impl Binder {
    /// A binder with the production budget, binding for real.
    pub fn new() -> Self {
        Self::within(BIND_BUDGET)
    }

    /// A binder that really binds, with `budget` instead of [`BIND_BUDGET`].
    fn within(budget: Duration) -> Self {
        Self {
            left: Cell::new(budget),
            attempt: Box::new(std::net::TcpListener::bind),
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// A binder over the given bind attempt and sleep, for the tests of the retry and
    /// of every place that binds.
    #[cfg(test)]
    fn scripted(
        budget: Duration,
        attempt: Box<dyn Fn(SocketAddr) -> io::Result<std::net::TcpListener>>,
        sleep: Box<dyn Fn(Duration)>,
    ) -> Self {
        Self {
            left: Cell::new(budget),
            attempt,
            sleep,
        }
    }

    /// A std listener on `addr`, retried for as much of the budget as is left.
    pub fn bind(&self, addr: SocketAddr) -> io::Result<std::net::TcpListener> {
        let before = self.left.get();
        let mut left = before;
        let mut retries = 0_u32;
        let mut sleep = |wait: Duration| {
            retries += 1;
            if retries == 1 {
                tracing::warn!(%addr, ?wait, "listen address not available yet, retrying the bind");
            } else {
                tracing::debug!(%addr, ?wait, "listen address still not available");
            }
            (self.sleep)(wait);
        };
        let got = retry_unavailable(&mut left, &mut sleep, &mut || (self.attempt)(addr));
        self.left.set(left);
        if let Err(e) = &got
            && e.kind() == io::ErrorKind::AddrNotAvailable
        {
            tracing::warn!(%addr, waited = ?(before - left), "listen address never became available, giving up");
        }
        got
    }
}

/// Runs `attempt` until it succeeds, fails with anything but `AddrNotAvailable`, or
/// `left` is spent; the last error is then the result. `sleep` is called with each wait
/// (250 ms, doubling to 2 s, the last one cut to what is left) and `left` is reduced by
/// it. Not generic: a generic function's copies count once each in the coverage ratchet.
pub(crate) fn retry_unavailable(
    left: &mut Duration,
    sleep: &mut dyn FnMut(Duration),
    attempt: &mut dyn FnMut() -> io::Result<std::net::TcpListener>,
) -> io::Result<std::net::TcpListener> {
    let mut wait = BIND_WAIT_FIRST;
    loop {
        match attempt() {
            Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable && !left.is_zero() => {
                let wait_now = wait.min(*left);
                sleep(wait_now);
                *left -= wait_now;
                wait = (wait * 2).min(BIND_WAIT_MAX);
            }
            other => return other,
        }
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
/// stops accepting only when it next looks at the listener; a process that stops
/// listening (a retire stops the worker listener) must not go on holding the socket
/// open, where connections would queue for nobody.
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
