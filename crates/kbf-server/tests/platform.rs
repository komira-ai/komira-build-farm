//! Platform routing end to end: daemons report their OS and architecture in `Hello`,
//! actions name theirs in REAPI platform properties, and the server gives each action
//! only to a daemon that satisfies it. An action no connected daemon can run waits with
//! a reason in its operation metadata, and is refused FAILED_PRECONDITION after the
//! unservable wait.

mod support;

use std::time::Duration;

use kbf_front::{ERROR_DOMAIN, NO_WORKER_REASON};
use kbf_proto::google::longrunning::Operation;
use kbf_proto::google::rpc::ErrorInfo;
use kbf_proto::reapi::{ExecuteOperationMetadata, execution_stage::Value as ExecStage};
use kbf_proto::worker::daemon_message;
use prost::Message;
use support::{Cell, FakeDaemon, Job, PROMPT, done, hello, hello_on, output, ran, response, stage};
use tokio::time::timeout;
use tonic::{Code, Streaming};

const MAC: &[(&str, &str)] = &[("arch", "arm64"), ("os", "macos")];

/// The reason in a queued operation's metadata, if it carries one.
fn waiting_reason(op: &Operation) -> Option<String> {
    let meta = op.metadata.as_ref().expect("metadata");
    let meta = ExecuteOperationMetadata::decode(meta.value.as_slice()).expect("metadata decodes");
    let partial = meta.partial_execution_metadata?;
    let [info] = partial.auxiliary_metadata.as_slice() else {
        panic!("one auxiliary entry expected: {partial:?}");
    };
    let info = ErrorInfo::decode(info.value.as_slice()).expect("an ErrorInfo");
    assert_eq!(
        (info.reason.as_str(), info.domain.as_str()),
        (NO_WORKER_REASON, ERROR_DOMAIN)
    );
    assert_eq!(meta.stage, ExecStage::Queued as i32);
    info.metadata.get("why").cloned()
}

/// The next operation update.
async fn next(ops: &mut Streaming<Operation>) -> Operation {
    timeout(PROMPT, ops.message())
        .await
        .expect("an update in time")
        .expect("a healthy stream")
        .expect("an update")
}

/// Catches: placement that ignores the platform. The Mac registers first and sorts
/// first by name, so first fit without matching would give it the Linux action, and the
/// Linux daemon the Mac action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_action_runs_on_a_daemon_of_its_platform() {
    let cell = Cell::start().await;
    let mut mac = FakeDaemon::connect(cell.worker_addr, hello_on("node-a", 4, 8, MAC))
        .await
        .expect("registered");
    let mut linux = cell.daemon("node-b", 4, 8).await;

    let linux_job = Job::new("linux", &[("OSFamily", "linux")]);
    let mac_job = Job::new("mac", &[("OSFamily", "Darwin"), ("ISA", "arm-a64")]);
    cell.upload(&linux_job.blobs()).await;
    cell.upload(&mac_job.blobs()).await;

    let mut linux_ops = cell.execute(&linux_job.action).await;
    let start = linux.start().await;
    assert_eq!(start.action_digest.as_ref(), Some(&linux_job.action.proto));
    mac.no_work().await;
    let built = output(&cell, "on linux", 0).await;
    assert!(linux.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut linux_ops).await;

    let mut mac_ops = cell.execute(&mac_job.action).await;
    let start = mac.start().await;
    assert_eq!(start.action_digest.as_ref(), Some(&mac_job.action.proto));
    linux.no_work().await;
    let built = output(&cell, "on the mac", 0).await;
    assert!(mac.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut mac_ops).await;
}

/// Catches: an action no connected daemon can run handed to one that cannot, left
/// queued forever without a word, or ended without the reason (or with a code a client
/// would retry, or with MISSING details that would make it re-upload and retry).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_action_no_daemon_can_run_waits_with_a_reason_then_is_refused() {
    let cell = Cell::start_with_unservable_wait(Duration::from_millis(500)).await;
    let mut linux = cell.daemon("node-b", 4, 8).await;
    let job = Job::new("mac only", &[("OSFamily", "macos")]);
    cell.upload(&job.blobs()).await;

    let mut ops = cell.execute(&job.action).await;
    let first = next(&mut ops).await;
    let why = waiting_reason(&first).expect("a reason from the start");
    assert!(why.contains("node-b") && why.contains("os=macos"), "{why}");
    linux.no_work().await;

    let last = done(&mut ops).await;
    let status = response(&last).status.expect("a status");
    assert_eq!(status.code, Code::FailedPrecondition as i32, "{status:?}");
    assert!(status.message.starts_with(&why), "{}", status.message);
    assert!(status.details.is_empty());
    linux.no_work().await;
}

/// Catches: a waiting action not placed when a daemon that can run it connects, its
/// callers still told it cannot run, and a caller that joins it while it waits never
/// told why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_can_run_it_ends_the_wait() {
    let cell = Cell::start().await;
    let _linux = cell.daemon("node-b", 4, 8).await;
    let job = Job::new("for the mac", &[("OSFamily", "darwin")]);
    cell.upload(&job.blobs()).await;

    let mut ops = cell.execute(&job.action).await;
    assert!(waiting_reason(&next(&mut ops).await).is_some());
    let mut joined = cell.execute(&job.action).await;
    assert!(
        waiting_reason(&next(&mut joined).await).is_some(),
        "a joiner not told why"
    );

    let mut mac = FakeDaemon::connect(cell.worker_addr, hello_on("node-m", 4, 8, MAC))
        .await
        .expect("registered");
    let start = mac.start().await;
    for stream in [&mut ops, &mut joined] {
        let mut op = next(stream).await;
        while stage(&op) == ExecStage::Queued as i32 {
            assert_eq!(waiting_reason(&op), None, "a reason after the wait ended");
            op = next(stream).await;
        }
        assert_eq!(stage(&op), ExecStage::Executing as i32);
    }
    let built = output(&cell, "on the mac", 0).await;
    assert!(mac.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut ops).await;
    done(&mut joined).await;
}

/// Catches: a node report without an architecture accepted (it could never be matched
/// safely), a resent report whose platform entries are dropped (a label added later is
/// never matched), and a malformed resent report taken anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hello_carries_what_placement_matches() {
    let cell = Cell::start().await;
    let mut no_arch = hello("node-x", 4, 8);
    no_arch.capabilities.retain(|c| c.key != "arch");
    let refused = FakeDaemon::connect(cell.worker_addr, no_arch)
        .await
        .err()
        .expect("refused");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(refused.message().contains("arch"), "{}", refused.message());

    let mut mac = FakeDaemon::connect(cell.worker_addr, hello_on("node-m", 4, 8, MAC))
        .await
        .expect("registered");
    let job = Job::new("pool", &[("label.pool", "darwin")]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    mac.no_work().await;

    // A garbled resend is ignored; the node keeps its report.
    let garbled = hello_on("node-m", 4, 8, &[("arch", "sparc"), ("os", "macos")]);
    mac.send(daemon_message::Message::Hello(garbled));
    mac.no_work().await;

    let labelled = hello_on(
        "node-m",
        4,
        8,
        &[("arch", "arm64"), ("os", "macos"), ("label.pool", "darwin")],
    );
    mac.send(daemon_message::Message::Hello(labelled));
    let start = mac.start().await;
    let built = output(&cell, "pooled", 0).await;
    assert!(mac.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut ops).await;
}
