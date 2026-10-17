//! What the tests run over: storage nodes that hold what they are told to and fail as
//! they are told to, a map a test sets, and a hook that records.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt as _};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_meta::{ActionRecord, ActionWriteError, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse, LocateRequest, LocateResponse,
    PutRangeRequest, PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use kbf_route::{MapSource, MapView, Nodes, ReadRepair, RefetchError, Router};
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use tonic::{Request, Response, Status};

/// The digest of `bytes`.
pub fn sha(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

/// The first blob, of the bytes `blob number <i>`, whose digest `want` accepts.
pub fn blob_where(want: impl Fn(&Digest) -> bool) -> (Vec<u8>, Digest) {
    (0_u64..100_000)
        .map(|i| format!("blob number {i}").into_bytes())
        .map(|bytes| {
            let digest = sha(&bytes);
            (bytes, digest)
        })
        .find(|(_, digest)| want(digest))
        .expect("a blob that fits")
}

/// A blob that `map` reads from the members `order`, first to last.
pub fn blob_read_from(map: &Map, order: &[u64]) -> (Vec<u8>, Digest) {
    blobs_read_from(map, order, 1).remove(0)
}

/// `count` different blobs that `map` reads from the members `order`.
pub fn blobs_read_from(map: &Map, order: &[u64], count: usize) -> Vec<(Vec<u8>, Digest)> {
    let blobs: Vec<_> = (0_u64..100_000)
        .map(|i| format!("blob number {i}").into_bytes())
        .map(|bytes| {
            let digest = sha(&bytes);
            (bytes, digest)
        })
        .filter(|(_, digest)| ids_of(&map.read_order(digest)) == order)
        .take(count)
        .collect();
    assert_eq!(blobs.len(), count, "blobs that fit");
    blobs
}

/// `count` different blobs whose primary owner, in `map`, is member `primary`.
pub fn blobs_with_primary(map: &Map, primary: u64, count: usize) -> Vec<(Vec<u8>, Digest)> {
    let blobs: Vec<_> = (0_u64..100_000)
        .map(|i| format!("blob number {i}").into_bytes())
        .map(|bytes| {
            let digest = sha(&bytes);
            (bytes, digest)
        })
        .filter(|(_, digest)| map.write_owners(digest).first() == Some(&MemberId(primary)))
        .take(count)
        .collect();
    assert_eq!(blobs.len(), count, "blobs that fit");
    blobs
}

/// A blob that `map` writes to the members `order`, primary first.
pub fn blob_written_to(map: &Map, order: &[u64]) -> (Vec<u8>, Digest) {
    blob_where(|d| ids_of(&map.write_owners(d)) == order)
}

fn ids_of(ids: &[MemberId]) -> Vec<u64> {
    ids.iter().map(|m| m.0).collect()
}

pub fn unavailable() -> StorageError {
    StorageError::Unavailable(Arc::new(std::fmt::Error))
}

/// A member in `state`, at the address `node-<id>:7000`.
pub fn member(id: u64, state: MemberState) -> Member {
    Member {
        id: MemberId(id),
        addr: format!("node-{id:02}:7000"),
        zone: String::new(),
        weight: 1,
        state,
    }
}

/// Active members `ids`.
pub fn active(ids: &[u64]) -> Vec<Member> {
    ids.iter()
        .map(|id| member(*id, MemberState::Active))
        .collect()
}

pub fn map(generation: u64, r: u8, members: Vec<Member>) -> Arc<Map> {
    Arc::new(Map::new(generation, members, Rule::Rendezvous, r, 1, Vec::new()).expect("a map"))
}

/// What a read does instead of serving the blob.
enum Plan {
    /// Fails before its first byte.
    Fail(StorageError),
    /// Passes this many chunks, then fails.
    Break(usize, StorageError),
}

/// A storage node.
pub struct Node {
    blobs: Mutex<BTreeMap<Digest, Bytes>>,
    /// Blobs that are generated, chunk by chunk, as they are read: how many chunks, and
    /// how many have been produced.
    huge: Mutex<BTreeMap<Digest, (usize, Arc<AtomicUsize>)>>,
    actions: Mutex<BTreeMap<Digest, (Digest, ActionResult)>>,
    /// The next calls other than reads fail with these.
    fail: Mutex<VecDeque<StorageError>>,
    /// The next puts fail with these, after the node's other checks pass.
    fail_puts: Mutex<VecDeque<StorageError>>,
    /// The next commits fail with these, after the node's other checks pass.
    fail_commits: Mutex<VecDeque<StorageError>>,
    /// What the next reads do.
    plans: Mutex<VecDeque<Plan>>,
    /// Every call fails while set.
    pub down: AtomicBool,
    /// A read waits for this before it answers.
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// The size of a chunk of a read.
    chunk: usize,
    /// `(digest, offset, limit)` of each read.
    pub reads: Mutex<Vec<(Digest, u64, u64)>>,
    pub finds: Mutex<Vec<Vec<Digest>>>,
    pub puts: Mutex<Vec<Digest>>,
    pub commits: Mutex<Vec<Digest>>,
    pub action_gets: AtomicUsize,
    /// What `put` answers instead of the digest, if set.
    put_answer: Mutex<Option<Digest>>,
    /// A plain `put` is refused, as a node in a chain refuses a blob it does not own (the mock
    /// refuses every plain put, owner or not).
    chain_rule: AtomicBool,
    /// A commit is refused as `Absent` unless the node holds the entry's result blob and
    /// closure, as a storage node does.
    pub check_closure: AtomicBool,
}

impl Node {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            blobs: Mutex::default(),
            huge: Mutex::default(),
            actions: Mutex::default(),
            fail: Mutex::default(),
            fail_puts: Mutex::default(),
            fail_commits: Mutex::default(),
            plans: Mutex::default(),
            down: AtomicBool::new(false),
            gate: Mutex::default(),
            chunk: 4,
            reads: Mutex::default(),
            finds: Mutex::default(),
            puts: Mutex::default(),
            commits: Mutex::default(),
            action_gets: AtomicUsize::new(0),
            put_answer: Mutex::default(),
            chain_rule: AtomicBool::new(false),
            check_closure: AtomicBool::new(false),
        })
    }

    pub fn hold(&self, bytes: &[u8]) -> Digest {
        let digest = sha(bytes);
        self.blobs
            .lock()
            .expect("lock")
            .insert(digest, Bytes::copy_from_slice(bytes));
        digest
    }

    /// The bytes held under `digest`, if any.
    pub fn blob(&self, digest: &Digest) -> Option<Bytes> {
        self.blobs.lock().expect("lock").get(digest).cloned()
    }

    /// Holds `bytes` under `digest`, whatever they hash to.
    pub fn hold_as(&self, digest: Digest, bytes: &[u8]) {
        self.blobs
            .lock()
            .expect("lock")
            .insert(digest, Bytes::copy_from_slice(bytes));
    }

    /// A blob of `chunks` chunks of 1 KiB, made as it is read; `produced` counts them.
    pub fn hold_generated(&self, chunks: usize) -> (Digest, Arc<AtomicUsize>) {
        let digest = Digest::new(
            kbf_types::DigestFunction::Sha256,
            [9; 32],
            (chunks * 1024) as u64,
        );
        let produced = self.hold_generated_as(digest, chunks);
        (digest, produced)
    }

    /// `chunks` chunks of 1 KiB under `digest`, whatever it says of their size, made as
    /// they are read; the count says how many have been.
    pub fn hold_generated_as(&self, digest: Digest, chunks: usize) -> Arc<AtomicUsize> {
        let produced = Arc::new(AtomicUsize::new(0));
        self.huge
            .lock()
            .expect("lock")
            .insert(digest, (chunks, Arc::clone(&produced)));
        produced
    }

    pub fn hold_action(&self, action: Digest, result_blob: Digest) {
        self.actions
            .lock()
            .expect("lock")
            .insert(action, (result_blob, ActionResult::default()));
    }

    /// The next call that is not a read fails with `error`.
    pub fn fail_next(&self, error: StorageError) {
        self.fail.lock().expect("lock").push_back(error);
    }

    /// The next put fails with `error`, while FindMissing and a commit still answer.
    pub fn fail_next_put(&self, error: StorageError) {
        self.fail_puts.lock().expect("lock").push_back(error);
    }

    /// The next commit fails with `error`, while FindMissing and a put still answer.
    pub fn fail_next_commit(&self, error: StorageError) {
        self.fail_commits.lock().expect("lock").push_back(error);
    }

    /// The next read, after the ones already planned, fails before its first byte.
    pub fn fail_read(&self, error: StorageError) {
        self.plans
            .lock()
            .expect("lock")
            .push_back(Plan::Fail(error));
    }

    /// The next read, after the ones already planned, passes `after` chunks and fails.
    pub fn break_read(&self, after: usize, error: StorageError) {
        self.plans
            .lock()
            .expect("lock")
            .push_back(Plan::Break(after, error));
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// Makes a commit check the entry's blobs against the node's own, as storage does.
    pub fn check_closure(&self) {
        self.check_closure.store(true, Ordering::SeqCst);
    }

    pub fn answer_puts_with(&self, digest: Digest) {
        *self.put_answer.lock().expect("lock") = Some(digest);
    }

    /// Makes reads wait until permits are added to the semaphore.
    pub fn gate(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *self.gate.lock().expect("lock") = Some(Arc::clone(&gate));
        gate
    }

    pub fn read_count(&self) -> usize {
        self.reads.lock().expect("lock").len()
    }

    pub fn read_log(&self) -> Vec<(u64, u64)> {
        self.reads
            .lock()
            .expect("lock")
            .iter()
            .map(|(_, offset, limit)| (*offset, *limit))
            .collect()
    }

    pub fn find_log(&self) -> Vec<Vec<Digest>> {
        self.finds.lock().expect("lock").clone()
    }

    pub fn put_log(&self) -> Vec<Digest> {
        self.puts.lock().expect("lock").clone()
    }

    /// The next planned put failure, if any, as an error: a put or an unowned put each
    /// consume one, after the node's other checks pass.
    fn take_put_failure(&self) -> Result<(), StorageError> {
        match self.fail_puts.lock().expect("lock").pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn store(&self, bytes: Bytes) -> Digest {
        let digest = self.hold(&bytes);
        self.puts.lock().expect("lock").push(digest);
        self.put_answer.lock().expect("lock").unwrap_or(digest)
    }

    /// Plain writes are refused from now on, as a chain's node refuses a blob it does not
    /// own (every plain put, owner or not); [`Storage::put_unowned`] still stores.
    pub fn refuse_plain_puts(&self) {
        self.chain_rule.store(true, Ordering::SeqCst);
    }

    fn check_down(&self) -> Result<(), StorageError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(unavailable());
        }
        Ok(())
    }

    /// A call other than a read fails now, if the node is down or was told to.
    fn check(&self) -> Result<(), StorageError> {
        self.check_down()?;
        match self.fail.lock().expect("lock").pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Storage for Node {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async move {
            self.finds.lock().expect("lock").push(digests.to_vec());
            self.check()?;
            let blobs = self.blobs.lock().expect("lock");
            Ok(digests
                .iter()
                .filter(|d| d.size_bytes != 0 && !blobs.contains_key(d))
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
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            self.reads
                .lock()
                .expect("lock")
                .push((*digest, offset, limit));
            let gate = self.gate.lock().expect("lock").clone();
            if let Some(gate) = gate {
                gate.acquire().await.expect("an open semaphore").forget();
            }
            self.check_down()?;
            let plan = self.plans.lock().expect("lock").pop_front();
            if let Some(Plan::Fail(error)) = plan {
                return Err(error);
            }
            if let Some((chunks, produced)) = self.huge.lock().expect("lock").get(digest).cloned() {
                let stream = stream::iter(0..chunks).map(move |_| {
                    produced.fetch_add(1, Ordering::SeqCst);
                    Ok(Bytes::from_static(&[0; 1024]))
                });
                return Ok(stream.boxed());
            }
            let blob = self
                .blobs
                .lock()
                .expect("lock")
                .get(digest)
                .cloned()
                .ok_or(StorageError::NotFound(*digest))?;
            if offset > blob.len() as u64 {
                return Err(StorageError::OutOfRange {
                    digest: *digest,
                    offset,
                });
            }
            let start = offset as usize;
            let end = match limit {
                0 => blob.len(),
                n => blob.len().min(start + n as usize),
            };
            let mut items: Vec<Result<Bytes, StorageError>> = (start..end)
                .step_by(self.chunk)
                .map(|at| Ok(blob.slice(at..end.min(at + self.chunk))))
                .collect();
            if let Some(Plan::Break(after, error)) = plan {
                items.truncate(after);
                items.push(Err(error));
            }
            Ok(stream::iter(items).boxed())
        })
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        Box::pin(async move {
            self.check()?;
            if self.chain_rule.load(Ordering::SeqCst) {
                return Err(StorageError::Invalid("not the primary of the blob".into()));
            }
            self.take_put_failure()?;
            Ok(self.store(bytes))
        })
    }

    fn put_unowned(&self, bytes: Bytes) -> Answer<'_, Digest> {
        Box::pin(async move {
            self.check()?;
            self.take_put_failure()?;
            Ok(self.store(bytes))
        })
    }

    fn stream(&self, _size: u64) -> Option<Box<dyn PutStream>> {
        None
    }

    fn segments(&self) -> Arc<dyn Segments> {
        Arc::new(NoSegments)
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        Box::pin(async move {
            self.action_gets.fetch_add(1, Ordering::SeqCst);
            self.check()?;
            Ok(self.actions.lock().expect("lock").get(action).cloned())
        })
    }

    fn prepare_action_result<'a>(&'a self, _: &'a ActionResult) -> Answer<'a, ActionRecord> {
        Box::pin(async { Err(StorageError::Internal("not routed".to_owned())) })
    }

    fn commit_action_record(
        &self,
        _role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        Box::pin(async move {
            self.check()?;
            if let Some(error) = self.fail_commits.lock().expect("lock").pop_front() {
                return Err(error);
            }
            if self.check_closure.load(Ordering::SeqCst) {
                let blobs = self.blobs.lock().expect("lock");
                let lacking: Vec<Digest> = std::iter::once(&record.result)
                    .chain(record.closure.iter())
                    .filter(|d| !blobs.contains_key(d))
                    .copied()
                    .collect();
                if !lacking.is_empty() {
                    return Err(StorageError::ActionWrite(ActionWriteError::Absent(lacking)));
                }
            }
            self.commits.lock().expect("lock").push(action);
            self.hold_action(action, record.result);
            Ok(())
        })
    }
}

