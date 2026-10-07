//! Leases the server no longer holds here (issue #23): a `Start` that arrives after the
//! window it names is not run, and a `Cancel` stops a running lease.

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_daemon::Event;
use kbf_proto::google::rpc::Code;
use kbf_proto::worker::LeaseId;
use support::{Harness, INTERVAL, PROMPT};
use tokio::time::Instant;

const LONG: Duration = Duration::from_secs(60);

fn lease(term: u64, seq: u64) -> LeaseId {
    LeaseId { term, seq }
}

fn id(term: u64, seq: u64) -> kbf_types::LeaseId {
    kbf_types::LeaseId::new(term, seq)
}

/// Catches (issue #23): a daemon that runs a `Start` delayed past its window, after the
/// scheduler gave its lease up and granted the operation again: the old lease and its
/// retry would run side by side, two runs of one self-fenced action. Also one that
/// counts the window from the `Start`'s arrival rather than from the send of the
/// heartbeat it names, one that reports a late `Start` (a Result the scheduler would
/// take as the lease's outcome while it may still hold it), and one that lists it.
///
/// The server here does not acknowledge heartbeats, so the daemon still knows when it
/// sent the old one: the `Start` is refused for its lateness alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_that_arrives_after_its_window_does_not_run() {
    let window = Duration::from_millis(500);
    let mut h = Harness::start("late-start", LONG, LONG).await;
    let mut peer = h.welcomed().await;
    peer.acking.store(false, Ordering::SeqCst);
    // The server took this heartbeat, then sent lease 1.1's Start, which is delayed.
    let old = peer.heartbeat().await.seq;
    tokio::time::sleep(window + INTERVAL).await;

    // Meanwhile the lease was given up and granted again as 1.2, whose Start names a
    // fresh heartbeat. It runs.
    let fresh = peer.heartbeat_after(Instant::now()).await.seq;
    assert!(fresh > old);
    peer.start_within(Some((1, 2)), fresh, window);
    h.started(1).await;

    // The delayed Start arrives now. It is not run, reported or listed.
    peer.start_within(Some((1, 1)), old, window);
    h.event(PROMPT, |e| {
        (*e == Event::StartExpired(id(1, 1))).then_some(())
    })
    .await
    .expect("the late Start is refused");
    // So is one that names no lease.
    peer.start_within(None, old, window);
    let listed = peer.heartbeat_after(Instant::now()).await;
    assert_eq!(listed.running, [lease(1, 2)]);
    assert_eq!(h.runtime.started(), [id(1, 2)], "the late Start ran");
    assert!(
        peer.result(4 * INTERVAL).await.is_none(),
        "a Result for a Start that never ran"
    );
}

/// Catches: a `Start` from a server that sends no window (one that predates the
/// field) refused, and the first `Start` of a stream, which names the Hello because
/// the server has taken no heartbeat yet, refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_without_a_window_or_naming_the_hello_runs() {
    let mut h = Harness::start("start-window-hello", LONG, LONG).await;
    let peer = h.welcomed().await;
    peer.start_within(Some((1, 1)), 0, Duration::from_secs(5));
    h.started(1).await;
    peer.start_within(Some((1, 2)), 99, Duration::ZERO);
    h.started(2).await;
    assert_eq!(h.runtime.started(), [id(1, 1), id(1, 2)]);
}

/// Catches (issue #23): a daemon that ignores the server's `Cancel` of a lease it runs
/// beside that lease's retry (the run goes on, wasting the room and running the
/// action twice), one that kills the wrong lease or every lease, one that stops
/// listing the cancelled lease before its run has stopped and its Result is
/// acknowledged, and one that fails on a `Cancel` of no lease or of a lease it does
/// not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_stops_a_running_duplicate() {
    let mut h = Harness::start("cancel", LONG, LONG).await;
    let mut peer = h.welcomed().await;
    // 1.1 is the late run of a lease given up; 1.2 is its retry.
    peer.start(1, 1, "action");
    peer.start(1, 2, "action");
    h.started(2).await;

    peer.cancel(None);
    peer.cancel(Some((1, 9)));
    peer.cancel(Some((1, 1)));
    h.event(PROMPT, |e| (*e == Event::Cancelled(id(1, 1))).then_some(()))
        .await
        .expect("the cancel began a kill");
    let (_, result) = peer
        .result(PROMPT)
        .await
        .expect("the cancelled run's Result");
    assert_eq!(result.lease_id, Some(lease(1, 1)));
    assert_eq!(
        result.status.map(|s| s.code),
        Some(Code::Aborted as i32),
        "the cancelled run did not end killed"
    );
    assert_eq!(h.runtime.killed(), [id(1, 1)]);
    let listed = peer.heartbeat_after(Instant::now()).await;
    assert_eq!(listed.running, [lease(1, 1), lease(1, 2)]);

    peer.ack_result(Some((1, 1)), false);
    let cleared = peer
        .expect(PROMPT, |m| match m {
            kbf_proto::worker::daemon_message::Message::Heartbeat(hb)
                if hb.running == [lease(1, 2)] =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
    assert!(
        cleared.is_some(),
        "the retry stopped or the cancelled lease is still listed"
    );
    assert_eq!(h.runtime.killed(), [id(1, 1)], "the retry was killed");
}

/// The same rules with a runtime that runs real processes. Catches: a `Cancel` that
/// does not stop the process of a cancelled lease (the action runs on for its full
/// minute, and its Result never comes in time), and a late `Start` run by this runtime.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_stops_a_process_and_a_late_start_runs_none() {
    use std::sync::Arc;

    use kbf_daemon::LocalRuntime;
    use support::memory::{MemoryCas, Spec};

    let cas = Arc::new(MemoryCas::default());
    let runtime = Arc::new(LocalRuntime::new(
        Arc::clone(&cas),
        support::scratch("cancel-local"),
    ));
    let mut h = Harness::with_runtime("cancel-local", runtime, LONG).await;
    let mut peer = h.welcomed().await;
    let sleeper = Spec::sh("sleep 60").store(&cas);
    let window = Duration::from_secs(5);

    // Heartbeat 99 was never sent on this stream.
    peer.start_action_within(Some((1, 1)), sleeper.clone(), 99, window);
    peer.start_action_within(None, sleeper.clone(), 99, window);
    h.event(PROMPT, |e| {
        (*e == Event::StartExpired(id(1, 1))).then_some(())
    })
    .await
    .expect("the late Start is refused");

    peer.start_action_within(Some((1, 2)), sleeper, 0, window);
    let listed = peer
        .expect(PROMPT, |m| match m {
            kbf_proto::worker::daemon_message::Message::Heartbeat(hb)
                if hb.running == [lease(1, 2)] =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
    assert!(listed.is_some(), "the lease did not start");
    peer.cancel(None);
    peer.cancel(Some((1, 9)));
    peer.cancel(Some((1, 2)));
    h.event(PROMPT, |e| (*e == Event::Cancelled(id(1, 2))).then_some(()))
        .await
        .expect("the cancel began a kill");
    let (_, result) = peer.result(PROMPT).await.expect("the killed run's Result");
    assert_eq!(result.lease_id, Some(lease(1, 2)));
    assert_eq!(result.status.map(|s| s.code), Some(Code::Aborted as i32));
}
