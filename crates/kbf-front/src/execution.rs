//! `Execution`: Execute and WaitExecution, over the cache and a [`Dispatch`].
//!
//! Execute follows RFC section 5.3, steps 3 to 7:
//! - the action cache is checked first (with the closure check, unless the client set
//!   `skip_cache_lookup`); a hit answers at once, `cached_result` set, and nothing runs;
//! - the `Action`, its `Command` and its whole input tree must be in the CAS; every blob
//!   that is not is listed in one FAILED_PRECONDITION, a `MISSING` violation each;
//! - the request goes to the [`Dispatch`] (the scheduler), which joins a running twin
//!   (in-flight dedup) or queues a new operation;
//! - the call streams the operation: `QUEUED`, `EXECUTING` once a lease for it has
//!   started, then the done operation with its `ExecuteResponse`. While no live worker
//!   can run it, the `QUEUED` operation's metadata says why, as a `google.rpc.ErrorInfo`
//!   (reason [`NO_WORKER_REASON`], domain [`ERROR_DOMAIN`], the explanation under
//!   metadata key `why`) in `partial_execution_metadata.auxiliary_metadata`.
//!
//! Execute is authorized by the [`Authorizers::execute`] authorizer against the
//! request's instance name, before anything else is read; WaitExecution by the same
//! authorizer against the instance name of the operation it names, once that is found
//! (an unknown name is NOT_FOUND whoever asks). The reads Execute makes for itself (the
//! action cache, the action, its inputs) are not authorized again. Each Execute runs
//! in an `execute` trace span carrying the instance name and the caller's public
//! authentication metadata (`caller`, `-` when there is none).
//!
//! WaitExecution streams the same updates for an operation name Execute returned, until
//! it is done. A finished operation is kept for a short retention the [`Dispatch`]
//! sets, in which waiting on it streams the done operation; after that it is
//! forgotten, waiting on it is NOT_FOUND, and the client's next Execute is answered
//! from the action cache (or runs the action again, if its result was not cached).
//!
//! When the server is going away ([`Closer::close`]), every open Execute and
//! WaitExecution stream that is not done ends UNAVAILABLE, which REAPI clients retry
//! (issue #168); one opened after that ends UNAVAILABLE after its first operation. A
//! WaitExecution on an operation kept finished is answered even then: its first
//! operation is the done one, and the stream ends there.
//!
//! What v0 sends to the scheduler: QoS `ci` for every call (the `x-kbf-qos` header is
//! not read yet), and every action is hermetic, so it may be joined (a `networked`
//! property does not exist yet). The lease kind is the platform's `kbf-lease` value,
//! `action` when absent; any other value than `action` or `whole_machine` is
//! INVALID_ARGUMENT. The booking is one core and 1 GiB ([`DEFAULT_RESOURCES`]; learned
//! sizes come with the estimator), or the whole cores of `kbf-book-cpus` and the GiB of
//! `kbf-book-mem-gib` where the platform names them: a value that is not a whole number
//! of at least 1, or that overflows the booking, is INVALID_ARGUMENT, and so is either
//! key on a `whole_machine` lease (the scheduler books the whole worker for one). The
//! platform's `gpu` value is the number of whole GPUs to book on top (0 when absent); a
//! value that is not a whole number is INVALID_ARGUMENT.
//!
//! The rest of the platform says which workers may run the action
//! (`kbf_caps::Request::from_platform`: `OSFamily`, `ISA`, `Arch` and kbf's capability
//! keys). A malformed one is INVALID_ARGUMENT, and so is one naming `ios.device` or an
//! `ios.device.<attribute>` key until devices are booked. One no kbf daemon can ever
//! run (an OS other than Linux or macOS, an architecture other than x86-64 or arm64) is
//! FAILED_PRECONDITION at once, with no `PreconditionFailure` detail: there is nothing
//! for the client to upload, and Bazel and Buck2 do not retry it.
//!
//! Every property name kbf reads is read without regard to case
//! (`kbf_caps::property_name`): `GPU`, `Kbf-Lease` and `osfamily` are `gpu`,
//! `kbf-lease` and `OSFamily`, never ignored. One name sent in two spellings is
//! INVALID_ARGUMENT.