/// A `Segments` service that serves nothing.
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

/// The nodes of a test farm, by member id, and the generation each call was made with.
#[derive(Default)]
pub struct Cluster {
    nodes: Mutex<BTreeMap<MemberId, Arc<Node>>>,
    /// `(member, generation)` of every call to [`Nodes::node`], in order.
    pub dialled: Mutex<Vec<(u64, u64)>>,
}

impl Cluster {
    /// Nodes 1 to `count`.
    pub fn of(count: u64) -> Arc<Self> {
        let cluster = Arc::new(Self::default());
        for id in 1..=count {
            cluster.add(id);
        }
        cluster
    }

    pub fn add(&self, id: u64) -> Arc<Node> {
        let node = Node::new();
        self.nodes
            .lock()
            .expect("lock")
            .insert(MemberId(id), Arc::clone(&node));
        node
    }

    pub fn node(&self, id: u64) -> Arc<Node> {
        self.nodes
            .lock()
            .expect("lock")
            .get(&MemberId(id))
            .cloned()
            .expect("a node")
    }

    pub fn dialled(&self) -> Vec<(u64, u64)> {
        self.dialled.lock().expect("lock").clone()
    }
}

impl Nodes for Cluster {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        self.dialled
            .lock()
            .expect("lock")
            .push((member.id.0, generation));
        let node = self.nodes.lock().expect("lock").get(&member.id).cloned();
        match node {
            Some(node) => Ok(node),
            None => Err(StorageError::Internal(format!("no node {}", member.id))),
        }
    }
}

