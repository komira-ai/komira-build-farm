//! `Execution` over gRPC, against the in-process cache and a scripted dispatch that
//! stands in for the scheduler: what Execute checks, what it submits, and how it streams
//! an operation's stages.

mod common;
#[path = "execution/platform.rs"]
mod platform_tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use common::{Blob, Farm};
use kbf_front::{
    DEFAULT_RESOURCES, Dispatch, ERROR_DOMAIN, Finished, NO_WORKER_REASON, Stage, Submission,
    Ticket,
};
use kbf_meta::Role;
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc::{self, PreconditionFailure};
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_proto::reapi::{
    self, Action, ActionResult, Command, Directory, DirectoryNode, ExecuteOperationMetadata,
    ExecuteRequest, ExecuteResponse, FileNode, GetCapabilitiesRequest, Platform,
    WaitExecutionRequest, platform,
};
use kbf_types::{ActionKey, LeaseKind, Qos};
use prost::Message;
use tokio::sync::watch;
use tonic::{Code, Status, Streaming};

/// A dispatch that records submissions and lets the test move each operation's stage.
#[derive(Default)]
struct Script {
    submitted: Mutex<Vec<Submission>>,
    stages: Mutex<Vec<(String, watch::Sender<Stage>)>>,
    refuse: AtomicBool,
    /// Whether a submission joins a twin already executing (its stage starts at
    /// `Executing`) instead of starting `Queued`.
    joined: AtomicBool,
}

impl Script {
    fn submitted(&self) -> Vec<Submission> {
        self.submitted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Moves operation `n` (in submission order) to `stage`.
    fn set(&self, n: usize, stage: Stage) {
        self.stages.lock().unwrap_or_else(PoisonError::into_inner)[n]
            .1
            .send_replace(stage);
    }

    /// Drops operation `n`'s sender, as a server going away would.
    fn abandon(&self, n: usize) {
        let mut stages = self.stages.lock().unwrap_or_else(PoisonError::into_inner);
        let name = stages[n].0.clone();
        let (gone, _) = watch::channel(Stage::Queued);
        stages[n] = (name, gone);
    }
}

impl Dispatch for Script {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(Status::unavailable("no scheduler"));
        }
        let mut stages = self.stages.lock().unwrap_or_else(PoisonError::into_inner);
        let name = format!("operations/{}", stages.len());
        let first = if self.joined.load(Ordering::SeqCst) {
            Stage::Executing
        } else {
            Stage::Queued
        };
        let (tx, rx) = watch::channel(first);
        stages.push((name.clone(), tx));
        let action = submission.request.key.action;
        let instance = submission.request.key.instance.clone();
        self.submitted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(submission);
        Ok(Ticket {
            name,
            instance,
            action,
            stage: rx,
        })
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        let stages = self.stages.lock().unwrap_or_else(PoisonError::into_inner);
        let (name, tx) = stages.iter().find(|(n, _)| n == name)?;
        Some(Ticket {
            name: name.clone(),
            instance: String::new(),
            action: kbf_segments::sha256(b"unused"),
            stage: tx.subscribe(),
        })
    }
}

/// An action, its command and input tree, as blobs.
struct Job {
    action: Blob,
    blobs: Vec<Blob>,
}

fn property(name: &str, value: &str) -> platform::Property {
    platform::Property {
        name: name.to_owned(),
        value: value.to_owned(),
    }
}

fn job(argv: &str, action_platform: &[(&str, &str)], do_not_cache: bool) -> Job {
    let input = Blob::new(format!("input of {argv}"));
    let root = Blob::of(&Directory {
        files: vec![FileNode {
            name: "in".to_owned(),
            digest: Some(input.proto.clone()),
            ..Default::default()
        }],
        ..Default::default()
    });
    let command = Blob::of(&Command {
        arguments: vec![argv.to_owned()],
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(root.proto.clone()),
        do_not_cache,
        platform: Some(Platform {
            properties: action_platform
                .iter()
                .map(|(n, v)| property(n, v))
                .collect(),
        }),
        ..Default::default()
    });
    Job {
        blobs: vec![action.clone(), command, root, input],
        action,
    }
}

/// How long a step that must happen may take before the test calls it hung: a hang
/// deadline, never a speed bound.
const HANG: std::time::Duration = std::time::Duration::from_secs(60);

async fn start(farm: &Farm, action: &Blob) -> Result<Streaming<Operation>, Status> {
    farm.exec()
        .execute(ExecuteRequest {
            instance_name: "main".to_owned(),
            action_digest: Some(action.proto.clone()),
            ..Default::default()
        })
        .await
        .map(tonic::Response::into_inner)
}

async fn next(ops: &mut Streaming<Operation>) -> Result<Option<Operation>, Status> {
    tokio::time::timeout(std::time::Duration::from_secs(5), ops.message())
        .await
        .expect("an update in time")
}

fn stage(op: &Operation) -> i32 {
    let meta = op.metadata.as_ref().expect("metadata");
    assert_eq!(
        meta.type_url,
        "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteOperationMetadata"
    );
    ExecuteOperationMetadata::decode(meta.value.as_slice())
        .expect("metadata decodes")
        .stage
}

