//! What the service tests run over: an in-memory [`Storage`] that can be told to fail,
//! to stall and to count, the `Segments` it hands out, a loopback server, and a server
//! that answers what a correct one never would.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use kbf_meta::{ActionRecord, ActionWriteError, Closure, Role};
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_proto::reapi::{self, ActionResult};
use kbf_proto::storage::action_store_server::{ActionStore, ActionStoreServer};
use kbf_proto::storage::{
    CommitRequest, CommitResponse, Entry, GetRequest, GetResponse, PrepareRequest, PrepareResponse,
    Record, prepare_response,
};
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest as SegmentsCommit,
    CommitResponse as SegmentsCommitResponse, LocateRequest, LocateResponse, PutRangeRequest,
    PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use kbf_types::Digest;
use tonic::transport::Channel;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

use crate::replicate::MapSource;
use crate::service::{Admission, Config, MAP_GENERATION_HEADER, RemoteStorage, StorageService};
use crate::{Answer, PutStream, READ_CHUNK_BYTES, ReadStream, Storage, StorageError};

/// The digest of `bytes`.
pub(crate) fn sha(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

/// `len` bytes that differ from place to place, so a lost, repeated or reordered chunk
/// shows.
pub(crate) fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

/// An in-memory store.
#[derive(Default)]
pub(crate) struct Mem {
    pub blobs: Arc<Mutex<BTreeMap<Digest, Bytes>>>,
    pub actions: Mutex<BTreeMap<Digest, (Digest, ActionResult)>>,
    /// Every call fails with this.
    pub fail: Mutex<Option<StorageError>>,
    /// A read fails with this after its first chunk.
    pub fail_reading: Mutex<Option<StorageError>>,
    /// A read never ends.
    pub endless: AtomicBool,
    /// How long a put takes.
    pub put_takes: Mutex<Duration>,
    pub puts: Arc<AtomicUsize>,
    /// Calls of `read_range`.
    pub reads: AtomicUsize,
    /// What a streamed write does ([`Mem::stream`]).
    pub knobs: Arc<Knobs>,
    /// Blobs stored by `put_streamed`.
    pub streamed: AtomicUsize,
    /// The most bytes `put_streamed` held at once.
    pub streamed_most_held: AtomicUsize,
    /// Notified for each piece `put_streamed` takes.
    pub piece_seen: tokio::sync::Notify,
    /// Notified when the pieces `put_streamed` takes end in an error.
    pub pieces_failed: tokio::sync::Notify,
    active: AtomicUsize,
    pub most_active: AtomicUsize,
    pub segments: Arc<Seen>,
    /// The handle the service asked for counted segment uploads with ([`Storage::segments_counted`]).
    pub counted_with: Mutex<Option<crate::service::OpenWrites>>,
}

impl Mem {
    fn failing(&self) -> Result<(), StorageError> {
        match self.fail.lock().expect("lock").clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Storage for Mem {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async move {
            self.knobs.asked.fetch_add(1, Ordering::SeqCst);
            if self.knobs.hang_find_missing.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            self.failing()?;
            let blobs = self.blobs.lock().expect("lock");
            Ok(digests
                .iter()
                .filter(|d| d.size_bytes != 0 && !blobs.contains_key(d))
                .copied()
                .collect())
        })
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async move {
            self.failing()?;
            let blobs = self.blobs.lock().expect("lock");
            if digest.size_bytes == 0 {
                return Ok(Bytes::new());
            }
            blobs
                .get(digest)
                .cloned()
                .ok_or(StorageError::NotFound(*digest))
        })
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.failing()?;
            if self.endless.load(Ordering::SeqCst) {
                return Ok(stream::repeat(Ok(Bytes::from_static(&[7; 1024]))).boxed());
            }
            if offset > digest.size_bytes {
                return Err(StorageError::OutOfRange {
                    digest: *digest,
                    offset,
                });
            }
            let blob = self.read_blob(digest).await?;
            let start = offset as usize;
            let end = match limit {
                0 => blob.len(),
                n => blob.len().min(start + n as usize),
            };
            let mut items: Vec<Result<Bytes, StorageError>> = (start..end)
                .step_by(READ_CHUNK_BYTES)
                .map(|at| Ok(blob.slice(at..end.min(at + READ_CHUNK_BYTES))))
                .collect();
            if let Some(e) = self.fail_reading.lock().expect("lock").clone() {
                items.truncate(1);
                items.push(Err(e));
            }
            Ok(stream::iter(items).boxed())
        })
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        Box::pin(async move {
            self.failing()?;
            self.puts.fetch_add(1, Ordering::SeqCst);
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_active.fetch_max(now, Ordering::SeqCst);
            let takes = *self.put_takes.lock().expect("lock");
            tokio::time::sleep(takes).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            let digest = sha(&bytes);
            self.blobs.lock().expect("lock").insert(digest, bytes);
            Ok(digest)
        })
    }

    /// A stream into this store when `knobs.enabled`, else `None` (the buffered path).
    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        assert!(
            !self.knobs.open_panics.load(Ordering::SeqCst),
            "stream panics"
        );
        self.knobs.enabled.load(Ordering::SeqCst).then(|| {
            self.knobs.begun.fetch_add(1, Ordering::SeqCst);
            Box::new(MemPut {
                blobs: Arc::clone(&self.blobs),
                puts: Arc::clone(&self.puts),
                knobs: Arc::clone(&self.knobs),
                size,
                data: Vec::new(),
            }) as Box<dyn PutStream>
        })
    }

    /// Keeps only a hasher and a count of what it holds: the blob itself is stored
    /// whole only so a test can read it back.
    fn put_streamed(
        &self,
        size: u64,
        mut bytes: BoxStream<'static, std::io::Result<Bytes>>,
        claim: BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        Box::pin(async move {
            self.failing()?;
            let mut whole = Vec::new();
            while let Some(piece) = bytes.next().await {
                let piece = piece.map_err(|e| {
                    self.pieces_failed.notify_one();
                    StorageError::Internal(e.to_string())
                })?;
                self.streamed_most_held
                    .fetch_max(piece.len(), Ordering::SeqCst);
                self.piece_seen.notify_one();
                whole.extend_from_slice(&piece);
            }
            let Some(claimed) = claim.await else {
                return Ok(None);
            };
            let actual = sha(&whole);
            if actual != claimed || whole.len() as u64 != size {
                return Err(StorageError::DigestMismatch { claimed, actual });
            }
            self.streamed.fetch_add(1, Ordering::SeqCst);
            self.blobs
                .lock()
                .expect("lock")
                .insert(actual, whole.into());
            Ok(Some(actual))
        })
    }

    fn segments(&self) -> Arc<dyn Segments> {
        Arc::clone(&self.segments) as Arc<dyn Segments>
    }

    fn segments_counted(&self, open: &crate::service::OpenWrites) -> Arc<dyn Segments> {
        *self.counted_with.lock().expect("lock") = Some(open.clone());
        self.segments()
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        Box::pin(async move {
            self.failing()?;
            Ok(self.actions.lock().expect("lock").get(action).cloned())
        })
    }

    /// The record of `result`: its stdout is the result blob, its exit code names how
    /// many blobs the closure holds; exit code 99 is a blob that is not held.
    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        Box::pin(async move {
            self.failing()?;
            if result.exit_code == 99 {
                return Err(ActionWriteError::Absent(vec![sha(b"missing")]).into());
            }
            if result.exit_code == 98 {
                return Err(ActionWriteError::Unreachable(vec![sha(b"unreadable")]).into());
            }
            Ok(ActionRecord {
                result: sha(&result.stdout_raw),
                closure: (0..result.exit_code)
                    .map(|i| sha(&[i as u8]))
                    .collect::<Closure>(),
            })
        })
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        Box::pin(async move {
            self.failing()?;
            // Open unless a test holds the commits (`Knobs::hold_commits`).
            let _permit = self.knobs.commit_gate.acquire().await;
            if role != Role::Daemon {
                return Err(ActionWriteError::NotDaemon.into());
            }
            let result = ActionResult {
                exit_code: record.closure.len() as i32,
                ..ActionResult::default()
            };
            self.actions
                .lock()
                .expect("lock")
                .insert(action, (record.result, result));
            Ok(())
        })
    }
}