/// A map source a test sets.
#[derive(Default)]
pub struct Cell {
    view: Mutex<Option<MapView>>,
    /// What the next fetches answer; a fetch with nothing queued fails.
    fetches: Mutex<VecDeque<Result<MapView, RefetchError>>>,
    /// The `at_least` of every fetch, in order.
    pub asked: Mutex<Vec<u64>>,
}

impl Cell {
    pub fn with(view: MapView) -> Arc<Self> {
        let cell = Self::default();
        *cell.view.lock().expect("lock") = Some(view);
        Arc::new(cell)
    }

    pub fn empty() -> Arc<Self> {
        Arc::default()
    }

    /// The map the process has from now on.
    pub fn set(&self, view: MapView) {
        *self.view.lock().expect("lock") = Some(view);
    }

    pub fn answer_fetch(&self, answer: Result<MapView, RefetchError>) {
        self.fetches.lock().expect("lock").push_back(answer);
    }

    pub fn fetched(&self) -> Vec<u64> {
        self.asked.lock().expect("lock").clone()
    }
}

impl MapSource for Cell {
    fn current(&self) -> Option<MapView> {
        self.view.lock().expect("lock").clone()
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            self.asked.lock().expect("lock").push(at_least);
            let answer = self.fetches.lock().expect("lock").pop_front();
            let answer = answer.unwrap_or_else(|| {
                Err(RefetchError {
                    at_least,
                    reason: "no frontend answered".to_owned(),
                })
            });
            if let Ok(view) = &answer {
                *self.view.lock().expect("lock") = Some(view.clone());
            }
            answer
        })
    }
}

