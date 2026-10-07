//! The daemon's CAS client against a real front cache in this process, and against a
//! ByteStream server that misbehaves.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use futures::stream;
use kbf_daemon::cas::{Cas, CasClient, CasError, WRITE_CHUNK_BYTES, digest_of, fetch, label};
use kbf_front::Cache;
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use support::memory::MemoryCas;
use support::scratch;
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

/// A file of `len` patterned bytes in `dir`, opened for reading, and its digest.
fn file_of(
    dir: &std::path::Path,
    name: &str,
    len: usize,
) -> (std::fs::File, kbf_proto::reapi::Digest) {
    let bytes: Vec<u8> = (0..len).map(|i| (i % 239) as u8).collect();
    let path = dir.join(name);
    std::fs::write(&path, &bytes).expect("write");
    (std::fs::File::open(&path).expect("open"), digest_of(&bytes))
}

/// Catches a streamed upload that loses, repeats or reorders bytes across message
/// boundaries (a file of several chunks, one of exactly one chunk, an empty one), and
/// a file shorter than its digest says, or not readable, taken as stored.
#[tokio::test]
async fn files_stream_through_the_front_in_chunks() {
    let cas = front().await;
    let dir = scratch("put-file");
    for (name, len) in [
        ("big", 2 * WRITE_CHUNK_BYTES + 12_345),
        ("one", WRITE_CHUNK_BYTES),
        ("empty", 0),
    ] {
        let (file, digest) = file_of(&dir, name, len);
        assert_eq!(
            cas.put_file(file, digest.clone()).await.expect(name),
            digest
        );
        let bytes = fetch(&cas, &digest).await.expect("fetch");
        assert_eq!(bytes.len(), len, "{name}");
    }
    // A digest that claims more bytes than the file holds: the front sees a short
    // write and refuses it.
    let (file, mut digest) = file_of(&dir, "short", 10);
    digest.size_bytes = 20;
    assert!(cas.put_file(file, digest).await.is_err());
    // A file opened for writing only cannot be read.
    let unreadable = std::fs::File::create(dir.join("write-only")).expect("create");
    let error = cas
        .put_file(unreadable, digest_of(b"x"))
        .await
        .expect_err("unreadable");
    assert!(
        matches!(&error, CasError::Unavailable(_, why) if why.starts_with("read: ")),
        "{error:?}"
    );
    // A CAS that commits less than was sent.
    let liar = CasClient::new(serve(Routes::new(ByteStreamServer::new(Liar))).await);
    let (file, digest) = file_of(&dir, "liar", 5);
    let error = liar.put_file(file, digest).await.expect_err("short commit");
    assert!(
        matches!(&error, CasError::Unavailable(_, why) if why.contains("committed 1 bytes of 5")),
        "{error:?}"
    );
}

/// Catches the trait's default `put_file` storing other bytes than the file's, taking
/// a digest that does not match as stored, or an unreadable file as empty.
#[tokio::test]
async fn the_default_put_file_reads_the_file_and_checks_its_digest() {
    let cas = MemoryCas::default();
    let dir = scratch("default-put-file");
    let (file, digest) = file_of(&dir, "f", 3000);
    assert_eq!(
        cas.put_file(file, digest.clone()).await.expect("stored"),
        digest
    );
    assert_eq!(cas.blob(&digest).map(|b| b.len()), Some(3000));
    let (file, _) = file_of(&dir, "g", 10);
    let wrong = digest_of(b"something else");
    let error = cas
        .put_file(file, wrong.clone())
        .await
        .expect_err("mismatch");
    assert!(
        matches!(&error, CasError::Corrupt(blob, _) if *blob == label(&wrong)),
        "{error:?}"
    );
    let unreadable = std::fs::File::create(dir.join("write-only")).expect("create");
    let error = cas
        .put_file(unreadable, wrong)
        .await
        .expect_err("unreadable");
    assert!(
        matches!(&error, CasError::Unavailable(_, why) if why.starts_with("read: ")),
        "{error:?}"
    );
}
