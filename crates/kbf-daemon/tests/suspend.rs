//! The fence across a suspend (issue #78). Each daemon here reads an injected clock
//! that the test jumps forward, as the suspend-counting clock jumps on a resume, while
//! the test's own clock and every tokio timer keep running at their usual pace.
//!
//! The fence time T is 30 s and the server asks for a heartbeat every 10 s, so neither
//! a fence deadline nor a heartbeat falls inside a test by the uptime clock: whatever
//! fences here was noticed through the injected clock.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Event, FakeRuntime};
use kbf_proto::google::rpc::Code;
use kbf_proto::worker::LeaseId;
use support::{Harness, PROMPT};
use tokio::time::Instant;

const T: Duration = Duration::from_secs(30);
const INTERVAL: Duration = Duration::from_secs(10);
const LONG: Duration = Duration::from_secs(60);
/// The recheck tick of the tests that rely on it.
const TICK: Duration = Duration::from_millis(200);

fn id(term: u64, seq: u64) -> kbf_types::LeaseId {
    kbf_types::LeaseId::new(term, seq)
}

fn code(result: &kbf_proto::worker::Result) -> i32 {
    result.status.as_ref().map_or(-1, |s| s.code)
}

/// Catches: the fence read on a clock that stops during suspend (the code before issue
/// #78, or an injected clock ignored), and a fence noticed only when its deadline comes
/// round on tokio's clock (no recheck tick): either way the woken lease runs on, here
/// for longer than the test waits. Also catches a fence on a suspend shorter than T.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspend_past_t_fences_running_work_within_a_tick() {
    let runtime = Arc::new(FakeRuntime::new(LONG));
    let (mut h, clock) = Harness::suspendable("suspend-tick", runtime, T, TICK).await;
    let mut peer = h.session().await;
    peer.hello().await;
    peer.welcome_every(INTERVAL);
    peer.start(1, 1, "action");
    h.started(1).await;

    clock.suspend(T / 2);
    assert!(
        peer.result(5 * TICK).await.is_none(),
        "fenced after a suspend shorter than T"
    );
    assert!(h.runtime.killed().is_empty());

    let resumed = Instant::now();
    clock.suspend(T);
    let (at, result) = peer
        .result(PROMPT)
        .await
        .expect("the woken daemon fences and reports");
    assert!(at - resumed < INTERVAL, "noticed only by a heartbeat");
    assert_eq!(result.lease_id, Some(LeaseId { term: 1, seq: 1 }));
    assert_eq!(code(&result), Code::Aborted as i32);
    assert_eq!(h.runtime.killed(), [id(1, 1)]);
    h.event(PROMPT, |e| {
        matches!(e, Event::Fenced(ids) if ids == &[id(1, 1)]).then_some(())
    })
    .await
    .expect("a Fenced event");
}

/// Catches: a Result sent for a run that finished after a resume past T without the
/// fence being checked first. The recheck tick is longer than the test, so the run's
/// end is the first thing that wakes the daemon after the jump; it must report the
/// fence's ABORTED, never the run's own success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_ends_after_a_suspend_past_t_reports_the_fence() {
    let runtime = Arc::new(FakeRuntime::new(Duration::from_millis(1500)));
    let (mut h, clock) = Harness::suspendable("suspend-result", runtime, T, LONG).await;
    let mut peer = h.session().await;
    peer.hello().await;
    peer.welcome_every(INTERVAL);
    peer.start(1, 2, "action");
    h.started(1).await;

    clock.suspend(2 * T);
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    assert_eq!(result.lease_id, Some(LeaseId { term: 1, seq: 2 }));
    assert_eq!(
        code(&result),
        Code::Aborted as i32,
        "the run's own Result was sent"
    );
    assert!(
        peer.result(Duration::from_millis(300)).await.is_none(),
        "a second Result for the lease"
    );
    h.event(PROMPT, |e| {
        matches!(e, Event::Fenced(ids) if ids == &[id(1, 2)]).then_some(())
    })
    .await
    .expect("a Fenced event");
}

