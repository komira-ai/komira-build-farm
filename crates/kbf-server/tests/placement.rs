//! Where an action ran and why it ran again, as the server tells it (issue #166): the
//! node and the queue time in the result's `ExecutedActionMetadata`, each grant in the
//! log at debug level, and each requeue at info with its operation, lease, node and
//! reason.

mod support;

use std::io::Write;
use std::sync::{Mutex, Once, PoisonError};
use std::time::{Duration, SystemTime};

use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::{ExecutedActionMetadata, WaitExecutionRequest};
use kbf_proto::worker::LeaseId;
use prost_types::Timestamp;
use support::{Cell, Client, Job, done, output, ran, stamped_response};
use tonic::Code;
use tonic::codec::Streaming;

/// Every line logged in this test binary. Each test names its own nodes, and reads
/// only the lines that name them.
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct SharedLog;

impl Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut log = LOG.lock().unwrap_or_else(PoisonError::into_inner);
        log.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs the subscriber that writes to [`LOG`], at debug level, once.
fn capture() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| SharedLog)
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
    });
}

/// The logged lines that contain every one of `parts`.
fn lines_with(parts: &[&str]) -> Vec<String> {
    let log = LOG.lock().unwrap_or_else(PoisonError::into_inner);
    String::from_utf8_lossy(&log)
        .lines()
        .filter(|line| parts.iter().all(|p| line.contains(p)))
        .map(str::to_owned)
        .collect()
}

fn lease_text(lease: Option<LeaseId>) -> String {
    let lease = lease.expect("a lease id");
    format!("lease={}.{}", lease.term, lease.seq)
}

fn time(ts: Option<&Timestamp>) -> SystemTime {
    SystemTime::try_from(*ts.expect("a timestamp")).expect("a time")
}

fn metadata(op: &Operation) -> ExecutedActionMetadata {
    let result = stamped_response(op).result.expect("a result");
    result.execution_metadata.expect("metadata")
}

/// Catches: an accepted result without the node that ran it in `worker` (every pilot
/// result had `""`), without a `queued_timestamp` (buck2 computed a 57-year queue
/// time), with one that is not the submission's time, and worker start and completion
/// times the daemon left out not filled in order; and a cached copy without them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_result_names_its_node_and_when_it_was_queued() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("meta-node", 4, 8).await;
    let job = Job::new("stamped", &[]);
    cell.upload(&job.blobs()).await;

    let submitted = SystemTime::now();
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let started = SystemTime::now();
    let result = output(&cell, "stamped output", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    let last = done(&mut ops).await;
    let answered = SystemTime::now();

    let m = metadata(&last);
    assert_eq!(m.worker, "meta-node");
    let queued = time(m.queued_timestamp.as_ref());
    let worker_start = time(m.worker_start_timestamp.as_ref());
    let worker_completed = time(m.worker_completed_timestamp.as_ref());
    assert!(submitted <= queued && queued <= started, "{m:?}");
    assert!(queued <= worker_start && worker_start <= started, "{m:?}");
    assert!(
        worker_start <= worker_completed && worker_completed <= answered,
        "{m:?}"
    );
    let cached = cell.cached_stamped(&job.action).await.expect("cached");
    assert_eq!(
        cached.execution_metadata,
        Some(m),
        "the cache holds another"
    );
}

/// Catches: a lease given up without an info line naming its operation, lease, node
/// and reason (a rerun could only be traced by the action printing its hostname); a
/// grant not logged at debug level with its node; a requeued operation's
/// `queued_timestamp` reset to the requeue; and a result naming the node that lost
/// the lease instead of the one that ran it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requeue_is_logged_and_the_result_names_the_node_that_ran_it() {
    capture();
    let cell = Cell::start().await;
    let mut first = cell.daemon("requeue-a", 4, 8).await;
    let job = Job::new("requeued", &[]);
    cell.upload(&job.blobs()).await;

    let submitted = SystemTime::now();
    let mut ops = cell.execute(&job.action).await;
    let name = ops
        .message()
        .await
        .expect("a stream")
        .expect("an update")
        .name;
    let lost = first.start().await;
    // The daemon reconnects with no room and lists nothing: the lease is given up at
    // once, and the operation goes to the other node.
    let mut other = cell.daemon("requeue-b", 4, 8).await;
    let mut again = cell.daemon("requeue-a", 0, 8).await;
    let requeued = SystemTime::now();
    assert!(again.heartbeat(&[]).await);
    let current = other.start().await;
    let result = output(&cell, "from the other node", 0).await;
    assert!(other.report(ran(current.lease_id, &result)).await.accepted);
    let m = metadata(&done(&mut ops).await);

    assert_eq!(m.worker, "requeue-b");
    let queued = time(m.queued_timestamp.as_ref());
    assert!(submitted <= queued && queued <= requeued, "{m:?}");

    let operation = format!("operation={name}");
    let given_up = lines_with(&[
        "INFO",
        "lease given up; requeued",
        &operation,
        &lease_text(lost.lease_id),
        "node=requeue-a",
        "reason=the daemon reconnected and does not list it as running",
    ]);
    assert_eq!(given_up.len(), 1, "{:?}", lines_with(&["requeue-"]));
    for (node, lease) in [
        ("requeue-a", lost.lease_id),
        ("requeue-b", current.lease_id),
    ] {
        let granted = lines_with(&[
            "DEBUG",
            "lease granted",
            &operation,
            &lease_text(lease),
            &format!("node={node}"),
        ]);
        assert_eq!(granted.len(), 1, "{node}: {:?}", lines_with(&["requeue-"]));
    }
}