fn response(op: &Operation) -> ExecuteResponse {
    assert!(op.done);
    let Some(operation::Result::Response(any)) = &op.result else {
        panic!("no response in {op:?}");
    };
    assert_eq!(
        any.type_url,
        "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse"
    );
    ExecuteResponse::decode(any.value.as_slice()).expect("response decodes")
}

/// The MISSING subjects of a FAILED_PRECONDITION, in order.
fn missing_subjects(status: &Status) -> Vec<String> {
    assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
    let details = rpc::Status::decode(status.details()).expect("details decode");
    assert_eq!(details.code, Code::FailedPrecondition as i32);
    let [any] = details.details.as_slice() else {
        panic!("one detail expected: {details:?}");
    };
    assert_eq!(
        any.type_url,
        "type.googleapis.com/google.rpc.PreconditionFailure"
    );
    PreconditionFailure::decode(any.value.as_slice())
        .expect("a PreconditionFailure")
        .violations
        .into_iter()
        .map(|v| {
            assert_eq!(v.r#type, "MISSING");
            v.subject
        })
        .collect()
}

fn subject(blob: &Blob) -> String {
    format!(
        "blobs/{}/{}",
        blob.digest.hash_hex(),
        blob.digest.size_bytes
    )
}

/// Catches: a request sent to the scheduler with the wrong key, QoS, size, fence or
/// cache flag; an operation stream that skips a stage, reports the wrong stage, does
/// not end after the done operation, or answers without the result or with
/// `cached_result` set for work that ran.
#[tokio::test]
async fn execute_submits_and_streams_each_stage() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let job = job("build", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;

    let mut ops = start(&farm, &job.action).await.expect("Execute");
    let queued = next(&mut ops).await.expect("healthy").expect("an update");
    assert_eq!(stage(&queued), ExecStage::Queued as i32);
    assert!(!queued.done && queued.result.is_none());
    assert_eq!(queued.name, "operations/0");
    let [submission] = script.submitted().try_into().expect("one submission");
    assert_eq!(
        submission.request.key,
        ActionKey {
            instance: "main".to_owned(),
            action: job.action.digest
        }
    );
    assert_eq!(submission.request.qos, Qos::Ci);
    assert_eq!(submission.request.resources, DEFAULT_RESOURCES);
    assert!(submission.request.hermetic && !submission.request.do_not_cache);
    assert_eq!(submission.request.kind, LeaseKind::Action);

    script.set(0, Stage::Executing);
    let executing = next(&mut ops).await.expect("healthy").expect("an update");
    assert_eq!(stage(&executing), ExecStage::Executing as i32);

    let result = ActionResult {
        exit_code: 3,
        ..Default::default()
    };
    script.set(0, Stage::Done(Finished::Ran(Box::new(result.clone()))));
    let last = next(&mut ops).await.expect("healthy").expect("an update");
    assert_eq!(stage(&last), ExecStage::Completed as i32);
    let answer = response(&last);
    assert_eq!(answer.result, Some(result));
    assert!(!answer.cached_result);
    assert_eq!(answer.status.map(|s| s.code), Some(Code::Ok as i32));
    assert_eq!(
        next(&mut ops).await.expect("healthy"),
        None,
        "more after done"
    );
}

/// Catches: a failure answered without its status (or with a result), a timeout's
/// partial stdout and stderr dropped or answered with OK (issue #45), and a stream
/// that hangs, or ends without an error, when its operation is abandoned.
#[tokio::test]
async fn failures_and_abandoned_operations_end_the_stream() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let job = job("fails", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;

    let mut failing = start(&farm, &job.action).await.expect("Execute");
    next(&mut failing).await.expect("healthy");
    let status = rpc::Status {
        code: Code::Internal as i32,
        message: "lost".to_owned(),
        details: Vec::new(),
    };
    script.set(0, Stage::Done(Finished::Failed(status.clone())));
    let answer = response(&next(&mut failing).await.expect("healthy").expect("done"));
    assert_eq!(answer.status, Some(status));
    assert_eq!(answer.result, None);

    let mut timed_out = start(&farm, &job.action).await.expect("Execute");
    next(&mut timed_out).await.expect("healthy");
    let status = rpc::Status {
        code: Code::DeadlineExceeded as i32,
        message: "the action timed out".to_owned(),
        details: Vec::new(),
    };
    let partial = ActionResult {
        stdout_digest: Some(Blob::new("started").proto),
        ..Default::default()
    };
    script.set(
        1,
        Stage::Done(Finished::TimedOut {
            status: status.clone(),
            partial: Box::new(partial.clone()),
        }),
    );
    let answer = response(&next(&mut timed_out).await.expect("healthy").expect("done"));
    assert_eq!(answer.status, Some(status));
    assert_eq!(answer.result, Some(partial));
    assert!(!answer.cached_result);

    let mut abandoned = start(&farm, &job.action).await.expect("Execute");
    next(&mut abandoned).await.expect("healthy");
    script.abandon(2);
    let gone = next(&mut abandoned)
        .await
        .expect_err("an abandoned operation");
    assert_eq!(gone.code(), Code::Unavailable);
}

/// Catches (issue #168): a server going away that leaves open Execute and
/// WaitExecution streams to die with the connection (UNKNOWN on the client) instead of
/// ending them UNAVAILABLE, which clients retry; a WaitExecution opened after the close
/// that stays open; an Execute after the close that is not refused; and a done operation already there lost to the close (each of eight
/// streams must get its done operation, so a close noticed first fails all but once in
/// 256 runs); and a WaitExecution after the close on an operation kept finished (issue
/// #165) ended UNAVAILABLE or left open instead of answered.
#[tokio::test]
async fn closing_ends_open_streams_unavailable() {
    let script = Arc::new(Script::default());
    let (closer, closing) = kbf_front::closing();
    let farm = Farm::with_execution_until(Arc::clone(&script), closing).await;
    let job = job("closed", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;

    let mut open = start(&farm, &job.action).await.expect("Execute");
    next(&mut open).await.expect("healthy").expect("queued");
    let mut waited = farm
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: "operations/0".to_owned(),
        })
        .await
        .expect("WaitExecution")
        .into_inner();
    next(&mut waited).await.expect("healthy").expect("queued");
    let mut finished = Vec::new();
    for n in 1..=8 {
        let mut ops = start(&farm, &job.action).await.expect("Execute");
        next(&mut ops).await.expect("healthy").expect("queued");
        script.set(n, Stage::Done(Finished::Ran(Box::default())));
        finished.push(ops);
    }

    closer.close();
    for (what, ops) in [("Execute", &mut open), ("WaitExecution", &mut waited)] {
        let gone = next(ops).await.expect_err(what);
        assert_eq!(gone.code(), Code::Unavailable, "{what}: {gone:?}");
        assert!(gone.message().contains("shutting down"), "{what}: {gone:?}");
        assert!(gone.message().contains("operations/0"), "{what}: {gone:?}");
    }
    for ops in &mut finished {
        let done = next(ops)
            .await
            .expect("healthy")
            .expect("the done operation");
        assert!(done.done, "{done:?}");
        assert_eq!(next(ops).await.expect("healthy"), None);
    }

    // A WaitExecution after the close on an operation kept finished (issue #165) is
    // answered with its done operation, not UNAVAILABLE, and the stream ends.
    let mut kept = farm
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: "operations/1".to_owned(),
        })
        .await
        .expect("WaitExecution after the close")
        .into_inner();
    let done = next(&mut kept).await.expect("healthy").expect("done");
    assert!(done.done, "{done:?}");
    assert_eq!(next(&mut kept).await.expect("healthy"), None);

    // An Execute after the close is refused at open (as in a drain). A WaitExecution
    // on a running operation opens, sends its first operation and ends UNAVAILABLE.
    let refused = start(&farm, &job.action).await.expect_err("Execute after");
    assert_eq!(refused.code(), Code::Unavailable, "{refused:?}");
    let mut late = farm
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: "operations/0".to_owned(),
        })
        .await
        .expect("WaitExecution after the close")
        .into_inner();
    next(&mut late).await.expect("healthy").expect("queued");
    let gone = next(&mut late).await.expect_err("opened after the close");
    assert_eq!(gone.code(), Code::Unavailable, "{gone:?}");
}