use std::collections::{BTreeSet, VecDeque};
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, stream};
use kbf_auth::{Authorizers, authorize};
use kbf_caps::FromPlatformError;
use kbf_objstore::ObjectStore;
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc::{
    self, ErrorInfo, PreconditionFailure, precondition_failure::Violation,
};
use kbf_proto::reapi::execution_server::Execution;
use kbf_proto::reapi::{
    self, Action, Command, Directory, ExecuteOperationMetadata, ExecuteRequest, ExecuteResponse,
    ExecutedActionMetadata, WaitExecutionRequest, execution_stage,
};
use kbf_sched::Request;
use kbf_types::{ActionKey, Digest, LeaseKind, Platform, Qos, Resources};
use prost::Message;
use prost_types::Any;
use tokio::sync::watch;
use tonic::{Code, Request as GrpcRequest, Response, Status};
use tracing::Instrument as _;

use crate::cache::{Cache, CacheError};
use crate::meta_log::MetaLog;
use crate::wire;

/// The reserved platform key that names the lease kind (RFC 3.4).
pub const LEASE_KIND_KEY: &str = "kbf-lease";

/// The lease kinds a request may name. The default is the first.
pub const LEASE_KINDS: [&str; 2] = ["action", "whole_machine"];

/// The platform key that asks for whole GPUs, each booked for the lease alone.
pub const GPU_KEY: &str = "gpu";

/// The reserved platform key that books this many whole cores instead of one.
pub const BOOK_CPUS_KEY: &str = "kbf-book-cpus";

/// The reserved platform key that books this many GiB of memory instead of one.
pub const BOOK_MEM_GIB_KEY: &str = "kbf-book-mem-gib";

/// The `ErrorInfo.reason` of a queued operation no live worker can run.
pub const NO_WORKER_REASON: &str = "NO_WORKER_CAN_RUN";

/// The `ErrorInfo.domain` of kbf's errors.
pub const ERROR_DOMAIN: &str = "kbf";

/// What v0 books for an action that names no size: one core and 1 GiB, plus the GPUs
/// its `gpu` property asks for. `kbf-book-cpus` and `kbf-book-mem-gib` replace either.
pub const DEFAULT_RESOURCES: Resources = Resources::new(1_000, 1 << 30);

/// One execution the front hands to the scheduler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    /// What to run, keyed for dedup, with the lease kind from the platform's
    /// `kbf-lease`.
    pub request: Request,
}

/// Where an operation is, as its callers see it.
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// Waiting for room.
    Queued,
    /// Queued, and no live worker can run it now: why.
    Waiting(String),
    /// A lease for it has started on a worker.
    Executing,
    /// Finished.
    Done(Finished),
}

/// How an operation finished.
#[derive(Clone, Debug, PartialEq)]
pub enum Finished {
    /// The action ran (whatever its exit code): its result.
    Ran(Box<reapi::ActionResult>),
    /// The farm could not run it: `INTERNAL` for an infrastructure failure,
    /// `DEADLINE_EXCEEDED` for a timeout.
    Failed(rpc::Status),
}

/// A caller's handle on an operation: its name and its stage as it changes.
#[derive(Debug)]
pub struct Ticket {
    /// The operation name the caller may pass to WaitExecution. It must never name
    /// another operation, including one of a later server process: a client can still
    /// hold it after a restart (issue #154).
    pub name: String,
    /// The instance name the operation was submitted under, which WaitExecution is
    /// authorized against.
    pub instance: String,
    /// The action digest, for the operation's metadata.
    pub action: Digest,
    /// The stage, updated until [`Stage::Done`]. A sender dropped before then means the
    /// server is going away; the stream ends UNAVAILABLE.
    pub stage: watch::Receiver<Stage>,
}

/// The scheduler as the front reaches it.
pub trait Dispatch: Send + Sync + 'static {
    /// Queues `submission` (or joins its running twin) and returns the caller's ticket.
    ///
    /// # Errors
    /// The scheduler cannot take work (UNAVAILABLE).
    fn submit(&self, submission: Submission) -> Result<Ticket, Status>;

    /// A new ticket on the operation called `name`, if it is unfinished or finished
    /// within the dispatcher's retention (its ticket's stage is then already done).
    fn wait(&self, name: &str) -> Option<Ticket>;
}

/// Tells the `Execution` service the server is going away: see [`closing`].
#[derive(Debug)]
pub struct Closer(watch::Sender<bool>);

