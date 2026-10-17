//! The frontend's proxy to a storage node (`kbf_front::Proxy`), over real gRPC to a fake
//! node that streams: ByteStream as a raw HTTP/2 pass-through (Read, Write and
//! QueryWriteStatus), the batch CAS calls and the action cache at the tonic level, and
//! what is common to both: authorization, owners and counters.
//!
//! Every test names the defect it catches. Memory is judged by counting bytes
//! (`harness`), never by timing or RSS.

#[path = "proxy/harness.rs"]
mod harness;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use harness::{
    CHUNK, Fake, HELD_BOUND, LEAD_BOUND, Lead, Replay, blob, down, eventually, front, node, owner,
    pattern, upload,
};
use http_body::Frame;
use kbf_auth::{
    AllowAuthenticator, AuthenticationMetadata, Authorizer, Authorizers, DenyAuthenticator,
};
use kbf_front::{Direction, Method};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, ReadRequest, ReadResponse, WriteRequest, WriteResponse,
};
use prost::Message as _;
use serde_json::{Value, json};
use tonic::codegen::{Service, http};
use tonic::transport::Channel;
use tonic::{Code, Request, Status};

const CHUNKS: usize = 32; // a 32 MiB blob
const SIZE: u64 = (CHUNK * CHUNKS) as u64;

fn read_req(name: &str) -> ReadRequest {
    ReadRequest {
        resource_name: name.to_owned(),
        read_offset: 0,
        read_limit: 0,
    }
}

/// Reads a blob to its end, slowly if `lead` is given (the consumer sleeps a millisecond
/// per message and says what it took); the number of messages, or the error that ended it.
async fn read_all(
    client: &mut ByteStreamClient<Channel>,
    name: &str,
    lead: Option<&Lead>,
) -> Result<usize, Status> {
    let mut s = client.read(read_req(name)).await?.into_inner();
    let mut i = 0;
    while let Some(m) = s.message().await? {
        if let Some(lead) = lead {
            assert!(
                m.data.iter().all(|b| *b == pattern(i)),
                "chunk {i} corrupted"
            );
            lead.consumed.fetch_add(m.data.len() as u64, SeqCst);
            lead.sample();
            tokio::time::sleep(Duration::from_millis(1)).await; // a slow client
        }
        i += 1;
    }
    Ok(i)
}

/// An upload of `chunks` messages of 1 MiB, generated on demand and counted as produced.
fn upload_stream(
    name: String,
    chunks: usize,
    lead: Arc<Lead>,
) -> impl futures::Stream<Item = WriteRequest> {
    stream::unfold(0usize, move |i| {
        let (lead, name) = (Arc::clone(&lead), name.clone());
        async move {
            if i >= chunks {
                return None;
            }
            lead.produced.fetch_add(CHUNK as u64, SeqCst);
            lead.sample();
            let m = WriteRequest {
                resource_name: if i == 0 { name } else { String::new() },
                write_offset: (i * CHUNK) as i64,
                finish_write: i == chunks - 1,
                data: vec![pattern(i); CHUNK],
            };
            Some((m, i + 1))
        }
    })
}

/// The protobuf size of the messages of [`upload_stream`].
fn upload_bytes(name: &str, chunks: usize) -> u64 {
    (0..chunks)
        .map(|i| {
            WriteRequest {
                resource_name: if i == 0 {
                    name.to_owned()
                } else {
                    String::new()
                },
                write_offset: (i * CHUNK) as i64,
                finish_write: i == chunks - 1,
                data: vec![0; CHUNK],
            }
            .encoded_len() as u64
        })
        .sum()
}

const PREFIX: u64 = 5; // the gRPC message prefix, counted on the wire

fn client(front: &harness::Front) -> ByteStreamClient<Channel> {
    ByteStreamClient::new(front.channel.clone())
}

fn allow() -> Authorizers {
    Authorizers::allow_all()
}

// ---------------------------------------------------------------------------------
// Memory: the proxy never holds a blob.

/// Catches: a Read proxy that collects the response before it sends (the whole 32 MiB
/// would sit in the proxy; the lead and the proxy's own held bytes would both be the
/// blob), and byte counters that are not exact per method and direction.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_streams_in_bounded_memory_and_is_counted_exactly() {
    let fake = Fake::new(false);
    let f = front(vec![owner("a", node(&fake).await)], allow(), None).await;
    let name = blob("main", '0', SIZE);
    let n = read_all(&mut client(&f), &name, Some(&fake.lead))
        .await
        .unwrap();
    assert_eq!(n, CHUNKS);
    assert!(fake.lead.peak() <= LEAD_BOUND, "lead {}", fake.lead.peak());
    let meter = f.proxy.meter();
    assert!(
        meter.held_peak() <= HELD_BOUND,
        "held {}",
        meter.held_peak()
    );
    let message = ReadResponse {
        data: vec![0; CHUNK],
    }
    .encoded_len() as u64
        + PREFIX;
    let to = read_req(&name).encoded_len() as u64 + PREFIX;
    assert_eq!(
        meter.bytes(Method::Read, Direction::FromStorage),
        CHUNKS as u64 * message
    );
    assert_eq!(meter.bytes(Method::Read, Direction::ToStorage), to);
    for other in [Method::Write, Method::QueryWriteStatus] {
        assert_eq!(meter.bytes(other, Direction::ToStorage), 0);
        assert_eq!(meter.bytes(other, Direction::FromStorage), 0);
    }
    assert!(
        eventually(|| meter.held() == 0).await,
        "held {}",
        meter.held()
    );
}

