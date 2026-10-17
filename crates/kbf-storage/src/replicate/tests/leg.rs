//! The call to the replica on its own, against servers that answer as no correct replica
//! would.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kbf_map::MemberId;
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};

use crate::StorageError;
use crate::replicate::{Leg, Peers, ReplicaLeg};
use crate::service::tests::fake::{connect, serve_odd, serve_routes};

fn message(first: bool, data: &[u8], finish: bool) -> WriteRequest {
    WriteRequest {
        resource_name: if first {
            format!("uploads/u/blobs/{}/{}", "ab".repeat(32), data.len())
        } else {
            String::new()
        },
        write_offset: 0,
        finish_write: finish,
        data: data.to_vec(),
    }
}

fn leg(channel: tonic::transport::Channel, size: u64) -> Leg {
    Leg::open(
        channel,
        1,
        MemberId(1),
        size,
        Duration::from_secs(30),
        Duration::from_secs(30),
    )
}

/// Catches an acknowledgement taken at its word: a replica that answers for other than the
/// blob's size (here 0) is an INTERNAL error, not a copy.
#[tokio::test]
async fn an_acknowledgement_of_the_wrong_size_is_not_a_copy() {
    let (_odd, channel) = serve_odd().await;
    let mut leg = leg(channel, 3);
    leg.send(message(true, b"abc", true)).await.expect("sent");
    let answer = leg.finish().await;
    let Err(StorageError::Internal(why)) = answer else {
        panic!("{answer:?}");
    };
    assert!(why.contains("3-byte write"), "{why}");
}

/// Catches a replica that answers before the blob is complete being waited on: the send
/// that finds the call ended says so, INTERNAL.
#[tokio::test]
async fn a_replica_that_answers_early_ends_the_leg() {
    let (_odd, channel) = serve_odd().await;
    let mut leg = leg(channel, 2000 * 1024);
    let mut ended = None;
    for i in 0..2000 {
        if let Err(e) = leg.send(message(i == 0, &[1; 1024], false)).await {
            ended = Some(e);
            break;
        }
        tokio::task::yield_now().await;
    }
    let Some(StorageError::Internal(why)) = ended else {
        panic!("{ended:?}");
    };
    assert!(why.contains("before the blob was complete"), "{why}");
}

/// Catches a leg to an address nothing listens on succeeding, or failing for a reason other
/// than the peer being unreachable: UNAVAILABLE.
#[tokio::test]
async fn a_leg_to_nothing_is_unavailable() {
    let peers = Peers::plain();
    let channel = peers.channel("127.0.0.1:1").expect("a channel");
    let mut leg = leg(channel, 3);
    let sent = leg.send(message(true, b"abc", true)).await;
    let answer = match sent {
        Ok(()) => leg.finish().await,
        Err(e) => Err(e),
    };
    assert!(
        matches!(answer, Err(StorageError::Unavailable(_))),
        "{answer:?}"
    );
}

/// Catches an address that is not one making a channel, or one that is made twice.
#[tokio::test]
async fn peers_make_one_channel_per_address_and_refuse_a_bad_one() {
    let peers = Peers::plain();
    assert!(matches!(
        peers.channel("not an address"),
        Err(StorageError::Internal(m)) if m.contains("not an address")
    ));
    peers.channel("127.0.0.1:1").expect("first");
    peers.channel("127.0.0.1:1").expect("cached");
    let tls = Peers::tls(tonic::transport::ClientTlsConfig::new());
    tls.channel("localhost:1").expect("a TLS channel");
    // An identity the TLS stack cannot read is the channel's error, not a connection's.
    let broken = Peers::tls(tonic::transport::ClientTlsConfig::new().identity(
        tonic::transport::Identity::from_pem("not a certificate", "not a key"),
    ));
    assert!(matches!(
        broken.channel("localhost:2"),
        Err(StorageError::Internal(m)) if m.contains("TLS for")
    ));
}

/// A ByteStream that reads the first message of a write and then never answers, and says when
/// the handler is dropped (its caller reset the stream).
struct Stuck(Arc<AtomicBool>);

struct SawEnd(Arc<AtomicBool>);

impl Drop for SawEnd {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

type Boxed<T> = futures::stream::BoxStream<'static, Result<T, tonic::Status>>;

#[tonic::async_trait]
impl ByteStream for Stuck {
    type ReadStream = Boxed<ReadResponse>;

    async fn read(
        &self,
        _: tonic::Request<ReadRequest>,
    ) -> Result<tonic::Response<Self::ReadStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("not asked"))
    }

    async fn write(
        &self,
        request: tonic::Request<tonic::Streaming<WriteRequest>>,
    ) -> Result<tonic::Response<WriteResponse>, tonic::Status> {
        let _ended = SawEnd(Arc::clone(&self.0));
        let mut stream = request.into_inner();
        let _ = stream.message().await?;
        futures::future::pending().await
    }

    async fn query_write_status(
        &self,
        _: tonic::Request<QueryWriteStatusRequest>,
    ) -> Result<tonic::Response<QueryWriteStatusResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not asked"))
    }
}

/// Catches a leg that outlives its write: when the primary's write ends (the writer left,
/// or another part of it failed) the call to the replica is reset, so a replica that is
/// not answering does not hold the call, and its half-received blob, for ever.
#[tokio::test]
async fn a_dropped_leg_resets_its_call() {
    let ended = Arc::new(AtomicBool::new(false));
    let routes = tonic::service::Routes::new(ByteStreamServer::new(Stuck(Arc::clone(&ended))));
    let channel = connect(serve_routes(routes, None).await).await;
    let mut leg = leg(channel, 6);
    leg.send(message(true, b"abc", false)).await.expect("sent");
    for _ in 0..500 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        // The handler is up once it has the message; it is not dropped yet.
    }
    assert!(!ended.load(Ordering::SeqCst), "the call ended on its own");
    drop(leg);
    for _ in 0..500 {
        if ended.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the replica's handler was not reset by the dropped leg");
}

