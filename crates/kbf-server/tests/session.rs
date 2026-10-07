//! The server side of `kbf.worker.v1` sessions: registration, the session boundary of
//! issue #25, heartbeats, and what the server refuses.

mod support;

use futures::channel::mpsc::unbounded;
use kbf_proto::worker::worker_client::WorkerClient;
use kbf_proto::worker::{Capability, DaemonMessage, Offer, daemon_message};
use support::{Cell, FakeDaemon, Job, done, hello, output, ran};
use tonic::Code;
use tonic::transport::Endpoint;

async fn refused(cell: &Cell, first: daemon_message::Message) -> Code {
    match FakeDaemon::open(cell.worker_addr, first).await {
        Err(status) => status.code(),
        Ok(_) => panic!("the stream was accepted"),
    }
}

/// Catches: a session opened by something other than `Hello`, an unsupported protocol
/// version, a Hello without a node id, or one whose node report lacks (or garbles) the
/// capacity entries placement needs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_first_message_is_refused() {
    let cell = Cell::start().await;
    let beat = daemon_message::Message::Offer(Offer::default());
    assert_eq!(refused(&cell, beat).await, Code::InvalidArgument);
    let mut newer = hello("node-a", 4, 8);
    newer.protocol_version = 2;
    assert_eq!(
        refused(&cell, daemon_message::Message::Hello(newer)).await,
        Code::FailedPrecondition
    );
    let mut older = hello("node-a", 4, 8);
    older.protocol_version = 0;
    assert_eq!(
        refused(&cell, daemon_message::Message::Hello(older)).await,
        Code::FailedPrecondition
    );
    let nameless = hello("", 4, 8);
    assert_eq!(
        refused(&cell, daemon_message::Message::Hello(nameless)).await,
        Code::InvalidArgument
    );
    let mut no_cpus = hello("node-a", 4, 8);
    no_cpus.capabilities.retain(|c| c.key != "cpus");
    let mut twice = hello("node-a", 4, 8);
    twice.capabilities.push(Capability {
        key: "mem_gib".to_owned(),
        value: "9".to_owned(),
    });
    let mut garbled = hello("node-a", 4, 8);
    garbled.capabilities[1].value = "lots".to_owned();
    for bad in [no_cpus, twice, garbled] {
        assert_eq!(
            refused(&cell, daemon_message::Message::Hello(bad)).await,
            Code::InvalidArgument
        );
    }
}

/// Catches: a stream that never says Hello held open forever (a slot for every
/// half-open connection), and one that ends before its Hello taken for a session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_without_hello_is_ended() {
    let cell = Cell::start().await;
    let channel = Endpoint::from_shared(format!("http://{}", cell.worker_addr))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let (tx, rx) = unbounded::<DaemonMessage>();
    let silent = WorkerClient::new(channel.clone()).session(rx).await;
    assert_eq!(silent.expect_err("no Hello").code(), Code::DeadlineExceeded);
    drop(tx);

    let (tx, rx) = unbounded::<DaemonMessage>();
    drop(tx);
    let ended = WorkerClient::new(channel).session(rx).await;
    assert_eq!(ended.expect_err("no Hello").code(), Code::InvalidArgument);
}

/// Catches (issue #25): a resent `Hello` treated as a new registration (an in-flight
/// lease the next heartbeat has not listed yet would be requeued at once and run
/// twice); a resent `Hello` whose new capacity placement does not see; a garbled
/// resend that ends the session; and messages without content that do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resent_hello_changes_capacity_only() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 1, 8).await;
    let first = Job::new("first", &[]);
    let second = Job::new("second", &[]);
    cell.upload(&first.blobs()).await;
    cell.upload(&second.blobs()).await;
    let mut first_ops = cell.execute(&first.action).await;
    let running = daemon.start().await;
    let _second_ops = cell.execute(&second.action).await;
    daemon.no_work().await;

    daemon.send_empty();
    daemon.send(daemon_message::Message::Offer(Offer::default()));
    let mut garbled = hello("node-a", 2, 8);
    garbled.capabilities.clear();
    daemon.send(daemon_message::Message::Hello(garbled));
    daemon.send(daemon_message::Message::Hello(hello("node-a", 2, 8)));
    let placed = daemon.start().await;
    assert_ne!(placed.lease_id, running.lease_id);
    // This heartbeat was sent before the first Start arrived, say: it lists nothing.
    // On the same session, inside the grace, the lease is kept.
    assert!(daemon.heartbeat(&[]).await);
    daemon.no_work().await;
    let result = output(&cell, "first", 0).await;
    assert!(daemon.report(ran(running.lease_id, &result)).await.accepted);
    done(&mut first_ops).await;
}

/// Catches (issue #25): heartbeats from a stream that a newer stream of the same node
/// has replaced still fed and acknowledged, which would keep the old session's daemon
/// from fencing and let its stale running set requeue the new session's leases.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replaced_stream_is_not_heard() {
    let cell = Cell::start().await;
    let mut old = cell.daemon("node-a", 4, 8).await;
    assert!(
        old.heartbeat(&[]).await,
        "a current heartbeat unacknowledged"
    );
    let mut new = cell.daemon("node-a", 4, 8).await;
    assert!(!old.heartbeat(&[]).await, "a replaced stream acknowledged");
    assert!(new.heartbeat(&[]).await);

    // The replaced stream's resent Hello does not resize the node either.
    let job = Job::new("after", &[]);
    cell.upload(&job.blobs()).await;
    old.send(daemon_message::Message::Hello(hello("node-a", 0, 0)));
    let _ops = cell.execute(&job.action).await;
    new.start().await;
}

/// Catches: an operation placed on a node whose stream has ended (its `Start` goes
/// nowhere) that is not given up when the node registers again, so it never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_placed_while_a_node_is_away_runs_when_it_returns() {
    let cell = Cell::start().await;
    let gone = cell.daemon("node-a", 4, 8).await;
    gone.close();
    let job = Job::new("while away", &[]);
    cell.upload(&job.blobs()).await;
    // Whether or not the server has seen the stream end, the ended stream stays the
    // node's link until it registers again, so the `Start` is lost either way.
    let mut ops = cell.execute(&job.action).await;

    let mut back = cell.daemon("node-a", 4, 8).await;
    assert!(back.heartbeat(&[]).await);
    let start = back.start().await;
    let result = output(&cell, "ran on return", 0).await;
    assert!(back.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;
}

/// Catches: a `Result` without a lease id that the server acknowledges (or treats as
/// some lease's), and a refusal of a lease it never granted that is not refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn results_for_no_lease_or_an_unknown_lease_are_not_accepted() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let result = output(&cell, "nobody asked", 0).await;
    daemon.send(daemon_message::Message::Result(ran(None, &result)));
    let unknown = Some(kbf_proto::worker::LeaseId { term: 9, seq: 9 });
    let ack = daemon.report(ran(unknown, &result)).await;
    assert!(!ack.accepted);
    let stray = daemon
        .expect_within(support::QUIET, |m| match m {
            kbf_proto::worker::server_message::Message::ResultAck(a) => Some(*a),
            _ => None,
        })
        .await;
    assert_eq!(stray, None, "a Result without a lease was acknowledged");
}