/// Catches: a Write proxy that collects the upload before it sends (the proxy would
/// hold the blob and the lead would be all of it), and inexact counters.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_streams_in_bounded_memory_and_is_counted_exactly() {
    let fake = Fake::new(true);
    let f = front(vec![owner("a", node(&fake).await)], allow(), None).await;
    let name = upload("main", '0', SIZE);
    let sent = upload_stream(name.clone(), CHUNKS, Arc::clone(&fake.lead));
    let done = client(&f)
        .write(Request::new(sent))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(done.committed_size, SIZE as i64);
    assert!(fake.lead.peak() <= LEAD_BOUND, "lead {}", fake.lead.peak());
    let meter = f.proxy.meter();
    assert!(
        meter.held_peak() <= HELD_BOUND,
        "held {}",
        meter.held_peak()
    );
    let wire = upload_bytes(&name, CHUNKS) + CHUNKS as u64 * PREFIX;
    assert_eq!(meter.bytes(Method::Write, Direction::ToStorage), wire);
    let answer = WriteResponse {
        committed_size: SIZE as i64,
    }
    .encoded_len() as u64
        + PREFIX;
    assert_eq!(meter.bytes(Method::Write, Direction::FromStorage), answer);
    assert_eq!(meter.bytes(Method::Read, Direction::ToStorage), 0);
    assert!(
        eventually(|| meter.held() == 0).await,
        "held {}",
        meter.held()
    );
}

/// The negative control: a proxy that collects (what the bounds above rule out) does
/// exceed them, so they can tell buffering from streaming. A raw service over the same
/// node that holds every frame of the request, or of the response, before passing any.
#[tokio::test(flavor = "multi_thread")]
async fn a_collecting_proxy_exceeds_the_bound() {
    let fake = Fake::new(false);
    let mut c = ByteStreamClient::new(harness::serve(harness::collecting(node(&fake).await)).await);
    read_all(&mut c, &blob("main", '0', SIZE), Some(&fake.lead))
        .await
        .unwrap();
    assert!(
        fake.lead.peak() > LEAD_BOUND * 5 / 2,
        "read lead {}",
        fake.lead.peak()
    );
    let fake = Fake::new(true);
    let mut c = ByteStreamClient::new(harness::serve(harness::collecting(node(&fake).await)).await);
    let sent = upload_stream(upload("main", '0', SIZE), CHUNKS, Arc::clone(&fake.lead));
    c.write(Request::new(sent)).await.unwrap();
    assert!(
        fake.lead.peak() > LEAD_BOUND * 5 / 2,
        "write lead {}",
        fake.lead.peak()
    );
}

// ---------------------------------------------------------------------------------
// No size cap.

/// Catches: a size cap anywhere on the pass-through (the old cache cap of 1 GiB refusing
/// the blob), a proxy that sizes a buffer by the declared size, and memory that
/// grows with the blob: a blob one byte over the old cap crosses in both directions in
/// bounded memory, and one that declares 16 GiB while sending 3 MiB allocates nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_imposes_no_size_cap() {
    /// One byte over the cap the cache once had.
    const OVER: u64 = (1 << 30) + 1;
    let fake = Fake::new(false);
    let f = front(vec![owner("a", node(&fake).await)], allow(), None).await;
    let chunks = (OVER.div_ceil(CHUNK as u64)) as usize;
    let n = read_all(&mut client(&f), &blob("main", '0', OVER), None)
        .await
        .unwrap();
    assert_eq!(n, chunks);
    let meter = f.proxy.meter();
    let message = ReadResponse {
        data: vec![0; CHUNK],
    }
    .encoded_len() as u64
        + PREFIX;
    // The last message is one byte; the others a chunk.
    let last = ReadResponse { data: vec![0; 1] }.encoded_len() as u64 + PREFIX;
    let from = meter.bytes(Method::Read, Direction::FromStorage);
    assert_eq!(from, (chunks as u64 - 1) * message + last);
    assert!(
        meter.held_peak() <= HELD_BOUND,
        "held {}",
        meter.held_peak()
    );

    let sent = stream::unfold(0u64, |sent| async move {
        let len = (OVER - sent).min(CHUNK as u64) as usize;
        (sent < OVER).then(|| {
            let first = sent == 0;
            let m = WriteRequest {
                resource_name: if first {
                    upload("main", '0', OVER)
                } else {
                    String::new()
                },
                write_offset: sent as i64,
                finish_write: sent + len as u64 == OVER,
                data: vec![pattern((sent / CHUNK as u64) as usize); len],
            };
            (m, sent + len as u64)
        })
    });
    let done = client(&f)
        .write(Request::new(sent))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(done.committed_size, OVER as i64);
    assert!(
        meter.held_peak() <= HELD_BOUND,
        "held {}",
        meter.held_peak()
    );

    let huge = upload("main", '0', 16 << 30);
    let few = upload_stream(huge, 3, Arc::new(Lead::default()));
    let done = client(&f)
        .write(Request::new(few))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(done.committed_size, 3 * CHUNK as i64);
    assert!(meter.held_peak() <= HELD_BOUND);
}

// ---------------------------------------------------------------------------------
// Raw calls: what a stock client's library will not send.