impl Closer {
    /// Ends every open Execute and WaitExecution stream that is not done UNAVAILABLE,
    /// and every one opened from now on after its first operation.
    pub fn close(&self) {
        self.0.send_replace(true);
    }
}

/// The `Execution` service's side of a [`Closer`]. A stream ends when its closer
/// closes; if the closer is dropped without closing, it never does.
#[derive(Clone, Debug)]
pub struct Closing(watch::Receiver<bool>);

impl Closing {
    /// Completes once the closer has closed; never, if it is dropped first.
    async fn closed(&mut self) {
        if self.0.wait_for(|closed| *closed).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// A [`Closer`] and the [`Closing`] to hand the `Execution` service.
#[must_use]
pub fn closing() -> (Closer, Closing) {
    let (closer, closing) = watch::channel(false);
    (Closer(closer), Closing(closing))
}

/// The `Execution` service over a [`Cache`] and a [`Dispatch`].
#[derive(Debug)]
pub struct ExecutionService<M, O, D> {
    cache: Arc<Cache<M, O>>,
    dispatch: Arc<D>,
    closing: Closing,
    authorizers: Arc<Authorizers>,
}

/// The gRPC path of Execute, as calls are logged.
const EXECUTE: &str = "/build.bazel.remote.execution.v2.Execution/Execute";

/// The gRPC path of WaitExecution, as calls are logged.
const WAIT_EXECUTION: &str = "/build.bazel.remote.execution.v2.Execution/WaitExecution";

impl<M, O, D> ExecutionService<M, O, D> {
    /// The service over `cache` and `dispatch`, whose streams end when `closing` does,
    /// each call authorized by `authorizers`.
    #[must_use]
    pub const fn with_authorizers(
        cache: Arc<Cache<M, O>>,
        dispatch: Arc<D>,
        closing: Closing,
        authorizers: Arc<Authorizers>,
    ) -> Self {
        Self {
            cache,
            dispatch,
            closing,
            authorizers,
        }
    }
}

/// The stream of operations an Execute or WaitExecution call returns.
pub type OperationStream = Pin<Box<dyn Stream<Item = Result<Operation, Status>> + Send>>;

#[tonic::async_trait]
impl<M, O, D> Execution for ExecutionService<M, O, D>
where
    M: MetaLog,
    O: ObjectStore + 'static,
    D: Dispatch,
{
    type ExecuteStream = OperationStream;
    type WaitExecutionStream = OperationStream;

    async fn execute(
        &self,
        request: GrpcRequest<ExecuteRequest>,
    ) -> Result<Response<OperationStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let instance = request.instance_name.as_str();
        authorize(&*self.authorizers.execute, &caller, EXECUTE, instance).await?;
        let span = tracing::info_span!(
            "execute",
            instance,
            caller = %caller.public_display()
        );
        self.execute_authorized(request).instrument(span).await
    }

    async fn wait_execution(
        &self,
        request: GrpcRequest<WaitExecutionRequest>,
    ) -> Result<Response<OperationStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let name = request.into_inner().name;
        let Some(ticket) = self.dispatch.wait(&name) else {
            return Err(Status::not_found(format!(
                "no operation {name:?}: finished operations are kept only briefly; \
                 Execute answers a finished one from the action cache"
            )));
        };
        let execute = &*self.authorizers.execute;
        authorize(execute, &caller, WAIT_EXECUTION, &ticket.instance).await?;
        Ok(Response::new(operations(ticket, self.closing.clone())))
    }
}

impl<M, O, D> ExecutionService<M, O, D>
where
    M: MetaLog,
    O: ObjectStore + 'static,
    D: Dispatch,
{
    /// Execute, once authorized: an action-cache hit, or the operation's stream.
    async fn execute_authorized(
        &self,
        request: ExecuteRequest,
    ) -> Result<Response<OperationStream>, Status> {
        wire::check_digest_function(request.digest_function)?;
        let action = wire::digest(request.action_digest.as_ref())?;
        if !request.skip_cache_lookup
            && let Some(result) = self.cache.action_result(&action).await?
        {
            let name = format!(
                "operations/cached/{}-{}",
                action.hash_hex(),
                action.size_bytes
            );
            tracing::debug!(%action, "answered from the action cache");
            let done = operation(
                &name,
                &action,
                &Stage::Done(Finished::Ran(Box::new(result))),
                true,
            );
            return Ok(Response::new(Box::pin(stream::iter([Ok(done)]))));
        }
        let submission = self.submission(request.instance_name, action).await?;
        let ticket = self.dispatch.submit(submission)?;
        tracing::debug!(%action, operation = %ticket.name, "submitted");
        Ok(Response::new(operations(ticket, self.closing.clone())))
    }
}

