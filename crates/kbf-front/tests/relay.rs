//! The Execute relay (`kbf_front::Relay`): what a frontend that has handed its roles to a
//! successor does with an `Execute` or `WaitExecution` that still reaches it. The
//! successor is a scripted `Execution` server on a loopback port (or, for the
//! authorization test, kbf-front's own service behind a policy), the old scheduler a
//! marker that answers with a name no successor writes, and the state store a scripted
//! [`Handed`].

mod common;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use common::Farm;
use futures::future::BoxFuture;
use futures::{StreamExt, stream};
use kbf_auth::{AuthenticationMetadata, Authenticator, Authorizers, Policy};
use kbf_front::{
    Backend, Cache, Dispatch, Finished, Handed, OpenStreams, OperationStream, Relay, Stage,
    Submission, Ticket, closing, progress,
};
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc;
use kbf_proto::reapi::execution_server::{Execution, ExecutionServer};
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_proto::reapi::{
    ExecuteOperationMetadata, ExecuteRequest, ExecuteResponse, WaitExecutionRequest,
};
use kbf_types::{Digest, DigestFunction, OperationId};
use prost::Message;
use prost_types::Any;
use tonic::codegen::http::request::Parts;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Code, Request, Status};

const HASH: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const INSTANCE: &str = "main";
const OP: &str = "operations/7-3";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn action() -> Digest {
    Digest::parse(DigestFunction::Sha256, &format!("{HASH}/3")).expect("a digest")
}

/// The operation `name` at `stage`, for the action [`action`]; a done one holds a failed
/// response with `code` (the relay does not read it, the caller does).
fn op(name: &str, stage: ExecStage, code: i32) -> Operation {
    let metadata = ExecuteOperationMetadata {
        stage: stage as i32,
        action_digest: Some(kbf_proto::reapi::Digest {
            hash: HASH.to_owned(),
            size_bytes: 3,
        }),
        ..ExecuteOperationMetadata::default()
    };
    let done = stage == ExecStage::Completed;
    let response = ExecuteResponse {
        status: Some(rpc::Status {
            code,
            ..rpc::Status::default()
        }),
        ..ExecuteResponse::default()
    };
    Operation {
        name: name.to_owned(),
        metadata: Some(Any {
            type_url:
                "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteOperationMetadata"
                    .to_owned(),
            value: metadata.encode_to_vec(),
        }),
        done,
        result: done.then(|| {
            operation::Result::Response(Any {
                type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse"
                    .to_owned(),
                value: response.encode_to_vec(),
            })
        }),
    }
}

/// The code of the failed response a done operation holds.
fn code_of(operation: &Operation) -> i32 {
    let Some(operation::Result::Response(any)) = &operation.result else {
        panic!("not done: {operation:?}");
    };
    ExecuteResponse::decode(any.value.as_slice())
        .expect("a response")
        .status
        .expect("a status")
        .code
}

/// What a call to the successor carried.
#[derive(Debug)]
struct Seen {
    call: &'static str,
    /// The instance (Execute) or the operation name (WaitExecution).
    subject: String,
    metadata: MetadataMap,
}

/// A successor that answers `items` to every call, and keeps what it saw.
struct Successor {
    items: Vec<Result<Operation, Status>>,
    /// Never answers the call.
    hang: bool,
    /// Leaves the stream open after `items`.
    open_ended: bool,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Successor {
    async fn answer(&self, seen: Seen) -> Result<tonic::Response<OperationStream>, Status> {
        lock(&self.seen).push(seen);
        if self.hang {
            std::future::pending::<()>().await;
        }
        let items = stream::iter(self.items.clone());
        if self.open_ended {
            return Ok(tonic::Response::new(Box::pin(
                items.chain(stream::pending()),
            )));
        }
        Ok(tonic::Response::new(Box::pin(items)))
    }
}

#[tonic::async_trait]
impl Execution for Successor {
    type ExecuteStream = OperationStream;
    type WaitExecutionStream = OperationStream;

    async fn execute(
        &self,
        request: Request<ExecuteRequest>,
    ) -> Result<tonic::Response<OperationStream>, Status> {
        let seen = Seen {
            call: "execute",
            subject: request.get_ref().instance_name.clone(),
            metadata: request.metadata().clone(),
        };
        self.answer(seen).await
    }