/// One gRPC message with this flag byte.
fn grpc(flag: u8, message: &[u8]) -> Vec<u8> {
    let mut out = vec![flag];
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// A body of these frames.
fn body_of(frames: Vec<Frame<Bytes>>) -> tonic::body::Body {
    tonic::body::Body::new(Replay(frames.into()))
}

/// Posts `body` to `path` with these headers, and says how the call ended: the status
/// in the headers or the trailers.
async fn raw(
    channel: &Channel,
    path: &str,
    headers: &[(&str, &str)],
    body: tonic::body::Body,
) -> Status {
    let mut request = http::Request::new(body);
    *request.method_mut() = http::Method::POST;
    *request.uri_mut() = path.parse().unwrap();
    request
        .headers_mut()
        .insert("content-type", "application/grpc".parse().unwrap());
    request
        .headers_mut()
        .insert("te", "trailers".parse().unwrap());
    for (k, v) in headers {
        request.headers_mut().insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    let mut channel = channel.clone();
    std::future::poll_fn(|cx| channel.poll_ready(cx))
        .await
        .unwrap();
    let response = match channel.call(request).await {
        Ok(response) => response,
        Err(e) => return Status::unavailable(e.to_string()),
    };
    let (head, mut body) = response.into_parts();
    if let Some(status) = Status::from_header_map(&head.headers) {
        return status;
    }
    while let Some(frame) =
        std::future::poll_fn(|cx| http_body::Body::poll_frame(std::pin::Pin::new(&mut body), cx))
            .await
    {
        let Ok(frame) = frame else {
            return Status::unknown("the response broke");
        };
        if let Some(status) = frame.trailers_ref().and_then(Status::from_header_map) {
            return status;
        }
    }
    Status::unknown("no status")
}

fn data(bytes: Vec<u8>) -> Vec<Frame<Bytes>> {
    vec![Frame::data(Bytes::from(bytes))]
}

const READ: &str = "/google.bytestream.ByteStream/Read";
const WRITE: &str = "/google.bytestream.ByteStream/Write";
const QUERY: &str = "/google.bytestream.ByteStream/QueryWriteStatus";

/// Catches: a compressed ByteStream message passed through (its name is hidden, so it
/// would be routed and authorized by nothing), a refusal that does not say why, and the
/// identity encoding refused. REAPI `compressed-blobs` names are plain text and pass
/// through to the node unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn compression_is_refused_and_compressed_blob_names_pass() {
    let fake = Fake::new(false);
    let f = front(vec![owner("a", node(&fake).await)], allow(), None).await;
    let name = blob("main", '0', 3);
    let message = read_req(&name).encode_to_vec();
    for (flag, headers) in [
        (1, vec![("grpc-encoding", "gzip")]),
        (1, vec![]),
        (0, vec![("grpc-encoding", "gzip")]),
        (0, vec![("grpc-encoding", "zstd")]),
    ] {
        let status = raw(
            &f.channel,
            READ,
            &headers,
            body_of(data(grpc(flag, &message))),
        )
        .await;
        assert_eq!(
            status.code(),
            Code::InvalidArgument,
            "flag {flag} {headers:?}"
        );
        assert!(
            status.message().contains("compression"),
            "{}",
            status.message()
        );
    }
    for (flag, call, message) in [(1, WRITE, grpc(1, b"xyz")), (1, QUERY, grpc(1, b"xyz"))] {
        let status = raw(&f.channel, call, &[], body_of(data(message))).await;
        assert_eq!(status.code(), Code::InvalidArgument, "{call} {flag}");
        assert!(
            status.message().contains("compression"),
            "{}",
            status.message()
        );
    }
    assert_eq!(fake.calls.load(SeqCst), 0, "nothing reached the node");

    let identity = [("grpc-encoding", "identity")];
    let ok = raw(
        &f.channel,
        READ,
        &identity,
        body_of(data(grpc(0, &message))),
    )
    .await;
    assert_eq!(ok.code(), Code::Ok);
    let compressed = format!("main/compressed-blobs/zstd/{}/3", harness::hash('0'));
    let n = read_all(&mut client(&f), &compressed, None).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(fake.names(), [name, compressed]);
}

/// Catches: a request the proxy cannot route reaching a node anyway or hanging (a name
/// that is not a blob's, an empty request, one that ends before its name, a Read request
/// over the unary limit, a method the service does not have), and a client that breaks
/// before it names anything leaving bytes counted as held.
#[tokio::test(flavor = "multi_thread")]
async fn what_cannot_be_routed_is_refused_before_a_node_is_asked() {
    let fake = Fake::new(false);
    let f = front(vec![owner("a", node(&fake).await)], allow(), None).await;
    let c = &f.channel;
    let named = |name: &str| data(grpc(0, &read_req(name).encode_to_vec()));
    let cases = [
        (READ, named("nothing"), Code::InvalidArgument, "blobs/"),
        (
            READ,
            named(&format!("main/blobs/{}", harness::hash('0'))),
            Code::InvalidArgument,
            "hash",
        ),
        (
            READ,
            data(grpc(0, &[])),
            Code::InvalidArgument,
            "shorter than its resource_name",
        ),
        (
            READ,
            data(vec![0, 0]),
            Code::InvalidArgument,
            "shorter than its resource_name",
        ),
        (WRITE, vec![], Code::InvalidArgument, "ended before"),
        (
            WRITE,
            data(grpc(0, &[0x12, 1, 7])),
            Code::InvalidArgument,
            "must be resource_name",
        ),
        (
            QUERY,
            data(grpc(0, &[0x0a, 1, b'x'])),
            Code::InvalidArgument,
            "uploads/",
        ),
        (
            READ,
            data(vec![0; 70_000]),
            Code::InvalidArgument,
            "over 65536 bytes",
        ),
        (
            "/google.bytestream.ByteStream/Nope",
            vec![],
            Code::Unimplemented,
            "not served",
        ),
    ];
    for (call, frames, code, why) in cases {
        let status = raw(c, call, &[], body_of(frames)).await;
        assert_eq!(status.code(), code, "{call} {why}");
        assert!(
            status.message().contains(why),
            "{call}: {}",
            status.message()
        );
    }
    assert_eq!(fake.calls.load(SeqCst), 0);

    // A client that breaks before it names anything: the call ends, nothing is held.
    struct Broken;
    impl http_body::Body for Broken {
        type Data = Bytes;
        type Error = Status;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<Frame<Bytes>, Status>>> {
            std::task::Poll::Ready(Some(Err(Status::cancelled("the client left"))))
        }
    }
    for call in [READ, WRITE] {
        let _ = raw(c, call, &[], tonic::body::Body::new(Broken)).await;
    }
    assert!(eventually(|| f.proxy.meter().held() == 0).await);
    assert_eq!(fake.calls.load(SeqCst), 0);
}

// ---------------------------------------------------------------------------------
// Authorization.

/// Allows `user` on instance names under `prefix`; nobody else.
struct Only {
    user: &'static str,
    prefix: &'static str,
}

impl Authorizer for Only {
    fn authorize<'a>(
        &'a self,
        metadata: &'a AuthenticationMetadata,
        instance_names: &'a [&'a str],
    ) -> futures::future::BoxFuture<'a, Vec<Result<(), Status>>> {
        let who = metadata
            .public()
            .and_then(|p| p.get("user"))
            .and_then(Value::as_str);
        let answers = instance_names
            .iter()
            .map(|name| {
                let under = *name == self.prefix || name.starts_with(&format!("{}/", self.prefix));
                if who == Some(self.user) && under {
                    Ok(())
                } else {
                    Err(Status::permission_denied(format!("{who:?} on {name:?}")))
                }
            })
            .collect();
        Box::pin(std::future::ready(answers))
    }
}

