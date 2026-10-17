//! A storage node that serves what it is told to, whatever was asked, and counts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse, LocateRequest, LocateResponse,
    PutRangeRequest, PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use kbf_route::{MapSource, MapView, Nodes, RefetchError, Router};
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_transfer::{RoutedCas, TransferConfig};
use kbf_types::Digest;
use tonic::{Request, Response, Status};

/// A directory these tests may make files in.
pub fn scratch() -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("routed-unit");
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

pub fn sha(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

pub fn unavailable() -> StorageError {
    StorageError::Unavailable(Arc::new(std::fmt::Error))
}

/// A node that answers every read with `data`, in chunks of 4 bytes.
pub struct Fake {
    data: Mutex<Bytes>,
    /// Blobs it serves under their own digests, whatever `data` is.
    blobs: Mutex<BTreeMap<Digest, Bytes>>,
    /// Whether it holds anything: if not, reads are NOT_FOUND.
    pub holds: AtomicBool,
    pub down: AtomicBool,
    /// Every read passes one chunk and then fails.
    pub breaks: AtomicBool,
    /// Digests FindMissing finds held.
    held: Mutex<BTreeSet<Digest>>,
    /// The next FindMissing calls fail with these.
    find_failures: Mutex<VecDeque<StorageError>>,
    pub finds: Mutex<Vec<Vec<Digest>>>,
    active: AtomicUsize,
    peak: AtomicUsize,
    /// Reads wait for permits on this.
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Reads pass this many chunks, then wait for a permit on this before the rest.
    pause: Mutex<Option<(usize, Arc<tokio::sync::Semaphore>)>>,
    /// How many reads were asked of it.
    pub reads: AtomicUsize,
}

impl Fake {
    pub fn new(data: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(Bytes::copy_from_slice(data)),
            blobs: Mutex::default(),
            holds: AtomicBool::new(true),
            down: AtomicBool::new(false),
            breaks: AtomicBool::new(false),
            held: Mutex::default(),
            find_failures: Mutex::default(),
            finds: Mutex::default(),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            gate: Mutex::default(),
            pause: Mutex::default(),
            reads: AtomicUsize::new(0),
        })
    }

    /// Makes every read pass `after` chunks and then wait for a permit on what is
    /// returned before it sends the rest.
    pub fn pause_after(&self, after: usize) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *self.pause.lock().expect("lock") = Some((after, Arc::clone(&gate)));
        gate
    }

    /// Answers every read with `data` from now on.
    pub fn set_data(&self, data: &[u8]) {
        *self.data.lock().expect("lock") = Bytes::copy_from_slice(data);
    }

    /// Serves `blob` to reads of its digest, as the node holding it would.
    pub fn serve(&self, blob: &[u8]) {
        self.blobs
            .lock()
            .expect("lock")
            .insert(sha(blob), Bytes::copy_from_slice(blob));
    }

    pub fn hold_digests(&self, digests: &[Digest]) {
        self.held
            .lock()
            .expect("lock")
            .extend(digests.iter().copied());
    }

    pub fn fail_next_find(&self, error: StorageError) {
        self.find_failures.lock().expect("lock").push_back(error);
    }

    pub fn gate(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *self.gate.lock().expect("lock") = Some(Arc::clone(&gate));
        gate
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    pub fn find_log(&self) -> Vec<Vec<Digest>> {
        self.finds.lock().expect("lock").clone()
    }
}

