//! Results on the wire: each kept until the server acknowledges it (issue #26), and
//! a lease that cannot run reported with the status REAPI gives its cause.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::Event;
use kbf_proto::google::rpc::{Code, PreconditionFailure};
use kbf_proto::worker::{LeaseId, daemon_message::Message};
use prost::Message as _;
use support::{Harness, INTERVAL, PROMPT};

const LONG: Duration = Duration::from_secs(60);

fn lease(term: u64, seq: u64) -> LeaseId {
    LeaseId { term, seq }
}

/// What a session saw, in order, up to and including the first Heartbeat.
async fn up_to_first_heartbeat(peer: &mut support::Peer) -> Vec<Message> {
    let mut seen = Vec::new();
    loop {
        let (_, m) = peer
            .expect(PROMPT, |m| Some(m.clone()))
            .await
            .expect("a daemon message");
        let heartbeat = matches!(m, Message::Heartbeat(_));
        seen.push(m);
        if heartbeat {
            return seen;
        }
    }
}

/// Catches a daemon that forgets a Result as soon as it is written (a heartbeat that
/// leaves it out makes the scheduler requeue a lease that finished), and one that
/// lists it forever after the server acknowledged it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_is_listed_until_acknowledged() {
    let mut h = Harness::start("unacked-listed", Duration::from_millis(50), LONG).await;
    let mut peer = h.welcomed().await;
    peer.start(1, 1, "action");
    peer.result(PROMPT).await.expect("a Result");
    // Two heartbeats after the Result: the lease has stopped running and is listed.
    for _ in 0..2 {
        assert_eq!(peer.heartbeat().await.running, [lease(1, 1)]);
    }
    peer.ack_result(Some((1, 1)), true);
    let cleared = peer
        .expect(PROMPT, |m| match m {
            Message::Heartbeat(hb) if hb.running.is_empty() => Some(()),
            _ => None,
        })
        .await;
    assert!(cleared.is_some(), "an acknowledged Result is forgotten");
}

/// Catches a daemon that does not resend an unacknowledged Result on its next stream,
/// or sends its first Heartbeat there before it (the server would requeue the lease
/// between the two).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unacknowledged_result_is_resent_before_the_first_heartbeat() {
    let mut h = Harness::start("unacked-resent", Duration::from_millis(50), LONG).await;
    let mut first = h.welcomed().await;
    first.start(2, 7, "action");
    let (_, sent) = first.result(PROMPT).await.expect("a Result");
    first.close();

    let mut second = h.session().await;
    second.hello().await;
    second.welcome();
    let seen = up_to_first_heartbeat(&mut second).await;
    let [Message::Result(resent), Message::Heartbeat(heartbeat)] = seen.as_slice() else {
        panic!("expected the Result, then a Heartbeat: {seen:?}");
    };
    assert_eq!(*resent, sent);
    assert_eq!(heartbeat.running, [lease(2, 7)]);
}

/// Catches a Result produced while no stream is up (the daemon is waiting for
/// Welcome) being lost, or sent before the server has welcomed the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_finished_offline_is_sent_after_welcome() {
    let mut h = Harness::start("unacked-offline", Duration::from_millis(300), LONG).await;
    let first = h.welcomed().await;
    first.start(4, 1, "action");
    h.started(1).await;
    first.close();

    let mut second = h.session().await;
    second.hello().await;
    // The lease finishes while the daemon waits for this Welcome.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        second.result(Duration::ZERO).await.is_none(),
        "a Result before Welcome"
    );
    second.welcome();
    let seen = up_to_first_heartbeat(&mut second).await;
    let [Message::Result(result), Message::Heartbeat(_)] = seen.as_slice() else {
        panic!("expected the Result, then a Heartbeat: {seen:?}");
    };
    assert_eq!(result.lease_id, Some(lease(4, 1)));
    assert_eq!(
        result.status.as_ref().map(|s| s.code),
        Some(Code::Ok as i32)
    );
}