fn only(user: &'static str, prefix: &'static str) -> Authorizers {
    let one = |_| Arc::new(Only { user, prefix }) as Arc<dyn Authorizer>;
    Authorizers {
        capabilities: one(()),
        cas_get: one(()),
        cas_put: one(()),
        cas_find_missing: one(()),
        ac_get: one(()),
        execute: one(()),
    }
}

fn caller(user: &str) -> Arc<AllowAuthenticator> {
    Arc::new(AllowAuthenticator::new(AuthenticationMetadata::new(
        Some(json!({ "user": user })),
        None,
    )))
}

/// Catches, on the raw path: a Read, Write or QueryWriteStatus served for an instance
/// the authorizer refuses (the instance name is read from the peeked resource name), one
/// refused whose first message already reached the node, an authorizer that does not see
/// who the authentication layer found (it would deny alice, or allow everyone), and a
/// call the authentication layer refuses reaching the service.
#[tokio::test(flavor = "multi_thread")]
async fn the_raw_path_asks_the_authorizer_of_the_peeked_instance() {
    let fake = Fake::new(false);
    let a = node(&fake).await;
    let f = front(
        vec![owner("a", a)],
        only("alice", "main"),
        Some(caller("alice")),
    )
    .await;
    let mut c = client(&f);
    assert_eq!(
        read_all(&mut c, &blob("main/ci", '0', 3), None)
            .await
            .unwrap(),
        1
    );
    let done = c
        .write(Request::new(upload_stream(
            upload("main", '0', 3),
            1,
            Arc::default(),
        )))
        .await;
    assert_eq!(done.unwrap().into_inner().committed_size, CHUNK as i64);
    let status = QueryWriteStatusRequest {
        resource_name: upload("main", '0', 3),
    };
    assert!(
        c.query_write_status(status)
            .await
            .unwrap()
            .into_inner()
            .complete
    );
    let served = fake.calls.load(SeqCst);
    assert_eq!(served, 3);

    // The wrong instance, on each call.
    let e = read_all(&mut c, &blob("other", '0', 3), None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(
        e.message().starts_with("Authorization: "),
        "{}",
        e.message()
    );
    let sent = upload_stream(upload("mainly", '0', 3), 1, Arc::default());
    let e = c.write(Request::new(sent)).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let status = QueryWriteStatusRequest {
        resource_name: upload("other", '0', 3),
    };
    assert_eq!(
        c.query_write_status(status).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert_eq!(
        fake.calls.load(SeqCst),
        served,
        "a refused call never reached the node"
    );

    // Bob may not read what alice may; and a refusing authenticator stops the call.
    let f = front(
        vec![owner("a", a)],
        only("alice", "main"),
        Some(caller("bob")),
    )
    .await;
    let e = read_all(&mut client(&f), &blob("main", '0', 3), None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let denied = Arc::new(DenyAuthenticator::new("no certificate"));
    let f = front(vec![owner("a", a)], allow(), Some(denied)).await;
    let e = read_all(&mut client(&f), &blob("main", '0', 3), None)
        .await
        .unwrap_err();
    assert_eq!(
        (e.code(), e.message()),
        (Code::Unauthenticated, "no certificate")
    );
    assert_eq!(fake.calls.load(SeqCst), served);
}

// ---------------------------------------------------------------------------------
// Owners: fall-through, failure after the first byte, a primary that changes.

/// Reads `name` through a proxy to `owners`, with the calls each fake saw.
async fn read_via(owners: Vec<kbf_front::Owner>, name: &str) -> Result<usize, Status> {
    let f = front(owners, allow(), None).await;
    read_all(&mut client(&f), name, None).await
}

/// Catches: a Read that fails because the first owner did before any byte was sent (an
/// unreachable node, a status in the headers, a status as the first thing in the stream)
/// when the next owner has the blob; a request that the first owner refused as invalid
/// being asked of the next; a blob that ends empty read as a failure; and, when every
/// owner fails, the answer being NOT_FOUND only if every one said it. Tried in rank order.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_falls_through_to_the_next_owner_before_its_first_byte() {
    let name = blob("main", '0', 3 * CHUNK as u64);
    for first in ['1', '3'] {
        let bad = Fake::forcing(first);
        let good = Fake::new(false);
        let owners = vec![
            owner("bad", node(&bad).await),
            owner("good", node(&good).await),
        ];
        assert_eq!(
            read_via(owners, &name).await.unwrap(),
            3,
            "first fails as {first}"
        );
        assert_eq!((bad.calls.load(SeqCst), good.calls.load(SeqCst)), (1, 1));
    }
    let good = Fake::new(false);
    let owners = vec![down(), owner("good", node(&good).await)];
    assert_eq!(
        read_via(owners, &name).await.unwrap(),
        3,
        "first unreachable"
    );
    assert_eq!(good.calls.load(SeqCst), 1);

    // A refusal of the request is not the node's to pass over.
    let (bad, good) = (Fake::forcing('4'), Fake::new(false));
    let owners = vec![
        owner("bad", node(&bad).await),
        owner("good", node(&good).await),
    ];
    let e = read_via(owners, &name).await.unwrap_err();
    assert_eq!(
        (e.code(), e.message()),
        (Code::InvalidArgument, "not a blob")
    );
    assert_eq!(good.calls.load(SeqCst), 0);

    // An empty blob is an answer, not a failure.
    let (empty, good) = (Fake::forcing('5'), Fake::new(false));
    let owners = vec![
        owner("empty", node(&empty).await),
        owner("good", node(&good).await),
    ];
    assert_eq!(read_via(owners, &name).await.unwrap(), 0);
    assert_eq!(good.calls.load(SeqCst), 0);

    // Every owner failing: NOT_FOUND only if each said it; else the first that could not
    // say, whichever order they come in (an owner that is down may hold the blob).
    let missing = Fake::forcing('1');
    let e = read_via(
        vec![
            owner("m", node(&missing).await),
            owner("n", node(&missing).await),
        ],
        &name,
    )
    .await
    .unwrap_err();
    assert_eq!((e.code(), e.message()), (Code::NotFound, "no such blob"));
    for owners in [
        vec![down(), owner("m", node(&missing).await)],
        vec![owner("m", node(&missing).await), down()],
    ] {
        let e = read_via(owners, &name).await.unwrap_err();
        assert_eq!(e.code(), Code::Unavailable);
        assert!(e.message().contains("storage node down"), "{}", e.message());
    }
    let e = read_via(vec![], &name).await.unwrap_err();
    assert_eq!(e.code(), Code::Unavailable);
}

/// Catches: a Read that tries the next owner after the first byte was sent (the client
/// would get two nodes' bytes spliced, or the first bytes twice), and a mid-stream error
/// that does not reach the client as the node's own status.
#[tokio::test(flavor = "multi_thread")]
async fn a_failure_after_the_first_byte_is_the_clients_to_resume() {
    let (lossy, good) = (Fake::forcing('2'), Fake::new(false));
    let owners = vec![
        owner("lossy", node(&lossy).await),
        owner("good", node(&good).await),
    ];
    let f = front(owners, allow(), None).await;
    let mut s = client(&f)
        .read(read_req(&blob("main", '0', 3 * CHUNK as u64)))
        .await
        .unwrap()
        .into_inner();
    s.message().await.unwrap().unwrap();
    s.message().await.unwrap().unwrap();
    let e = s.message().await.unwrap_err();
    assert_eq!((e.code(), e.message()), (Code::DataLoss, "lost"));
    assert_eq!(
        good.calls.load(SeqCst),
        0,
        "no second owner after the first byte"
    );
}

/// A TCP forwarder to `target` that drops both of its sockets when told to: a node that
/// dies mid-response.
async fn cutter(target: SocketAddr) -> (SocketAddr, tokio::sync::watch::Sender<bool>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (cut, cut_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        while let Ok((mut from, _)) = listener.accept().await {
            let mut cut_rx = cut_rx.clone();
            tokio::spawn(async move {
                let mut to = tokio::net::TcpStream::connect(target).await.unwrap();
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut from, &mut to) => {}
                    _ = cut_rx.wait_for(|cut| *cut) => {}
                }
            });
        }
    });
    (addr, cut)
}