/// Catches (komira-build-farm#791): a drain that ends open streams (Buck2 does not
/// recover a broken Execute stream, so every action in flight would fail) instead of
/// leaving them open until their action is done; a drain that lets a new Execute in;
/// and a drain that refuses a WaitExecution on an operation still running. The
/// planted mutant is `Closer::drain` closing: the open stream then ends UNAVAILABLE
/// at its first read after the drain.
#[tokio::test]
async fn a_drain_keeps_open_streams_and_refuses_new_executes() {
    let script = Arc::new(Script::default());
    let (closer, closing) = kbf_front::closing();
    let farm = Farm::with_execution_until(Arc::clone(&script), closing).await;
    let job = job("drained", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;

    let mut open = start(&farm, &job.action).await.expect("Execute");
    next(&mut open).await.expect("healthy").expect("queued");

    closer.drain();
    script.set(0, Stage::Executing);
    let running = next(&mut open)
        .await
        .expect("the stream survives the drain")
        .expect("executing");
    assert_eq!(stage(&running), ExecStage::Executing as i32);

    let refused = start(&farm, &job.action).await.expect_err("a new Execute");
    assert_eq!(refused.code(), Code::Unavailable, "{refused:?}");
    assert!(refused.message().contains("draining"), "{refused:?}");
    assert_eq!(script.submitted().len(), 1, "the refused call was queued");

    let mut waited = farm
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: "operations/0".to_owned(),
        })
        .await
        .expect("WaitExecution during the drain")
        .into_inner();
    next(&mut waited)
        .await
        .expect("healthy")
        .expect("executing");

    script.set(0, Stage::Done(Finished::Ran(Box::default())));
    for (what, ops) in [("Execute", &mut open), ("WaitExecution", &mut waited)] {
        let done = next(ops).await.expect(what).expect("the done operation");
        assert!(done.done, "{what}: {done:?}");
        assert_eq!(next(ops).await.expect(what), None, "{what}");
    }
}