/// What a streamed write of [`Mem`] does, and what it has seen.
pub(crate) struct Knobs {
    /// Whether [`Mem::stream`] offers a stream.
    pub enabled: AtomicBool,
    /// Streams opened so far.
    pub begun: AtomicUsize,
    /// `Mem::stream` panics, as a store's blocking call that fails would.
    pub open_panics: AtomicBool,
    /// Chunks and bytes appended so far, over every stream.
    pub chunks: AtomicUsize,
    pub bytes: AtomicU64,
    /// Appends wait while this is true.
    pub hold: (Mutex<bool>, Condvar),
    /// Commits wait for a permit: take them all to hold commits, add them back to release.
    pub commit_gate: tokio::sync::Semaphore,
    pub fault: Mutex<StreamFault>,
    /// `find_missing` calls that have arrived, answered or not.
    pub asked: AtomicUsize,
    /// `find_missing` never answers while this is true: a node that is up and not answering.
    pub hang_find_missing: AtomicBool,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            begun: AtomicUsize::new(0),
            open_panics: AtomicBool::new(false),
            chunks: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            hold: (Mutex::new(false), Condvar::new()),
            commit_gate: tokio::sync::Semaphore::new(tokio::sync::Semaphore::MAX_PERMITS),
            fault: Mutex::new(StreamFault::None),
            asked: AtomicUsize::new(0),
            hang_find_missing: AtomicBool::new(false),
        }
    }
}