/// Catches: a node that dies mid-response ending the client's call as a clean end (a
/// truncated blob that looks whole), a hang, or an owner tried again after bytes were
/// sent. The client gets an error after fewer messages than the blob has.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_dies_mid_response_is_an_error_not_a_short_blob() {
    let (dying, good) = (Fake::new(false), Fake::new(false));
    let (cut_addr, cut) = cutter(node(&dying).await).await;
    let owners = vec![owner("dying", cut_addr), owner("good", node(&good).await)];
    let f = front(owners, allow(), None).await;
    let mut s = client(&f)
        .read(read_req(&blob("main", '0', SIZE)))
        .await
        .unwrap()
        .into_inner();
    let mut got = 0;
    let error = loop {
        match s.message().await {
            Ok(Some(_)) => got += 1,
            Ok(None) => panic!("a truncated blob ended cleanly after {got} messages"),
            Err(e) => break e,
        }
        if got == 2 {
            cut.send(true).unwrap();
        }
    };
    assert!(got < CHUNKS, "{got}");
    assert_ne!(error.code(), Code::Ok);
    assert_eq!(good.calls.load(SeqCst), 0);
    assert!(eventually(|| f.proxy.meter().held() == 0).await);
}

/// Catches: an upload that goes on to a node that is no longer its blob's primary (the
/// new primary would hold half a blob and the client would be told it was written), an
/// upload answered with the node's complaint about the broken body instead of
/// UNAVAILABLE, and a Write that is retried by the proxy.
#[tokio::test(flavor = "multi_thread")]
async fn a_primary_that_changes_mid_write_is_unavailable() {
    let (first, second) = (Fake::new(false), Fake::new(false));
    let (a, b) = (node(&first).await, node(&second).await);
    let f = front(vec![owner("a", a)], allow(), None).await;
    let (tx, rx) = tokio::sync::mpsc::channel::<WriteRequest>(1);
    let sent = stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|m| (m, rx)) });
    let mut c = client(&f);
    let call = tokio::spawn(async move { c.write(Request::new(sent)).await });
    let message = |i: usize, last: bool| WriteRequest {
        resource_name: if i == 0 {
            upload("main", '0', 4 * CHUNK as u64)
        } else {
            String::new()
        },
        write_offset: (i * CHUNK) as i64,
        finish_write: last,
        data: vec![pattern(i); CHUNK],
    };
    tx.send(message(0, false)).await.unwrap();
    tx.send(message(1, false)).await.unwrap();
    assert!(eventually(|| first.lead.consumed.load(SeqCst) >= 2 * CHUNK as u64).await);
    // The map moves the blob to b.
    f.owners.replace(vec![owner("b", b)]);
    tx.send(message(2, false)).await.unwrap();
    // Were the upload to go on, it would end here, and the assertion below say so.
    let _ = tx.send(message(3, true)).await;
    let e = call.await.unwrap().unwrap_err();
    assert_eq!(e.code(), Code::Unavailable);
    assert!(e.message().contains("changed from a"), "{}", e.message());
    assert_eq!(
        second.calls.load(SeqCst),
        0,
        "the frontend does not retry a write"
    );
}

