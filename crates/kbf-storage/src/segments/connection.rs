//! Which connection a call came on, and when that connection closed: an upload belongs
//! to the connection that began it and is dropped when it closes
//! (`docs/design/write-path.md`, section 4.2).
//!
//! A listener's accepted streams are wrapped in [`Tracked`] ([`Connections::track`]),
//! which registers the connection under its peer address while the stream lives. Its
//! connect info is the inner stream's own, so tonic's `remote_addr` and `peer_certs`
//! (and the worker listener's admission, which reads them) see what they saw before.
//! A call finds its connection by the address tonic reports ([`Connections::of`]); a
//! TCP peer address names one live connection at a time.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;
use tonic::transport::server::{Connected, TcpConnectInfo};

/// The live connections of one listener, by peer address.
#[derive(Clone, Debug, Default)]
pub struct Connections {
    inner: Arc<Registry>,
}

#[derive(Debug, Default)]
struct Registry {
    next: AtomicU64,
    live: Mutex<HashMap<SocketAddr, Connection>>,
}

impl Registry {
    fn live(&self) -> MutexGuard<'_, HashMap<SocketAddr, Connection>> {
        // The map is consistent after every statement, so a panic elsewhere leaves
        // nothing half-done.
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One accepted connection: an id no other connection of the process gets, and a
/// signal that it closed.
#[derive(Clone, Debug)]
pub struct Connection {
    id: u64,
    open: watch::Receiver<()>,
}

impl Connection {
    /// The connection's id.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Completes once the connection's stream has been dropped (the peer closed it, or
    /// the server did). Nothing is ever sent on the channel, so `changed` ends only
    /// when its sender, held by the stream's registration, is dropped.
    pub async fn closed(&self) {
        let _ = self.open.clone().changed().await;
    }
}

impl Connections {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `stream`, registered as a live connection until it is dropped. A stream whose
    /// peer address is unknown is passed through untracked.
    pub fn track<IO>(&self, stream: IO) -> Tracked<IO>
    where
        IO: Connected<ConnectInfo = TcpConnectInfo>,
    {
        let peer = stream.connect_info().remote_addr;
        let registration = peer.map(|peer| {
            let (open, receiver) = watch::channel(());
            let id = self.inner.next.fetch_add(1, Ordering::Relaxed) + 1;
            let connection = Connection { id, open: receiver };
            self.inner.live().insert(peer, connection);
            Registration {
                registry: Arc::clone(&self.inner),
                peer,
                id,
                _open: open,
            }
        });
        Tracked {
            stream,
            _registration: registration,
        }
    }

    /// The live connection whose peer is `peer`, if one is tracked.
    #[must_use]
    pub fn of(&self, peer: Option<SocketAddr>) -> Option<Connection> {
        self.inner.live().get(&peer?).cloned()
    }
}

/// Removes its connection from the registry, and signals it closed, when dropped.
#[derive(Debug)]
struct Registration {
    registry: Arc<Registry>,
    peer: SocketAddr,
    id: u64,
    /// Dropped with the registration: every [`Connection::closed`] completes.
    _open: watch::Sender<()>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut live = self.registry.live();
        // A newer connection from the same address replaced this one: leave it.
        if live.get(&self.peer).is_some_and(|c| c.id == self.id) {
            live.remove(&self.peer);
        }
    }
}

/// An accepted stream that keeps its connection registered while it lives.
#[derive(Debug)]
pub struct Tracked<IO> {
    stream: IO,
    _registration: Option<Registration>,
}

impl<IO: Connected> Connected for Tracked<IO> {
    type ConnectInfo = IO::ConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.stream.connect_info()
    }
}

impl<IO: AsyncRead + Unpin> AsyncRead for Tracked<IO> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl<IO: AsyncWrite + Unpin> AsyncWrite for Tracked<IO> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}

#[cfg(test)]
mod tests;