/// A dispatch whose first `submit` stops inside the call until the test lets it go,
/// over a [`Script`]: the window between an Execute's drain check and its stream.
struct Gated {
    script: Arc<Script>,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Dispatch for Gated {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let entered = self
            .entered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let release = self
            .release
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(entered) = entered {
            let _ = entered.send(());
        }
        if let Some(release) = release {
            // Blocks this thread, which is why the test runs on several.
            let _ = release.recv();
        }
        self.script.submit(submission)
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        self.script.wait(name)
    }
}

/// Catches an Execute that passed the drain check and is then refused, or ended, by a
/// drain that began while its `submit` was running: the call was taken before the
/// drain, so its stream is kept until its action is done, like every stream open at the
/// drain (komira-build-farm#791: Buck2 does not recover a stream that breaks). The
/// drain begins, and the next Execute is refused, while the first is inside `submit`.
/// Mutant: a second `closing.draining()` check after `submit` (returning UNAVAILABLE)
/// fails the first `expect` below. `Dispatch::submit` is synchronous, so the test needs
/// a thread for the blocked handler and one for itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drain_that_begins_during_submit_keeps_the_stream() {
    let script = Arc::new(Script::default());
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release_tx, release) = std::sync::mpsc::channel();
    let gated = Arc::new(Gated {
        script: Arc::clone(&script),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release)),
    });
    let (closer, closing) = kbf_front::closing();
    let farm = Farm::with_execution_until(gated, closing).await;
    let job = job("drained mid-submit", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;

    let opening = start(&farm, &job.action);
    let draining = async {
        tokio::time::timeout(HANG, entered)
            .await
            .expect("the Execute reaches submit")
            .expect("the dispatch is alive");
        closer.drain();
        let refused = start(&farm, &job.action).await.expect_err("a new Execute");
        assert_eq!(refused.code(), Code::Unavailable, "{refused:?}");
        release_tx.send(()).expect("submit is waiting");
    };
    let (opened, ()) = tokio::join!(opening, draining);

    let mut open = opened.expect("the Execute taken before the drain keeps its stream");
    let queued = next(&mut open).await.expect("healthy").expect("queued");
    assert_eq!(stage(&queued), ExecStage::Queued as i32);
    script.set(0, Stage::Executing);
    let running = next(&mut open).await.expect("kept").expect("executing");
    assert_eq!(stage(&running), ExecStage::Executing as i32);
    script.set(0, Stage::Done(Finished::Ran(Box::default())));
    let done = next(&mut open).await.expect("kept").expect("done");
    assert!(done.done, "{done:?}");
    assert_eq!(next(&mut open).await.expect("ends"), None);
    assert_eq!(
        script.submitted().len(),
        1,
        "only the first call was queued"
    );
}

