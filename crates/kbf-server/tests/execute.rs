//! The Execute join end to end, in one process: a REAPI client, the server, and a fake
//! daemon on the worker stream. Each test drives the wire on both sides.

#[path = "execute/memory.rs"]
mod memory;
mod support;

use std::time::Duration;

use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_proto::reapi::{ActionResult, WaitExecutionRequest};
use kbf_proto::worker::daemon_message;
use kbf_proto::worker::server_message::Message;
use support::{
    Blob, Cell, FakeDaemon, Job, done, done_within_quiet, failed, hello_on, output, ran, response,
    stage,
};
use tonic::Code;

/// Runs `job` on `daemon` to completion and returns the result it reported.
/// The daemon reports with no status at all, which counts as OK.
async fn run_once(
    cell: &support::Client,
    daemon: &mut support::FakeDaemon,
    job: &Job,
) -> ActionResult {
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(cell, "built once", 0).await;
    let mut report = ran(start.lease_id, &result);
    report.status = None;
    assert!(daemon.report(report).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    result
}

/// Catches: a `Start` sent before (or without) the lease offer that the grant's commit
/// follows; a `Start` naming another action or kind, or without the booking; an
/// operation that never reaches EXECUTING or never completes; a completed operation
/// that does not carry the daemon's `ActionResult`; an accepted result missing from the action cache, or a
/// `ResultAck` that does not say it was accepted; and a second result accepted for a
/// finished operation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_runs_on_a_daemon_and_caches_the_result() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("build", &[]);
    cell.upload(&job.blobs()).await;

    let mut ops = cell.execute(&job.action).await;
    let first_work = daemon
        .expect("work", |m| match m {
            Message::LeaseOffer(_) | Message::Start(_) => Some(m.clone()),
            _ => None,
        })
        .await;
    let Message::LeaseOffer(offer) = first_work else {
        panic!("{first_work:?} came before the offer");
    };
    let start = daemon.start().await;
    assert_eq!(
        offer.lease_id, start.lease_id,
        "the Start is the offered lease's"
    );
    assert_eq!(start.kind, "action");
    assert_eq!(start.action_digest.as_ref(), Some(&job.action.proto));
    assert_eq!(offer.action_digest, start.action_digest);
    // The booking reaches the daemon, which sizes the lease's cgroup from it.
    assert_eq!(
        (start.millicpus, start.memory_bytes),
        (
            kbf_front::DEFAULT_RESOURCES.cpu_millis,
            kbf_front::DEFAULT_RESOURCES.memory_bytes
        )
    );

    let first = ops.message().await.expect("a stream").expect("an update");
    assert!(!first.done);
    assert_eq!(stage(&first), ExecStage::Executing as i32, "placed at once");
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));

    let result = output(&cell, "the build output", 0).await;
    let ack = daemon.report(ran(start.lease_id, &result)).await;
    assert!(ack.accepted, "{ack:?}");
    let last = done(&mut ops).await;
    assert_eq!(stage(&last), ExecStage::Completed as i32);
    let answer = response(&last);
    assert_eq!(answer.result, Some(result.clone()));
    assert!(!answer.cached_result);
    assert_eq!(answer.status.map(|s| s.code), Some(Code::Ok as i32));
    assert_eq!(cell.cached(&job.action).await, Ok(result.clone()));

    // A copy resent after a reconnect is acknowledged, not accepted a second time.
    let other = output(&cell, "a second copy that differs", 0).await;
    let ack = daemon.report(ran(start.lease_id, &other)).await;
    assert!(!ack.accepted, "a second result accepted for one operation");
    assert_eq!(cell.cached(&job.action).await, Ok(result));
}

