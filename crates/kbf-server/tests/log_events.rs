//! The events the durable log exists for, at INFO and WARN, one line each: where an
//! operation was placed, how a lease ended (with the failure kind), a requeue with
//! the farm-fault budget it spent, and the operation failing once the budget is gone.
//! The log is read through a global subscriber, in a test binary of its own, which no
//! other test's events can reach or race.

mod support;

use std::io::Write;
use std::sync::{Mutex, Once, PoisonError};

use kbf_proto::worker::LeaseId;
use kbf_server::write_tier::report_round;
use support::{Cell, Job, done, failed, output, ran, report_every_run};
use tonic::Code;

/// Every line logged in this test binary. Each test names its own nodes and operations
/// and reads only the lines that name them.
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

/// Installs the subscriber that writes to [`LOG`], at INFO (the server's level), once.
fn capture() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| SharedLog)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
    });
    // Tests run in parallel: a callsite another test's thread first reached while this
    // subscriber was being installed may have cached "never" (tests/thread_log_capture.rs),
    // so re-evaluate every callsite against it now that it is the global default.
    tracing_core::callsite::rebuild_interest_cache();
}

/// The logged lines that contain every one of `parts`.
fn lines_with(parts: &[&str]) -> Vec<String> {
    SharedLog.flush().expect("a flush");
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

/// The REAPI name of the operation `ops` answers, as the log names it.
async fn name_of(
    ops: &mut tonic::codec::Streaming<kbf_proto::google::longrunning::Operation>,
) -> String {
    ops.message()
        .await
        .expect("a stream")
        .expect("an update")
        .name
}

/// Catches: a placement not in the log at INFO (it was DEBUG, so an operator reading a
/// build's log saw no scheduling decision at all), a placement line that does not name
/// its operation, lease and worker, and a lease that ends OK without a line saying so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placement_and_a_lease_that_finished_ok_are_logged() {
    capture();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("ev-ok", 4, 8).await;
    let job = Job::new("logged placement", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let name = name_of(&mut ops).await;
    let start = daemon.start().await;
    let result = output(&cell, "done", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    let operation = format!("operation={name}");
    let lease = lease_text(start.lease_id);
    let placed = lines_with(&[" INFO ", "lease granted", &operation, &lease, "node=ev-ok"]);
    assert_eq!(placed.len(), 1, "{:?}", lines_with(&["ev-ok"]));
    let finished = lines_with(&[
        " INFO ",
        "lease finished ok",
        &operation,
        &lease,
        "node=ev-ok",
        "accepted=true",
    ]);
    assert_eq!(finished.len(), 1, "{:?}", lines_with(&["ev-ok"]));
}

/// Catches: a failed lease whose log line lacks the failure kind or the node, a
/// requeue after a farm fault that does not say how much of the farm-fault budget is
/// spent (an operator cannot tell a first stumble from the last chance), and an
/// operation that fails for good once the budget is spent without a WARN naming it.
/// The daemon reports the farm's failure (`Internal`) on every run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_lease_a_requeue_and_an_exhausted_budget_are_logged() {
    capture();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("ev-fail", 4, 8).await;
    let job = Job::new("logged failures", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let name = name_of(&mut ops).await;
    let first = daemon.start().await;
    report_every_run(&mut daemon, first, |lease| failed(lease, Code::Internal)).await;
    done(&mut ops).await;

    let operation = format!("operation={name}");
    let failed_leases = lines_with(&[
        " WARN ",
        "lease finished failed",
        &operation,
        "node=ev-fail",
        "failure=Infra",
    ]);
    assert_eq!(failed_leases.len(), 3, "{:?}", lines_with(&["ev-fail"]));
    for spent in 1..=2 {
        let requeued = lines_with(&[
            " INFO ",
            "lease given up; requeued",
            &operation,
            "node=ev-fail",
            "infra failure",
            &format!("farm_runs={spent} farm_budget=2"),
        ]);
        assert_eq!(
            requeued.len(),
            1,
            "run {spent}: {:?}",
            lines_with(&["ev-fail"])
        );
    }
    let exhausted = lines_with(&[
        " WARN ",
        "operation finished failed",
        &operation,
        "failure=Infra",
        "farm_runs=3 farm_budget=2",
    ]);
    assert_eq!(exhausted.len(), 1, "{:?}", lines_with(&["ev-fail"]));
}

/// Catches: a flusher round that found a corrupt or conflicting object and said
/// nothing (the round's report was dropped, so a lost record was invisible), and one
/// that logs every ordinary round (a line a second, forever).
#[test]
fn a_flush_round_with_corrupt_or_conflicting_objects_warns_and_a_quiet_one_does_not() {
    capture();
    report_round(&kbf_front::FlushRound::default());
    assert_eq!(lines_with(&["flush found objects"]).len(), 0);

    let round = kbf_front::FlushRound {
        conflicts: vec![object(1)],
        corrupt: vec![object(2), object(3)],
        ..kbf_front::FlushRound::default()
    };
    report_round(&round);
    let warned = lines_with(&[
        " WARN ",
        "flush found objects",
        "conflicts=1",
        "corrupt=2",
        "dropped_blobs=0",
    ]);
    assert_eq!(warned.len(), 1, "{:?}", lines_with(&["flush"]));
}

fn object(seq: u64) -> kbf_meta::ObjectId {
    kbf_meta::ObjectId::new(kbf_meta::Epoch::new(1), seq)
}
