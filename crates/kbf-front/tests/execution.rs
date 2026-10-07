//! `Execution` over gRPC, against the in-process cache and a scripted dispatch that
//! stands in for the scheduler: what Execute checks, what it submits, and how it streams
//! an operation's stages.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use common::{Blob, Farm};
use kbf_front::{DEFAULT_RESOURCES, Dispatch, Finished, Stage, Submission, Ticket};
use kbf_meta::Role;
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc::{self, PreconditionFailure};
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_proto::reapi::{
    self, Action, ActionResult, Command, Directory, DirectoryNode, ExecuteOperationMetadata,
    ExecuteRequest, ExecuteResponse, FileNode, GetCapabilitiesRequest, Platform,
    WaitExecutionRequest, platform,
};
use kbf_types::{ActionKey, Qos};
use prost::Message;
use tokio::sync::watch;
use tonic::{Code, Status, Streaming};

/// A dispatch that records submissions and lets the test move each operation's stage.
#[derive(Default)]
struct Script {
    submitted: Mutex<Vec<Submission>>,
    stages: Mutex<Vec<(String, watch::Sender<Stage>)>>,
    refuse: AtomicBool,
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
        let (tx, rx) = watch::channel(Stage::Queued);
        stages.push((name.clone(), tx));
        let action = submission.request.key.action;
        self.submitted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(submission);
        Ok(Ticket {
            name,
            action,
            stage: rx,
        })
    }

    fn join(&self, key: &ActionKey) -> Option<Ticket> {
        let stages = self.stages.lock().unwrap_or_else(PoisonError::into_inner);
        let submitted = self
            .submitted
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let n = submitted
            .iter()
            .position(|s| &s.request.key == key && s.request.joinable())?;
        let (name, tx) = &stages[n];
        if matches!(*tx.borrow(), Stage::Done(_)) {
            return None;
        }
        Some(Ticket {
            name: name.clone(),
            action: key.action,
            stage: tx.subscribe(),
        })
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        let stages = self.stages.lock().unwrap_or_else(PoisonError::into_inner);
        let (name, tx) = stages.iter().find(|(n, _)| n == name)?;
        Some(Ticket {
            name: name.clone(),
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
    assert_eq!(submission.kind, "action");

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

/// Catches: a failure answered without its status (or with a result), and a stream
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

    let mut abandoned = start(&farm, &job.action).await.expect("Execute");
    next(&mut abandoned).await.expect("healthy");
    script.abandon(1);
    let gone = next(&mut abandoned)
        .await
        .expect_err("an abandoned operation");
    assert_eq!(gone.code(), Code::Unavailable);
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

/// Catches: a lease kind read from the wrong place (REAPI 2.2 clients put the platform
/// in the Action, older ones in the Command), an empty Action platform taken over the
/// Command's, an unknown kind run as a shared action,
/// and a platform with a repeated property accepted.
#[tokio::test]
async fn the_lease_kind_comes_from_the_platform() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;

    let in_action = job("mac", &[("kbf-lease", "whole_machine")], true);
    farm.upload(&in_action.blobs.iter().collect::<Vec<_>>())
        .await;
    start(&farm, &in_action.action).await.expect("Execute");

    // An older client: the Action has no platform, or an empty one (as a 2.2 client
    // that sets none may send), and the Command has one.
    for (argv, action_platform) in [
        ("old", None),
        ("old, empty", Some(Platform { properties: vec![] })),
    ] {
        let older = job(argv, &[], false);
        #[allow(deprecated)]
        let command = Blob::of(&Command {
            arguments: vec![argv.to_owned()],
            platform: Some(Platform {
                properties: vec![property("kbf-lease", "whole_machine")],
            }),
            ..Default::default()
        });
        let action = Blob::of(&Action {
            command_digest: Some(command.proto.clone()),
            input_root_digest: Some(older.blobs[2].proto.clone()),
            platform: action_platform,
            ..Default::default()
        });
        farm.upload(&[&action, &command, &older.blobs[2], &older.blobs[3]])
            .await;
        start(&farm, &action).await.expect("Execute");
    }

    let submitted = script.submitted();
    assert_eq!(submitted[0].kind, "whole_machine");
    assert!(submitted[0].request.do_not_cache, "do_not_cache dropped");
    assert!(!submitted[0].request.joinable());
    assert_eq!(
        submitted[1].kind, "whole_machine",
        "the Command's platform ignored"
    );
    assert_eq!(
        submitted[2].kind, "whole_machine",
        "an empty Action platform hid the Command's"
    );

    for (why, props) in [
        ("an unknown kind", vec![("kbf-lease", "vm")]),
        (
            "a repeated property",
            vec![("os", "linux"), ("os", "linux")],
        ),
    ] {
        let bad = job(why, &props, false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(why);
        assert_eq!(status.code(), Code::InvalidArgument, "{why}");
    }
    assert_eq!(script.submitted().len(), 3);
}

/// Catches: Execute that checks a running twin's inputs before joining it (RFC 5.3:
/// the join, step 4, comes before the input check, step 5), so its caller is refused
/// MISSING or pays for the reads; and a join that submits a second operation or hands
/// back a ticket that does not follow the twin.
#[tokio::test]
async fn a_running_twin_is_joined_before_its_inputs_are_checked() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    // The scheduler runs a twin whose blobs this cell does not hold.
    let twin = job("twin", &[], false);
    let request = kbf_sched::Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: twin.action.digest,
        },
        qos: Qos::Ci,
        resources: DEFAULT_RESOURCES,
        hermetic: true,
        do_not_cache: false,
    };
    let kind = "action".to_owned();
    script
        .submit(Submission { request, kind })
        .expect("the twin");

    let mut ops = start(&farm, &twin.action)
        .await
        .expect("joined, not refused MISSING");
    assert_eq!(script.submitted().len(), 1, "a join submitted again");
    let first = next(&mut ops).await.expect("a stream").expect("an update");
    assert_eq!(stage(&first), ExecStage::Queued as i32);
    script.set(0, Stage::Executing);
    let running = next(&mut ops).await.expect("a stream").expect("an update");
    assert_eq!(
        stage(&running),
        ExecStage::Executing as i32,
        "not the twin's"
    );
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
