//! What the invocation record keeps of each lease's report, end to end
//! (`docs/design/ui-data.md` 4.2, phase U1): a fake daemon reports, and the row the
//! server's recorder holds is read back. Each test names the defect it catches.

use std::time::Duration;

use kbf_invocation::{AttemptEnd, MemoryKillKind as Kill, Outcome, WallTime};
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::{self, CgroupUsage, MemoryKill, NumaNodeBytes, ResourceUsage};
use kbf_sched::RequeueReason;
use kbf_types::Failure;
use prost_types::Timestamp;
use tonic::Code;

use crate::invocations::{BUILD, execute, record, upload};
use crate::support::{Blob, Cell, Job, done, failed, output, ran};

const GIB: u64 = 1 << 30;

/// Catches (ui-data 9, U1 red): a run's cgroup block dropped on its way to the row
/// (`memory_peak_bytes` read nowhere but the line), any REAPI time the daemon sent not
/// kept, `queued` not the server's, and a run with no report the scheduler took (the
/// lost lease) given made-up times.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_keeps_its_cgroup_block_and_its_reapi_times() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("measured", &[]);
    upload(&cell, BUILD, &job.blobs()).await;
    let mut ops = execute(&cell, BUILD, &job.action).await;
    let start = daemon.start().await;
    let ts = |secs: i64| {
        Some(Timestamp {
            seconds: 1_900_000_000 + secs,
            nanos: 0,
        })
    };
    let usage = ResourceUsage {
        wall_micros: 4_000_000,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(8_000_000),
            memory_peak_bytes: Some(3 * GIB),
            numa: vec![NumaNodeBytes { node: 1, bytes: 9 }],
            ..CgroupUsage::default()
        }),
        ..ResourceUsage::default()
    };
    let mut result = output(&cell, "measured", 0).await;
    result.execution_metadata = Some(ExecutedActionMetadata {
        worker_start_timestamp: ts(1),
        input_fetch_start_timestamp: ts(2),
        input_fetch_completed_timestamp: ts(3),
        execution_start_timestamp: ts(4),
        execution_completed_timestamp: ts(8),
        output_upload_start_timestamp: ts(9),
        output_upload_completed_timestamp: ts(10),
        worker_completed_timestamp: ts(11),
        auxiliary_metadata: vec![usage.to_any()],
        ..ExecutedActionMetadata::default()
    });
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    let inv = record(&cell, BUILD).await;
    let [row] = inv.rows() else {
        panic!("one row: {:?}", inv.rows());
    };
    let [attempt] = row.attempts.as_slice() else {
        panic!("one attempt: {:?}", row.attempts);
    };
    let run = attempt.run.as_deref().expect("the report's run");
    let u = run.usage.as_ref().expect("its usage");
    assert_eq!(u.memory_peak_bytes, Some(3 * GIB));
    assert_eq!(u.parallelism(), Some(2.0));
    assert_eq!(u.numa.len(), 1);
    let at = |secs: u64| {
        Some(WallTime::from_unix_micros(
            (1_900_000_000 + secs) * 1_000_000,
        ))
    };
    let t = run.times;
    assert_eq!(
        [
            t.worker_start,
            t.input_fetch_start,
            t.input_fetch_completed,
            t.execution_start,
            t.execution_completed,
            t.output_upload_start,
            t.output_upload_completed,
            t.worker_completed
        ],
        [at(1), at(2), at(3), at(4), at(8), at(9), at(10), at(11)]
    );
    let queued = t.queued.expect("the server's queued time");
    assert!(queued <= attempt.granted, "submitted before granted");
    let d = row.durations();
    assert_eq!(d.execution, Some(4_000_000));
    assert_eq!(
        (d.input_fetch, d.output_upload),
        (Some(1_000_000), Some(1_000_000))
    );
    assert_eq!(attempt.memory_kill(), Some(Kill::None));
}

/// Catches (ui-data 9, U1 red): the attempt that ends the operation, killed at the
/// largest node, recorded without its booking and cap (only a requeue had them), a
/// cap taken from another run, and a requeued kill's cap not kept. With the default
/// finished retention the scheduler still holds the operation when the report is
/// recorded, so its runs are read from there; see the zero-retention twin below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_at_the_largest_node_keeps_its_booking_and_cap() {
    kill_at_the_largest_node(Cell::start().await).await;
}

/// Catches (review of ui-data 9): the booking and cap of the kill that ends the
/// operation read only from the scheduler's operation, which `--finished-retention-secs
/// 0` has already dropped when the report is recorded; they must come from the
/// operation's answer (plant `.find(|_| false)` on the answers in `take_report`: red
/// here, green above).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_finished_retention_a_kill_keeps_its_booking_and_cap() {
    let cell = Cell::start_with(kbf_sched::UNSERVABLE_WAIT, Duration::ZERO).await;
    kill_at_the_largest_node(cell).await;
}