/// Catches a daemon that does not fence while no stream is up (its work would run on
/// beside the copy the scheduler re-dispatches), that loses the fenced lease's ABORTED
/// Result instead of sending it after the next Welcome, or that also reports the
/// killed run's own outcome (a second Result for the lease, or one that replaces the
/// fence's: the lease was fenced, not killed by the server).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_fenced_offline_is_reported_once_after_welcome() {
    let t = Duration::from_millis(600);
    let mut h = Harness::start("fenced-offline", LONG, t).await;
    let first = h.welcomed().await;
    first.start(5, 1, "action");
    h.started(1).await;
    first.close();

    let mut second = h.session().await;
    second.hello().await;
    // No Welcome: the daemon fences while it waits for one.
    h.event(t + PROMPT, |e| {
        matches!(e, Event::Fenced(ids) if ids == &[kbf_types::LeaseId::new(5, 1)]).then_some(())
    })
    .await
    .expect("fenced while offline");
    assert_eq!(h.runtime.killed(), [kbf_types::LeaseId::new(5, 1)]);
    // Long enough for the killed run's outcome to reach the daemon, still offline.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        second.result(Duration::ZERO).await.is_none(),
        "a Result before Welcome"
    );
    second.welcome();
    let seen = up_to_first_heartbeat(&mut second).await;
    let [Message::Result(result), Message::Heartbeat(heartbeat)] = seen.as_slice() else {
        panic!("expected one Result, then a Heartbeat: {seen:?}");
    };
    assert_eq!(result.lease_id, Some(lease(5, 1)));
    let status = result.status.as_ref().expect("a status");
    assert_eq!(status.code, Code::Aborted as i32);
    assert!(status.message.contains("self-fenced"), "{}", status.message);
    assert_eq!(heartbeat.running, [lease(5, 1)]);
}

/// Catches a Start for a lease whose Result is still unacknowledged being run again
/// (a second Result for one lease), and a refused Start's Result not being kept like
/// any other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reported_lease_is_not_started_again() {
    let mut h = Harness::start("unacked-restart", Duration::from_millis(50), LONG).await;
    let mut peer = h.welcomed().await;
    peer.start(3, 1, "action");
    peer.result(PROMPT).await.expect("a Result");
    peer.start(3, 1, "action");
    peer.start(3, 2, "whole_machine");
    let (_, refused) = peer.result(PROMPT).await.expect("the refusal");
    assert_eq!(refused.lease_id, Some(lease(3, 2)));
    assert!(
        peer.result(4 * INTERVAL).await.is_none(),
        "no second Result"
    );
    assert_eq!(h.runtime.started().len(), 1);
    assert_eq!(peer.heartbeat().await.running, [lease(3, 1), lease(3, 2)]);
}

/// Catches a lease whose input is not in the CAS reported as an infrastructure
/// failure, or without the blob a client must upload (a `MISSING` violation naming
/// `blobs/<hash>/<size>`), and an invalid action reported as the farm's failure.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_input_is_reported_as_missing() {
    use kbf_daemon::LocalRuntime;
    use kbf_daemon::cas::digest_of;
    use support::memory::{MemoryCas, Spec};

    let cas = Arc::new(MemoryCas::default());
    let runtime = Arc::new(LocalRuntime::new(
        Arc::clone(&cas),
        support::scratch("missing-wire"),
    ));
    let mut h = Harness::with_runtime("missing-wire", runtime, LONG).await;
    let mut peer = h.welcomed().await;

    let action = Spec::sh("cat data")
        .inputs(&[("data", b"absent", false)])
        .store(&cas);
    let data = digest_of(b"absent");
    cas.remove(&data);
    peer.start_action(1, 1, "action", action);
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    let status = result.status.expect("a status");
    assert_eq!(status.code, Code::FailedPrecondition as i32);
    let blob = format!("{}/{}", data.hash, data.size_bytes);
    assert!(status.message.contains(&blob), "{}", status.message);
    assert!(result.action_result.is_none());
    let [detail] = status.details.as_slice() else {
        panic!("one detail: {:?}", status.details);
    };
    assert_eq!(
        detail.type_url,
        "type.googleapis.com/google.rpc.PreconditionFailure"
    );
    let failure = PreconditionFailure::decode(detail.value.as_slice()).expect("decodes");
    let [violation] = failure.violations.as_slice() else {
        panic!("one violation: {failure:?}");
    };
    assert_eq!(violation.r#type, "MISSING");
    assert_eq!(violation.subject, format!("blobs/{blob}"));

    let mut invalid = Spec::sh("true");
    invalid.working_directory = "/abs".to_owned();
    peer.start_action(1, 2, "action", invalid.store(&cas));
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    assert_eq!(
        result.status.map(|s| s.code),
        Some(Code::InvalidArgument as i32)
    );
}
