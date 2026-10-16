use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, DuplexStream, ReadBuf,
};
use tonic::transport::server::{Connected, TcpConnectInfo};

use super::Connections;

/// One end of an in-memory stream that says it came from `peer`.
struct Peer {
    io: DuplexStream,
    peer: Option<SocketAddr>,
}

impl Connected for Peer {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> TcpConnectInfo {
        TcpConnectInfo {
            local_addr: None,
            remote_addr: self.peer,
        }
    }
}

impl AsyncRead for Peer {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Peer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

fn addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 40_000))
}

/// A tracked stream from `peer`, and the other end of it.
fn accepted(peer: Option<SocketAddr>) -> (Peer, DuplexStream) {
    let (io, other) = tokio::io::duplex(1024);
    (Peer { io, peer }, other)
}

/// Catches a closed connection that unregisters whichever connection now has its
/// peer address (mutant: the registration removes its address without checking the
/// id): two connections from one address, the older closed first, leave the newer
/// registered and open; closing the newer unregisters the address and ends its
/// `closed` wait.
#[tokio::test]
async fn an_older_connection_from_the_same_address_leaves_the_newer_registered() {
    let connections = Connections::new();
    let older = connections.track(accepted(Some(addr())).0);
    let older_id = connections.of(Some(addr())).expect("older").id();
    let newer = connections.track(accepted(Some(addr())).0);
    let newer_connection = connections.of(Some(addr())).expect("newer");
    assert_ne!(newer_connection.id(), older_id);
    drop(older);
    assert_eq!(
        connections.of(Some(addr())).map(|c| c.id()),
        Some(newer_connection.id()),
        "the newer connection is still registered"
    );
    drop(newer);
    assert!(connections.of(Some(addr())).is_none());
    tokio::time::timeout(Duration::from_secs(10), newer_connection.closed())
        .await
        .expect("closed once its stream is dropped");
}

/// Catches a wrapper that loses bytes or a direction: bytes written through a tracked
/// stream arrive at the other end, bytes sent back are read through it, and a
/// shutdown reaches the other end as end of stream. A stream with no peer address is
/// passed through untracked.
#[tokio::test]
async fn a_tracked_stream_carries_bytes_both_ways() {
    let connections = Connections::new();
    let (stream, mut other) = accepted(None);
    let mut tracked = connections.track(stream);
    assert!(connections.of(None).is_none());
    assert!(!tracked.is_write_vectored());
    tracked.write_all(b"to the peer").await.expect("write");
    tracked.flush().await.expect("flush");
    let mut got = [0; 11];
    other.read_exact(&mut got).await.expect("read");
    assert_eq!(&got, b"to the peer");
    other.write_all(b"back").await.expect("write back");
    let mut back = [0; 4];
    tracked.read_exact(&mut back).await.expect("read back");
    assert_eq!(&back, b"back");
    tracked.shutdown().await.expect("shutdown");
    let mut rest = Vec::new();
    other.read_to_end(&mut rest).await.expect("end of stream");
    assert!(rest.is_empty());
}