/// Catches: a QueryWriteStatus that asks a replica when the primary is down (it reports
/// what a replica holds), one that is routed by a name it was not given, and the answer
/// of the primary not passed through with its bytes counted.
#[tokio::test(flavor = "multi_thread")]
async fn a_status_query_asks_the_primary_only() {
    let (first, second) = (Fake::new(false), Fake::new(false));
    let owners = vec![
        owner("a", node(&first).await),
        owner("b", node(&second).await),
    ];
    let f = front(owners, allow(), None).await;
    let name = upload("main", '0', 7);
    let request = QueryWriteStatusRequest {
        resource_name: name.clone(),
    };
    let got = client(&f)
        .query_write_status(request.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!((got.committed_size, got.complete), (7, true));
    assert_eq!((first.names(), second.calls.load(SeqCst)), (vec![name], 0));
    let meter = f.proxy.meter();
    assert_eq!(
        meter.bytes(Method::QueryWriteStatus, Direction::ToStorage),
        request.encoded_len() as u64 + PREFIX
    );
    assert_eq!(
        meter.bytes(Method::QueryWriteStatus, Direction::FromStorage),
        got.encoded_len() as u64 + PREFIX
    );

    let f = front(vec![down(), owner("b", node(&second).await)], allow(), None).await;
    let e = client(&f)
        .query_write_status(request.clone())
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::Unavailable);
    assert_eq!(second.calls.load(SeqCst), 0);
    // No node at all: a Write and a status query are UNAVAILABLE too.
    let f = front(vec![], allow(), None).await;
    assert_eq!(
        client(&f)
            .query_write_status(request)
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    let sent = upload_stream(upload("main", '0', 3), 1, Arc::default());
    assert_eq!(
        client(&f)
            .write(Request::new(sent))
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
}

// ---------------------------------------------------------------------------------
// What a pass-through must keep: headers, deadlines, statuses, cancellation.

/// Catches: headers dropped on the way to the node (the request id and credentials a
/// build is traced and authorized by, a binary header, the deadline), and a status of the
/// node changed on the way back, or an unreachable node answered as anything but
/// UNAVAILABLE.
#[tokio::test(flavor = "multi_thread")]
async fn headers_deadlines_and_statuses_pass_unchanged() {
    let fake = Fake::new(false);
    let f = front(vec![owner("a", node(&fake).await)], allow(), None).await;
    let mut request = Request::new(read_req(&blob("main", '0', 3)));
    request
        .metadata_mut()
        .insert("x-request-id", "req-42".parse().unwrap());
    request
        .metadata_mut()
        .insert("authorization", "Bearer t".parse().unwrap());
    request.metadata_mut().insert_bin(
        "x-request-meta-bin",
        tonic::metadata::MetadataValue::from_bytes(b"\x01\x02"),
    );
    request.set_timeout(Duration::from_secs(30));
    let mut s = client(&f).read(request).await.unwrap().into_inner();
    s.message().await.unwrap();
    let md = fake.seen.lock().unwrap()[0].metadata.clone();
    assert_eq!(md.get("x-request-id").unwrap(), "req-42");
    assert_eq!(md.get("authorization").unwrap(), "Bearer t");
    assert!(md.get_bin("x-request-meta-bin").is_some());
    assert!(md.get("grpc-timeout").is_some(), "deadline not propagated");

    let e = read_all(&mut client(&f), &blob("main", '1', 3), None)
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.message()), (Code::NotFound, "no such blob"));
    let f = front(vec![down()], allow(), None).await;
    let e = read_all(&mut client(&f), &blob("main", '0', 3), None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::Unavailable);
    let sent = upload_stream(upload("main", '0', 3), 1, Arc::default());
    assert_eq!(
        client(&f)
            .write(Request::new(sent))
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
}

/// Catches: a client that leaves mid-Read not stopping the node's producer (the node
/// would read the rest of the blob for nobody), because the cancellation is not passed.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_leaves_stops_the_node() {
    let fake = Fake::new(false);
    let f = front(vec![owner("a", node(&fake).await)], allow(), None).await;
    let mut s = client(&f)
        .read(read_req(&blob("main", '0', SIZE)))
        .await
        .unwrap()
        .into_inner();
    s.message().await.unwrap();
    s.message().await.unwrap();
    drop(s);
    assert!(
        eventually(|| fake.dropped.load(SeqCst)).await,
        "the node kept producing"
    );
    assert!(fake.lead.produced.load(SeqCst) < SIZE);
    assert!(eventually(|| f.proxy.meter().held() == 0).await);
}

// ---------------------------------------------------------------------------------
// The tonic level: batch CAS calls and the action cache, over a real storage service.

mod tonic_level {
    use super::*;
    use kbf_front::{Cache, CacheStorage, SegmentsConfig};
    use kbf_proto::reapi::action_cache_client::ActionCacheClient;
    use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
    use kbf_proto::reapi::{
        ActionResult, BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest as Wire,
        FindMissingBlobsRequest, GetActionResultRequest, GetChunkMappingRequest, GetTreeRequest,
        RegisterChunkMappingRequest, SpliceBlobRequest, SplitBlobRequest,
        UpdateActionResultRequest, batch_update_blobs_request,
    };
    use kbf_storage::service::{Admission, Config, StorageService};
    use tonic::service::Routes;