    async fn wait_execution(
        &self,
        request: Request<WaitExecutionRequest>,
    ) -> Result<tonic::Response<OperationStream>, Status> {
        let seen = Seen {
            call: "wait",
            subject: request.get_ref().name.clone(),
            metadata: request.metadata().clone(),
        };
        self.answer(seen).await
    }
}

/// The old scheduler: answers every call with a name no successor writes.
struct Old(Arc<Mutex<u32>>);

impl Old {
    fn answer(&self) -> Result<tonic::Response<OperationStream>, Status> {
        *lock(&self.0) += 1;
        let done = op("operations/old-0", ExecStage::Completed, 0);
        Ok(tonic::Response::new(Box::pin(stream::iter([Ok(done)]))))
    }
}

#[tonic::async_trait]
impl Execution for Old {
    type ExecuteStream = OperationStream;
    type WaitExecutionStream = OperationStream;

    async fn execute(
        &self,
        _: Request<ExecuteRequest>,
    ) -> Result<tonic::Response<OperationStream>, Status> {
        self.answer()
    }

    async fn wait_execution(
        &self,
        _: Request<WaitExecutionRequest>,
    ) -> Result<tonic::Response<OperationStream>, Status> {
        self.answer()
    }
}

/// The state store as the relay asks about it, scripted.
struct Rows {
    handed: AtomicBool,
    found: Mutex<Result<Option<OperationId>, String>>,
    asked: Mutex<Vec<(Option<String>, Digest)>>,
    looks: Mutex<VecDeque<Result<Option<Stage>, String>>>,
    followed: Mutex<Vec<OperationId>>,
}

impl Handed for Rows {
    fn handed(&self) -> bool {
        self.handed.load(Ordering::SeqCst)
    }

    fn operation_of(
        &self,
        instance: Option<&str>,
        action: &Digest,
    ) -> Result<Option<OperationId>, String> {
        lock(&self.asked).push((instance.map(str::to_owned), *action));
        lock(&self.found).clone()
    }

    fn look(&self, operation: OperationId) -> Result<Option<Stage>, String> {
        lock(&self.followed).push(operation);
        lock(&self.looks).pop_front().unwrap_or(Ok(None))
    }
}

/// A relay over a scripted successor, an old scheduler that counts its answers, and
/// scripted rows (handed over, the operation found as 41, `looks` for each follow).
struct Rig {
    relay: Relay<Old>,
    rows: Arc<Rows>,
    old: Arc<Mutex<u32>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    backend: Backend,
}

type Looks = Vec<Result<Option<Stage>, String>>;

impl Rig {
    async fn new(items: Vec<Result<Operation, Status>>, looks: Looks) -> Self {
        Self::over(items, looks, |successor| successor).await
    }

    async fn over(
        items: Vec<Result<Operation, Status>>,
        looks: Looks,
        script: impl FnOnce(Successor) -> Successor,
    ) -> Self {
        let seen = Arc::default();
        let successor = script(Successor {
            items,
            hang: false,
            open_ended: false,
            seen: Arc::clone(&seen),
        });
        let channel = serve(successor).await;
        Self::to(channel, looks, seen)
    }

    fn to(channel: Channel, looks: Looks, seen: Arc<Mutex<Vec<Seen>>>) -> Self {
        let rows = Arc::new(Rows {
            handed: AtomicBool::new(true),
            found: Mutex::new(Ok(Some(OperationId(41)))),
            asked: Mutex::default(),
            looks: Mutex::new(looks.into()),
            followed: Mutex::default(),
        });
        let backend = Backend::from(Arc::new(Cache::memory()));
        let old = Arc::default();
        let handed: Arc<dyn Handed> = Arc::<Rows>::clone(&rows);
        let relay = Relay::new(Old(Arc::clone(&old)), handed, channel, backend.clone())
            .following_every(Duration::from_millis(1));
        Self {
            relay,
            rows,
            old,
            seen,
            backend,
        }
    }

    async fn execute(&self) -> Vec<Result<Operation, Status>> {
        let response = self.relay.execute(execute()).await.expect("opened");
        all(response.into_inner()).await
    }