/// Catches: a result written to the action cache when it arrives rather than when the
/// scheduler accepts it, so a stale lease's result (its lease was given up and granted
/// again) reaches the cache; and a result accepted from a node that does not hold the
/// operation's current lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_lease_result_is_rejected_and_never_cached() {
    let cell = Cell::start().await;
    let mut first = cell.daemon("node-a", 4, 8).await;
    // Registered with no CPU, so nothing is placed on it.
    let mut other = cell.daemon("node-b", 0, 8).await;
    let job = Job::new("flaky", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let stale = first.start().await;

    // node-a's daemon restarts and lists nothing: the old lease is given up at once
    // and the operation granted again, on the new stream.
    let mut again = cell.daemon("node-a", 4, 8).await;
    assert!(again.heartbeat(&[]).await);
    let current = again.start().await;
    assert_ne!(current.lease_id, stale.lease_id);

    let stale_result = output(&cell, "from the stale lease", 0).await;
    let ack = again.report(ran(stale.lease_id, &stale_result)).await;
    assert!(!ack.accepted, "a stale lease's result accepted");
    assert_eq!(
        cell.cached(&job.action).await,
        Err(Code::NotFound),
        "cached"
    );
    assert!(
        !cell.holds(&Blob::of(&stale_result)).await,
        "a stale lease's result was taken in (its ActionResult stored)"
    );

    let thief = output(&cell, "from a node without the lease", 0).await;
    let ack = other.report(ran(current.lease_id, &thief)).await;
    assert!(!ack.accepted, "another node's result accepted");
    assert_eq!(
        cell.cached(&job.action).await,
        Err(Code::NotFound),
        "cached"
    );
    assert!(
        !done_within_quiet(&mut ops).await,
        "answered by a refused result"
    );

    let result = output(&cell, "from the current lease", 0).await;
    assert!(again.report(ran(current.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    assert_eq!(cell.cached(&job.action).await, Ok(result));
    // The old stream's daemon still holds nothing the server will take.
    let late = output(&cell, "late, on the old stream", 0).await;
    assert!(!first.report(ran(stale.lease_id, &late)).await.accepted);
}

/// Catches: a result written to the action cache before the scheduler accepts it. The
/// result arrives while its lease is current, and the lease is given up while the
/// server checks the result's outputs: the scheduler then refuses it, and the cache
/// must never see it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_whose_lease_is_given_up_while_it_is_checked_is_never_cached() {
    let cell = Cell::start().await;
    let mut first = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("raced", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let lost = first.start().await;
    let raced = output(&cell, "from the lease given up", 0).await;

    cell.cache.meta().query.close_next();
    first.send(daemon_message::Message::Result(ran(lost.lease_id, &raced)));
    cell.cache.meta().query.held().await;
    let mut again = cell.daemon("node-a", 4, 8).await;
    assert!(again.heartbeat(&[]).await);
    let current = again.start().await;
    cell.cache.meta().query.open();

    let ack = first
        .expect("ResultAck", |m| match m {
            Message::ResultAck(a) => Some(*a),
            _ => None,
        })
        .await;
    assert!(!ack.accepted, "a result whose lease was given up accepted");
    assert_eq!(
        cell.cached(&job.action).await,
        Err(Code::NotFound),
        "cached"
    );

    let result = output(&cell, "from the current lease", 0).await;
    assert!(again.report(ran(current.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    assert_eq!(cell.cached(&job.action).await, Ok(result));
}

/// Catches: an Execute that dispatches an action whose result is cached (work run
/// again), or answers it without `cached_result`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cached_action_is_answered_without_dispatch() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("cached", &[]);
    cell.upload(&job.blobs()).await;
    let result = run_once(&cell, &mut daemon, &job).await;

    let mut ops = cell.execute(&job.action).await;
    let only = ops.message().await.expect("a stream").expect("an update");
    let answer = response(&only);
    assert!(answer.cached_result);
    assert_eq!(answer.result, Some(result));
    assert_eq!(
        ops.message().await.expect("a stream"),
        None,
        "more after done"
    );
    daemon.no_work().await;

    // `skip_cache_lookup` runs it again.
    let mut ops = cell.execute_with(&job.action, true).await.expect("Execute");
    let start = daemon.start().await;
    let fresh = output(&cell, "built again", 0).await;
    assert!(daemon.report(ran(start.lease_id, &fresh)).await.accepted);
    assert!(!response(&done(&mut ops).await).cached_result);
}

/// Catches: in-flight dedup that does not join identical concurrent Executes (the
/// action runs twice), and a joined caller that is not answered with the one result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_concurrent_executes_dispatch_once() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 8, 16).await;
    let job = Job::new("shared", &[]);
    cell.upload(&job.blobs()).await;

    let mut one = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let mut two = cell.execute(&job.action).await;
    let joined = two.message().await.expect("a stream").expect("an update");
    assert_eq!(
        stage(&joined),
        ExecStage::Executing as i32,
        "joiner sees it run"
    );
    daemon.no_work().await;

    let result = output(&cell, "built for both", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut one).await).result, Some(result.clone()));
    assert_eq!(response(&done(&mut two).await).result, Some(result));
}

/// Catches: `do_not_cache` work that is joined to a twin, or written to the action
/// cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn do_not_cache_is_neither_joined_nor_cached() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 8, 16).await;
    let mut job = Job::new("uncached", &[]);
    let action = kbf_proto::reapi::Action {
        command_digest: Some(job.command.proto.clone()),
        input_root_digest: Some(job.root.proto.clone()),
        do_not_cache: true,
        ..Default::default()
    };
    job.action = Blob::of(&action);
    cell.upload(&job.blobs()).await;

    let mut one = cell.execute(&job.action).await;
    let mut two = cell.execute(&job.action).await;
    let a = daemon.start().await;
    let b = daemon.start().await;
    assert_ne!(a.lease_id, b.lease_id);
    let result = output(&cell, "not for the cache", 0).await;
    assert!(daemon.report(ran(a.lease_id, &result)).await.accepted);
    assert!(daemon.report(ran(b.lease_id, &result)).await.accepted);
    done(&mut one).await;
    done(&mut two).await;
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
}

/// Catches: a failing action (non-zero exit) written to the action cache, which would
/// answer every later build with the failure (RFC 5.8: failures are not cached).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_action_is_answered_but_not_cached() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("fails", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&cell, "a test failed", 1).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
}

