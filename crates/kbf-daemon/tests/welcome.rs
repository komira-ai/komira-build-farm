//! What the daemon makes of the start of a stream and of the server messages it does not
//! act on: a first message it refuses ends the stream and it opens another, a Welcome
//! that never comes is given up on, and a repeated Welcome, an empty message or an offer
//! naming no lease change nothing.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Event, FakeRuntime};
use kbf_proto::worker::{HeartbeatAck, Welcome, daemon_message, server_message};
use support::{Harness, PROMPT};
use tokio::time::Instant;

/// A fence time long enough that a Welcome interval of seconds does not fit it by accident.
const FENCE: Duration = Duration::from_secs(40);

/// Drives a daemon whose server answers its Hello with `first`, and checks the daemon
/// refuses it: it opens another stream (a daemon that took `first` for a Welcome would
/// stay on this one) and sent no heartbeat on the refused one.
async fn refuses(name: &str, first: server_message::Message) {
    let mut h = Harness::start(name, Duration::ZERO, FENCE).await;
    let mut refused = h.session().await;
    refused.hello().await;
    refused.send(first);
    let mut next = h.session().await;
    next.hello().await;
    let heartbeat = refused
        .expect(Duration::ZERO, |m| {
            matches!(m, daemon_message::Message::Heartbeat(_)).then_some(())
        })
        .await;
    assert!(heartbeat.is_none(), "a heartbeat on a refused stream");
}

fn welcome(protocol_version: u32, interval: Duration) -> server_message::Message {
    server_message::Message::Welcome(Welcome {
        protocol_version,
        heartbeat_interval_ms: u64::try_from(interval.as_millis()).expect("fits"),
        ..Welcome::default()
    })
}

/// Catches a daemon that takes any first message for the server's Welcome: it would
/// heartbeat a stream whose server never registered it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_message_that_is_not_a_welcome_is_refused() {
    refuses(
        "welcome-not",
        server_message::Message::HeartbeatAck(HeartbeatAck { seq: 1 }),
    )
    .await;
}

/// Catches a daemon that does not compare the protocol version of the Welcome with its
/// own: it would speak this version to a server that speaks another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_welcome_of_another_protocol_version_is_refused() {
    refuses("welcome-version", welcome(2, Duration::from_millis(100))).await;
}

/// Catches a daemon that takes a heartbeat interval of zero (it would heartbeat in a
/// loop), and one that takes an interval too long for its fence (it would be fenced
/// before it noticed the silence of a server that heartbeats that slowly).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_welcome_with_an_interval_that_does_not_fit_is_refused() {
    refuses("welcome-zero", welcome(1, Duration::ZERO)).await;
    refuses("welcome-long", welcome(1, FENCE / 2)).await;
}

/// Catches a daemon that waits for ever for a Welcome: a server that accepts the stream
/// and says nothing would hold it on that stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_never_welcomes_is_given_up_on() {
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let mut h =
        Harness::with_welcome_timeout("welcome-silent", runtime, Duration::from_millis(300)).await;
    let mut silent = h.session().await;
    silent.hello().await;
    let mut next = h.session().await;
    next.hello().await;
}

/// Catches a daemon that ends its stream on messages it has no use for: a second Welcome
/// on the stream, a message with no content, and an offer that names no lease. It goes on
/// heartbeating, and the offer after them is still handled (events come in order, so the
/// first `Offered` is the lease the offer after them names).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_the_daemon_has_no_use_for_leave_the_stream_up() {
    let mut h = Harness::start("welcome-ignored", Duration::ZERO, FENCE).await;
    let mut peer = h.welcomed().await;
    peer.welcome();
    peer.send_empty();
    peer.offer_without_lease();
    peer.offer(1, 1);
    let offered = h
        .event(PROMPT, |e| match e {
            Event::Offered(id) => Some(*id),
            _ => None,
        })
        .await
        .expect("the offer is handled")
        .1;
    assert_eq!(offered, kbf_types::LeaseId::new(1, 1));
    peer.heartbeat_after(Instant::now()).await;
}