impl<M, O, D> ExecutionService<M, O, D>
where
    M: MetaLog,
    O: ObjectStore,
{
    /// Reads the action and checks its inputs are all held; the scheduler's request.
    async fn submission(&self, instance: String, action: Digest) -> Result<Submission, Status> {
        let Some(bytes) = self.read_input(&action).await? else {
            return Err(missing(&[action]));
        };
        let decoded = Action::decode(bytes).map_err(|e| {
            Status::invalid_argument(format!("action {action} does not decode: {e}"))
        })?;
        let command_digest = required(decoded.command_digest.as_ref(), "command_digest")?;
        let root = required(decoded.input_root_digest.as_ref(), "input_root_digest")?;

        let mut absent = Vec::new();
        let command = match self.read_input(&command_digest).await? {
            Some(bytes) => Some(Command::decode(bytes).map_err(|e| {
                Status::invalid_argument(format!("command {command_digest} does not decode: {e}"))
            })?),
            None => {
                absent.push(command_digest);
                None
            }
        };
        absent.extend(self.missing_inputs(root).await?);
        if !absent.is_empty() {
            return Err(missing(&absent));
        }

        let platform = properties(platform(&decoded, command.as_ref()))?;
        let kind = lease_kind(&platform)?;
        let gpus = gpus(&platform)?;
        let booked = booking(&platform, kind)?;
        let needs = needs(&platform)?;
        Ok(Submission {
            request: Request {
                key: ActionKey { instance, action },
                qos: Qos::Ci,
                kind,
                resources: booked.with_gpus(gpus),
                hermetic: true,
                do_not_cache: decoded.do_not_cache,
                needs,
            },
        })
    }

    /// Every blob of the input tree under `root` that is not held: directories that
    /// cannot be read, and files FindMissingBlobs reports.
    async fn missing_inputs(&self, root: Digest) -> Result<Vec<Digest>, Status> {
        let mut absent = Vec::new();
        let mut files = BTreeSet::new();
        let mut seen = BTreeSet::from([root]);
        let mut queue = VecDeque::from([root]);
        while let Some(digest) = queue.pop_front() {
            let Some(bytes) = self.read_input(&digest).await? else {
                absent.push(digest);
                continue;
            };
            let directory = Directory::decode(bytes).map_err(|e| {
                Status::invalid_argument(format!("input directory {digest} does not decode: {e}"))
            })?;
            for file in &directory.files {
                files.insert(wire::digest(file.digest.as_ref())?);
            }
            for child in &directory.directories {
                let child = wire::digest(child.digest.as_ref())?;
                if seen.insert(child) {
                    queue.push_back(child);
                }
            }
        }
        let files: Vec<Digest> = files.into_iter().collect();
        absent.extend(self.cache.find_missing(&files).await?);
        Ok(absent)
    }

    /// A blob's bytes, or `None` if it is not held. Anything else (unreachable, store
    /// down) fails the call: the input exists, so it is not MISSING.
    async fn read_input(&self, digest: &Digest) -> Result<Option<Bytes>, Status> {
        match self.cache.read_blob(digest).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(CacheError::NotFound(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// The action's platform. REAPI 2.2 moved it into the `Action`; older clients set it
/// on the `Command` only, which REAPI still asks servers to read.
#[allow(deprecated)]
fn platform<'a>(action: &'a Action, command: Option<&'a Command>) -> Option<&'a reapi::Platform> {
    action
        .platform
        .as_ref()
        .filter(|p| !p.properties.is_empty())
        .or_else(|| command.and_then(|c| c.platform.as_ref()))
}

fn required(d: Option<&reapi::Digest>, field: &str) -> Result<Digest, Status> {
    match d {
        Some(d) => wire::digest(Some(d)),
        None => Err(Status::invalid_argument(format!(
            "the action has no {field}"
        ))),
    }
}

/// The platform's properties that kbf reads, each under the name kbf reads it by
/// (`kbf_caps::property_name`), or INVALID_ARGUMENT for a repeated one, also when
/// repeated in another spelling (`gpu` and `GPU`).
fn properties(platform: Option<&reapi::Platform>) -> Result<Platform, Status> {
    let properties = platform.map_or(&[][..], |p| p.properties.as_slice());
    let sent = Platform::from_properties(properties.iter().map(|p| (&*p.name, &*p.value)))
        .map_err(|e| Status::invalid_argument(e.to_string()))?;
    let mut read = Platform::new();
    for (name, value) in sent.canonical() {
        let Some(key) = kbf_caps::property_name(name) else {
            continue;
        };
        read.insert(&*key, value).map_err(|_| {
            Status::invalid_argument(format!(
                "platform property {key:?} is given twice, in two spellings (one is {name:?})"
            ))
        })?;
    }
    Ok(read)
}

/// The number of GPUs a platform asks for, or INVALID_ARGUMENT.
fn gpus(platform: &Platform) -> Result<u64, Status> {
    let Some(n) = platform.get(GPU_KEY) else {
        return Ok(0);
    };
    n.parse().map_err(|_| {
        Status::invalid_argument(format!(
            "platform property {GPU_KEY}={n:?} is not a whole number of GPUs"
        ))
    })
}

/// What the lease books: [`DEFAULT_RESOURCES`], with `kbf-book-cpus` whole cores and
/// `kbf-book-mem-gib` GiB in place of the default where the platform names them, or
/// INVALID_ARGUMENT for a value that is not a whole number of at least 1 in plain
/// digits ([`starts_plain`]), one too large to book, or either key on a `whole_machine`
/// lease.
fn booking(platform: &Platform, kind: LeaseKind) -> Result<Resources, Status> {
    let mut booked = DEFAULT_RESOURCES;
    for (key, unit, slot) in [
        (BOOK_CPUS_KEY, 1_000, &mut booked.cpu_millis),
        (BOOK_MEM_GIB_KEY, 1 << 30, &mut booked.memory_bytes),
    ] {
        let Some(value) = platform.get(key) else {
            continue;
        };
        if kind != LeaseKind::Action {
            return Err(Status::invalid_argument(format!(
                "platform property {key} sizes an {:?} lease only, not a {:?} one",
                LeaseKind::Action.name(),
                kind.name()
            )));
        }
        *slot = Some(value)
            .filter(|v| starts_plain(v))
            .and_then(|v| v.parse::<u64>().ok())
            .and_then(|n| n.checked_mul(unit))
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "platform property {key}={value:?} is not a whole number from 1 to {} \
                     in plain digits",
                    u64::MAX / unit
                ))
            })?;
    }
    Ok(booked)
}