    fn relayed(&self) -> (u64, u64) {
        let metrics = self.backend.metrics();
        (
            metrics.handover_relayed.get(),
            metrics.handover_relays_open.get(),
        )
    }
}

async fn serve(successor: Successor) -> Channel {
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(async move {
        Server::builder()
            .add_service(ExecutionServer::new(successor))
            .serve_with_incoming(incoming)
            .await
            .expect("serve");
    });
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect")
}

fn execute() -> Request<ExecuteRequest> {
    Request::new(ExecuteRequest {
        instance_name: INSTANCE.to_owned(),
        ..ExecuteRequest::default()
    })
}

fn wait() -> Request<WaitExecutionRequest> {
    Request::new(WaitExecutionRequest {
        name: OP.to_owned(),
    })
}

async fn all(stream: OperationStream) -> Vec<Result<Operation, Status>> {
    stream.collect().await
}

/// Each item as its operation name, or its status code.
fn names(items: &[Result<Operation, Status>]) -> Vec<String> {
    let name = |item: &Result<Operation, Status>| match item {
        Ok(operation) => operation.name.clone(),
        Err(status) => format!("{:?}", status.code()),
    };
    items.iter().map(name).collect()
}

/// The status a call was refused with.
fn refusal<T>(call: Result<T, Status>) -> Status {
    match call {
        Err(status) => status,
        Ok(_) => panic!("the call was answered"),
    }
}

fn failed(code: i32) -> Stage {
    Stage::Done(Finished::Failed(rpc::Status {
        code,
        ..rpc::Status::default()
    }))
}

fn queued(name: &str) -> Result<Operation, Status> {
    Ok(op(name, ExecStage::Queued, 0))
}

/// Catches: a relay that changes a frontend that has not handed over (a call answered by
/// the successor, or counted), which would make the relay more than inert. Mutant: the
/// handed check removed from `execute` and `wait_execution`: the successor is called.
#[tokio::test]
async fn before_the_handover_the_wrapped_service_answers() {
    let done = Ok(op("operations/new-0", ExecStage::Completed, 0));
    let rig = Rig::new(vec![done], vec![]).await;
    rig.rows.handed.store(false, Ordering::SeqCst);
    assert_eq!(names(&rig.execute().await), ["operations/old-0"]);
    let waited = rig.relay.wait_execution(wait()).await.expect("answered");
    assert_eq!(names(&all(waited.into_inner()).await), ["operations/old-0"]);
    assert_eq!(*lock(&rig.old), 2);
    assert!(lock(&rig.seen).is_empty(), "the successor was called");
    assert_eq!(rig.relayed(), (0, 0));
}

/// Catches: a call after the handover answered by the old scheduler (the marker name
/// comes back), the successor's operation name replaced, a stage lost or reordered, the
/// stream cut before its done operation, an Execute that does not carry its instance, and
/// the relay counters wrong (one call counted once; open until the stream ends). The
/// operation is looked up once, by instance and action, while it is unfinished; the rows
/// are never followed while the successor's stream holds. Mutant: `execute` answering
/// `self.inner.execute` when handed.
#[tokio::test]
async fn an_execute_after_the_handover_is_the_successors_to_its_end() {
    let items = vec![
        queued("operations/new-5"),
        Ok(op("operations/new-5", ExecStage::Executing, 0)),
        Ok(op("operations/new-5", ExecStage::Completed, 3)),
    ];
    let rig = Rig::new(items, vec![]).await;
    let response = rig.relay.execute(execute()).await.expect("opened");
    assert_eq!(
        rig.relayed(),
        (1, 1),
        "a relay is open until its stream ends"
    );
    let items = all(response.into_inner()).await;
    assert_eq!(names(&items), ["operations/new-5"; 3]);
    let done = items[2].as_ref().expect("done");
    assert!(done.done);
    assert_eq!(code_of(done), 3);
    assert_eq!(*lock(&rig.old), 0, "the old scheduler answered");
    let seen = lock(&rig.seen);
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].call, seen[0].subject.as_str()),
        ("execute", INSTANCE)
    );
    assert_eq!(rig.relayed(), (1, 0));
    assert_eq!(
        *lock(&rig.rows.asked),
        [(Some(INSTANCE.to_owned()), action())]
    );
    assert!(lock(&rig.rows.followed).is_empty());
}