/// Catches: a daemon that, waiting for Welcome on a new stream, fences on tokio's clock
/// or never rechecks: a suspend past T while no stream is up must fence too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspend_past_t_while_offline_fences() {
    let runtime = Arc::new(FakeRuntime::new(LONG));
    let (mut h, clock) = Harness::suspendable("suspend-offline", runtime, T, TICK).await;
    let mut first = h.session().await;
    first.hello().await;
    first.welcome_every(INTERVAL);
    first.start(1, 3, "action");
    h.started(1).await;
    first.close();

    let mut second = h.session().await;
    second.hello().await;
    // No Welcome: the daemon waits for one, offline.
    clock.suspend(2 * T);
    h.event(PROMPT, |e| {
        matches!(e, Event::Fenced(ids) if ids == &[id(1, 3)]).then_some(())
    })
    .await
    .expect("fenced while offline");
    assert_eq!(h.runtime.killed(), [id(1, 3)]);
}

/// Catches: the Start window measured on a clock that stops during suspend, or on
/// another clock than the one its heartbeats' send times were read on. A Start naming
/// the first heartbeat runs while inside its window; one handled after a suspend
/// longer than the window arrived late by the suspend-counting clock and must not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_that_waited_out_a_suspend_does_not_run() {
    let runtime = Arc::new(FakeRuntime::new(LONG));
    let (mut h, clock) = Harness::suspendable("suspend-start", runtime, T, LONG).await;
    let mut peer = h.session().await;
    peer.hello().await;
    peer.welcome_every(INTERVAL);
    // The first heartbeat goes out at Welcome; the server acknowledges it.
    assert_eq!(peer.heartbeat().await.seq, 1);
    // Within the window by the injected clock: it runs.
    peer.start_within(Some((1, 3)), 1, Duration::from_secs(14));
    h.started(1).await;

    // Less than T, so contact is not lost; more than the Start's window.
    clock.suspend(T / 2);
    peer.start_within(Some((1, 4)), 1, Duration::from_secs(14));
    h.event(PROMPT, |e| {
        (*e == Event::StartExpired(id(1, 4))).then_some(())
    })
    .await
    .expect("the late Start is dropped");
    assert_eq!(h.runtime.started(), [id(1, 3)], "a late Start ran");
}

/// Catches: a Start run on a node whose contact lapsed during a suspend. Nothing was
/// running to fence, but the suspend outlasted T, so the scheduler may have given the
/// node up; the Start is refused with UNAVAILABLE rather than run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_after_a_suspend_past_t_is_refused() {
    let runtime = Arc::new(FakeRuntime::new(LONG));
    let (mut h, clock) = Harness::suspendable("suspend-refused", runtime, T, LONG).await;
    let mut peer = h.session().await;
    peer.hello().await;
    peer.welcome_every(INTERVAL);
    assert_eq!(peer.heartbeat().await.seq, 1);

    clock.suspend(2 * T);
    // No window: the Start is judged on contact alone.
    peer.start_within(Some((1, 5)), 1, Duration::ZERO);
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    assert_eq!(result.lease_id, Some(LeaseId { term: 1, seq: 5 }));
    assert_eq!(code(&result), Code::Unavailable as i32);
    assert!(
        h.runtime.started().is_empty(),
        "a Start ran without contact"
    );
}

/// Catches: offline, a run that ends after a resume past T reported before the fence is
/// checked; its own success would go out after the next Welcome. The daemon waits for
/// Welcome on a second stream when the clock jumps (the 300 ms sleep makes sure it is
/// waiting there, not still connecting), the run ends while it still waits, and only
/// then does the Welcome come: the resent Result must be the fence's ABORTED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_offline_run_ending_after_a_suspend_past_t_reports_the_fence() {
    let runtime = Arc::new(FakeRuntime::new(Duration::from_millis(1500)));
    let (mut h, clock) = Harness::suspendable("suspend-offline-done", runtime, T, LONG).await;
    let mut first = h.session().await;
    first.hello().await;
    first.welcome_every(INTERVAL);
    first.start(1, 7, "action");
    h.started(1).await;
    first.close();

    let mut second = h.session().await;
    second.hello().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    clock.suspend(2 * T);
    // The run (1.5 s) ends while the daemon still waits for Welcome.
    tokio::time::sleep(Duration::from_secs(2)).await;
    second.welcome_every(INTERVAL);
    let (_, result) = second.result(PROMPT).await.expect("a Result");
    assert_eq!(result.lease_id, Some(LeaseId { term: 1, seq: 7 }));
    assert_eq!(
        code(&result),
        Code::Aborted as i32,
        "the run's own Result was sent"
    );
}