    /// A real storage service (kbf-front's cache as the store) on a loopback port.
    async fn storage() -> (Arc<dyn kbf_storage::Storage>, SocketAddr) {
        let cache = Arc::new(Cache::memory());
        let store: Arc<dyn kbf_storage::Storage> =
            Arc::new(CacheStorage::new(cache, SegmentsConfig::DEFAULT));
        let routes: Routes = StorageService::new(
            Arc::clone(&store),
            Admission::Open,
            Config {
                write_bytes_in_flight: 64 << 20,
            },
        )
        .routes();
        let incoming =
            tonic::transport::server::TcpIncoming::bind(([127, 0, 0, 1], 0).into()).unwrap();
        let addr = incoming.local_addr().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_routes(routes)
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });
        (store, addr)
    }

    fn wire(bytes: &[u8]) -> Wire {
        kbf_front::digest_to_proto(&kbf_segments::sha256(bytes))
    }

    fn cas(f: &harness::Front) -> ContentAddressableStorageClient<Channel> {
        ContentAddressableStorageClient::new(f.channel.clone())
    }

    fn item(digest: Wire, data: &[u8], compressor: i32) -> batch_update_blobs_request::Request {
        batch_update_blobs_request::Request {
            digest: Some(digest),
            data: data.to_vec(),
            compressor,
        }
    }

    /// Catches: a batch answered as a whole where each blob answers alone (one bad blob
    /// failing its neighbours), a blob stored under a digest it does not have, a
    /// compressed or malformed one stored, FindMissing answers out of the order asked or
    /// with a held digest, a read whose NOT_FOUND or INVALID_ARGUMENT is not its own, a
    /// batch over the total limit accepted, and a second owner not tried when the first
    /// is down.
    #[tokio::test(flavor = "multi_thread")]
    async fn batches_answer_item_by_item_and_land_in_storage() {
        let (store, addr) = storage().await;
        let f = front(vec![down(), owner("real", addr)], allow(), None).await;
        let mut c = cas(&f);
        let (good, other) = (b"a good blob".as_slice(), b"another".as_slice());
        let malformed = Wire {
            hash: "zz".into(),
            size_bytes: 1,
        };
        let items = vec![
            item(wire(good), good, 0),
            item(wire(other), good, 0),
            item(wire(good), good, 1),
            item(malformed.clone(), good, 0),
        ];
        // The first owner is down: a write goes to the primary, which is unreachable.
        let updated = c
            .batch_update_blobs(BatchUpdateBlobsRequest {
                requests: items.clone(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        let codes: Vec<i32> = updated
            .responses
            .iter()
            .map(|r| r.status.as_ref().unwrap().code)
            .collect();
        let (unavailable, invalid) = (Code::Unavailable as i32, Code::InvalidArgument as i32);
        assert_eq!(codes, [unavailable, invalid, invalid, invalid]);

        // With the real node first, the same batch stores the good blob.
        let f = front(vec![owner("real", addr), down()], allow(), None).await;
        let mut c = cas(&f);
        let updated = c
            .batch_update_blobs(BatchUpdateBlobsRequest {
                requests: items,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        let codes: Vec<i32> = updated
            .responses
            .iter()
            .map(|r| r.status.as_ref().unwrap().code)
            .collect();
        assert_eq!(codes, [0, invalid, invalid, invalid]);
        assert_eq!(updated.responses[3].digest.as_ref().unwrap().hash, "zz");
        let held = kbf_segments::sha256(good);
        assert!(
            store.find_missing(&[held]).await.unwrap().is_empty(),
            "stored on the node"
        );
        let absent = [wire(b"absent x"), wire(good), wire(b"absent y"), wire(good)];
        let asked = FindMissingBlobsRequest {
            blob_digests: absent.to_vec(),
            ..Default::default()
        };
        let missing = c
            .find_missing_blobs(asked)
            .await
            .unwrap()
            .into_inner()
            .missing_blob_digests;
        assert_eq!(missing, [absent[0].clone(), absent[2].clone()]);
        let bad = FindMissingBlobsRequest {
            blob_digests: vec![malformed.clone()],
            ..Default::default()
        };
        assert_eq!(
            c.find_missing_blobs(bad).await.unwrap_err().code(),
            Code::InvalidArgument
        );
        let sha1 = FindMissingBlobsRequest {
            digest_function: 2,
            ..Default::default()
        };
        assert_eq!(
            c.find_missing_blobs(sha1).await.unwrap_err().code(),
            Code::InvalidArgument
        );

        // Reads: the first owner is down, the second answers; each blob alone.
        let f = front(vec![down(), owner("real", addr)], allow(), None).await;
        let mut c = cas(&f);
        let asked = BatchReadBlobsRequest {
            digests: vec![wire(good), wire(b"absent"), malformed],
            ..Default::default()
        };
        let read = c
            .batch_read_blobs(asked)
            .await
            .unwrap()
            .into_inner()
            .responses;
        let answers: Vec<(i32, usize)> = read
            .iter()
            .map(|r| (r.status.as_ref().unwrap().code, r.data.len()))
            .collect();
        assert_eq!(
            answers,
            [(0, good.len()), (Code::NotFound as i32, 0), (invalid, 0)]
        );
        assert_eq!(read[2].digest.as_ref().unwrap().hash, "zz");

        // Over the limit: the whole call is refused, nothing stored or read.
        let big = BatchReadBlobsRequest {
            digests: vec![Wire {
                hash: "a".repeat(64),
                size_bytes: kbf_front::MAX_BATCH_TOTAL_BYTES as i64 + 1,
            }],
            ..Default::default()
        };
        assert_eq!(
            c.batch_read_blobs(big).await.unwrap_err().code(),
            Code::InvalidArgument
        );
        let big = vec![0u8; kbf_front::MAX_BATCH_TOTAL_BYTES + 1];
        let update = BatchUpdateBlobsRequest {
            requests: vec![item(wire(&big), &big, 0)],
            ..Default::default()
        };
        assert_eq!(
            c.batch_update_blobs(update).await.unwrap_err().code(),
            Code::InvalidArgument
        );
        let sha1 = BatchUpdateBlobsRequest {
            digest_function: 2,
            ..Default::default()
        };
        assert_eq!(
            c.batch_update_blobs(sha1).await.unwrap_err().code(),
            Code::InvalidArgument
        );
        let sha1 = BatchReadBlobsRequest {
            digest_function: 2,
            ..Default::default()
        };
        assert_eq!(
            c.batch_read_blobs(sha1).await.unwrap_err().code(),
            Code::InvalidArgument
        );
    }

    /// Catches: a farm with no node answering a question (FindMissing must never answer
    /// for digests nobody holds), and a read or a lookup that fails as anything but
    /// UNAVAILABLE.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_node_is_unavailable_never_an_answer() {
        let f = front(vec![], allow(), None).await;
        let mut c = cas(&f);
        let digests = vec![wire(b"x")];
        let ask = FindMissingBlobsRequest {
            blob_digests: digests.clone(),
            ..Default::default()
        };
        assert_eq!(
            c.find_missing_blobs(ask).await.unwrap_err().code(),
            Code::Unavailable
        );
        let read = BatchReadBlobsRequest {
            digests,
            ..Default::default()
        };
        let read = c.batch_read_blobs(read).await.unwrap().into_inner();
        assert_eq!(
            read.responses[0].status.as_ref().unwrap().code,
            Code::Unavailable as i32
        );
        let lookup = GetActionResultRequest {
            action_digest: Some(wire(b"a")),
            ..Default::default()
        };
        let mut ac = ActionCacheClient::new(f.channel.clone());
        assert_eq!(
            ac.get_action_result(lookup).await.unwrap_err().code(),
            Code::Unavailable
        );
    }

    /// Catches: an action-cache hit that is not the node's (a lookup answered from this
    /// process), a miss that is not NOT_FOUND, a client write accepted, and a digest
    /// function other than SHA-256 accepted.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_action_cache_is_the_nodes() {
        let (store, addr) = storage().await;
        let result = ActionResult {
            exit_code: 3,
            stdout_raw: b"out".to_vec(),
            ..Default::default()
        };
        let record = store.prepare_action_result(&result).await.unwrap();
        let action = kbf_segments::sha256(b"an action");
        store
            .commit_action_record(kbf_meta::Role::Daemon, action, record)
            .await
            .unwrap();
        let f = front(vec![owner("real", addr)], allow(), None).await;
        let mut ac = ActionCacheClient::new(f.channel.clone());
        let get = |digest: Wire| GetActionResultRequest {
            action_digest: Some(digest),
            ..Default::default()
        };
        let hit = ac
            .get_action_result(get(kbf_front::digest_to_proto(&action)))
            .await
            .unwrap();
        assert_eq!(
            (hit.get_ref().exit_code, &hit.get_ref().stdout_raw[..]),
            (3, &b"out"[..])
        );
        let miss = ac
            .get_action_result(get(wire(b"unknown")))
            .await
            .unwrap_err();
        assert_eq!(miss.code(), Code::NotFound);
        let write = UpdateActionResultRequest {
            action_digest: Some(wire(b"x")),
            ..Default::default()
        };
        assert_eq!(
            ac.update_action_result(write).await.unwrap_err().code(),
            Code::PermissionDenied
        );
        let sha1 = GetActionResultRequest {
            digest_function: 2,
            ..get(wire(b"x"))
        };
        assert_eq!(
            ac.get_action_result(sha1).await.unwrap_err().code(),
            Code::InvalidArgument
        );
    }

    /// Catches, on the tonic path: a call served for an instance its authorizer refuses
    /// (each method asks the authorizer of its kind, of its request's instance name, as
    /// the cache's services do), a refusal that comes after the work, an authorizer that
    /// does not see the authentication layer's caller, and the calls the proxy does not
    /// serve answering anything but UNIMPLEMENTED once authorized.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_tonic_path_asks_the_authorizer_of_the_instance() {
        let (store, addr) = storage().await;
        let f = front(
            vec![owner("real", addr)],
            only("alice", "main"),
            Some(caller("alice")),
        )
        .await;
        let mut c = cas(&f);
        let mut ac = ActionCacheClient::new(f.channel.clone());
        let denied = |e: Status| (e.code(), e.message().starts_with("Authorization: "));
        let refused = (Code::PermissionDenied, true);
        let upd = |instance: &str| BatchUpdateBlobsRequest {
            instance_name: instance.into(),
            requests: vec![item(wire(b"x"), b"x", 0)],
            ..Default::default()
        };
        assert!(c.batch_update_blobs(upd("main")).await.is_ok());
        assert_eq!(
            denied(c.batch_update_blobs(upd("other")).await.unwrap_err()),
            refused
        );
        assert!(
            store
                .find_missing(&[kbf_segments::sha256(b"x")])
                .await
                .unwrap()
                .is_empty()
        );
        let find = |instance: &str| FindMissingBlobsRequest {
            instance_name: instance.into(),
            ..Default::default()
        };
        assert!(c.find_missing_blobs(find("main/ci")).await.is_ok());
        assert_eq!(
            denied(c.find_missing_blobs(find("mainly")).await.unwrap_err()),
            refused
        );
        let read = |instance: &str| BatchReadBlobsRequest {
            instance_name: instance.into(),
            ..Default::default()
        };
        assert!(c.batch_read_blobs(read("main")).await.is_ok());
        assert_eq!(
            denied(c.batch_read_blobs(read("other")).await.unwrap_err()),
            refused
        );
        let get = |instance: &str| GetActionResultRequest {
            instance_name: instance.into(),
            action_digest: Some(wire(b"a")),
            ..Default::default()
        };
        assert_eq!(
            ac.get_action_result(get("main")).await.unwrap_err().code(),
            Code::NotFound
        );
        assert_eq!(
            denied(ac.get_action_result(get("other")).await.unwrap_err()),
            refused
        );

        // The calls not served: UNIMPLEMENTED for alice on main, refused elsewhere.
        for instance in ["main", "other"] {
            let want = if instance == "main" {
                Code::Unimplemented
            } else {
                Code::PermissionDenied
            };
            let n = instance.to_owned();
            let tree = GetTreeRequest {
                instance_name: n.clone(),
                ..Default::default()
            };
            assert_eq!(
                c.get_tree(tree).await.unwrap_err().code(),
                want,
                "GetTree {instance}"
            );
            let split = SplitBlobRequest {
                instance_name: n.clone(),
                ..Default::default()
            };
            assert_eq!(
                c.split_blob(split).await.unwrap_err().code(),
                want,
                "SplitBlob {instance}"
            );
            let mapping = GetChunkMappingRequest {
                instance_name: n.clone(),
                ..Default::default()
            };
            assert_eq!(
                c.get_chunk_mapping(mapping).await.unwrap_err().code(),
                want,
                "GetChunkMapping {instance}"
            );
            let splice = SpliceBlobRequest {
                instance_name: n.clone(),
                ..Default::default()
            };
            assert_eq!(
                c.splice_blob(splice).await.unwrap_err().code(),
                want,
                "SpliceBlob {instance}"
            );
            let register = RegisterChunkMappingRequest {
                instance_name: n,
                ..Default::default()
            };
            let sent = stream::iter([register]);
            let e = c
                .register_chunk_mapping(Request::new(sent))
                .await
                .unwrap_err();
            assert_eq!(e.code(), want, "RegisterChunkMapping {instance}");
        }
        let empty = stream::iter(Vec::<RegisterChunkMappingRequest>::new());
        let e = c
            .register_chunk_mapping(Request::new(empty))
            .await
            .unwrap_err();
        assert_eq!(
            e.code(),
            Code::PermissionDenied,
            "no instance named is the empty instance"
        );
    }
}