/// Catches: WaitExecution not relayed or relayed with another name, and an instance
/// invented for a call that has none (the operation is then found by its action alone).
#[tokio::test]
async fn a_wait_execution_after_the_handover_is_the_successors_to_its_end() {
    let items = vec![
        Ok(op(OP, ExecStage::Executing, 0)),
        Ok(op(OP, ExecStage::Completed, 0)),
    ];
    let rig = Rig::new(items, vec![]).await;
    let response = rig.relay.wait_execution(wait()).await.expect("opened");
    assert_eq!(names(&all(response.into_inner()).await), [OP; 2]);
    let seen = lock(&rig.seen);
    assert_eq!((seen[0].call, seen[0].subject.as_str()), ("wait", OP));
    assert_eq!(*lock(&rig.rows.asked), [(None, action())]);
}

/// Catches: an answer the successor gave at once (an action-cache hit) looked up in the
/// state store for nothing: a done first operation is never followed, so it is never
/// looked for.
#[tokio::test]
async fn a_done_first_operation_is_not_looked_up() {
    let done = Ok(op("operations/cached/x", ExecStage::Completed, 0));
    let rig = Rig::new(vec![done], vec![]).await;
    assert_eq!(names(&rig.execute().await), ["operations/cached/x"]);
    assert!(lock(&rig.rows.asked).is_empty());
}

/// Catches: the caller's metadata dropped in the relay (the successor must authorize the
/// caller, not the relay): a bearer token and a binary header reach it, and the deadline does not (the relay
/// holds it).
/// Mutant: `forwarded` building the call with empty metadata.
#[tokio::test]
async fn the_callers_metadata_reaches_the_successor() {
    let done = Ok(op(OP, ExecStage::Completed, 0));
    let rig = Rig::new(vec![done], vec![]).await;
    let mut request = execute();
    request
        .metadata_mut()
        .insert("authorization", MetadataValue::from_static("Bearer caller"));
    request
        .metadata_mut()
        .insert_bin("x-request-bin", MetadataValue::from_bytes(&[9, 8, 7]));
    request.set_timeout(Duration::from_secs(60));
    all(rig
        .relay
        .execute(request)
        .await
        .expect("opened")
        .into_inner())
    .await;
    let seen = lock(&rig.seen);
    let metadata = &seen[0].metadata;
    assert_eq!(
        metadata.get("authorization").expect("token"),
        "Bearer caller"
    );
    let bin = metadata.get_bin("x-request-bin").expect("binary header");
    assert_eq!(bin.to_bytes().expect("bytes").as_ref(), [9, 8, 7]);
    assert!(
        metadata.get("grpc-timeout").is_none(),
        "the deadline was forwarded"
    );
}

struct Token;

impl Authenticator for Token {
    fn authenticate<'a>(
        &'a self,
        call: &'a Parts,
    ) -> BoxFuture<'a, Result<Arc<AuthenticationMetadata>, Status>> {
        let ok = call.headers.get("x-relay-token").is_some_and(|v| v == "ok");
        Box::pin(std::future::ready(if ok {
            Ok(Arc::new(AuthenticationMetadata::default()))
        } else {
            Err(Status::unauthenticated("no token"))
        }))
    }
}

struct Nothing;

impl Dispatch for Nothing {
    fn submit(&self, _: Submission) -> Result<Ticket, Status> {
        Err(Status::unavailable("no scheduler"))
    }

    fn wait(&self, _: &str) -> Option<Ticket> {
        None
    }
}

/// Catches: the successor not authorizing again: a relayed call without the caller's
/// credentials is the successor's UNAUTHENTICATED (not an answer from this process), and
/// the same call with them gets past the successor's authentication to its handler (here
/// a WaitExecution of an operation it does not know: NOT_FOUND). Mutant: the metadata
/// dropped: the second call is UNAUTHENTICATED too.
#[tokio::test]
async fn the_successor_authorizes_a_relayed_call_again() {
    let policy = Policy {
        authenticator: Arc::new(Token),
        authorizers: Authorizers::allow_all(),
        invocations_get: None,
        blobs_get: None,
    };
    let successor = Farm::with_policy(Arc::new(Nothing), policy).await;
    let rig = Rig::to(successor.channel(), vec![], Arc::default());
    let denied = refusal(rig.relay.wait_execution(wait()).await);
    assert_eq!(denied.code(), Code::Unauthenticated);
    let mut request = wait();
    request
        .metadata_mut()
        .insert("x-relay-token", MetadataValue::from_static("ok"));
    let reached = refusal(rig.relay.wait_execution(request).await);
    assert_eq!(reached.code(), Code::NotFound);
    assert_eq!(
        rig.relayed(),
        (2, 0),
        "a refused open leaves nothing counted open"
    );
}