/// An action killed at its own limit on a 2 GiB node twice (1 GiB booked, then the
/// whole node), and what its row keeps of both kills.
async fn kill_at_the_largest_node(cell: Cell) {
    // Books 1 GiB first (the test ladder), then the whole 2 GiB node.
    let mut daemon = cell.daemon("node-b", 4, 2).await;
    let job = Job::new("hungry", &[]);
    upload(&cell, BUILD, &job.blobs()).await;
    let mut ops = execute(&cell, BUILD, &job.action).await;
    let killed = |lease| worker::Result {
        memory_kill: MemoryKill::OwnLimit as i32,
        ..failed(lease, Code::ResourceExhausted)
    };
    let first = daemon.start().await;
    assert!(daemon.report(killed(first.lease_id)).await.accepted);
    let second = daemon.start().await;
    assert!(daemon.report(killed(second.lease_id)).await.accepted);
    let status = crate::support::response(&done(&mut ops).await)
        .status
        .expect("a status");
    assert_eq!(status.code, Code::FailedPrecondition as i32);

    let inv = record(&cell, BUILD).await;
    let [row] = inv.rows() else {
        panic!("one row");
    };
    let [requeued, last] = row.attempts.as_slice() else {
        panic!("two attempts: {:?}", row.attempts);
    };
    assert_eq!(
        requeued.end,
        Some(AttemptEnd::Requeued(RequeueReason::OutOfMemory {
            booked: GIB,
            raised: 2 * GIB
        }))
    );
    assert!(matches!(
        last.end,
        Some(AttemptEnd::Failed(Failure::OutOfMemory, _))
    ));
    let run = last.run.as_deref().expect("the last run");
    assert_eq!(run.booked_memory_bytes, Some(2 * GIB));
    assert_eq!(run.cap_bytes, Some(2 * GIB));
    let first_run = requeued.run.as_deref().expect("the first run");
    assert_eq!(first_run.booked_memory_bytes, Some(GIB));
    assert_eq!(first_run.cap_bytes, Some(2 * GIB));
    assert_eq!(
        [requeued.memory_kill(), last.memory_kill()],
        [Some(Kill::OwnLimit); 2]
    );
}

/// Catches (review of ui-data 9): a run killed under node memory pressure recorded
/// without its booking, on a requeued attempt or on the one that ends the operation
/// (plant: drop `| Failure::NodeMemoryPressure` from the `killed` match in
/// `take_report`), or given a cap (a pressure kill has none). Default retention: the
/// last kill's booking is read from the scheduler's operation; see the twin below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_pressure_kill_keeps_its_booking() {
    node_pressure_kills(Cell::start().await).await;
}

/// Catches (review of ui-data 9): as above with `--finished-retention-secs 0`, where
/// the last kill's booking must come from the operation's answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_finished_retention_a_node_pressure_kill_keeps_its_booking() {
    let cell = Cell::start_with(kbf_sched::UNSERVABLE_WAIT, Duration::ZERO).await;
    node_pressure_kills(cell).await;
}

/// An action killed under node pressure three times at its 1 GiB booking, which uses
/// up its reruns, and what its row keeps of each kill.
async fn node_pressure_kills(cell: Cell) {
    let mut daemon = cell.daemon("node-p", 4, 2).await;
    let job = Job::new("crowded", &[]);
    upload(&cell, BUILD, &job.blobs()).await;
    let mut ops = execute(&cell, BUILD, &job.action).await;
    for _ in 0..3 {
        let start = daemon.start().await;
        let report = worker::Result {
            memory_kill: MemoryKill::NodePressure as i32,
            ..failed(start.lease_id, Code::ResourceExhausted)
        };
        assert!(daemon.report(report).await.accepted);
    }
    let status = crate::support::response(&done(&mut ops).await)
        .status
        .expect("a status");
    assert_eq!(status.code, Code::Internal as i32, "{status:?}");

    let inv = record(&cell, BUILD).await;
    let [row] = inv.rows() else {
        panic!("one row");
    };
    let [first, second, last] = row.attempts.as_slice() else {
        panic!("three attempts: {:?}", row.attempts);
    };
    for requeued in [first, second] {
        assert_eq!(
            requeued.end,
            Some(AttemptEnd::Requeued(RequeueReason::NodeMemoryPressure))
        );
    }
    assert!(matches!(
        last.end,
        Some(AttemptEnd::Failed(Failure::NodeMemoryPressure, _))
    ));
    for (n, attempt) in [first, second, last].into_iter().enumerate() {
        let run = attempt.run.as_deref().expect("a run");
        assert_eq!(run.booked_memory_bytes, Some(GIB), "attempt {n}");
        assert_eq!(run.cap_bytes, None, "attempt {n}");
        assert_eq!(
            attempt.memory_kill(),
            Some(Kill::NodePressure),
            "attempt {n}"
        );
    }
}

/// Catches (ui-data 9, U1 red): a timed-out run's logs dropped from its row (only a
/// run's stored result was listed), or the partial result listed as a stored one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_run_lists_its_logs() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-c", 4, 8).await;
    let job = Job::new("slow", &[]);
    upload(&cell, BUILD, &job.blobs()).await;
    let mut ops = execute(&cell, BUILD, &job.action).await;
    let start = daemon.start().await;
    let (stdout, stderr) = (Blob::new("half a line"), Blob::new("warning: slow\n"));
    cell.upload(&[&stdout, &stderr]).await;
    let mut report = failed(start.lease_id, Code::DeadlineExceeded);
    report.action_result = Some(ActionResult {
        stdout_digest: Some(stdout.proto.clone()),
        stderr_digest: Some(stderr.proto.clone()),
        ..ActionResult::default()
    });
    assert!(daemon.report(report).await.accepted);
    done(&mut ops).await;

    let inv = record(&cell, BUILD).await;
    let [row] = inv.rows() else {
        panic!("one row");
    };
    assert_eq!(row.outcome, Some(Outcome::TimedOut));
    let listing = row.listing.as_ref().expect("the partial result's listing");
    assert_eq!(
        (listing.stdout, listing.stderr),
        (Some(stdout.digest), Some(stderr.digest))
    );
    assert_eq!(listing.result, None, "a partial result is never stored");
    assert_eq!(
        listing.exit_code, None,
        "a stopped action has no exit code (not the partial result's 0)"
    );
    assert!(listing.outputs.is_empty());
}