/// Catches: an attempt's failure passed to the callers as the daemon's status rather
/// than the RFC's (INTERNAL for the farm's failure, DEADLINE_EXCEEDED for a timeout);
/// an INVALID_ARGUMENT (the action's own fault, such as an image named by tag) turned
/// into INTERNAL, which a client retries, or answered without the daemon's reason; a
/// failed attempt placed again instead of answered (each case's Start must name its
/// own action); an OK result whose outputs were never uploaded, or that carries no result at all,
/// accepted as a result (callers would get files nobody can fetch); a failed attempt
/// that also carries a valid `ActionResult` taken as completed (the protocol sets
/// `action_result` only with OK); and a failure written to the action cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_attempts_answer_with_the_rfc_codes() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 8, 16).await;
    let never_uploaded = ActionResult {
        stdout_digest: Some(Blob::new("never uploaded").proto),
        ..Default::default()
    };
    let stored = output(&cell, "stored, but the attempt failed", 0).await;
    let stored_too = stored.clone();
    let cases = [
        (
            "aborted with a result",
            Some(Code::Aborted),
            Some(stored.clone()),
            Code::Internal,
        ),
        (
            "timed out with a result",
            Some(Code::DeadlineExceeded),
            Some(stored),
            Code::DeadlineExceeded,
        ),
        ("aborted", Some(Code::Aborted), None, Code::Internal),
        (
            "timed out",
            Some(Code::DeadlineExceeded),
            None,
            Code::DeadlineExceeded,
        ),
        (
            "outputs missing",
            Some(Code::Ok),
            Some(never_uploaded),
            Code::Internal,
        ),
        ("OK without a result", Some(Code::Ok), None, Code::Internal),
        (
            "invalid",
            Some(Code::InvalidArgument),
            None,
            Code::InvalidArgument,
        ),
        (
            "invalid with a result",
            Some(Code::InvalidArgument),
            Some(stored_too),
            Code::InvalidArgument,
        ),
    ];
    for (name, code, result, want) in cases {
        let job = Job::new(name, &[]);
        cell.upload(&job.blobs()).await;
        let mut ops = cell.execute(&job.action).await;
        let start = daemon.start().await;
        assert_eq!(
            start.action_digest.as_ref(),
            Some(&job.action.proto),
            "{name}"
        );
        let mut report = failed(start.lease_id, code.expect("a code"));
        report.action_result = result;
        assert!(daemon.report(report).await.accepted, "{name}");
        let answer = response(&done(&mut ops).await);
        assert_eq!(answer.result, None, "{name}");
        let status = answer.status.expect("a status");
        assert_eq!(status.code, want as i32, "{name}");
        if want == Code::InvalidArgument {
            // The daemon's reason (`failed` sends "test") reaches the client.
            assert_eq!(status.message, "test", "{name}");
        }
        assert_eq!(
            cell.cached(&job.action).await,
            Err(Code::NotFound),
            "{name}"
        );
    }
}