/// Catches (issues #165 and #166 together): a requeued operation whose result,
/// answered again by WaitExecution within the finished retention, loses the node that
/// ran it or its queue time; a requeue line that no longer names the operation (its
/// callers forgotten before the line is written); a requeued operation kept past the
/// retention (WaitExecution still answers); and a late result under the given-up lease,
/// after the operation is dropped, that the farm does not acknowledge or accepts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requeued_operation_is_answered_within_the_retention_then_forgotten() {
    capture();
    let retention = Duration::from_secs(1);
    let cell = Cell::start_with(kbf_sched::UNSERVABLE_WAIT, retention).await;
    let mut first = cell.daemon("retain-a", 4, 8).await;
    let job = Job::new("requeued, then retired", &[]);
    cell.upload(&job.blobs()).await;

    let mut ops = cell.execute(&job.action).await;
    let name = ops
        .message()
        .await
        .expect("a stream")
        .expect("an update")
        .name;
    let lost = first.start().await;
    let mut other = cell.daemon("retain-b", 4, 8).await;
    let mut again = cell.daemon("retain-a", 0, 8).await;
    assert!(again.heartbeat(&[]).await);
    let current = other.start().await;
    let result = output(&cell, "ran on retain-b", 0).await;
    assert!(other.report(ran(current.lease_id, &result)).await.accepted);
    let answered = metadata(&done(&mut ops).await);
    assert_eq!(answered.worker, "retain-b");

    let mut waited = wait(&cell, &name).await.expect("kept for the retention");
    let kept = waited
        .message()
        .await
        .expect("a stream")
        .expect("the done operation");
    assert_eq!(metadata(&kept), answered, "the retained answer differs");

    let operation = format!("operation={name}");
    let given_up = lines_with(&[
        "lease given up; requeued",
        &operation,
        &lease_text(lost.lease_id),
        "node=retain-a",
    ]);
    assert_eq!(given_up.len(), 1, "{:?}", lines_with(&["retain-"]));

    // The server ticks every 50 ms, so the operation is dropped soon after this.
    tokio::time::sleep(retention + Duration::from_millis(500)).await;
    assert_eq!(wait(&cell, &name).await.err(), Some(Code::NotFound));
    let late = again.report(ran(lost.lease_id, &result)).await;
    assert!(!late.accepted, "a result for a dropped operation accepted");
    assert_eq!(wait(&cell, &name).await.err(), Some(Code::NotFound));
}

/// Catches: a requeue line written after the operation it names was refused and,
/// with a zero finished retention, dropped and its callers forgotten in the same
/// input, so the line names no operation. Here the reconnected node has no room and
/// no other node is up, so with a zero unservable wait the requeued operation is
/// refused at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requeue_refused_and_dropped_at_once_is_logged_with_its_operation() {
    capture();
    let cell = Cell::start_with(Duration::ZERO, Duration::ZERO).await;
    let mut first = cell.daemon("dropped-a", 4, 8).await;
    let job = Job::new("requeued, refused, dropped", &[]);
    cell.upload(&job.blobs()).await;

    let mut ops = cell.execute(&job.action).await;
    let name = ops
        .message()
        .await
        .expect("a stream")
        .expect("an update")
        .name;
    let lost = first.start().await;
    let mut again = cell.daemon("dropped-a", 0, 8).await;
    assert!(again.heartbeat(&[]).await);
    let refused = stamped_response(&done(&mut ops).await);
    let status = refused.status.expect("a status");
    assert_eq!(status.code, Code::FailedPrecondition as i32, "{status:?}");

    let given_up = lines_with(&[
        "lease given up; requeued",
        &format!("operation={name} "),
        &lease_text(lost.lease_id),
        "node=dropped-a",
    ]);
    assert_eq!(given_up.len(), 1, "{:?}", lines_with(&["dropped-a"]));
    assert_eq!(wait(&cell, &name).await.err(), Some(Code::NotFound));
}

/// WaitExecution on `name`: its stream, or the code.
async fn wait(cell: &Client, name: &str) -> Result<Streaming<Operation>, Code> {
    let ops = cell
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: name.to_owned(),
        })
        .await
        .map_err(|s| s.code())?;
    Ok(ops.into_inner())
}
