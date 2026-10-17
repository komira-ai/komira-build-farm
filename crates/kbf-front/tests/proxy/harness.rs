//! A fake storage node that streams, the rigs that put a proxy in front of it, and the
//! counters that tell streaming from buffering by bytes, never by timing.
//!
//! The bounds are upper bounds on a safety property ("the proxy never holds more than
//! N bytes"), so a slow machine cannot fail them; a fast one cannot hide a buffering
//! proxy, because the source runs ahead during the consumer's sleeps.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use http_body::Frame;
use kbf_auth::{AuthenticateLayer, Authenticator, Authorizers};
use kbf_front::{Channels, Owner, Owners, Proxy, Routed};
use kbf_map::{Map, Member, MemberId};
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_route::{MapSource, MapView, Nodes, RefetchError};
use kbf_storage::service::RemoteStorage;
use kbf_storage::{Storage, StorageError};
use tonic::metadata::MetadataMap;
use tonic::service::Routes;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status, Streaming};

pub const CHUNK: usize = 1 << 20;
/// The most the whole path (both h2 hops, proxy, kernel) may hold ahead of the consumer:
/// 10 of 32 chunks. Measured peaks are 5 to 7.
pub const LEAD_BOUND: u64 = 10 * CHUNK as u64;
/// What the proxy itself may hold: one chunk, where a collecting proxy holds the blob.
pub const HELD_BOUND: u64 = CHUNK as u64;

pub fn pattern(i: usize) -> u8 {
    (i * 31 + 7) as u8
}

/// A hash whose first character picks what the fake node does with the blob (see
/// [`Fake::read`]).
pub fn hash(kind: char) -> String {
    format!("{kind}{}", "a".repeat(63))
}

/// A Read name on `instance`.
pub fn blob(instance: &str, kind: char, size: u64) -> String {
    format!("{instance}/blobs/{}/{size}", hash(kind))
}

/// A Write name on `instance`.
pub fn upload(instance: &str, kind: char, size: u64) -> String {
    format!("{instance}/uploads/u1/blobs/{}/{size}", hash(kind))
}

/// How far the producer is ahead of the consumer, end to end, in bytes: everything the
/// source made and the sink has not yet taken, wherever it sits (proxy, h2 windows,
/// kernel buffers).
#[derive(Debug, Default)]
pub struct Lead {
    pub produced: AtomicU64,
    pub consumed: AtomicU64,
    peak: AtomicU64,
}

impl Lead {
    pub fn sample(&self) {
        let p = self.produced.load(SeqCst);
        let c = self.consumed.load(SeqCst);
        self.peak.fetch_max(p.saturating_sub(c), SeqCst);
    }
    pub fn peak(&self) -> u64 {
        self.peak.load(SeqCst)
    }
}

/// One call that reached the fake node.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: &'static str,
    pub name: String,
    pub metadata: MetadataMap,
}

/// A storage node: generates a blob lazily and checks an uploaded one as it arrives,
/// holding neither. The blob's size is the last segment of its name. By the first
/// character of its hash:
/// - `1`: NOT_FOUND in the headers, no data.
/// - `2`: two chunks, then DATA_LOSS.
/// - `3`: the stream's first item is UNAVAILABLE (before any data).
/// - `4`: INVALID_ARGUMENT in the headers.
/// - `5`: an empty blob.
/// - anything else: the blob.
#[derive(Default)]
pub struct Fake {
    pub lead: Arc<Lead>,
    pub seen: Mutex<Vec<Seen>>,
    pub dropped: Arc<AtomicBool>,
    pub calls: AtomicUsize,
    /// Writes take a millisecond per message, as a slow disk does.
    pub slow: bool,
    /// Treats every Read as a blob of this kind, whatever its name says.
    pub force: Option<char>,
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, SeqCst);
    }
}

/// `(hash, size)` of the blob a name ends with.
fn tail(name: &str) -> (&str, u64) {
    let mut parts = name.rsplit('/');
    let size = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (parts.next().unwrap_or(""), size)
}

impl Fake {
    pub fn new(slow: bool) -> Arc<Self> {
        Arc::new(Self {
            slow,
            ..Self::default()
        })
    }

    /// A node that answers every Read as blob kind `kind`.
    pub fn forcing(kind: char) -> Arc<Self> {
        Arc::new(Self {
            force: Some(kind),
            ..Self::default()
        })
    }

    fn note(&self, method: &'static str, name: &str, metadata: &MetadataMap) {
        self.calls.fetch_add(1, SeqCst);
        self.seen.lock().unwrap().push(Seen {
            method,
            name: name.to_owned(),
            metadata: metadata.clone(),
        });
    }

    pub fn names(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }
}