/// Catches: a stream to the successor that breaks being the end of the call (Buck2 does
/// not recover a stream that breaks after it started): the relay follows the operation in
/// the state store, found by its action while it was unfinished, with the successor's name
/// kept, showing a stage when the rows change it, nothing while they say nothing or the
/// same, and the done operation at the end, which ends the stream and the relay. Two ways
/// to break: an error, and a clean end before done. Mutants: no fallback (the stream
/// errors at the break); the rows followed under another operation id.
#[tokio::test]
async fn a_broken_stream_is_followed_from_the_done_rows() {
    for items in [
        vec![
            queued("operations/new-5"),
            Err(Status::unavailable("restarted")),
        ],
        vec![queued("operations/new-5")],
    ] {
        let looks = vec![
            Ok(None),
            Ok(Some(Stage::Executing)),
            Ok(Some(Stage::Executing)),
            Ok(None),
            Ok(Some(failed(5))),
        ];
        let rig = Rig::new(items, looks).await;
        let items = all(rig
            .relay
            .execute(execute())
            .await
            .expect("opened")
            .into_inner())
        .await;
        assert_eq!(names(&items), ["operations/new-5"; 3]);
        let stages: Vec<bool> = items
            .iter()
            .map(|item| item.as_ref().expect("an operation").done)
            .collect();
        assert_eq!(stages, [false, false, true]);
        assert_eq!(code_of(items[2].as_ref().expect("done")), 5);
        assert_eq!(*lock(&rig.rows.followed), [OperationId(41); 5]);
        assert_eq!(*lock(&rig.old), 0);
        assert_eq!(rig.relayed(), (1, 0));
    }
}

/// Catches: a relay that follows an operation it cannot name, or invents an answer for a
/// stream that never said anything: with nothing to follow (the stream broke before its
/// first operation; the operation has no unfinished row, or the rows cannot be read; the
/// first operation cannot be read) the call ends with the status the stream broke with,
/// UNAVAILABLE when it only ended.
#[tokio::test]
async fn a_stream_that_cannot_be_followed_ends_as_it_broke() {
    let broke = Status::data_loss("successor broke");
    let rig = Rig::new(vec![Err(broke)], vec![]).await;
    assert_eq!(names(&rig.execute().await), ["DataLoss"]);
    assert!(lock(&rig.rows.asked).is_empty());

    let rig = Rig::new(vec![], vec![]).await;
    assert_eq!(names(&rig.execute().await), ["Unavailable"]);

    for found in [Ok(None), Err("rows unreadable".to_owned())] {
        let rig = Rig::new(vec![queued("operations/new-5")], vec![]).await;
        *lock(&rig.rows.found) = found;
        assert_eq!(
            names(&rig.execute().await),
            ["operations/new-5", "Unavailable"]
        );
        assert!(lock(&rig.rows.followed).is_empty());
    }

    let unreadable = Operation {
        name: "operations/new-5".to_owned(),
        ..Operation::default()
    };
    let rig = Rig::new(vec![Ok(unreadable)], vec![]).await;
    assert_eq!(
        names(&rig.execute().await),
        ["operations/new-5", "Unavailable"]
    );
    assert!(lock(&rig.rows.asked).is_empty());

    let mut bad = op("operations/new-5", ExecStage::Queued, 0);
    let mut metadata =
        ExecuteOperationMetadata::decode(bad.metadata.as_ref().expect("m").value.as_slice())
            .expect("decodes");
    metadata.action_digest = None;
    bad.metadata.as_mut().expect("m").value = metadata.encode_to_vec();
    let rig = Rig::new(vec![Ok(bad)], vec![]).await;
    assert_eq!(
        names(&rig.execute().await),
        ["operations/new-5", "Unavailable"]
    );
    assert!(lock(&rig.rows.asked).is_empty());
}