/// Catches `kbf_front_executes_started_total` counting a call the drain refused (a
/// rollout waiting for it to stand still would never see it flat), missing a call that
/// was taken (a cache hit is one), or counting a WaitExecution; and
/// `kbf_front_open_executions` not following the streams a client holds over the wire
/// (a WaitExecution counts, one the client drops lowers it) or missing from `/metrics`.
/// Mutants: the counter raised before the drain check; raised in `wait_execution`.
#[tokio::test]
async fn started_counts_the_executes_taken_and_open_executions_follows_the_streams() {
    let script = Arc::new(Script::default());
    let (closer, closing) = kbf_front::closing();
    let farm = Farm::with_execution_until(Arc::clone(&script), closing).await;
    let metrics = &farm.cache.metrics().execute;
    let job = job("counted", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    assert_eq!(
        (metrics.started.get(), metrics.open_executions.get()),
        (0, 0)
    );

    let mut ops = start(&farm, &job.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("QUEUED");
    assert_eq!(
        (metrics.started.get(), metrics.open_executions.get()),
        (1, 1)
    );
    let wait = || async {
        farm.exec()
            .wait_execution(WaitExecutionRequest {
                name: "operations/0".to_owned(),
            })
            .await
            .expect("WaitExecution")
            .into_inner()
    };
    let mut waited = wait().await;
    next(&mut waited).await.expect("healthy").expect("QUEUED");
    assert_eq!(
        (metrics.started.get(), metrics.open_executions.get()),
        (1, 2),
        "a WaitExecution is a stream, not a start"
    );

    closer.drain();
    start(&farm, &job.action).await.expect_err("refused");
    assert_eq!(metrics.started.get(), 1, "a refused Execute was counted");

    let text = farm.cache.metrics().render();
    for family in [
        "kbf_front_open_executions 2",
        "kbf_front_executes_started_total 1",
    ] {
        assert!(
            text.lines().any(|l| l == family),
            "{family} not in:\n{text}"
        );
    }
    drop(waited);
    for _ in 0..200 {
        if metrics.open_executions.get() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(
        metrics.open_executions.get(),
        1,
        "a dropped stream stayed open"
    );
    script.set(0, Stage::Done(Finished::Ran(Box::default())));
    next(&mut ops).await.expect("healthy").expect("done");
    assert_eq!(next(&mut ops).await.expect("healthy"), None);
    assert_eq!(metrics.open_executions.get(), 0);
}

/// Catches: an Execute that submits an action whose result is cached, answers a hit
/// without `cached_result`, or ignores `skip_cache_lookup`.
#[tokio::test]
async fn a_cache_hit_is_answered_without_submitting() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let job = job("cached", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    let result = ActionResult::default();
    farm.cache
        .write_action_result(Role::Daemon, job.action.digest, &result)
        .await
        .expect("write");

    let mut ops = start(&farm, &job.action).await.expect("Execute");
    let only = next(&mut ops).await.expect("healthy").expect("an update");
    let answer = response(&only);
    assert!(answer.cached_result);
    assert_eq!(answer.result, Some(result));
    assert_eq!(stage(&only), ExecStage::Completed as i32);
    assert_eq!(next(&mut ops).await.expect("healthy"), None);
    assert!(script.submitted().is_empty(), "a hit was submitted");

    farm.exec()
        .execute(ExecuteRequest {
            action_digest: Some(job.action.proto.clone()),
            skip_cache_lookup: true,
            ..Default::default()
        })
        .await
        .expect("Execute");
    assert_eq!(script.submitted().len(), 1, "skip_cache_lookup ignored");
}

/// Catches: an Execute that is not counted, or whose phases are not each recorded once
/// (prepare before the scheduler, the submit call, the time queued, the whole call);
/// `kbf_execute_in_flight` or `kbf_execute_queued` left raised when the stream ends or
/// when the operation starts executing; a cache hit that is counted as an error or as
/// a call to the scheduler; and the families missing from `/metrics`.
#[tokio::test]
async fn execute_is_measured_phase_by_phase() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let metrics = &farm.cache.metrics().execute;
    let job = job("measured", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;

    let mut ops = start(&farm, &job.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("QUEUED");
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (1, 1));
    assert_eq!(metrics.requests.get(), 0, "counted before it ended");
    assert_eq!((metrics.prepare.count(), metrics.dispatch.count()), (1, 1));
    assert_eq!(metrics.queued_seconds.count(), 0);

    script.set(0, Stage::Executing);
    next(&mut ops).await.expect("healthy").expect("EXECUTING");
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (1, 0));
    assert_eq!(metrics.queued_seconds.count(), 1);

    script.set(0, Stage::Done(Finished::Ran(Box::default())));
    next(&mut ops).await.expect("healthy").expect("done");
    assert_eq!(next(&mut ops).await.expect("healthy"), None);
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (0, 0));
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (1, 0));
    for (name, count) in [
        ("prepare", metrics.prepare.count()),
        ("dispatch", metrics.dispatch.count()),
        ("queued", metrics.queued_seconds.count()),
        ("total", metrics.total.count()),
    ] {
        assert_eq!(count, 1, "{name}");
    }

    // A cache hit is a call that succeeded, and never reached the scheduler.
    let hit = self::job("hit", &[], false);
    farm.upload(&hit.blobs.iter().collect::<Vec<_>>()).await;
    farm.cache
        .write_action_result(Role::Daemon, hit.action.digest, &ActionResult::default())
        .await
        .expect("write");
    let mut ops = start(&farm, &hit.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("done");
    assert_eq!(next(&mut ops).await.expect("healthy"), None);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (2, 0));
    assert_eq!(
        metrics.prepare.count(),
        1,
        "a hit does not reach the scheduler"
    );
    assert_eq!(metrics.total.count(), 2);

    let text = farm.cache.metrics().render();
    for family in [
        "kbf_execute_requests_total 2",
        "kbf_execute_errors_total 0",
        "kbf_execute_in_flight 0",
        "kbf_execute_queued 0",
        "kbf_execute_prepare_seconds_count 1",
        "kbf_execute_dispatch_seconds_count 1",
        "kbf_execute_queued_seconds_count 1",
        "kbf_execute_total_seconds_count 2",
    ] {
        assert!(
            text.lines().any(|l| l == family),
            "{family} not in:\n{text}"
        );
    }
}

