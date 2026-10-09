//! Where an action ran and why it ran again, as the server tells it (issue #166): the
//! node and the queue time in the result's `ExecutedActionMetadata`, each grant in the
//! log at debug level, and each requeue at info with its operation, lease, node and
//! reason.

mod support;

use std::io::Write;
use std::sync::{Mutex, Once, PoisonError};
use std::time::SystemTime;

use kbf_proto::reapi::ExecutedActionMetadata;
use kbf_proto::worker::LeaseId;
use prost_types::Timestamp;
use support::{Cell, Job, done, output, ran, stamped_response};

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

fn metadata(op: &kbf_proto::google::longrunning::Operation) -> ExecutedActionMetadata {
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
