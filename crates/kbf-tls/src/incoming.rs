//! The accepted connections of a listener whose TLS configuration can change.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tonic::Request;
use tonic::codegen::http::Extensions;
use tonic::transport::server::{Connected, TcpConnectInfo, TcpIncoming, TlsConnectInfo};

use crate::source::{Poller, TlsSource};

/// How long a client has to finish its handshake before the connection is dropped, so a
/// client that connects and says nothing holds nothing for long.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The chain a client presented, leaf first, as tonic reports it.
type Chain = Arc<Vec<CertificateDer<'static>>>;

/// What a service learns about a connection.
#[derive(Clone, Debug)]
pub struct ConnInfo {
    /// The TCP addresses.
    pub tcp: TcpConnectInfo,
    /// The certificates the client presented and the handshake verified; `None` for a
    /// plain connection or a TLS client that presented none.
    pub certs: Option<Chain>,
}

enum Io {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

/// An accepted connection: plain, or past its TLS handshake.
pub struct Conn {
    io: Io,
    info: ConnInfo,
}

impl Conn {
    /// The peer's address, if the socket still knows it.
    #[must_use]
    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.info.tcp.remote_addr
    }
}

impl Connected for Conn {
    type ConnectInfo = ConnInfo;

    fn connect_info(&self) -> ConnInfo {
        self.info.clone()
    }
}

macro_rules! each_io {
    ($self:expr, $io:ident => $call:expr) => {
        match &mut $self.get_mut().io {
            Io::Plain($io) => $call,
            Io::Tls($io) => $call,
        }
    };
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        each_io!(self, io => Pin::new(io).poll_read(cx, buf))
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        each_io!(self, io => Pin::new(io).poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        each_io!(self, io => Pin::new(io).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        each_io!(self, io => Pin::new(io).poll_shutdown(cx))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        each_io!(self, io => Pin::new(io).poll_write_vectored(cx, bufs))
    }

    fn is_write_vectored(&self) -> bool {
        match &self.io {
            Io::Plain(io) => io.is_write_vectored(),
            Io::Tls(io) => io.is_write_vectored(),
        }
    }
}

/// The TLS half of an [`Incoming`]: the source, the handshakes running, and the task that
/// keeps the source current.
struct Tls {
    source: TlsSource,
    handshake: Duration,
    pending: JoinSet<Option<Conn>>,
    _poller: Poller,
}

/// The connections a listener accepted, as a stream a tonic server takes
/// (`serve_with_incoming`). With a [`TlsSource`], each is handshaken under the source's
/// configuration as of its accept, in a task of its own, so a client that is slow to
/// handshake delays no one; one whose handshake fails is dropped. Without, they are the
/// plain connections of `tcp`.
pub struct Incoming {
    tcp: TcpIncoming,
    tls: Option<Tls>,
}

impl Incoming {
    /// The connections of `tcp`, secured by `tls` if there is one. With one, starts the task
    /// that looks at its files every interval, which ends when this is dropped; so it must
    /// be called inside a Tokio runtime.
    #[must_use]
    pub fn new(tcp: TcpIncoming, tls: Option<TlsSource>) -> Self {
        Self {
            tcp,
            tls: tls.map(|source| Tls {
                handshake: HANDSHAKE_TIMEOUT,
                pending: JoinSet::new(),
                _poller: source.poll(),
                source,
            }),
        }
    }

    /// The same with `handshake` as the time a client has to complete its handshake.
    #[must_use]
    pub fn with_handshake_timeout(mut self, handshake: Duration) -> Self {
        if let Some(tls) = &mut self.tls {
            tls.handshake = handshake;
        }
        self
    }
}

impl Incoming {
    /// What accepting `tcp` leads to: a plain listener's connection at once, a TLS
    /// listener's handshake started in a task of its own (its connection comes from
    /// `poll_next` when it ends).
    fn admit(&mut self, tcp: TcpStream) -> Option<Conn> {
        let info = tcp.connect_info();
        let Some(tls) = &mut self.tls else {
            return Some(Conn {
                io: Io::Plain(tcp),
                info: ConnInfo {
                    tcp: info,
                    certs: None,
                },
            });
        };
        let acceptor = TlsAcceptor::from(tls.source.config());
        tls.pending
            .spawn(handshake(acceptor, tcp, info, tls.handshake));
        None
    }
}

impl Stream for Incoming {
    type Item = io::Result<Conn>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // A TCP listener's stream does not end, and an accept error is the stream's
        // item, which tonic logs and goes on from.
        while let Poll::Ready(Some(accepted)) = Pin::new(&mut this.tcp).poll_next(cx) {
            if let Some(ready) = accepted.map(|tcp| this.admit(tcp)).transpose() {
                return Poll::Ready(Some(ready));
            }
        }
        if let Some(tls) = &mut this.tls {
            // A handshake that failed ends as `None`: nothing for the server to serve.
            while let Poll::Ready(Some(done)) = tls.pending.poll_join_next(cx) {
                if let Ok(Some(conn)) = done {
                    return Poll::Ready(Some(Ok(conn)));
                }
            }
        }
        Poll::Pending
    }
}

async fn handshake(
    acceptor: TlsAcceptor,
    tcp: TcpStream,
    info: TcpConnectInfo,
    limit: Duration,
) -> Option<Conn> {
    match tokio::time::timeout(limit, acceptor.accept(tcp)).await {
        Ok(Ok(tls)) => {
            let certs = tls
                .get_ref()
                .1
                .peer_certificates()
                .map(|chain| Arc::new(chain.iter().map(|c| c.clone().into_owned()).collect()));
            Some(Conn {
                io: Io::Tls(Box::new(tls)),
                info: ConnInfo { tcp: info, certs },
            })
        }
        Ok(Err(e)) => {
            tracing::debug!(peer = ?info.remote_addr, error = %e, "TLS handshake failed");
            None
        }
        Err(_) => {
            tracing::debug!(peer = ?info.remote_addr, "TLS handshake timed out");
            None
        }
    }
}

/// The certificates the caller of `request` presented, leaf first: from a connection of an
/// [`Incoming`] or of tonic's own TLS acceptor. `None` on a plain connection or without a
/// client certificate.
#[must_use]
pub fn peer_certs<T>(request: &Request<T>) -> Option<Chain> {
    match request.extensions().get::<ConnInfo>() {
        Some(info) => info.certs.clone(),
        None => request.peer_certs(),
    }
}

/// [`peer_certs`] for a layer, which sees the HTTP request's extensions before tonic
/// has made a `Request` of it.
#[must_use]
pub fn peer_certs_of(extensions: &Extensions) -> Option<Chain> {
    match extensions.get::<ConnInfo>() {
        Some(info) => info.certs.clone(),
        None => extensions
            .get::<TlsConnectInfo<TcpConnectInfo>>()
            .and_then(TlsConnectInfo::peer_certs),
    }
}

/// The address of the caller of `request`, from either kind of connection.
#[must_use]
pub fn remote_addr<T>(request: &Request<T>) -> Option<SocketAddr> {
    match request.extensions().get::<ConnInfo>() {
        Some(info) => info.tcp.remote_addr,
        None => request.remote_addr(),
    }
}
