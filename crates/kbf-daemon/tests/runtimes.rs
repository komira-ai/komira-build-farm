//! A daemon with two runtimes, each serving one lease kind: every Start reaches the one
//! that serves its kind, a kind neither serves is refused, each lease is killed through
//! the runtime it runs on, and every Hello lists both drivers.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{DriverReport, FakeRuntime, Runtimes};
use kbf_proto::google::rpc::Code;
use kbf_proto::worker::Hello;
use kbf_types::{LeaseId, LeaseKind};
use support::{Harness, PROMPT};
use tokio::sync::watch;
use tokio::time::Instant;

const LONG: Duration = Duration::from_secs(60);

/// Runtime A serves `action` as driver `fake-a`; B serves `whole_machine` as `fake-b`.
fn two() -> (Arc<FakeRuntime>, Arc<FakeRuntime>, Runtimes) {
    let a = Arc::new(FakeRuntime::new(LONG).serving(LeaseKind::Action, "fake-a"));
    let b = Arc::new(FakeRuntime::new(LONG).serving(LeaseKind::WholeMachine, "fake-b"));
    let runtimes = Runtimes::new(Arc::clone(&a))
        .and(Arc::clone(&b))
        .expect("A and B serve different kinds");
    (a, b, runtimes)
}

/// Waits up to [`PROMPT`] for `done`.
async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + PROMPT;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn code(result: &kbf_proto::worker::Result) -> i32 {
    result.status.as_ref().map_or(-1, |s| s.code)
}

/// The `drivers` entries of a Hello, in its (sorted) order.
fn drivers(hello: &Hello) -> Vec<&str> {
    hello
        .capabilities
        .iter()
        .filter(|c| c.key == "drivers")
        .map(|c| c.value.as_str())
        .collect()
}

/// Catches: a Start run on the first runtime whatever its kind (a whole-machine lease
/// run as an action, beside other leases), a Start run on every runtime that could
/// take it, and a Cancel that kills through another runtime than the lease's own (the
/// lease would run on, the scheduler taking its node as free).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_start_runs_on_the_runtime_serving_its_kind() {
    let (a, b, runtimes) = two();
    let mut h = Harness::with_runtimes("by-kind", Arc::clone(&a), runtimes, LONG, None).await;
    let mut peer = h.welcomed().await;

    peer.start(1, 1, "whole_machine");
    until("B starts the whole_machine lease", || {
        !b.started().is_empty()
    })
    .await;
    peer.start(1, 2, "action");
    until("A starts the action lease", || !a.started().is_empty()).await;
    assert_eq!(b.started(), [LeaseId::new(1, 1)], "B ran an action lease");
    assert_eq!(
        a.started(),
        [LeaseId::new(1, 2)],
        "A ran a whole_machine lease"
    );

    peer.cancel(Some((1, 1)));
    let (_, result) = peer.result(PROMPT).await.expect("the cancelled lease ends");
    assert_eq!(result.lease_id.map(|l| (l.term, l.seq)), Some((1, 1)));
    assert_eq!(code(&result), Code::Aborted as i32);
    assert_eq!(b.killed(), [LeaseId::new(1, 1)]);
    assert_eq!(a.killed(), Vec::<LeaseId>::new(), "A killed B's lease");
}

/// Catches: a fence that kills every lease through one runtime (the other runtime's
/// lease would run on beside the copy the scheduler dispatches again), or that
/// reports a lease it never killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fence_kills_each_lease_through_its_own_runtime() {
    let (a, b, runtimes) = two();
    let t = Duration::from_millis(800);
    let mut h = Harness::with_runtimes("fence-both", Arc::clone(&a), runtimes, t, None).await;
    let mut peer = h.welcomed().await;
    peer.start(1, 1, "whole_machine");
    peer.start(1, 2, "action");
    until("both leases start", || {
        !a.started().is_empty() && !b.started().is_empty()
    })
    .await;

    peer.acking
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let mut fenced = Vec::new();
    for _ in 0..2 {
        let (_, result) = peer.result(t + PROMPT).await.expect("a fenced lease");
        assert_eq!(code(&result), Code::Aborted as i32);
        fenced.extend(result.lease_id.map(|l| (l.term, l.seq)));
    }
    fenced.sort_unstable();
    assert_eq!(fenced, [(1, 1), (1, 2)]);
    assert_eq!(b.killed(), [LeaseId::new(1, 1)]);
    assert_eq!(a.killed(), [LeaseId::new(1, 2)]);
}

/// Catches: a kind that neither runtime serves run on one of them (the first, or any)
/// instead of refused at once with FAILED_PRECONDITION, as a daemon with one runtime
/// refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kind_no_runtime_serves_is_refused() {
    let (a, b, runtimes) = two();
    let mut h = Harness::with_runtimes("neither", Arc::clone(&a), runtimes, LONG, None).await;
    let mut peer = h.welcomed().await;
    peer.start(1, 3, "vm");
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    assert_eq!(result.lease_id.map(|l| (l.term, l.seq)), Some((1, 3)));
    assert_eq!(code(&result), Code::FailedPrecondition as i32);
    let message = result.status.map(|s| s.message).unwrap_or_default();
    assert_eq!(message, r#"no driver here serves lease kind "vm""#);
    assert!(a.started().is_empty() && b.started().is_empty());
}

/// Catches: a Hello that lists only the first runtime's driver (or only those of the
/// report the daemon was given), so the server never places a whole_machine lease
/// here; the second driver dropped from the Hello a new stream sends; and dropped when
/// a driver's report changes, which rebuilds the report from the one the daemon
/// started with and resends the Hello.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_hello_lists_every_runtimes_driver() {
    let (a, _b, runtimes) = two();
    let (send, receive) = watch::channel(DriverReport::default());
    let mut h =
        Harness::with_runtimes("union", Arc::clone(&a), runtimes, LONG, Some(receive)).await;
    assert_eq!(
        h.report
            .capabilities()
            .iter()
            .filter(|c| c.key == "drivers")
            .count(),
        1
    );

    let mut peer = h.session().await;
    let first = peer.hello().await;
    assert_eq!(drivers(&first), ["fake-a", "fake-b"]);
    peer.welcome();

    send.send_replace(DriverReport {
        entries: vec![("xcode".to_owned(), "16B40".to_owned())],
        xcodes: Vec::new(),
    });
    let changed = peer.hello().await;
    assert!(
        changed.capabilities.iter().any(|c| c.key == "xcode"),
        "the driver's change: {changed:?}"
    );
    assert_eq!(drivers(&changed), ["fake-a", "fake-b"], "after a change");

    peer.close();
    let again = h.session().await.hello().await;
    assert_eq!(drivers(&again), ["fake-a", "fake-b"], "on a new stream");
    assert_eq!(again.report_hash, changed.report_hash);
}