/// Whether `value` starts with a digit from 1 to 9. A size must also parse as a `u64`,
/// which takes nothing but ASCII digits after an optional `+`; together that leaves
/// one spelling of each size, with no sign and no leading zero. Parsing alone also
/// takes `+4` and `04`, which book the same as `4` under a different action digest,
/// and so a different cache entry.
fn starts_plain(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|b| (b'1'..=b'9').contains(&b))
}

/// What a worker must offer to run an action with `platform`: INVALID_ARGUMENT for a
/// malformed platform, FAILED_PRECONDITION for one no kbf daemon can ever run.
fn needs(platform: &Platform) -> Result<kbf_caps::Request, Status> {
    kbf_caps::Request::from_platform(platform.canonical()).map_err(|e| match e {
        FromPlatformError::Invalid(e) => {
            Status::invalid_argument(format!("platform properties: {e}"))
        }
        FromPlatformError::NeverServed(why) => Status::failed_precondition(why),
    })
}

/// The lease kind a platform names, or INVALID_ARGUMENT for one not in
/// [`LEASE_KINDS`] (`vm` among them, until the front books it).
fn lease_kind(platform: &Platform) -> Result<LeaseKind, Status> {
    let Some(kind) = platform.get(LEASE_KIND_KEY) else {
        return Ok(LeaseKind::Action);
    };
    LeaseKind::from_name(kind)
        .filter(|k| LEASE_KINDS.contains(&k.name()))
        .ok_or_else(|| {
            Status::invalid_argument(format!(
                "platform property {LEASE_KIND_KEY}={kind:?} is not one of {LEASE_KINDS:?}"
            ))
        })
}

