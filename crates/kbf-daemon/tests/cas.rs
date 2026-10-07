//! The daemon's CAS client against a real front cache in this process, and against a
//! ByteStream server that misbehaves.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use futures::stream;
use kbf_daemon::cas::{
    Cas, CasClient, CasError, Chunks, WRITE_CHUNK_BYTES, digest_of, fetch, label,
};
use kbf_front::Cache;
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_proto::reapi::Digest;
use support::memory::MemoryCas;
use tonic::service::Routes;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status, Streaming};

/// Serves `routes` on a loopback port; returns a channel to it.
async fn serve(routes: Routes) -> Channel {
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    tokio::spawn(
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming),
    );
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect")
}

async fn front() -> CasClient {
    CasClient::new(serve(kbf_front::routes(Arc::new(Cache::memory()))).await)
}

/// Catches a client that loses or reorders bytes on the way in or out: an empty blob,
/// a small one, and one large enough to take several ByteStream messages each way.
#[tokio::test]
async fn blobs_round_trip_through_the_front() {
    let cas = front().await;
    let large: Vec<u8> = (0..(2 * WRITE_CHUNK_BYTES + 12_345))
        .map(|i| (i % 251) as u8)
        .collect();
    for bytes in [Vec::new(), b"small".to_vec(), large] {
        let digest = cas.put(bytes.clone()).await.expect("put");
        assert_eq!(digest, digest_of(&bytes));
        assert_eq!(fetch(&cas, &digest).await.expect("fetch"), bytes);
    }
}

/// Catches a blob the front does not hold reported as anything but missing (the lease
/// would fail as the farm's fault and be retried, when the client must upload it).
#[tokio::test]
async fn a_blob_the_front_lacks_is_missing() {
    let cas = front().await;
    let digest = digest_of(b"never uploaded");
    assert_eq!(
        cas.get(&digest).await,
        Err(CasError::Missing(label(&digest)))
    );
}

/// Catches a front that cannot be reached reported as a missing blob.
#[tokio::test]
async fn an_unreachable_front_is_unavailable() {
    // A port nothing listens on: bound, then released.
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr");
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy();
    let cas = CasClient::new(channel);
    let digest = digest_of(b"x");
    assert!(matches!(
        cas.get(&digest).await,
        Err(CasError::Unavailable(..))
    ));
    assert!(matches!(
        cas.put(b"x".to_vec()).await,
        Err(CasError::Unavailable(..))
    ));
}

/// Catches bytes that do not match their digest being trusted.
#[tokio::test]
async fn fetch_refuses_bytes_that_do_not_match() {
    let cas = MemoryCas::default();
    let digest = cas.insert(b"good".to_vec());
    cas.corrupt(&digest, b"evil".to_vec());
    let error = fetch(&cas, &digest).await.expect_err("refused");
    assert_eq!(
        error,
        CasError::Corrupt(label(&digest), label(&digest_of(b"evil")))
    );
}

/// A ByteStream server that breaks its promises: a read stream that fails after its
/// first message, and a write it answers with fewer bytes than were sent.
struct Liar;

#[tonic::async_trait]
impl ByteStream for Liar {
    type ReadStream = stream::Iter<std::vec::IntoIter<Result<ReadResponse, Status>>>;

    async fn read(&self, _: Request<ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        Ok(Response::new(stream::iter(vec![
            Ok(ReadResponse {
                data: b"part".to_vec(),
            }),
            Err(Status::data_loss("broke mid-read")),
        ])))
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let mut stream = request.into_inner();
        while stream.message().await?.is_some() {}
        Ok(Response::new(WriteResponse { committed_size: 1 }))
    }

    async fn query_write_status(
        &self,
        _: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }
}

/// Catches a read that breaks midway returned as a short blob, and a write the CAS did
/// not fully commit taken as stored (an output named in a result would be missing).
#[tokio::test]
async fn broken_calls_are_unavailable() {
    let cas = CasClient::new(serve(Routes::new(ByteStreamServer::new(Liar))).await);
    let digest = digest_of(b"whatever");
    let read = cas.get(&digest).await;
    assert!(
        matches!(&read, Err(CasError::Unavailable(blob, why)) if *blob == label(&digest) && why.contains("broke mid-read")),
        "{read:?}"
    );
    let write = cas.put(b"three".to_vec()).await;
    assert!(
        matches!(&write, Err(CasError::Unavailable(_, why)) if why.contains("committed 1 bytes of 5")),
        "{write:?}"
    );
    // The Liar answers QueryWriteStatus with an error; the client never asks it.
    let mut client = kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient::new(
        serve(Routes::new(ByteStreamServer::new(Liar))).await,
    );
    let status = client
        .query_write_status(QueryWriteStatusRequest::default())
        .await
        .expect_err("unimplemented");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
}

