//! Memory kills end to end, in one process: a REAPI client, the server, and a fake
//! daemon that reports each run killed for memory as `Result.memory_kill` names it
//! (failure classes, 6.1). No driver sets the field yet; the fake daemon stands in for
//! the one that will.
//!
//! A module of the `execute` test binary, not a binary of its own: `Farm` is generic,
//! and coverage counts each generic function by its best-covered instantiation, so the
//! memory paths of `Farm::report` must run in the same binary as its other paths.

use std::io::Write;
use std::sync::{Mutex, Once, PoisonError};
use std::time::Duration;

use crate::support::{Cell, FakeDaemon, Job, done, failed, output, ran, response};
use kbf_proto::google::rpc::ErrorInfo;
use kbf_proto::worker::{self, MemoryKill};
use prost::Message;
use tonic::Code;

const GIB: u64 = 1 << 30;

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

/// Installs the subscriber that writes to [`LOG`], once.
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

/// A Result for `lease` killed for memory as `kill` says, with the status a driver
/// sends with it.
fn killed(lease: Option<worker::LeaseId>, kill: MemoryKill) -> worker::Result {
    worker::Result {
        memory_kill: kill as i32,
        ..failed(lease, Code::ResourceExhausted)
    }
}

/// Reports `start` killed as `kill`, which the server must accept, and returns the
/// next `Start`, which must book `next_gib`.
async fn rerun(
    daemon: &mut FakeDaemon,
    start: &worker::Start,
    kill: MemoryKill,
    next_gib: u64,
) -> worker::Start {
    let ack = daemon.report(killed(start.lease_id, kill)).await;
    assert!(ack.accepted, "a memory kill refused: {ack:?}");
    let next = daemon.start().await;
    assert_ne!(next.lease_id, start.lease_id);
    assert_eq!(next.action_digest, start.action_digest);
    assert_eq!(next.memory_bytes, next_gib * GIB, "after {kill:?}");
    next
}

/// Catches: an own-limit kill answered at once instead of run again (the farm's case
/// shown to the user as the action's), a rerun that does not double (1 -> 3 GiB) or
/// passes the largest node, a kill at the largest node run again instead of answered,
/// an answer that does not say the action needs more memory than any node has (or
/// says it with another code, or without the `ErrorInfo` a client can read), the
/// ladder not logged, and a later Execute of the action that does not start at the
/// remembered booking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_own_limit_kill_doubles_up_to_the_largest_node_then_tells_the_client() {
    capture();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("oom-node", 4, 8).await;
    let job = Job::new("hungry", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;

    let first = daemon.start().await;
    assert_eq!(first.memory_bytes, GIB);
    let second = rerun(&mut daemon, &first, MemoryKill::OwnLimit, 2).await;
    let third = rerun(&mut daemon, &second, MemoryKill::OwnLimit, 4).await;
    let fourth = rerun(&mut daemon, &third, MemoryKill::OwnLimit, 8).await;

    // A copy of a kill already taken, resent after a reconnect, climbs nothing.
    let copy = daemon
        .report(killed(third.lease_id, MemoryKill::OwnLimit))
        .await;
    assert!(!copy.accepted);
    daemon.no_work().await;

    let ack = daemon
        .report(killed(fourth.lease_id, MemoryKill::OwnLimit))
        .await;
    assert!(ack.accepted);
    let status = response(&done(&mut ops).await).status.expect("a status");
    assert_eq!(status.code, Code::FailedPrecondition as i32, "{status:?}");
    assert_eq!(
        status.message,
        "kbf: the action needs more memory than any node offers: it passed its memory \
         limit with 8 GiB booked on oom-node, the largest node for its platform. Runs: 4 \
         (1 GiB on oom-node, 2 GiB on oom-node, 4 GiB on oom-node, 8 GiB on oom-node)."
    );
    let [detail] = &status.details[..] else {
        panic!("{status:?}");
    };
    assert_eq!(detail.type_url, "type.googleapis.com/google.rpc.ErrorInfo");
    let info = ErrorInfo::decode(detail.value.as_slice()).expect("an ErrorInfo");
    assert_eq!(info.reason, "ACTION_OUT_OF_MEMORY");
    assert_eq!(info.domain, "kbf");
    assert_eq!(info.metadata["node"], "oom-node");
    assert_eq!(info.metadata["booked_bytes"], (8 * GIB).to_string());
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
    let logged = lines_with(&["node=oom-node", "lease given up; requeued"]);
    assert_eq!(logged.len(), 3, "{logged:?}");
    assert!(
        logged[0].contains("passed its memory limit with 1 GiB booked"),
        "{logged:?}"
    );

    // The next Execute of the action starts at the booking it was raised to.
    let mut again = cell.execute(&job.action).await;
    let start = daemon.start().await;
    assert_eq!(start.memory_bytes, 8 * GIB, "the remembered booking");
    let result = output(&cell, "fits at last", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut again).await).result, Some(result));
}