/// Catches: an Execute that fails before the scheduler (an input missing, so
/// FAILED_PRECONDITION; the scheduler refusing, so UNAVAILABLE) not counted as an error,
/// one observed as reaching the scheduler when it did not, `in_flight` left raised by
/// an error return, and a caller that leaves a queued operation not counted as an
/// error with its gauges lowered.
#[tokio::test]
async fn failed_and_abandoned_executes_count_as_errors() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let metrics = &farm.cache.metrics().execute;

    let absent = Blob::new("an action nobody uploaded");
    let status = start(&farm, &absent).await.expect_err("missing");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (1, 1));
    assert_eq!(metrics.in_flight.get(), 0);
    assert_eq!((metrics.prepare.count(), metrics.dispatch.count()), (0, 0));
    assert_eq!(metrics.total.count(), 1);

    // The action, its command and its input tree are held, but one input file is not.
    let lacking = job("lacking an input", &[], false);
    let [held @ .., input] = lacking.blobs.as_slice() else {
        panic!("a job has blobs");
    };
    farm.upload(&held.iter().collect::<Vec<_>>()).await;
    let status = start(&farm, &lacking.action)
        .await
        .expect_err("missing input");
    assert_eq!(missing_subjects(&status), [subject(input)]);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (2, 2));
    assert_eq!((metrics.prepare.count(), metrics.dispatch.count()), (0, 0));
    assert_eq!(metrics.in_flight.get(), 0);

    let job = job("refused", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    script.refuse.store(true, Ordering::SeqCst);
    let status = start(&farm, &job.action).await.expect_err("refused");
    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (3, 3));
    assert_eq!((metrics.prepare.count(), metrics.dispatch.count()), (1, 1));
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (0, 0));

    script.refuse.store(false, Ordering::SeqCst);
    let mut ops = start(&farm, &job.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("QUEUED");
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (1, 1));
    drop(ops);
    for _ in 0..200 {
        if metrics.in_flight.get() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (0, 0));
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (4, 4));
}

/// Catches: an Execute that joins a twin already executing (its first update is
/// EXECUTING) leaving `kbf_execute_queued` raised, or not observing `queued_seconds`
/// (a call that never saw QUEUED would then be absent from the histogram).
#[tokio::test]
async fn joining_a_running_twin_leaves_no_queued_stream() {
    let script = Arc::new(Script::default());
    script.joined.store(true, Ordering::SeqCst);
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let metrics = &farm.cache.metrics().execute;
    let job = job("joined", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;

    let mut ops = start(&farm, &job.action).await.expect("Execute");
    let first = next(&mut ops).await.expect("healthy").expect("an update");
    assert_eq!(stage(&first), ExecStage::Executing as i32);
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (1, 0));
    assert_eq!(metrics.queued_seconds.count(), 1);

    script.set(0, Stage::Done(Finished::Ran(Box::default())));
    next(&mut ops).await.expect("healthy").expect("done");
    assert_eq!(next(&mut ops).await.expect("healthy"), None);
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (0, 0));
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (1, 0));
    assert_eq!(metrics.queued_seconds.count(), 1, "observed once");
}

/// Waits until `metrics.in_flight` reads `want`: a stream the client dropped is
/// dropped by the server some time later.
async fn in_flight_settles(metrics: &kbf_front::metrics::ExecuteMetrics, want: u64) {
    for _ in 0..200 {
        if metrics.in_flight.get() == want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(metrics.in_flight.get(), want, "in_flight did not settle");
}

/// Catches: a stream that ends UNAVAILABLE because its operation was abandoned, or
/// because the server is closing, not counted as an error; and either leaving
/// `in_flight` or `queued` raised.
#[tokio::test]
async fn abandoned_and_closed_streams_count_as_errors() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let metrics = &farm.cache.metrics().execute;
    let job = job("abandoned", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    let mut ops = start(&farm, &job.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("QUEUED");
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (1, 1));
    script.abandon(0);
    let gone = next(&mut ops).await.expect_err("abandoned");
    assert_eq!(gone.code(), Code::Unavailable);
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (0, 0));
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (1, 1));

    let script = Arc::new(Script::default());
    let (closer, closing) = kbf_front::closing();
    let farm = Farm::with_execution_until(Arc::clone(&script), closing).await;
    let metrics = &farm.cache.metrics().execute;
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    let mut ops = start(&farm, &job.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("QUEUED");
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (1, 1));
    closer.close();
    let gone = next(&mut ops).await.expect_err("closing");
    assert_eq!(gone.code(), Code::Unavailable);
    assert!(gone.message().contains("shutting down"), "{gone:?}");
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (0, 0));
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (1, 1));
}