/// Catches: callers answered before the accepted result's action-cache entry is
/// written, so a client that looks the action up as soon as its Execute finishes
/// misses a result the farm has. The write is held at a gate: no answer may arrive
/// while it is held, and once it is let through the answer and the entry are both
/// there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cache_entry_is_written_before_the_callers_are_answered() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("written first", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&cell, "in the cache first", 0).await;

    let gate = &cell.cache.meta().action_write;
    gate.close_next();
    daemon.send(daemon_message::Message::Result(ran(
        start.lease_id,
        &result,
    )));
    gate.held().await;
    assert!(
        !done_within_quiet(&mut ops).await,
        "answered before the action-cache write"
    );
    gate.open();
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    assert_eq!(cell.cached(&job.action).await, Ok(result));
    let ack = daemon
        .expect("ResultAck", |m| match m {
            Message::ResultAck(a) => Some(*a),
            _ => None,
        })
        .await;
    assert!(ack.accepted);
}

/// Catches: an accepted result whose action-cache write fails left unanswered (its
/// callers would wait forever) or answered as a failure. The run succeeded; only the
/// cache misses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_cache_write_still_answers_the_callers() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("unwritten", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&cell, "answered, not cached", 0).await;

    cell.cache.meta().fail_next_action_write();
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    let answer = response(&done(&mut ops).await);
    assert_eq!(answer.result, Some(result));
    assert_eq!(answer.status.map(|s| s.code), Some(Code::Ok as i32));
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
}

/// Catches: a WaitExecution that cannot find a running operation by the name Execute
/// gave, or does not deliver its result; and one that answers for a name it never gave.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_execution_follows_a_running_operation() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("waited", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let name = ops
        .message()
        .await
        .expect("a stream")
        .expect("an update")
        .name;
    let mut waited = cell
        .exec()
        .wait_execution(WaitExecutionRequest { name: name.clone() })
        .await
        .expect("WaitExecution")
        .into_inner();
    let start = daemon.start().await;
    let result = output(&cell, "for the waiter", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    let last = done(&mut waited).await;
    assert_eq!(last.name, name);
    assert_eq!(response(&last).result, Some(result));
}

/// Catches (issue #165): a finished operation forgotten as soon as it is answered,
/// so a client whose Execute stream broke just before the answer cannot get it by
/// WaitExecution, and runs again an action whose result the cache does not keep (a
/// non-zero exit); and one kept past the retention (the farm's memory then grows with
/// every operation). Within the retention WaitExecution streams the done operation
/// with its result, once; after it the name is NOT_FOUND.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_operation_is_waited_on_within_the_retention_then_not_found() {
    let retention = Duration::from_secs(1);
    let cell = Cell::start_with(kbf_sched::UNSERVABLE_WAIT, retention).await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("exits 1", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&cell, "not cached", 1).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    let name = done(&mut ops).await.name;
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));

    let mut waited = wait_stream(&cell, &name).await.expect("kept");
    let first = waited
        .message()
        .await
        .expect("a stream")
        .expect("an update");
    assert_eq!(first.name, name);
    assert_eq!(response(&first).result, Some(result));
    assert!(
        waited.message().await.expect("a stream").is_none(),
        "more after done"
    );

    tokio::time::sleep(retention + Duration::from_millis(500)).await;
    assert_eq!(wait_name(&cell, &name).await, Err(Code::NotFound));
}