impl Storage for Fake {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async move {
            self.finds.lock().expect("lock").push(digests.to_vec());
            if let Some(error) = self.find_failures.lock().expect("lock").pop_front() {
                return Err(error);
            }
            let held = self.held.lock().expect("lock");
            Ok(digests
                .iter()
                .filter(|d| !held.contains(d))
                .copied()
                .collect())
        })
    }

    fn read_blob<'a>(&'a self, _: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async { Err(StorageError::Internal("the router reads ranges".to_owned())) })
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        _offset: u64,
        _limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            let gate = self.gate.lock().expect("lock").clone();
            if let Some(gate) = gate {
                gate.acquire().await.expect("an open semaphore").forget();
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if self.down.load(Ordering::SeqCst) {
                return Err(unavailable());
            }
            if !self.holds.load(Ordering::SeqCst) {
                return Err(StorageError::NotFound(*digest));
            }
            let own = self.blobs.lock().expect("lock").get(digest).cloned();
            let data = own.unwrap_or_else(|| self.data.lock().expect("lock").clone());
            let mut chunks: Vec<Result<Bytes, StorageError>> = (0..data.len())
                .step_by(4)
                .map(|at| Ok(data.slice(at..data.len().min(at + 4))))
                .collect();
            if self.breaks.load(Ordering::SeqCst) {
                chunks.truncate(1);
                chunks.push(Err(unavailable()));
            }
            let pause = self.pause.lock().expect("lock").clone();
            let Some((after, gate)) = pause else {
                return Ok(stream::iter(chunks).boxed());
            };
            let rest = chunks.split_off(after.min(chunks.len()));
            let wait = stream::once(async move {
                gate.acquire().await.expect("an open semaphore").forget();
            })
            .filter_map(|()| async { None });
            Ok(stream::iter(chunks)
                .chain(wait)
                .chain(stream::iter(rest))
                .boxed())
        })
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(unavailable());
            }
            Ok(sha(&bytes))
        })
    }

    fn stream(&self, _size: u64) -> Option<Box<dyn PutStream>> {
        None
    }

    fn segments(&self) -> Arc<dyn Segments> {
        Arc::new(NoSegments)
    }

    fn action_result<'a>(&'a self, _: &'a Digest) -> Answer<'a, Option<(Digest, ActionResult)>> {
        Box::pin(async { Ok(None) })
    }

    fn prepare_action_result<'a>(&'a self, _: &'a ActionResult) -> Answer<'a, ActionRecord> {
        Box::pin(async { Err(StorageError::Internal("not used".to_owned())) })
    }

    fn commit_action_record(&self, _: Role, _: Digest, _: ActionRecord) -> Answer<'_, ()> {
        Box::pin(async { Err(StorageError::Internal("not used".to_owned())) })
    }
}

struct NoSegments;

#[tonic::async_trait]
impl Segments for NoSegments {
    async fn begin(&self, _: Request<BeginRequest>) -> Result<Response<BeginResponse>, Status> {
        Err(Status::unimplemented("no segments"))
    }

    async fn put_range(
        &self,
        _: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        Err(Status::unimplemented("no segments"))
    }

    async fn commit(&self, _: Request<CommitRequest>) -> Result<Response<CommitResponse>, Status> {
        Err(Status::unimplemented("no segments"))
    }

    async fn locate(&self, _: Request<LocateRequest>) -> Result<Response<LocateResponse>, Status> {
        Err(Status::unimplemented("no segments"))
    }

    async fn read_range(
        &self,
        _: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        Err(Status::unimplemented("no segments"))
    }
}

/// A map that never changes.
pub struct Fixed(pub MapView);

impl MapSource for Fixed {
    fn current(&self) -> Option<MapView> {
        Some(self.0.clone())
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            Err(RefetchError {
                at_least,
                reason: "the test map does not change".to_owned(),
            })
        })
    }
}

/// Every member is `fake`.
struct Only(Arc<Fake>);

impl Nodes for Only {
    fn node(&self, _: &Member, _: u64) -> Result<Arc<dyn Storage>, StorageError> {
        Ok(self.0.clone())
    }
}

/// A data plane over a farm of one member, which is `fake`, within `config`.
pub fn plane(fake: &Arc<Fake>, config: &TransferConfig) -> Arc<RoutedCas> {
    let member = Member {
        id: MemberId(1),
        addr: "node-01:7000".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let map = Map::new(1, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("a map");
    let view = MapView {
        map: Arc::new(map),
        previous: None,
    };
    let router = Router::new(Arc::new(Fixed(view)), Arc::new(Only(Arc::clone(fake))));
    Arc::new(RoutedCas::new(router, config))
}