/// Catches: a busy node's kill that raises the booking (the mutant: it is the node's
/// fault), one answered at once, or rerun without end; a node whose pressure is not
/// logged for operators; and the farm's error after the reruns that does not name the
/// node and the kills, or is not INTERNAL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_node_kill_reruns_with_the_same_booking_and_is_reported() {
    capture();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("busy-node", 4, 8).await;
    let job = Job::new("innocent", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;

    let first = daemon.start().await;
    let second = rerun(&mut daemon, &first, MemoryKill::NodePressure, 1).await;
    let third = rerun(&mut daemon, &second, MemoryKill::NodePressure, 1).await;
    let ack = daemon
        .report(killed(third.lease_id, MemoryKill::NodePressure))
        .await;
    assert!(ack.accepted);

    let status = response(&done(&mut ops).await).status.expect("a status");
    assert_eq!(status.code, Code::Internal as i32, "{status:?}");
    assert_eq!(
        status.message,
        "kbf farm fault on busy-node: the node killed the action for memory while it was \
         under its own limit (node memory pressure), and its reruns are used up. Operator \
         fix: find what else holds memory on the node. Runs: 3 (1 GiB on busy-node, node \
         memory pressure, 1 GiB on busy-node, node memory pressure, 1 GiB on busy-node, \
         node memory pressure)."
    );
    for kills in 1..=3 {
        let line = format!("kills={kills}");
        let logged = lines_with(&["node=busy-node", &line, "node memory pressure", "WARN"]);
        assert_eq!(logged.len(), 1, "kill {kills}: {logged:?}");
    }

    // No floor was left: the next Execute books what the action asks.
    let _again = cell.execute(&job.action).await;
    assert_eq!(daemon.start().await.memory_bytes, GIB);
}

/// Catches a server that reads `memory_kill` on an OK Result (the action ran to its
/// end and its result would be thrown away for a rerun).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ok_result_is_never_a_memory_kill() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("ok-node", 4, 8).await;
    let job = Job::new("ran", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&cell, "done", 0).await;
    let report = worker::Result {
        memory_kill: MemoryKill::OwnLimit as i32,
        ..ran(start.lease_id, &result)
    };
    assert!(daemon.report(report).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
    daemon.no_work().await;
}

/// Catches a status without `memory_kill` (every daemon today) read as a memory
/// kill: it ends the operation INTERNAL as before, and is not run again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_without_a_memory_kill_is_not_rerun() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("plain-node", 4, 8).await;
    let job = Job::new("plain", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let unknown = worker::Result {
        memory_kill: 7,
        ..failed(start.lease_id, Code::ResourceExhausted)
    };
    assert!(daemon.report(unknown).await.accepted);
    let status = response(&done(&mut ops).await).status.expect("a status");
    assert_eq!(status.code, Code::Internal as i32);
    daemon.no_work().await;
}

/// Catches the answer's text read from the scheduler's operation after it was
/// answered: with a zero finished retention the scheduler drops the operation as it
/// answers, and the callers would be told of no run, on no node; and a busy node's
/// last kill (the one that finishes the operation) not logged for operators.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_answer_names_the_runs_without_a_finished_retention() {
    capture();
    let cell = Cell::start_with(Duration::from_secs(300), Duration::ZERO).await;
    let mut daemon = cell.daemon("tiny-node", 4, 1).await;
    let job = Job::new("too big", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    assert_eq!(
        start.memory_bytes, GIB,
        "the default booking is the whole node"
    );
    assert!(
        daemon
            .report(killed(start.lease_id, MemoryKill::OwnLimit))
            .await
            .accepted
    );
    let status = response(&done(&mut ops).await).status.expect("a status");
    assert!(
        status
            .message
            .ends_with("1 GiB booked on tiny-node, the largest node for its platform. Runs: 1 (1 GiB on tiny-node)."),
        "{status:?}"
    );

    let mut daemon = cell.daemon("tiny-busy", 4, 1).await;
    let job = Job::new("squeezed", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let mut start = daemon.start().await;
    for _ in 0..2 {
        start = rerun(&mut daemon, &start, MemoryKill::NodePressure, 1).await;
    }
    assert!(
        daemon
            .report(killed(start.lease_id, MemoryKill::NodePressure))
            .await
            .accepted
    );
    let status = response(&done(&mut ops).await).status.expect("a status");
    assert!(
        status.message.contains("Runs: 3 (1 GiB on tiny-busy"),
        "{status:?}"
    );
    let last = lines_with(&["node=tiny-busy", "kills=3", "node memory pressure"]);
    assert_eq!(last.len(), 1, "{last:?}");
}