/// `len` patterned bytes, their digest, and the same bytes as a stream of chunks of
/// at most `chunk` bytes.
fn chunked(len: usize, chunk: usize) -> (Vec<u8>, Digest, Chunks) {
    let bytes: Vec<u8> = (0..len).map(|i| (i % 239) as u8).collect();
    let pieces: Vec<std::io::Result<Vec<u8>>> =
        bytes.chunks(chunk.max(1)).map(|c| Ok(c.to_vec())).collect();
    let digest = digest_of(&bytes);
    (bytes, digest, Box::pin(stream::iter(pieces)))
}

/// A stream that yields `first`, then fails.
fn failing(first: &[u8]) -> Chunks {
    Box::pin(stream::iter(vec![
        Ok(first.to_vec()),
        Err(std::io::Error::other("disk gone")),
    ]))
}

/// Catches a streamed upload that loses, repeats or reorders bytes across message
/// boundaries (chunks smaller than, equal to and larger than a message, and an empty
/// blob); and a stream shorter than its digest says, or one that fails, taken as
/// stored.
#[tokio::test]
async fn chunks_stream_through_the_front() {
    let cas = front().await;
    for (len, chunk) in [
        (2 * WRITE_CHUNK_BYTES + 12_345, 1000),
        (WRITE_CHUNK_BYTES, WRITE_CHUNK_BYTES),
        (3 * WRITE_CHUNK_BYTES + 7, 3 * WRITE_CHUNK_BYTES + 7),
        (0, 1),
    ] {
        let (bytes, digest, chunks) = chunked(len, chunk);
        let stored = cas.put_chunks(digest.clone(), chunks).await;
        assert_eq!(stored.as_ref(), Ok(&digest), "{len} in chunks of {chunk}");
        assert_eq!(fetch(&cas, &digest).await.expect("fetch"), bytes);
    }
    // The stream ends 10 bytes short of the digest: the front refuses the write.
    let (_, mut digest, chunks) = chunked(10, 4);
    digest.size_bytes = 20;
    assert!(cas.put_chunks(digest, chunks).await.is_err());
    // A chunk that cannot be read.
    let error = cas
        .put_chunks(digest_of(b"abcdef"), failing(b"abc"))
        .await
        .expect_err("unreadable");
    assert!(
        matches!(&error, CasError::Read(_, why) if why == "disk gone"),
        "{error:?}"
    );
    // A CAS that commits less than was sent.
    let liar = CasClient::new(serve(Routes::new(ByteStreamServer::new(Liar))).await);
    let (_, digest, chunks) = chunked(5, 2);
    let error = liar
        .put_chunks(digest, chunks)
        .await
        .expect_err("short commit");
    assert!(
        matches!(&error, CasError::Unavailable(_, why) if why.contains("committed 1 bytes of 5")),
        "{error:?}"
    );
}

/// Catches the trait's default `put_chunks` storing other bytes than the chunks',
/// taking a digest that does not match as stored, or a failed chunk as the end.
#[tokio::test]
async fn the_default_put_chunks_gathers_and_checks_its_digest() {
    let cas = MemoryCas::default();
    let (bytes, digest, chunks) = chunked(3000, 700);
    assert_eq!(
        cas.put_chunks(digest.clone(), chunks).await,
        Ok(digest.clone())
    );
    assert_eq!(cas.blob(&digest), Some(bytes));
    let (_, _, chunks) = chunked(10, 3);
    let wrong = digest_of(b"something else");
    let error = cas
        .put_chunks(wrong.clone(), chunks)
        .await
        .expect_err("mismatch");
    assert!(
        matches!(&error, CasError::Corrupt(blob, _) if *blob == label(&wrong)),
        "{error:?}"
    );
    let error = cas
        .put_chunks(wrong, failing(b"x"))
        .await
        .expect_err("unreadable");
    assert!(matches!(&error, CasError::Read(..)), "{error:?}");
    assert!(
        error
            .to_string()
            .contains("could not be read for storing: disk gone")
    );
}
