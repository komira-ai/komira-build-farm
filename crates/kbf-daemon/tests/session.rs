//! The daemon against an in-process fake server over a loopback mutual-TLS stream.
//!
//! Every test here also proves the TLS setup: the fake server requires a client
//! certificate from its CA, and the daemon verifies the server's against the CA file it
//! was given. A daemon that presented no certificate would never reach Hello.

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_daemon::Event;
use kbf_proto::google::rpc::Code;
use kbf_proto::worker::{Hello, LeaseId};
use prost::Message;
use sha2::{Digest, Sha256};
use support::{Harness, INTERVAL, PROMPT};
use tokio::time::Instant;

const LONG: Duration = Duration::from_secs(60);

fn lease(term: u64, seq: u64) -> Option<LeaseId> {
    Some(LeaseId { term, seq })
}

fn code(result: &kbf_proto::worker::Result) -> i32 {
    result.status.as_ref().map_or(-1, |s| s.code)
}

/// Catches: a Hello that leaves out detected capabilities (any CPU feature or ISA
/// level the kernel reports), that is unsorted, whose hash is not SHA-256 of the
/// encoded entries, or whose heartbeats carry a different hash; and a daemon that
/// cannot complete mutual TLS.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_carries_every_detected_capability() {
    let mut h = Harness::start("registration", LONG, Duration::from_secs(40)).await;
    let mut peer = h.session().await;
    let hello = peer.hello().await;
    peer.welcome();
    let heartbeat = peer.heartbeat().await;

    // Detected independently of the daemon's report builder.
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").expect("read /proc/cpuinfo");
    let cpu = kbf_caps::CpuCaps::from_linux_cpuinfo(&cpuinfo).expect("parse cpuinfo");
    let has = |key: &str, value: &str| {
        hello
            .capabilities
            .iter()
            .any(|c| c.key == key && c.value == value)
    };
    assert!(!cpu.features().is_empty(), "this host reports CPU features");
    for feature in cpu.features() {
        assert!(has("cpu.features", feature), "feature {feature} missing");
    }
    let levels = cpu.levels();
    assert!(!levels.is_empty(), "this host reaches an ISA level");
    for level in levels {
        assert!(has("isa_level", level.name()), "isa_level {level} missing");
    }
    assert!(has("arch", cpu.arch().name()));
    assert!(has("os", "linux"));
    assert!(has("drivers", "fake"));

    let pairs: Vec<(&str, &str)> = hello
        .capabilities
        .iter()
        .map(|c| (c.key.as_str(), c.value.as_str()))
        .collect();
    assert!(pairs.is_sorted(), "capabilities sorted by key, then value");
    let only_caps = Hello {
        capabilities: hello.capabilities.clone(),
        ..Hello::default()
    };
    let want_hash = Sha256::digest(only_caps.encode_to_vec()).to_vec();
    assert_eq!(hello.report_hash, want_hash);
    assert_eq!(heartbeat.report_hash, want_hash);
    assert_eq!(hello.protocol_version, 1);
    assert_eq!(hello.node_id, "node-1");
    assert_eq!(hello.capabilities, h.report.capabilities());
}

/// Catches: a daemon that starts work when a lease is offered rather than when its
/// committed Start arrives (it could run a lease the scheduler never commits), and one
/// that does not run, report, or list in heartbeats a lease it was told to start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_starts_only_on_start() {
    let mut h = Harness::start("start-only", Duration::from_millis(600), LONG).await;
    let mut peer = h.welcomed().await;

    peer.offer(1, 1);
    h.event(PROMPT, |e| {
        (*e == Event::Offered(kbf_types::LeaseId::new(1, 1))).then_some(())
    })
    .await
    .expect("the daemon handled the offer");
    // Several heartbeat intervals in which nothing may start.
    assert!(
        peer.result(4 * INTERVAL).await.is_none(),
        "no Result for an offer"
    );
    assert!(h.runtime.started().is_empty(), "work started on an offer");

    peer.start(1, 1, "action");
    h.started(1).await;
    assert_eq!(h.runtime.started(), [kbf_types::LeaseId::new(1, 1)]);
    let listed = peer
        .expect(PROMPT, |m| match m {
            kbf_proto::worker::daemon_message::Message::Heartbeat(hb)
                if hb.running == [LeaseId { term: 1, seq: 1 }] =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
    assert!(listed.is_some(), "a heartbeat lists the running lease");
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    assert_eq!(result.lease_id, lease(1, 1));
    assert_eq!(code(&result), Code::Ok as i32);
    assert!(result.action_result.is_some());
}

/// Catches: a daemon that runs a lease kind no runtime of its serves, instead of
/// refusing it with FAILED_PRECONDITION.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unserved_lease_kind_is_refused() {
    let mut h = Harness::start("unserved-kind", LONG, LONG).await;
    let mut peer = h.welcomed().await;
    peer.start(1, 5, "whole_machine");
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    assert_eq!(result.lease_id, lease(1, 5));
    assert_eq!(code(&result), Code::FailedPrecondition as i32);
    assert!(h.runtime.started().is_empty());
}