impl Knobs {
    /// Holds (or releases) every append.
    pub(crate) fn hold_appends(&self, hold: bool) {
        *self.hold.0.lock().expect("lock") = hold;
        self.hold.1.notify_all();
    }

    /// Holds every commit.
    pub(crate) fn hold_commits(&self) {
        self.commit_gate
            .forget_permits(tokio::sync::Semaphore::MAX_PERMITS);
    }

    /// Lets every held commit through.
    pub(crate) fn release_commits(&self) {
        self.commit_gate.add_permits(
            tokio::sync::Semaphore::MAX_PERMITS - self.commit_gate.available_permits(),
        );
    }
}

/// Where a streamed write of [`Mem`] fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamFault {
    None,
    AppendFails,
    AppendPanics,
    FinishFails,
    FinishPanics,
    CommitFails,
}

/// A blob being written to [`Mem`] as it arrives.
struct MemPut {
    blobs: Arc<Mutex<BTreeMap<Digest, Bytes>>>,
    puts: Arc<AtomicUsize>,
    knobs: Arc<Knobs>,
    size: u64,
    data: Vec<u8>,
}

impl PutStream for MemPut {
    fn append(&mut self, chunk: &[u8]) -> Result<(), StorageError> {
        let (lock, wake) = &self.knobs.hold;
        let mut held = lock.lock().expect("lock");
        while *held {
            held = wake.wait(held).expect("wait");
        }
        drop(held);
        let fault = *self.knobs.fault.lock().expect("lock");
        match fault {
            StreamFault::AppendFails => return Err(StorageError::Internal("the disk".into())),
            StreamFault::AppendPanics => panic!("append panics"),
            _ => {}
        }
        self.knobs.chunks.fetch_add(1, Ordering::SeqCst);
        self.knobs
            .bytes
            .fetch_add(chunk.len() as u64, Ordering::SeqCst);
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(
        self: Box<Self>,
        digest: Digest,
        crc32c: u32,
    ) -> Result<Box<dyn crate::StreamedPut>, StorageError> {
        let fault = *self.knobs.fault.lock().expect("lock");
        match fault {
            StreamFault::FinishFails => return Err(StorageError::Internal("the disk".into())),
            StreamFault::FinishPanics => panic!("finish panics"),
            _ => {}
        }
        assert_eq!(
            self.data.len() as u64,
            self.size,
            "a stream of the wrong size"
        );
        assert_eq!(crc32c::crc32c(&self.data), crc32c, "a wrong CRC-32C");
        assert_eq!(
            sha(&self.data),
            digest,
            "finish was not given the blob's digest"
        );
        Ok(Box::new(MemStreamed {
            blobs: self.blobs,
            puts: self.puts,
            knobs: self.knobs,
            digest,
            data: Bytes::from(self.data),
        }))
    }
}

struct MemStreamed {
    blobs: Arc<Mutex<BTreeMap<Digest, Bytes>>>,
    puts: Arc<AtomicUsize>,
    knobs: Arc<Knobs>,
    digest: Digest,
    data: Bytes,
}

impl crate::StreamedPut for MemStreamed {
    fn commit(self: Box<Self>) -> Answer<'static, ()> {
        Box::pin(async move {
            let _permit = self.knobs.commit_gate.acquire().await.expect("open");
            if *self.knobs.fault.lock().expect("lock") == StreamFault::CommitFails {
                return Err(StorageError::Internal("the disk".into()));
            }
            self.blobs
                .lock()
                .expect("lock")
                .insert(self.digest, self.data);
            self.puts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

/// The `Segments` calls a service forwarded, and the map generation each carried.
#[derive(Default)]
pub(crate) struct Seen {
    pub calls: Mutex<Vec<(&'static str, Option<String>)>>,
}

impl Seen {
    fn note<T>(&self, call: &'static str, request: &Request<T>) {
        let generation = request
            .metadata()
            .get(MAP_GENERATION_HEADER)
            .map(|v| v.to_str().expect("ASCII").to_owned());
        self.calls.lock().expect("lock").push((call, generation));
    }
}

#[tonic::async_trait]
impl Segments for Seen {
    async fn begin(&self, r: Request<BeginRequest>) -> Result<Response<BeginResponse>, Status> {
        self.note("begin", &r);
        Ok(Response::new(BeginResponse { upload: 7 }))
    }

    async fn put_range(
        &self,
        r: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        self.note("put_range", &r);
        Ok(Response::new(PutRangeResponse {}))
    }

    async fn commit(
        &self,
        r: Request<SegmentsCommit>,
    ) -> Result<Response<SegmentsCommitResponse>, Status> {
        self.note("commit", &r);
        Ok(Response::new(SegmentsCommitResponse {
            committed: vec![reapi::Digest {
                hash: "ab".repeat(32),
                size_bytes: 3,
            }],
        }))
    }

    async fn locate(&self, r: Request<LocateRequest>) -> Result<Response<LocateResponse>, Status> {
        self.note("locate", &r);
        Ok(Response::new(LocateResponse::default()))
    }

    async fn read_range(
        &self,
        r: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        self.note("read_range", &r);
        Ok(Response::new(ReadRangeResponse { bytes: vec![1, 2] }))
    }
}

/// Serves `routes` on a loopback port, with `tls` if given.
pub(crate) async fn serve_routes(
    routes: tonic::service::Routes,
    tls: Option<tonic::transport::ServerTlsConfig>,
) -> SocketAddr {
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("address");
    let mut server = tonic::transport::Server::builder();
    if let Some(tls) = tls {
        server = server.tls_config(tls).expect("TLS config");
    }
    tokio::spawn(server.add_routes(routes).serve_with_incoming(incoming));
    addr
}

/// The calls a spied server received, in order: which service, and the map-generation
/// header the call carried.
pub(crate) type Spied = Arc<Mutex<Vec<(&'static str, Option<String>)>>>;

/// An interceptor that records `service` and the header of a call before its handler
/// runs, and lets it through.
fn spy(
    service: &'static str,
    seen: &Spied,
) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone + use<> {
    let seen = Arc::clone(seen);
    move |request: Request<()>| {
        let header = request
            .metadata()
            .get(MAP_GENERATION_HEADER)
            .map(|v| v.to_str().expect("ASCII").to_owned());
        seen.lock().expect("lock").push((service, header));
        Ok(request)
    }
}

/// The real service over `mem` on a loopback port, each of its four services behind an
/// interceptor that records what the call carried (the real handlers do not read the
/// map generation, so this is where a test sees that it arrived).
pub(crate) async fn serve_spied(mem: &Arc<Mem>) -> (Spied, Channel) {
    use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
    use kbf_proto::worker::segments_server::SegmentsServer;
    use tonic::service::interceptor::InterceptedService;
    let seen = Spied::default();
    let storage: Arc<dyn Storage> = Arc::clone(mem) as Arc<dyn Storage>;
    let config = Config {
        write_bytes_in_flight: 1 << 30,
    };
    let service = Arc::new(StorageService::new(storage, Admission::Open, config));
    let routes = tonic::service::Routes::new(InterceptedService::new(
        ContentAddressableStorageServer::from_arc(Arc::clone(&service)),
        spy("cas", &seen),
    ))
    .add_service(InterceptedService::new(
        ByteStreamServer::from_arc(Arc::clone(&service)),
        spy("bytestream", &seen),
    ))
    .add_service(InterceptedService::new(
        SegmentsServer::from_arc(Arc::clone(&service)),
        spy("segments", &seen),
    ))
    .add_service(InterceptedService::new(
        ActionStoreServer::from_arc(service),
        spy("actions", &seen),
    ));
    let addr = serve_routes(routes, None).await;
    (seen, connect(addr).await)
}

/// A service over `mem`, serving on a loopback port in plain text.
pub(crate) async fn serve(mem: &Arc<Mem>, write_bytes_in_flight: u32) -> Channel {
    let storage: Arc<dyn Storage> = Arc::clone(mem) as Arc<dyn Storage>;
    let service = StorageService::new(
        storage,
        Admission::Open,
        Config {
            write_bytes_in_flight,
        },
    );
    let addr = serve_routes(service.routes(), None).await;
    connect(addr).await
}

/// The map a node holds, which a test changes between calls: a one-member map in which
/// the node is member 1.
pub(crate) struct Held(Mutex<Option<Arc<kbf_map::Map>>>);

impl Held {
    /// A node holding a map of generation `generation` (a one-member map).
    pub(crate) fn at(generation: u64) -> Arc<Self> {
        let held = Arc::new(Self(Mutex::new(None)));
        held.set(generation);
        held
    }

    /// A node with no map yet.
    pub(crate) fn none() -> Arc<Self> {
        Arc::new(Self(Mutex::new(None)))
    }

    /// The node learns the map of generation `generation`.
    pub(crate) fn set(&self, generation: u64) {
        let member = kbf_map::Member {
            id: kbf_map::MemberId(1),
            addr: "127.0.0.1:1".to_owned(),
            zone: String::new(),
            weight: 1,
            state: kbf_map::MemberState::Active,
        };
        let map = kbf_map::Map::new(
            generation,
            vec![member],
            kbf_map::Rule::Rendezvous,
            1,
            1,
            Vec::new(),
        )
        .expect("a valid map");
        *self.0.lock().expect("lock") = Some(Arc::new(map));
    }
}

impl MapSource for Held {
    fn current(&self) -> Option<Arc<kbf_map::Map>> {
        self.0.lock().expect("lock").clone()
    }

    fn me(&self) -> Option<kbf_map::MemberId> {
        Some(kbf_map::MemberId(1))
    }
}

/// A service over `mem` that judges writes by `held`, on a loopback port.
pub(crate) async fn serve_mapped(mem: &Arc<Mem>, held: &Arc<Held>) -> Channel {
    let storage: Arc<dyn Storage> = Arc::clone(mem) as Arc<dyn Storage>;
    let service = StorageService::new(
        storage,
        Admission::Open,
        Config {
            write_bytes_in_flight: 1 << 30,
        },
    )
    .with_map(Arc::clone(held) as Arc<dyn MapSource>);
    connect(serve_routes(service.routes(), None).await).await
}

pub(crate) async fn connect(addr: SocketAddr) -> Channel {
    Channel::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect")
}

/// A plain-text service over a new [`Mem`], and a client of it tagging map generation 5.
pub(crate) async fn pair() -> (Arc<Mem>, RemoteStorage, Channel) {
    let mem = Arc::new(Mem::default());
    let channel = serve(&mem, 1 << 30).await;
    (mem, RemoteStorage::new(channel.clone(), 5), channel)
}

/// The generation a request carried, as a service that records it saw it.
#[derive(Default)]
pub(crate) struct Odd {
    /// The map generation of each action-store request, in the order asked.
    pub generations: Mutex<Vec<u64>>,
}

type Boxed<T> = BoxStream<'static, Result<T, Status>>;

#[tonic::async_trait]
impl ByteStream for Odd {
    type ReadStream = Boxed<ReadResponse>;

    /// Always three bytes, whatever was asked.
    async fn read(&self, _: Request<ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        let data = vec![ReadResponse {
            data: vec![1, 2, 3],
        }];
        Ok(Response::new(
            stream::iter(data.into_iter().map(Ok)).boxed(),
        ))
    }

    /// Acknowledges no bytes.
    async fn write(
        &self,
        _: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        Ok(Response::new(WriteResponse { committed_size: 0 }))
    }

    async fn query_write_status(
        &self,
        _: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("not asked"))
    }
}

#[tonic::async_trait]
impl ActionStore for Odd {
    /// An entry whose result digest is not a digest.
    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        self.generations
            .lock()
            .expect("lock")
            .push(request.into_inner().map_generation);
        Ok(Response::new(GetResponse {
            entry: Some(Entry {
                result_digest: Some(reapi::Digest {
                    hash: "nope".to_owned(),
                    size_bytes: 1,
                }),
                result: None,
            }),
        }))
    }

    /// No outcome, or when the result asks (exit code 1 or 2) a record whose result, or
    /// whose closure, is not a digest.
    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        let request = request.into_inner();
        self.generations
            .lock()
            .expect("lock")
            .push(request.map_generation);
        let good = Some(reapi::Digest {
            hash: "ab".repeat(32),
            size_bytes: 1,
        });
        let outcome = match request.result.map_or(0, |r| r.exit_code) {
            1 => Some(prepare_response::Outcome::Record(Record {
                result: None,
                closure: Vec::new(),
            })),
            2 => Some(prepare_response::Outcome::Record(Record {
                result: good,
                closure: vec![reapi::Digest::default()],
            })),
            _ => None,
        };
        Ok(Response::new(PrepareResponse { outcome }))
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        self.generations
            .lock()
            .expect("lock")
            .push(request.into_inner().map_generation);
        Ok(Response::new(CommitResponse { refused: None }))
    }
}

/// [`Odd`] on a loopback port, and what it saw.
pub(crate) async fn serve_odd() -> (Arc<Odd>, Channel) {
    let odd = Arc::new(Odd::default());
    let routes = tonic::service::Routes::new(ByteStreamServer::from_arc(Arc::clone(&odd)))
        .add_service(ActionStoreServer::from_arc(Arc::clone(&odd)));
    let addr = serve_routes(routes, None).await;
    (odd, connect(addr).await)
}