/// WaitExecution on `name`: its stream, or the code.
async fn wait_stream(
    cell: &support::Client,
    name: &str,
) -> Result<tonic::Streaming<kbf_proto::google::longrunning::Operation>, Code> {
    let ops = cell
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: name.to_owned(),
        })
        .await
        .map_err(|s| s.code())?;
    Ok(ops.into_inner())
}

/// WaitExecution on `name`: the name of the first operation it streams, or the code.
async fn wait_name(cell: &support::Client, name: &str) -> Result<String, Code> {
    let mut ops = wait_stream(cell, name).await?;
    Ok(ops
        .message()
        .await
        .expect("a healthy stream")
        .expect("an update")
        .name)
}

/// Catches (issue #154): a WaitExecution that still answers the old spelling
/// `operations/{n}`, which a client of a server from before the term was in the name
/// can hold across an upgrade restart. It would attach that client to whichever
/// operation of the new process has number `n`. The operation running as number 0 is
/// found by its own name only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_execution_refuses_the_name_spelling_without_a_term() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("number 0", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let name = ops
        .message()
        .await
        .expect("a stream")
        .expect("an update")
        .name;
    daemon.start().await;
    assert!(name.ends_with("-0"), "{name} is not number 0");

    assert_eq!(wait_name(&cell, "operations/0").await, Err(Code::NotFound));
    assert_eq!(wait_name(&cell, &name).await, Ok(name));
}

/// Catches: a refused operation whose caller is never forgotten (WaitExecution on its
/// name would replay the refusal forever, and the farm keep it), or forgotten at once
/// (a caller that reconnects within the retention loses the reason). Within the
/// retention WaitExecution streams the refusal; after it, NOT_FOUND, as for every
/// other finished operation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_operation_is_forgotten_after_the_retention() {
    let retention = Duration::from_secs(1);
    let cell = Cell::start_with(Duration::from_millis(300), retention).await;
    let mut linux = cell.daemon("node-b", 4, 8).await;
    let job = Job::new("mac only", &[("OSFamily", "macos")]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let name = ops
        .message()
        .await
        .expect("a stream")
        .expect("an update")
        .name;
    assert_eq!(
        wait_name(&cell, &name).await,
        Ok(name.clone()),
        "while queued"
    );

    let status = response(&done(&mut ops).await).status.expect("a status");
    assert_eq!(status.code, Code::FailedPrecondition as i32, "{status:?}");
    let mut waited = wait_stream(&cell, &name).await.expect("kept");
    let first = waited
        .message()
        .await
        .expect("a stream")
        .expect("an update");
    let again = response(&first).status.expect("a status");
    assert_eq!(again, status, "the refusal is not the one answered");
    linux.no_work().await;

    tokio::time::sleep(retention + Duration::from_millis(500)).await;
    assert_eq!(wait_name(&cell, &name).await, Err(Code::NotFound));
}

/// Catches: a `kbf-lease` kind dropped on the way to the `Start` (a whole-machine
/// lease run as a shared one); a whole-machine lease offered to a daemon whose drivers
/// serve only actions (`mac-0` sorts first, so placement that ignores the kind offers
/// it there); and one that books less than the whole daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_whole_machine_lease_starts_only_on_a_daemon_that_serves_it() {
    let cell = Cell::start().await;
    let mut shared = cell.daemon("mac-0", 4, 8).await;
    let report = [("arch", "arm64"), ("drivers", "native-whole-machine")];
    let mut whole = FakeDaemon::connect(cell.worker_addr, hello_on("mac-1", 4, 8, &report))
        .await
        .expect("registered");
    let job = Job::new("whole", &[("kbf-lease", "whole_machine")]);
    cell.upload(&job.blobs()).await;
    let _ops = cell.execute(&job.action).await;
    let offer = whole.offer().await;
    let start = whole.start().await;
    assert_eq!(offer.kind, "whole_machine");
    assert_eq!(start.kind, "whole_machine");
    assert_eq!((start.millicpus, start.memory_bytes), (4_000, 8 << 30));
    shared.no_work().await;
}