/// Catches: a follower that cannot read the rows left waiting for ever: the stream ends
/// UNAVAILABLE with the reason.
#[tokio::test]
async fn rows_that_cannot_be_read_end_the_followed_stream() {
    let looks = vec![Err("disk gone".to_owned())];
    let rig = Rig::new(vec![queued("operations/new-5")], looks).await;
    let items = rig.execute().await;
    assert_eq!(names(&items), ["operations/new-5", "Unavailable"]);
    let status = items[1].as_ref().expect_err("an error");
    assert!(status.message().contains("operations/new-5"), "{status}");
    assert!(status.message().contains("disk gone"), "{status}");
    assert_eq!(rig.relayed(), (1, 0));
}

/// Catches: a followed stream that goes quiet between stage changes (a load balancer that
/// counts idle time in data frames would cut it): at every tick of the progress ticker the
/// operation is sent again at its current stage, though the rows say nothing new and the
/// look interval is an hour.
#[tokio::test]
async fn a_followed_stream_is_sent_again_at_every_progress_tick() {
    let rig = Rig::new(vec![queued("operations/new-5")], vec![]).await;
    let (ticker, ticking) = progress(Duration::from_millis(2));
    tokio::spawn(ticker.run());
    let streams = OpenStreams {
        closing: closing().1,
        progress: ticking,
    };
    let Rig { relay, .. } = rig;
    let relay = relay
        .with_streams(streams)
        .following_every(Duration::from_secs(3600));
    let response = relay.execute(execute()).await.expect("opened");
    let items: Vec<_> = response.into_inner().take(3).collect().await;
    assert_eq!(names(&items), ["operations/new-5"; 3]);
    for item in &items {
        assert!(!item.as_ref().expect("an operation").done);
    }
}

/// Catches: a relayed stream that outlives a shutdown (the process would never exit): once
/// the closer closes, a stream the successor still holds and one followed from the rows
/// both end UNAVAILABLE.
#[tokio::test]
async fn a_closing_server_ends_relayed_streams() {
    let open = |broken: bool| async move {
        let items = if broken {
            vec![
                queued("operations/new-5"),
                Err(Status::unavailable("restarted")),
            ]
        } else {
            vec![queued("operations/new-5")]
        };
        let rig = Rig::over(items, vec![], |successor| Successor {
            open_ended: !broken,
            ..successor
        })
        .await;
        let (closer, closing) = closing();
        let Rig { relay, .. } = rig;
        let relay = relay.with_streams(OpenStreams {
            closing,
            progress: kbf_front::Progress::off(),
        });
        let response = relay.execute(execute()).await.expect("opened");
        let mut stream = response.into_inner();
        let first = stream.next().await.expect("an item").expect("an operation");
        assert_eq!(first.name, "operations/new-5");
        closer.close();
        let ended = stream.next().await.expect("an item").expect_err("ends");
        assert_eq!(ended.code(), Code::Unavailable);
        assert!(ended.message().contains("shutting down"), "{ended}");
        assert!(stream.next().await.is_none());
    };
    open(false).await;
    open(true).await;
}

/// Catches: a call the successor never starts held past the caller's deadline (nothing
/// bounds it otherwise: there is no queue and no flag): it ends DEADLINE_EXCEEDED, and
/// leaves nothing counted open. A call with no deadline is not given one (the other tests
/// open and finish with none). Mutant: the deadline not applied to the open.
#[tokio::test]
async fn the_callers_deadline_ends_a_call_that_never_starts() {
    let rig = Rig::over(vec![], vec![], |successor| Successor {
        hang: true,
        ..successor
    })
    .await;
    let mut request = execute();
    request.set_timeout(Duration::from_millis(50));
    let ended = refusal(rig.relay.execute(request).await);
    assert_eq!(ended.code(), Code::DeadlineExceeded);
    assert_eq!(rig.relayed(), (1, 0));
    assert_eq!(lock(&rig.seen).len(), 1, "the call reached the successor");
}

/// Catches: an unreachable successor answered from the old scheduler or swallowed: the
/// call is refused with the transport's status, which Buck2 retries when it opens a call.
#[tokio::test]
async fn an_unreachable_successor_refuses_the_call() {
    let dead = {
        let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        incoming.local_addr().expect("address")
    };
    let channel = Endpoint::from_shared(format!("http://{dead}"))
        .expect("endpoint")
        .connect_lazy();
    let rig = Rig::to(channel, vec![], Arc::default());
    let refused = refusal(rig.relay.execute(execute()).await);
    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(*lock(&rig.old), 0);
    assert_eq!(rig.relayed(), (1, 0));
}