#[tonic::async_trait]
impl ByteStream for Fake {
    type ReadStream = BoxStream<'static, Result<ReadResponse, Status>>;

    async fn read(&self, r: Request<ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        let metadata = r.metadata().clone();
        let name = r.into_inner().resource_name;
        self.note("Read", &name, &metadata);
        let (hash, size) = tail(&name);
        let kind = self.force.or_else(|| hash.chars().next()).unwrap_or('0');
        match kind {
            '1' => return Err(Status::not_found("no such blob")),
            '4' => return Err(Status::invalid_argument("not a blob")),
            '3' => {
                let once = stream::once(async { Err(Status::unavailable("going away")) });
                return Ok(Response::new(once.boxed()));
            }
            _ => {}
        }
        let size = if kind == '5' { 0 } else { size };
        let lead = Arc::clone(&self.lead);
        let guard = DropFlag(Arc::clone(&self.dropped));
        let chunks = size.div_ceil(CHUNK as u64) as usize;
        let s = stream::unfold((0usize, guard), move |(i, g)| {
            let lead = Arc::clone(&lead);
            async move {
                if kind == '2' && i == 2 {
                    return Some((Err(Status::data_loss("lost")), (usize::MAX, g)));
                }
                if i >= chunks {
                    return None;
                }
                let len = (size - (i * CHUNK) as u64).min(CHUNK as u64) as usize;
                lead.produced.fetch_add(len as u64, SeqCst);
                lead.sample();
                Some((
                    Ok(ReadResponse {
                        data: vec![pattern(i); len],
                    }),
                    (i + 1, g),
                ))
            }
        });
        Ok(Response::new(s.boxed()))
    }

    async fn write(
        &self,
        r: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let metadata = r.metadata().clone();
        let mut s = r.into_inner();
        let (mut i, mut total) = (0usize, 0i64);
        while let Some(m) = s.message().await? {
            if i == 0 {
                self.note("Write", &m.resource_name, &metadata);
            }
            // The first, middle and last byte of the message stand for all of it.
            let at = [0, m.data.len() / 2, m.data.len().saturating_sub(1)];
            assert!(
                at.iter()
                    .all(|a| m.data.get(*a).is_none_or(|b| *b == pattern(i))),
                "chunk {i} corrupted"
            );
            total += m.data.len() as i64;
            self.lead.consumed.fetch_add(m.data.len() as u64, SeqCst);
            self.lead.sample();
            if self.slow {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            i += 1;
            if m.finish_write {
                return Ok(Response::new(WriteResponse {
                    committed_size: total,
                }));
            }
        }
        Err(Status::invalid_argument("no finish_write"))
    }

    async fn query_write_status(
        &self,
        r: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        let metadata = r.metadata().clone();
        let name = r.into_inner().resource_name;
        self.note("QueryWriteStatus", &name, &metadata);
        let (_, size) = tail(&name);
        Ok(Response::new(QueryWriteStatusResponse {
            committed_size: size as i64,
            complete: true,
        }))
    }
}

/// Serves `routes` on a loopback port and connects to it.
pub async fn serve(routes: Routes) -> Channel {
    let incoming = TcpIncoming::bind(([127, 0, 0, 1], 0).into()).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    tokio::spawn(async move {
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming)
            .await
            .expect("serve");
    });
    Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .expect("connect")
}

/// A fake node on a loopback port.
pub async fn node(fake: &Arc<Fake>) -> SocketAddr {
    let incoming = TcpIncoming::bind(([127, 0, 0, 1], 0).into()).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    let routes = Routes::new(ByteStreamServer::from_arc(Arc::clone(fake)));
    tokio::spawn(async move {
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming)
            .await
            .expect("serve");
    });
    addr
}

pub fn channel(addr: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_lazy()
}

pub fn owner(id: &str, addr: SocketAddr) -> Owner {
    Owner::new(id, channel(addr))
}

/// An owner nobody listens at.
pub fn down() -> Owner {
    owner("down", ([127, 0, 0, 1], 1).into())
}

/// A proxy to `owners`, served as kbf-server serves its REAPI listener (the
/// authentication layer around the routes, when there is an authenticator); and a
/// connection to it.
pub struct Front {
    pub channel: Channel,
    pub proxy: Proxy,
    pub owners: Owners,
}

pub async fn front(
    owners: Vec<Owner>,
    authorizers: Authorizers,
    authenticator: Option<Arc<dyn Authenticator>>,
) -> Front {
    front_over(Owners::ranked(owners), authorizers, authenticator).await
}

