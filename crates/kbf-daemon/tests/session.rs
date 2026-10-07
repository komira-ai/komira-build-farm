//! The daemon against an in-process fake server over a loopback mutual-TLS stream.
//!
//! Every test here also proves the TLS setup: the fake server requires a client
//! certificate from its CA, and the daemon verifies the server's against the CA file it
//! was given. A daemon that presented no certificate would never reach Hello.

mod support;

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