pub fn view(map: &Arc<Map>) -> MapView {
    MapView {
        map: Arc::clone(map),
        previous: None,
    }
}

/// A router over a cell holding `map` and a test cluster.
pub fn router(map: &Arc<Map>, cluster: &Arc<Cluster>) -> (Router, Arc<Cell>) {
    let cell = Cell::with(view(map));
    let router = Router::new(cell.clone(), cluster.clone());
    (router, cell)
}

/// A repair hook that writes down what it is told.
#[derive(Default)]
pub struct Hook {
    pub told: Mutex<Vec<(Digest, u64, Vec<u64>)>>,
}

impl ReadRepair for Hook {
    fn found_elsewhere(&self, digest: &Digest, found_at: MemberId, missing_at: &[MemberId]) {
        self.told.lock().expect("lock").push((
            *digest,
            found_at.0,
            missing_at.iter().map(|m| m.0).collect(),
        ));
    }
}

impl Hook {
    pub fn told(&self) -> Vec<(Digest, u64, Vec<u64>)> {
        self.told.lock().expect("lock").clone()
    }
}

/// The bytes of a stream, and how many chunks it was.
pub async fn collect(mut stream: ReadStream) -> Result<(Vec<u8>, usize), StorageError> {
    let mut bytes = Vec::new();
    let mut chunks = 0;
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk?);
        chunks += 1;
    }
    Ok((bytes, chunks))
}