/// Catches: a WaitExecution stream not counted in `kbf_execute_in_flight` (the
/// `wait()` guard not wired into the service), one that also moves `requests`,
/// `errors`, `total` or `queued`, and a dropped or finished one leaving `in_flight`
/// raised.
#[tokio::test]
async fn wait_execution_counts_only_in_flight() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let metrics = &farm.cache.metrics().execute;
    let job = job("waited for", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    let mut ops = start(&farm, &job.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("QUEUED");
    let wait = || async {
        farm.exec()
            .wait_execution(WaitExecutionRequest {
                name: "operations/0".to_owned(),
            })
            .await
            .expect("WaitExecution")
            .into_inner()
    };

    let mut waited = wait().await;
    next(&mut waited).await.expect("healthy").expect("QUEUED");
    assert_eq!(metrics.in_flight.get(), 2);
    assert_eq!(metrics.queued.get(), 1, "only the Execute is queued");

    // A WaitExecution the client leaves mid-way is not an Execute error.
    let mut dropped = wait().await;
    next(&mut dropped).await.expect("healthy").expect("QUEUED");
    assert_eq!(metrics.in_flight.get(), 3);
    drop(dropped);
    in_flight_settles(metrics, 2).await;

    script.set(0, Stage::Done(Finished::Ran(Box::default())));
    next(&mut waited).await.expect("healthy").expect("done");
    assert_eq!(next(&mut waited).await.expect("healthy"), None);
    next(&mut ops).await.expect("healthy").expect("done");
    assert_eq!(next(&mut ops).await.expect("healthy"), None);
    in_flight_settles(metrics, 0).await;
    assert_eq!(metrics.queued.get(), 0);
    assert_eq!(
        (
            metrics.requests.get(),
            metrics.errors.get(),
            metrics.total.count()
        ),
        (1, 0, 1),
        "only the Execute is a request"
    );
}

/// Catches: an Execute submitted while blobs it needs are missing (the daemon would
/// fail to fetch them), and a FAILED_PRECONDITION that names fewer than every missing
/// blob, names them in a form clients cannot parse, or checks the top of the input
/// tree only.
#[tokio::test]
async fn missing_inputs_are_named_one_violation_each() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;

    let absent_action = Blob::new("an action nobody uploaded");
    let status = start(&farm, &absent_action).await.expect_err("no action");
    assert_eq!(missing_subjects(&status), [subject(&absent_action)]);

    let deep_file = Blob::new("deep and missing");
    let present_file = Blob::new("present");
    let deep = Blob::of(&Directory {
        files: vec![FileNode {
            name: "deep".to_owned(),
            digest: Some(deep_file.proto.clone()),
            ..Default::default()
        }],
        ..Default::default()
    });
    let absent_dir = Blob::of(&Directory {
        files: vec![FileNode {
            name: "never seen".to_owned(),
            digest: Some(Blob::new("under an absent dir").proto),
            ..Default::default()
        }],
        ..Default::default()
    });
    let dir = |name: &str, blob: &Blob| DirectoryNode {
        name: name.to_owned(),
        digest: Some(blob.proto.clone()),
    };
    let root = Blob::of(&Directory {
        files: vec![FileNode {
            name: "here".to_owned(),
            digest: Some(present_file.proto.clone()),
            ..Default::default()
        }],
        // `deep` twice: walked once.
        directories: vec![dir("a", &deep), dir("b", &deep), dir("c", &absent_dir)],
        ..Default::default()
    });
    let command = Blob::of(&Command {
        arguments: vec!["never uploaded".to_owned()],
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(root.proto.clone()),
        ..Default::default()
    });
    farm.upload(&[&action, &root, &deep, &present_file]).await;
    let status = start(&farm, &action).await.expect_err("missing inputs");
    assert_eq!(
        missing_subjects(&status),
        [subject(&command), subject(&absent_dir), subject(&deep_file)]
    );
    assert!(script.submitted().is_empty());
}

/// Catches: malformed actions, commands, trees or digests submitted (or reported as
/// missing) instead of refused as INVALID_ARGUMENT.
#[tokio::test]
async fn malformed_requests_are_invalid() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let garbage = Blob::new(vec![0xff, 0xff, 0xff]);
    let good = job("good", &[], false);
    farm.upload(&[&garbage]).await;
    farm.upload(&good.blobs.iter().collect::<Vec<_>>()).await;
    let command = good.blobs[1].proto.clone();
    let root = good.blobs[2].proto.clone();
    let bad_digest = reapi::Digest {
        hash: "nothex".to_owned(),
        size_bytes: 1,
    };
    let dir_with = |files: Vec<FileNode>, directories: Vec<DirectoryNode>| {
        Blob::of(&Directory {
            files,
            directories,
            ..Default::default()
        })
    };
    let bad_file = dir_with(
        vec![FileNode {
            name: "f".to_owned(),
            digest: Some(bad_digest.clone()),
            ..Default::default()
        }],
        Vec::new(),
    );
    let bad_child = dir_with(
        Vec::new(),
        vec![DirectoryNode {
            name: "d".to_owned(),
            digest: Some(bad_digest.clone()),
        }],
    );
    farm.upload(&[&bad_file, &bad_child]).await;
    let actions = [
        ("an action that does not decode", None),
        (
            "no command digest",
            Some(Action {
                input_root_digest: Some(root.clone()),
                ..Default::default()
            }),
        ),
        (
            "no input root",
            Some(Action {
                command_digest: Some(command.clone()),
                ..Default::default()
            }),
        ),
        (
            "a command that does not decode",
            Some(Action {
                command_digest: Some(garbage.proto.clone()),
                input_root_digest: Some(root.clone()),
                ..Default::default()
            }),
        ),
        (
            "a root that does not decode",
            Some(Action {
                command_digest: Some(command.clone()),
                input_root_digest: Some(garbage.proto.clone()),
                ..Default::default()
            }),
        ),
        (
            "a malformed file digest",
            Some(Action {
                command_digest: Some(command.clone()),
                input_root_digest: Some(bad_file.proto.clone()),
                ..Default::default()
            }),
        ),
        (
            "a malformed directory digest",
            Some(Action {
                command_digest: Some(command.clone()),
                input_root_digest: Some(bad_child.proto.clone()),
                ..Default::default()
            }),
        ),
        (
            "a malformed command digest",
            Some(Action {
                command_digest: Some(bad_digest.clone()),
                input_root_digest: Some(root.clone()),
                ..Default::default()
            }),
        ),
    ];
    for (why, action) in actions {
        let blob = action.map_or_else(|| garbage.clone(), |a| Blob::of(&a));
        farm.upload(&[&blob]).await;
        let status = start(&farm, &blob).await.expect_err(why);
        assert_eq!(status.code(), Code::InvalidArgument, "{why}: {status:?}");
    }
    let status = farm
        .exec()
        .execute(ExecuteRequest {
            action_digest: Some(good.action.proto.clone()),
            digest_function: reapi::digest_function::Value::Md5 as i32,
            ..Default::default()
        })
        .await
        .expect_err("MD5");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(script.submitted().is_empty());
}

