//! The engine against kbf-front's CAS services, and against a ByteStream server that
//! breaks its promises.

use std::sync::Arc;

use futures::stream;
use kbf_front::Cache;
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_proto::reapi::Digest;
use kbf_transfer::{
    Cas, CasError, Chunks, MAX_MESSAGE_BYTES, TransferConfig, Transfers, WRITE_CHUNK_BYTES,
    digest_of, fetch, label,
};
use tonic::service::Routes;
use tonic::{Request, Response, Status, Streaming};

use super::engine;
use super::fake::serve;

async fn front() -> Arc<Transfers> {
    let addr = serve(kbf_front::routes(Arc::new(Cache::memory()))).await;
    engine(addr, TransferConfig::default())
}

/// Catches the engine decoding less than the front may send: its limit must be the
/// front's.
#[test]
fn the_decode_limit_is_the_fronts_message_limit() {
    assert_eq!(MAX_MESSAGE_BYTES, kbf_front::MAX_MESSAGE_BYTES);
    assert_eq!(
        kbf_transfer::MAX_CONTROL_MESSAGE_BYTES,
        kbf_front::MAX_CONTROL_MESSAGE_BYTES
    );
}

/// Catches an engine that loses or reorders bytes on the way in or out: an empty blob,
/// a small one, and one large enough to take several ByteStream messages each way;
/// and FindMissing against the front's own answer (a blob stored once is present).
#[tokio::test]
async fn blobs_round_trip_through_the_front() {
    let cas = front().await;
    let large: Vec<u8> = (0..(2 * WRITE_CHUNK_BYTES + 12_345))
        .map(|i| (i % 251) as u8)
        .collect();
    for bytes in [Vec::new(), b"small".to_vec(), large] {
        let digest = cas.put(bytes.clone()).await.expect("put");
        assert_eq!(digest, digest_of(&bytes));
        assert_eq!(fetch(&*cas, &digest).await.expect("fetch"), bytes);
    }
    let scope = cas.scope(&kbf_types::LeaseId::new(1, 1));
    assert_eq!(scope.put(b"small".to_vec()).await, Ok(digest_of(b"small")));
    let stats = scope.stats();
    assert_eq!(
        (
            stats.find_missing_digests,
            stats.outputs_present,
            stats.write_calls
        ),
        (1, 1, 0)
    );
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

/// Catches a front that cannot be reached reported as a missing blob, and a call to
/// it retried more than `--transfer-retries` times or not at all.
#[tokio::test]
async fn an_unreachable_front_is_unavailable_after_the_retries() {
    // A port nothing listens on: bound and never listening, held for the whole test so
    // no other test binds it meanwhile (a dial gets RST).
    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket
        .bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
        .expect("bind");
    let addr = socket.local_addr().expect("addr");
    let config = TransferConfig {
        retries: 2,
        ..TransferConfig::default()
    };
    let cas = engine(addr, config);
    let scope = cas.scope(&kbf_types::LeaseId::new(1, 2));
    let digest = digest_of(b"x");
    assert!(matches!(
        scope.get(&digest).await,
        Err(CasError::Unavailable(..))
    ));
    assert!(matches!(
        scope.put(b"x".to_vec()).await,
        Err(CasError::Unavailable(..))
    ));
    let stats = scope.stats();
    // Each call: one attempt and two retries.
    assert_eq!(
        (stats.read_calls, stats.write_calls, stats.retries),
        (3, 3, 4)
    );
    drop(socket);
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

async fn liar() -> Arc<Transfers> {
    engine(
        serve(Routes::new(ByteStreamServer::new(Liar))).await,
        TransferConfig::default(),
    )
}

/// Catches a read that breaks midway returned as a short blob, and a write the CAS did
/// not fully commit taken as stored (an output named in a result would be missing).
/// The Liar serves no FindMissing, so every blob is uploaded.
#[tokio::test]
async fn broken_calls_are_unavailable() {
    let cas = liar().await;
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
    // The Liar answers QueryWriteStatus with an error; the engine never asks it.
    let addr = serve(Routes::new(ByteStreamServer::new(Liar))).await;
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut client =
        kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient::new(channel);
    let status = client
        .query_write_status(QueryWriteStatusRequest::default())
        .await
        .expect_err("unimplemented");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
}

/// `len` patterned bytes, their digest, and the same bytes as a stream of chunks of
/// at most `chunk` bytes.
pub fn chunked(len: usize, chunk: usize) -> (Vec<u8>, Digest, Chunks) {
    let bytes: Vec<u8> = (0..len).map(|i| (i % 239) as u8).collect();
    let pieces: Vec<std::io::Result<Vec<u8>>> =
        bytes.chunks(chunk.max(1)).map(|c| Ok(c.to_vec())).collect();
    let digest = digest_of(&bytes);
    (bytes, digest, Box::pin(stream::iter(pieces)))
}

/// A stream that yields `first`, then fails.
pub fn failing(first: &[u8]) -> Chunks {
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
        assert_eq!(fetch(&*cas, &digest).await.expect("fetch"), bytes);
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
    let (_, digest, chunks) = chunked(5, 2);
    let error = liar()
        .await
        .put_chunks(digest, chunks)
        .await
        .expect_err("short commit");
    assert!(
        matches!(&error, CasError::Unavailable(_, why) if why.contains("committed 1 bytes of 5")),
        "{error:?}"
    );
}