/// A ByteStream that answers a write at once, unread, for the size it is given: a replica that holds
/// the blob already.
struct Holds(i64);

#[tonic::async_trait]
impl ByteStream for Holds {
    type ReadStream = Boxed<ReadResponse>;

    async fn read(
        &self,
        _: tonic::Request<ReadRequest>,
    ) -> Result<tonic::Response<Self::ReadStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("not asked"))
    }

    async fn write(
        &self,
        _: tonic::Request<tonic::Streaming<WriteRequest>>,
    ) -> Result<tonic::Response<WriteResponse>, tonic::Status> {
        Ok(tonic::Response::new(WriteResponse {
            committed_size: self.0,
        }))
    }

    async fn query_write_status(
        &self,
        _: tonic::Request<QueryWriteStatusRequest>,
    ) -> Result<tonic::Response<QueryWriteStatusResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not asked"))
    }
}

/// Catches a replica's early answer for the blob's whole size being an error: it holds the
/// blob, so the leg is done, every message after it is dropped without error (far more than
/// the queue holds are sent) and finishing it is a copy. Mutant: `ended` failing on any
/// early answer.
#[tokio::test]
async fn an_early_answer_for_the_whole_blob_is_a_copy() {
    let size = 2000 * 1024;
    let routes = tonic::service::Routes::new(ByteStreamServer::new(Holds(size)));
    let channel = connect(serve_routes(routes, None).await).await;
    let mut leg = leg(channel, size as u64);
    for i in 0..2000 {
        leg.send(message(i == 0, &[1; 1024], false))
            .await
            .expect("sent or dropped");
        tokio::task::yield_now().await;
    }
    leg.finish().await.expect("a copy");
}

/// Catches an early answer for another size than the blob's being taken for a copy.
#[tokio::test]
async fn an_early_answer_for_another_size_is_not_a_copy() {
    let routes = tonic::service::Routes::new(ByteStreamServer::new(Holds(5)));
    let channel = connect(serve_routes(routes, None).await).await;
    let mut leg = leg(channel, 2000 * 1024);
    let mut ended = None;
    for i in 0..2000 {
        if let Err(e) = leg.send(message(i == 0, &[1; 1024], false)).await {
            ended = Some(e);
            break;
        }
        tokio::task::yield_now().await;
    }
    let Some(StorageError::Internal(why)) = ended else {
        panic!("{ended:?}");
    };
    assert!(why.contains("before the blob was complete"), "{why}");
}

/// A ByteStream that answers every write with the status `code`, unread.
struct Declines(tonic::Code);

#[tonic::async_trait]
impl ByteStream for Declines {
    type ReadStream = Boxed<ReadResponse>;

    async fn read(
        &self,
        _: tonic::Request<ReadRequest>,
    ) -> Result<tonic::Response<Self::ReadStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("not asked"))
    }

    async fn write(
        &self,
        _: tonic::Request<tonic::Streaming<WriteRequest>>,
    ) -> Result<tonic::Response<WriteResponse>, tonic::Status> {
        Err(tonic::Status::new(self.0, "declined"))
    }

    async fn query_write_status(
        &self,
        _: tonic::Request<QueryWriteStatusRequest>,
    ) -> Result<tonic::Response<QueryWriteStatusResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not asked"))
    }
}

/// What a write of 2000 KiB to a replica that answers `code` at once came to: `Ok(None)` if
/// the replica holds a copy, `Ok(Some(_))` if it was lost (owed), `Err` if it refused. The
/// messages are more than the leg queues, so the answer is met while sending.
async fn replica_answering(code: tonic::Code) -> Result<Option<StorageError>, StorageError> {
    let routes = tonic::service::Routes::new(ByteStreamServer::new(Declines(code)));
    let channel = connect(serve_routes(routes, None).await).await;
    let mut leg = ReplicaLeg::open(
        channel,
        1,
        MemberId(1),
        2000 * 1024,
        Duration::from_secs(30),
        Duration::from_secs(30),
    );
    for i in 0..2000 {
        leg.send(message(i == 0, &[1; 1024], i == 1999)).await?;
        tokio::task::yield_now().await;
    }
    leg.finish().await
}

/// Catches a replica that declined being taken for one that could not be reached (the write
/// would be answered with one copy and a debt over the replica's refusal): only the codes
/// that say nothing was reached or answered in time are a lost replica; ABORTED, DATA_LOSS,
/// RESOURCE_EXHAUSTED, CANCELLED and a bare FAILED_PRECONDITION are the write's failure.
/// Mutant: `lift` mapping every code it does not know to `Unavailable`.
#[tokio::test]
async fn a_replica_that_declines_is_not_lost() {
    use tonic::Code;
    for code in [
        Code::Aborted,
        Code::DataLoss,
        Code::ResourceExhausted,
        Code::Cancelled,
        Code::FailedPrecondition,
    ] {
        let answer = replica_answering(code).await;
        assert!(
            matches!(&answer, Err(StorageError::Internal(m)) if m.contains(&format!("{code:?}"))),
            "{code:?}: {answer:?}"
        );
    }
    for code in [Code::Unavailable, Code::DeadlineExceeded, Code::Unknown] {
        let answer = replica_answering(code).await;
        assert!(
            matches!(answer, Ok(Some(StorageError::Unavailable(_)))),
            "{code:?}: {answer:?}"
        );
    }
}