/// FAILED_PRECONDITION with one `MISSING` violation per blob, as REAPI asks.
fn missing(digests: &[Digest]) -> Status {
    let failure = PreconditionFailure {
        violations: digests
            .iter()
            .map(|d| Violation {
                r#type: "MISSING".to_owned(),
                subject: format!("blobs/{}/{}", d.hash_hex(), d.size_bytes),
                description: String::new(),
            })
            .collect(),
    };
    let message = format!("{} input blob(s) are not in the CAS", digests.len());
    let details = rpc::Status {
        code: Code::FailedPrecondition as i32,
        message: message.clone(),
        details: vec![any(
            "google.rpc.PreconditionFailure",
            failure.encode_to_vec(),
        )],
    };
    Status::with_details(
        Code::FailedPrecondition,
        message,
        Bytes::from(details.encode_to_vec()),
    )
}

fn any(type_name: &str, value: Vec<u8>) -> Any {
    Any {
        type_url: format!("type.googleapis.com/{type_name}"),
        value,
    }
}

/// The operation `name` at `stage`.
fn operation(name: &str, action: &Digest, stage: &Stage, cached: bool) -> Operation {
    let mut partial = None;
    let (value, result) = match stage {
        Stage::Queued => (execution_stage::Value::Queued, None),
        Stage::Waiting(why) => {
            let info = ErrorInfo {
                reason: NO_WORKER_REASON.to_owned(),
                domain: ERROR_DOMAIN.to_owned(),
                metadata: [("why".to_owned(), why.clone())].into(),
            };
            partial = Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![any("google.rpc.ErrorInfo", info.encode_to_vec())],
                ..ExecutedActionMetadata::default()
            });
            (execution_stage::Value::Queued, None)
        }
        Stage::Executing => (execution_stage::Value::Executing, None),
        Stage::Done(finished) => {
            let response = match finished {
                Finished::Ran(result) => ExecuteResponse {
                    result: Some((**result).clone()),
                    cached_result: cached,
                    status: Some(wire::rpc_ok()),
                    ..ExecuteResponse::default()
                },
                Finished::Failed(status) => ExecuteResponse {
                    status: Some(status.clone()),
                    ..ExecuteResponse::default()
                },
            };
            let response = any(
                "build.bazel.remote.execution.v2.ExecuteResponse",
                response.encode_to_vec(),
            );
            (
                execution_stage::Value::Completed,
                Some(operation::Result::Response(response)),
            )
        }
    };
    let metadata = ExecuteOperationMetadata {
        stage: value as i32,
        action_digest: Some(wire::digest_to_proto(action)),
        digest_function: reapi::digest_function::Value::Sha256 as i32,
        partial_execution_metadata: partial,
        ..ExecuteOperationMetadata::default()
    };
    Operation {
        name: name.to_owned(),
        metadata: Some(any(
            "build.bazel.remote.execution.v2.ExecuteOperationMetadata",
            metadata.encode_to_vec(),
        )),
        done: result.is_some(),
        result,
    }
}

/// The operation as its stage changes: the stage now, then each change, ending after
/// the done operation. A stage that stops changing before it is done (the operation was
/// abandoned), or `closing` closing first (the server is going away), ends the stream
/// UNAVAILABLE. A change already there is sent before a close is noticed.
fn operations(ticket: Ticket, closing: Closing) -> OperationStream {
    let Ticket {
        name,
        action,
        stage,
        ..
    } = ticket;
    let state = Some((stage, closing, true));
    Box::pin(stream::unfold(state, move |state| {
        let name = name.clone();
        async move {
            let (mut stage, mut closing, first) = state?;
            if !first {
                let gone = tokio::select! {
                    biased;
                    changed = stage.changed() => changed
                        .is_err()
                        .then(|| format!("operation {name} was abandoned")),
                    () = closing.closed() => Some(format!(
                        "the server is shutting down; operation {name} ends here, Execute it again"
                    )),
                };
                if let Some(why) = gone {
                    return Some((Err(Status::unavailable(why)), None));
                }
            }
            let now = stage.borrow_and_update().clone();
            let next = (!matches!(now, Stage::Done(_))).then_some((stage, closing, false));
            Some((Ok(operation(&name, &action, &now, false)), next))
        }
    }))
}