/// Catches: contact counted from when a Welcome arrives rather than from when the Hello
/// it answers was sent. The Hello goes out before a suspend past T and the Welcome comes
/// after it: the server heard nothing sent after the resume, so contact stays lost and a
/// Start is refused UNAVAILABLE. Nothing is running, so no fence hides the difference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_welcome_for_a_hello_sent_before_a_suspend_does_not_restore_contact() {
    let runtime = Arc::new(FakeRuntime::new(LONG));
    let (mut h, clock) = Harness::suspendable("suspend-welcome", runtime, T, LONG).await;
    let mut first = h.session().await;
    first.hello().await;
    first.welcome_every(INTERVAL);
    assert_eq!(first.heartbeat().await.seq, 1);
    first.close();

    let mut second = h.session().await;
    second.hello().await;
    clock.suspend(2 * T);
    second.welcome_every(INTERVAL);
    // No window: the Start is judged on contact alone.
    second.start_within(Some((1, 8)), 1, Duration::ZERO);
    let (_, result) = second.result(PROMPT).await.expect("a Result");
    assert_eq!(result.lease_id, Some(LeaseId { term: 1, seq: 8 }));
    assert_eq!(code(&result), Code::Unavailable as i32);
    assert!(
        h.runtime.started().is_empty(),
        "a Start ran without contact"
    );
}

/// Catches: a stream set up after a resume past T whose Hello goes out, and whose
/// Welcome would count as contact, before the fence is checked. The server holds the
/// new connection while the clock jumps, so nothing wakes the daemon between the jump
/// and the connection completing; that completion must fence before the Hello is sent.
/// (The Hello's send time is after the resume, so its Welcome would otherwise renew
/// contact with the lease still running.) The daemon's own event order is the
/// evidence: a check made only by a later wake would put `Fenced` after `HelloSent`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_after_a_suspend_past_t_fences_before_its_hello() {
    let runtime = Arc::new(FakeRuntime::new(LONG));
    let (mut h, clock) = Harness::suspendable("suspend-connect", runtime, T, LONG).await;
    let mut first = h.session().await;
    first.hello().await;
    first.welcome_every(INTERVAL);
    first.start(1, 9, "action");
    h.started(1).await;

    h.hold_connections(true);
    first.close();
    h.event(PROMPT, |e| {
        matches!(e, Event::Disconnected(_)).then_some(())
    })
    .await
    .expect("the stream ends");
    // Past the 100 ms reconnect wait: the daemon is connecting, held by the server.
    tokio::time::sleep(Duration::from_millis(500)).await;
    clock.suspend(2 * T);
    h.hold_connections(false);

    let mut fenced = false;
    let (_, fenced_first) = h
        .event(PROMPT, |e| match e {
            Event::Fenced(ids) if ids == &[id(1, 9)] => {
                fenced = true;
                None
            }
            Event::HelloSent => Some(fenced),
            _ => None,
        })
        .await
        .expect("a Hello on the new stream");
    assert!(fenced_first, "the Hello went out before the fence");
    assert_eq!(h.runtime.killed(), [id(1, 9)]);

    let mut second = h.session().await;
    second.hello().await;
    second.welcome_every(INTERVAL);
    let (_, result) = second.result(PROMPT).await.expect("the fence's Result");
    assert_eq!(result.lease_id, Some(LeaseId { term: 1, seq: 9 }));
    assert_eq!(code(&result), Code::Aborted as i32);
}
