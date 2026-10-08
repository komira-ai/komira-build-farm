//! The lease epoch a Welcome names, and the action every Result echoes (issue #137).
//!
//! A server that restarts loses its leases. A daemon still holding a lease of the old
//! process, as a run or as an unacknowledged Result, must not let it meet a lease the
//! new process grants: on a Welcome that names another epoch it kills those runs and
//! forgets those Results, unsent.

mod support;

use std::time::Duration;

use kbf_daemon::Event;
use kbf_proto::worker::{LeaseId, daemon_message::Message};
use support::{Harness, INTERVAL, PROMPT};

const LONG: Duration = Duration::from_secs(60);

fn lease(term: u64, seq: u64) -> LeaseId {
    LeaseId { term, seq }
}

/// Each `(term, seq)` as the daemon's own lease id.
fn ids(pairs: &[(u64, u64)]) -> Vec<kbf_types::LeaseId> {
    pairs
        .iter()
        .map(|&(term, seq)| kbf_types::LeaseId::new(term, seq))
        .collect()
}

/// What a session saw, in order, up to and including the first Heartbeat.
async fn up_to_first_heartbeat(peer: &mut support::Peer) -> Vec<Message> {
    let mut seen = Vec::new();
    loop {
        let (_, m) = peer
            .expect(PROMPT, |m| {
                (!matches!(m, Message::NodeStatus(_))).then(|| m.clone())
            })
            .await
            .expect("a daemon message");
        let heartbeat = matches!(m, Message::Heartbeat(_));
        seen.push(m);
        if heartbeat {
            return seen;
        }
    }
}

/// Catches issue #137 on the daemon's side, and a Result that does not echo its
/// Start's action (a run's, and a refusal's). A stream welcomed with the same lease
/// epoch resends unacknowledged Results as before; one welcomed with another epoch (a
/// new server process, which never granted those leases) must neither resend nor list
/// them, so a lease id the new process grants can never meet an old Result. A Start of
/// the new epoch with the old lease's sequence number then runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_lease_epoch_forgets_the_results_of_the_old_one() {
    let mut h = Harness::start("epoch-results", Duration::from_millis(50), LONG).await;
    let mut first = h.session().await;
    first.hello().await;
    first.welcome_epoch(5);
    first.start(5, 1, "action");
    let (_, ran) = first.result(PROMPT).await.expect("a Result");
    assert_eq!(ran.lease_id, Some(lease(5, 1)));
    assert_eq!(
        ran.action_digest,
        Some(support::digest()),
        "no action echoed"
    );
    first.start(5, 2, "no-such-kind");
    let (_, refused) = first.result(PROMPT).await.expect("a refusal");
    assert_eq!(refused.lease_id, Some(lease(5, 2)));
    assert_eq!(refused.action_digest, Some(support::digest()));
    first.close();

    let mut second = h.session().await;
    second.hello().await;
    second.welcome_epoch(5);
    let seen = up_to_first_heartbeat(&mut second).await;
    let [
        Message::Result(a),
        Message::Result(b),
        Message::Heartbeat(heartbeat),
    ] = seen.as_slice()
    else {
        panic!("expected both Results, then a Heartbeat: {seen:?}");
    };
    assert_eq!((a, b), (&ran, &refused));
    assert_eq!(heartbeat.running, [lease(5, 1), lease(5, 2)]);
    second.close();

    let mut third = h.session().await;
    third.hello().await;
    third.welcome_epoch(7);
    let seen = up_to_first_heartbeat(&mut third).await;
    let [Message::Heartbeat(heartbeat)] = seen.as_slice() else {
        panic!("expected a Heartbeat and no Result: {seen:?}");
    };
    assert!(heartbeat.running.is_empty(), "{heartbeat:?}");
    let (_, dropped) = h
        .event(PROMPT, |e| match e {
            Event::Superseded(dropped) => Some(dropped.clone()),
            _ => None,
        })
        .await
        .expect("the old epoch's leases dropped");
    assert_eq!(dropped, ids(&[(5, 1), (5, 2)]));

    third.start(7, 1, "action");
    let (_, new) = third.result(PROMPT).await.expect("the new lease's Result");
    assert_eq!(new.lease_id, Some(lease(7, 1)));
    assert_eq!(h.runtime.started(), ids(&[(5, 1), (7, 1)]));
}

/// Catches issue #137's second interleaving on the daemon's side: a run of the old
/// lease epoch going on after a Welcome of a new one (a Start of the new server whose
/// lease id equalled it would be taken as a resend, and the old run's Result as the
/// new lease's). It is killed at once, its Result is never sent, and it is listed only
/// until its run has stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_lease_epoch_kills_the_runs_of_the_old_one() {
    let mut h = Harness::start("epoch-runs", LONG, LONG).await;
    let mut first = h.session().await;
    first.hello().await;
    first.welcome_epoch(5);
    first.start(5, 1, "action");
    h.started(1).await;
    first.close();

    let mut second = h.session().await;
    second.hello().await;
    second.welcome_epoch(7);
    h.event(PROMPT, |e| {
        matches!(e, Event::Superseded(dropped) if *dropped == ids(&[(5, 1)])).then_some(())
    })
    .await
    .expect("the old epoch's run dropped");
    second.start(7, 1, "action");
    h.started(2).await;
    assert_eq!(h.runtime.started(), ids(&[(5, 1), (7, 1)]));
    assert_eq!(h.runtime.killed(), ids(&[(5, 1)]));
    let deadline = tokio::time::Instant::now() + PROMPT;
    while second.heartbeat().await.running != [lease(7, 1)] {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old run is listed"
        );
    }
    let stray = second
        .expect(10 * INTERVAL, |m| match m {
            Message::Result(r) => Some(r.lease_id),
            _ => None,
        })
        .await;
    assert_eq!(stray, None, "a Result was sent");
}

/// Catches a daemon that drops leases on a Welcome that names no epoch (a server
/// that predates the field): it cannot tell, so it keeps them, as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_welcome_without_an_epoch_drops_nothing() {
    let mut h = Harness::start("epoch-none", Duration::from_millis(50), LONG).await;
    let mut first = h.session().await;
    first.hello().await;
    first.welcome_epoch(5);
    first.start(5, 1, "action");
    let (_, ran) = first.result(PROMPT).await.expect("a Result");
    first.close();

    let mut second = h.session().await;
    second.hello().await;
    second.welcome();
    let seen = up_to_first_heartbeat(&mut second).await;
    let [Message::Result(resent), Message::Heartbeat(heartbeat)] = seen.as_slice() else {
        panic!("expected the Result, then a Heartbeat: {seen:?}");
    };
    assert_eq!(resent, &ran);
    assert_eq!(heartbeat.running, [lease(5, 1)]);
}