/// [`front`] to these `owners`, however they route.
pub async fn front_over(
    owners: Owners,
    authorizers: Authorizers,
    authenticator: Option<Arc<dyn Authenticator>>,
) -> Front {
    let proxy = Proxy::new(owners.clone(), Arc::new(authorizers));
    let routes = match authenticator {
        Some(a) => Routes::from(
            proxy
                .routes()
                .into_axum_router()
                .layer(AuthenticateLayer::new(a)),
        ),
        None => proxy.routes(),
    };
    Front {
        channel: serve(routes).await,
        proxy,
        owners,
    }
}

/// Waits for `ready`, up to five seconds.
pub async fn eventually(mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ready()
}

/// A body of fixed frames, as the control proxy replays what it collected.
pub struct Replay(pub std::collections::VecDeque<Frame<Bytes>>);

impl http_body::Body for Replay {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, Status>>> {
        std::task::Poll::Ready(self.0.pop_front().map(Ok))
    }
}

/// The negative control: a raw ByteStream proxy to `node` that holds every frame of a
/// request, or of a response, before it passes any. What the bounds rule out.
pub fn collecting(node: SocketAddr) -> Routes {
    use std::future::poll_fn;
    use std::pin::Pin;
    use tonic::body::Body;
    use tonic::codegen::{Service, http};

    #[derive(Clone)]
    struct Collecting(Channel);

    impl tonic::server::NamedService for Collecting {
        const NAME: &'static str = "google.bytestream.ByteStream";
    }

    async fn all(mut body: Body) -> std::collections::VecDeque<Frame<Bytes>> {
        let mut frames = std::collections::VecDeque::new();
        while let Some(f) = poll_fn(|cx| http_body::Body::poll_frame(Pin::new(&mut body), cx)).await
        {
            frames.push_back(f.unwrap());
        }
        frames
    }

    impl Service<http::Request<Body>> for Collecting {
        type Response = http::Response<Body>;
        type Error = std::convert::Infallible;
        type Future =
            Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<Body>) -> Self::Future {
            let mut storage = self.0.clone();
            Box::pin(async move {
                let (parts, body) = req.into_parts();
                let held = Body::new(Replay(all(body).await));
                poll_fn(|cx| storage.poll_ready(cx)).await.unwrap();
                let resp = storage
                    .call(http::Request::from_parts(parts, held))
                    .await
                    .unwrap();
                let (head, body) = resp.into_parts();
                let held = Body::new(Replay(all(Body::new(body)).await));
                Ok(http::Response::from_parts(head, held))
            })
        }
    }

    Routes::new(Collecting(channel(node)))
}

/// A map the frontend holds that never changes.
struct Held(Arc<Map>);

impl MapSource for Held {
    fn current(&self) -> Option<MapView> {
        Some(MapView {
            map: Arc::clone(&self.0),
            previous: None,
        })
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            Err(RefetchError {
                at_least,
                reason: "nothing newer".to_owned(),
            })
        })
    }
}

/// The plain-text connection to each member's fake node.
struct Wires(BTreeMap<MemberId, Channel>);

impl Wires {
    fn to(&self, member: &Member) -> Result<Channel, StorageError> {
        self.0
            .get(&member.id)
            .cloned()
            .ok_or_else(|| StorageError::Internal(format!("no node {}", member.id)))
    }
}

impl Nodes for Wires {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        Ok(Arc::new(RemoteStorage::new(self.to(member)?, generation)))
    }
}

impl Channels for Wires {
    fn channel(&self, member: &Member) -> Result<Channel, StorageError> {
        self.to(member)
    }
}

/// Owners routed by `map`, whose members are the fake nodes at `addrs` (member 1 first),
/// and whose `--storage` node is nobody.
pub fn routed_owners(map: &Arc<Map>, addrs: &[SocketAddr]) -> Owners {
    routed_owners_over(Arc::new(Held(Arc::clone(map))), addrs)
}

/// [`routed_owners`] by a map source of the test's.
pub fn routed_owners_over(maps: Arc<dyn MapSource>, addrs: &[SocketAddr]) -> Owners {
    let wires = Arc::new(Wires(
        addrs
            .iter()
            .zip(1..)
            .map(|(addr, id)| (MemberId(id), channel(*addr)))
            .collect(),
    ));
    Owners::routed(Arc::new(Routed::new(maps, wires.clone(), wires)), down())
}

/// A map that is there for one look and gone for the next, and so on: a frontend whose
/// map is lost between deciding to route and routing.
pub struct Flicker {
    pub map: Arc<Map>,
    pub looks: AtomicUsize,
}

impl MapSource for Flicker {
    fn current(&self) -> Option<MapView> {
        let look = self.looks.fetch_add(1, SeqCst);
        look.is_multiple_of(2).then(|| MapView {
            map: Arc::clone(&self.map),
            previous: None,
        })
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            Err(RefetchError {
                at_least,
                reason: "nothing newer".to_owned(),
            })
        })
    }
}