/// Catches: a daemon that never fences (its work runs on beside the copy the scheduler
/// re-dispatches), one that fences long before T, and one that kills without
/// reporting. The server keeps the stream open but stops acknowledging heartbeats.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_fences_after_t_without_contact() {
    let t = Duration::from_millis(800);
    let mut h = Harness::start("fence", LONG, t).await;
    let mut peer = h.welcomed().await;
    peer.start(1, 2, "action");
    h.started(1).await;

    peer.acking
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let silent_from = Instant::now();
    let (at, result) = peer
        .result(t + PROMPT)
        .await
        .expect("the daemon fences and reports");
    assert_eq!(result.lease_id, lease(1, 2));
    assert_eq!(code(&result), Code::Aborted as i32);
    assert_eq!(h.runtime.killed(), [kbf_types::LeaseId::new(1, 2)]);
    // The newest acknowledged heartbeat went out at most one interval (plus a round
    // trip) before the server fell silent.
    let waited = at - silent_from;
    assert!(
        waited >= t - 2 * INTERVAL,
        "fenced after {waited:?}, T is {t:?}"
    );
    h.event(PROMPT, |e| {
        matches!(e, Event::Fenced(ids) if ids == &[kbf_types::LeaseId::new(1, 2)]).then_some(())
    })
    .await
    .expect("a Fenced event");
}

/// Catches: a daemon that does not notice heartbeats going unacknowledged (no gap
/// declared), one that does not notice them resume, and one that fences on a gap
/// shorter than T.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heartbeat_gap_is_detected() {
    let mut h = Harness::start("gap", LONG, Duration::from_secs(3)).await;
    let peer = h.welcomed().await;
    peer.start(1, 3, "action");
    h.started(1).await;

    peer.acking
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (_, silent_for) = h
        .event(PROMPT, |e| match e {
            Event::HeartbeatGap { silent_for } => Some(*silent_for),
            _ => None,
        })
        .await
        .expect("a HeartbeatGap event");
    assert!(
        silent_for >= 2 * INTERVAL,
        "gap declared after {silent_for:?}"
    );
    peer.acking.store(true, std::sync::atomic::Ordering::SeqCst);
    h.event(PROMPT, |e| (*e == Event::ContactRestored).then_some(()))
        .await
        .expect("a ContactRestored event");
    assert!(h.runtime.killed().is_empty(), "a short gap fenced");
}

/// Catches: a daemon that kills its leases when a stream drops instead of at T, one
/// that does not reconnect, and one that loses a Result finished across a reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_survives_a_reconnect_shorter_than_t() {
    // The lease outlives T, so it finishes only if the new stream renewed contact.
    let t = Duration::from_millis(1500);
    let mut h = Harness::start("reconnect", Duration::from_millis(3000), t).await;
    let first = h.welcomed().await;
    first.start(1, 4, "action");
    h.started(1).await;
    first.close();

    let mut second = h.welcomed().await;
    let (_, result) = second
        .result(PROMPT)
        .await
        .expect("a Result on the new stream");
    assert_eq!(result.lease_id, lease(1, 4));
    assert_eq!(code(&result), Code::Ok as i32);
    assert!(
        h.runtime.killed().is_empty(),
        "the reconnect fenced the lease"
    );
}

/// Catches: a daemon that drops the server's `ResultAck` (or fails on it), and one that
/// reports an acknowledgement without a lease id as if it named a lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_ack_is_reported() {
    let mut h = Harness::start("result-ack", LONG, LONG).await;
    let peer = h.welcomed().await;
    peer.ack_result(None, true);
    peer.ack_result(Some((1, 6)), false);
    let (_, (lease, accepted)) = h
        .event(PROMPT, |e| match e {
            Event::ResultAcknowledged { lease, accepted } => Some((*lease, *accepted)),
            _ => None,
        })
        .await
        .expect("a ResultAcknowledged event");
    assert_eq!((lease, accepted), (kbf_types::LeaseId::new(1, 6), false));
}

/// A wire lease id (issue #23 tests).
fn wire(term: u64, seq: u64) -> LeaseId {
    LeaseId { term, seq }
}

/// A lease id as the daemon's events name it.
fn id(term: u64, seq: u64) -> kbf_types::LeaseId {
    kbf_types::LeaseId::new(term, seq)
}

// Issue #23: leases the server no longer holds here. A `Start` that arrives after the
// window it names is not run, and a `Cancel` stops a running lease. These live in this
// binary with the other session tests so that one binary covers the whole session loop.

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
    assert_eq!(listed.running, [wire(1, 2)]);
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
    assert_eq!(result.lease_id, Some(wire(1, 1)));
    assert_eq!(
        result.status.map(|s| s.code),
        Some(Code::Aborted as i32),
        "the cancelled run did not end killed"
    );
    assert_eq!(h.runtime.killed(), [id(1, 1)]);
    let listed = peer.heartbeat_after(Instant::now()).await;
    assert_eq!(listed.running, [wire(1, 1), wire(1, 2)]);

    peer.ack_result(Some((1, 1)), false);
    let cleared = peer
        .expect(PROMPT, |m| match m {
            kbf_proto::worker::daemon_message::Message::Heartbeat(hb)
                if hb.running == [wire(1, 2)] =>
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
                if hb.running == [wire(1, 2)] =>
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
    assert_eq!(result.lease_id, Some(wire(1, 2)));
    assert_eq!(result.status.map(|s| s.code), Some(Code::Aborted as i32));
}