/// Catches: an operation no worker can run streamed as plainly QUEUED, so a build
/// hangs with no word of why, and a reason left in the metadata once it is gone.
#[tokio::test]
async fn a_waiting_operation_says_why_in_its_metadata() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let job = job("wait", &[("OSFamily", "darwin")], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    let mut ops = start(&farm, &job.action).await.expect("Execute");

    let partial = |op: &Operation| {
        let meta = op.metadata.as_ref().expect("metadata");
        ExecuteOperationMetadata::decode(meta.value.as_slice())
            .expect("metadata decodes")
            .partial_execution_metadata
    };
    let queued = next(&mut ops).await.expect("healthy").expect("an update");
    assert_eq!(partial(&queued), None);

    let why = "none of the 1 live worker(s) satisfies the action's platform";
    script.set(0, Stage::Waiting(why.to_owned()));
    let waiting = next(&mut ops).await.expect("healthy").expect("an update");
    assert_eq!(stage(&waiting), ExecStage::Queued as i32);
    assert!(!waiting.done);
    let [info] = partial(&waiting)
        .expect("partial metadata")
        .auxiliary_metadata
        .try_into()
        .expect("one entry");
    assert_eq!(info.type_url, "type.googleapis.com/google.rpc.ErrorInfo");
    let info = rpc::ErrorInfo::decode(info.value.as_slice()).expect("an ErrorInfo");
    assert_eq!(info.reason, NO_WORKER_REASON);
    assert_eq!(info.domain, ERROR_DOMAIN);
    assert_eq!(info.metadata.get("why").map(String::as_str), Some(why));

    script.set(0, Stage::Queued);
    let again = next(&mut ops).await.expect("healthy").expect("an update");
    assert_eq!(stage(&again), ExecStage::Queued as i32);
    assert_eq!(partial(&again), None);
}

/// Catches: a WaitExecution that does not follow the named operation, or that answers
/// for a name it does not know instead of NOT_FOUND; and a refusal from the scheduler
/// that is swallowed.
#[tokio::test]
async fn wait_execution_and_refusals() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let job = job("waited", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    let _ops = start(&farm, &job.action).await.expect("Execute");

    let mut waited = farm
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: "operations/0".to_owned(),
        })
        .await
        .expect("WaitExecution")
        .into_inner();
    let first = next(&mut waited)
        .await
        .expect("healthy")
        .expect("an update");
    assert_eq!(first.name, "operations/0");
    script.set(0, Stage::Done(Finished::Ran(Box::default())));
    assert!(
        next(&mut waited)
            .await
            .expect("healthy")
            .expect("done")
            .done
    );

    let unknown = farm
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: "operations/9".to_owned(),
        })
        .await
        .expect_err("unknown");
    assert_eq!(unknown.code(), Code::NotFound);

    script.refuse.store(true, Ordering::SeqCst);
    let refused = start(&farm, &job.action).await.expect_err("refused");
    assert_eq!(refused.code(), Code::Unavailable);
}

/// Catches: an input the cache holds but cannot read (its object is gone) reported as
/// MISSING, which would make the client upload it again forever instead of retrying.
#[tokio::test]
async fn an_unreadable_input_is_unavailable_not_missing() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let job = job("unreadable", &[], false);
    farm.upload(&[&job.action]).await;
    farm.delete_object_of(&job.action).await;
    let status = start(&farm, &job.action).await.expect_err("unreadable");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
}

/// Catches: a front serving `Execution` that does not advertise it (clients would
/// never send Execute), or advertises a digest function it does not accept.
#[tokio::test]
async fn capabilities_advertise_execution() {
    let farm = Farm::with_execution(Arc::new(Script::default())).await;
    let caps = farm
        .caps()
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .expect("GetCapabilities")
        .into_inner();
    let exec = caps.execution_capabilities.expect("execution capabilities");
    let sha256 = reapi::digest_function::Value::Sha256 as i32;
    assert!(exec.exec_enabled);
    assert_eq!(exec.digest_function, sha256);
    assert_eq!(exec.digest_functions, [sha256]);
}
