//! The server side of `kbf.worker.v1` sessions: registration, the session boundary of
//! issue #25, heartbeats, and what the server refuses.

mod support;

use futures::channel::mpsc::unbounded;
use kbf_proto::worker::worker_client::WorkerClient;
use kbf_proto::worker::{
    Capability, DaemonMessage, Heartbeat, LeaseId, Offer, daemon_message, server_message,
};
use support::{
    Blob, Cell, FakeDaemon, Job, done, done_within_quiet, failed, hello, output, ran, response,
};
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

/// Catches (issue #79): a resent `Hello` whose `node_id` is not the stream's node read
/// for its capacity only (its `node_id` never compared), which would let a stream speak
/// for a node it did not register as; and one that is refused but leaves the stream
/// open. The node itself is not punished: its next stream registers and gets work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resent_hello_naming_another_node_ends_the_stream() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 1, 8).await;
    assert!(daemon.heartbeat(&[]).await);
    daemon.send(daemon_message::Message::Hello(hello("node-b", 4, 8)));
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::PermissionDenied);
    assert!(
        ended.message().contains("\"node-b\"") && ended.message().contains("node-a"),
        "{}",
        ended.message()
    );

    let mut back = cell.daemon("node-a", 1, 8).await;
    let job = Job::new("after", &[]);
    cell.upload(&job.blobs()).await;
    let _ops = cell.execute(&job.action).await;
    back.start().await;
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

/// Catches (issue #140): a second daemon process registering as a node (a cloned
/// machine, or a second daemon holding the node's certificate) whose first heartbeat
/// gives up the lease the first process still runs, so the operation is placed again on
/// the second while the first runs it until it fences: twice at once. The first
/// process's result, though its stream was replaced, still answers the operation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_daemon_process_does_not_take_over_what_the_first_runs() {
    let cell = Cell::start().await;
    let mut first = cell.daemon_process("node-a", "first", 4, 8).await;
    assert!(first.heartbeat(&[]).await);
    let job = Job::new("runs on the first daemon", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let lease = first.start().await.lease_id;

    let mut second = cell.daemon_process("node-a", "second", 4, 8).await;
    assert!(second.heartbeat(&[]).await);
    assert!(second.heartbeat(&[]).await);
    second.no_work().await;

    let result = output(&cell, "from the first daemon", 0).await;
    assert!(first.report(ran(lease, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
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
    // Give the server a moment to see the stream end.
    tokio::time::sleep(support::INTERVAL).await;
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

/// Catches: a heartbeat's running set that does not reach the scheduler as the leases
/// the daemon named (a lease id read with its term and sequence number swapped, or
/// dropped), across the session boundary where it decides at once. A daemon back on a
/// new stream (the same process) that still runs its lease and lists it keeps it: no
/// second `Start`, and its result is accepted. One whose first heartbeat leaves the
/// lease out (here it names only the swapped id) has the operation placed again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_returning_daemon_keeps_the_leases_its_first_heartbeat_lists() {
    let cell = Cell::start().await;
    let mut first = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("re-adopted", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let lease = first.start().await.lease_id.expect("a lease id");
    assert_ne!(
        lease.term, lease.seq,
        "a swapped id must name another lease"
    );

    let mut again = cell.daemon("node-a", 4, 8).await;
    assert!(again.heartbeat(&[lease]).await);
    again.no_work().await;
    let result = output(&cell, "from the re-adopted run", 0).await;
    assert!(again.report(ran(Some(lease), &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));

    // Two more runs: the second one's lease (seq 2) is the one left out.
    let mut leases = Vec::new();
    for argv in ["re-adopted again", "not re-adopted"] {
        let job = Job::new(argv, &[]);
        cell.upload(&job.blobs()).await;
        let _ops = cell.execute(&job.action).await;
        leases.push(again.start().await.lease_id.expect("a lease id"));
    }
    let (kept, lost) = (leases[0], leases[1]);
    assert_ne!(lost.term, lost.seq, "a swapped id must name another lease");
    let swapped = kbf_proto::worker::LeaseId {
        term: lost.seq,
        seq: lost.term,
    };
    let mut third = cell.daemon("node-a", 4, 8).await;
    assert!(third.heartbeat(&[kept, swapped]).await);
    let placed = third.start().await;
    assert_ne!(placed.lease_id, Some(lost));
    assert_ne!(placed.lease_id, Some(kept));
    third.no_work().await;
}

/// Catches: a result accepted, or its outputs taken in, for a lease the scheduler gave
/// up while its operation waits in the queue for room (the operation has no current
/// lease at all): its callers would be answered with a run the farm wrote off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_for_a_lease_given_up_while_queued_is_refused() {
    let cell = Cell::start().await;
    let mut first = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("queued again", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let given_up = first.start().await;

    // The daemon returns with no room and lists nothing: the lease is given up and the
    // operation waits in the queue.
    let mut back = cell.daemon("node-a", 0, 8).await;
    assert!(back.heartbeat(&[]).await);
    back.no_work().await;
    let late = output(&cell, "from the lease given up", 0).await;
    assert!(!back.report(ran(given_up.lease_id, &late)).await.accepted);
    assert!(
        !cell.holds(&Blob::of(&late)).await,
        "the given-up lease's result was taken in (its ActionResult stored)"
    );
    assert!(
        !done_within_quiet(&mut ops).await,
        "answered by a refused result"
    );

    // With room again it runs under a new lease.
    back.send(daemon_message::Message::Hello(hello("node-a", 4, 8)));
    let again = back.start().await;
    assert_ne!(again.lease_id, given_up.lease_id);
    let result = output(&cell, "from the new lease", 0).await;
    assert!(back.report(ran(again.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
}

/// Catches (issue #23): a `Start` that names no heartbeat, or not the newest one the
/// server took on the stream (a daemon would measure the `Start`'s age from an older
/// send and refuse it, or from a newer one and accept a late one); a late, older seq
/// moving it back; a window other than the scheduler's 14 s; and a new stream that
/// carries the old stream's seq instead of naming its own Hello.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_names_the_newest_heartbeat_taken_and_the_window() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let first_job = Job::new("before any heartbeat", &[]);
    cell.upload(&first_job.blobs()).await;
    let _first_ops = cell.execute(&first_job.action).await;
    let first = daemon.start().await;
    assert_eq!((first.heartbeat_seq, first.valid_for_ms), (0, 14_000));

    let running = [first.lease_id.expect("a lease id")];
    assert!(daemon.heartbeat(&running).await);
    assert!(daemon.heartbeat(&running).await);
    daemon.send(daemon_message::Message::Heartbeat(Heartbeat {
        seq: 1,
        report_hash: Vec::new(),
        running: running.to_vec(),
    }));
    daemon
        .expect("the late heartbeat's ack", |m| match m {
            server_message::Message::HeartbeatAck(a) if a.seq == 1 => Some(()),
            _ => None,
        })
        .await;
    let second_job = Job::new("after two heartbeats", &[]);
    cell.upload(&second_job.blobs()).await;
    let _second_ops = cell.execute(&second_job.action).await;
    let second = daemon.start().await;
    assert_eq!((second.heartbeat_seq, second.valid_for_ms), (2, 14_000));

    let mut again = cell.daemon("node-a", 4, 8).await;
    let third_job = Job::new("on a new stream", &[]);
    cell.upload(&third_job.blobs()).await;
    let _third_ops = cell.execute(&third_job.action).await;
    let third = again.start().await;
    assert_eq!(
        third.heartbeat_seq, 0,
        "the old stream's seq on the new one"
    );
}

/// Catches (issue #23): a lease the daemon lists after the server gave it up and placed
/// its operation again (a late `Start` ran after all) left running beside the retry,
/// with no `Cancel`; a `Cancel` of the retry the daemon rightly holds, or of a lease
/// the server never granted; and the cancelled run's Result accepted over the retry's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listed_lease_the_server_gave_up_is_cancelled() {
    let cell = Cell::start().await;
    let mut first = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("cancelled", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let lost = first.start().await.lease_id.expect("a lease id");

    // The daemon returns and its first heartbeat leaves the lease out: given up and
    // placed again.
    let mut back = cell.daemon("node-a", 4, 8).await;
    assert!(back.heartbeat(&[]).await);
    let retry = back.start().await.lease_id.expect("a lease id");
    assert_ne!(retry, lost);
    assert_eq!(
        back.cancelled().await,
        None,
        "a Cancel before any lease is listed"
    );

    // The lost lease's Start ran after all: the daemon lists both.
    assert!(back.heartbeat(&[lost, retry]).await);
    assert_eq!(back.cancelled().await, Some(lost));
    assert_eq!(back.cancelled().await, None, "the retry was cancelled too");
    let term = retry.term;
    let never = [
        LeaseId { term, seq: 99 },
        LeaseId {
            term: term + 1,
            seq: 0,
        },
    ];
    assert!(back.heartbeat(&[retry, never[0], never[1]]).await);
    assert_eq!(
        back.cancelled().await,
        None,
        "a lease never granted was cancelled"
    );

    let aborted = back.report(failed(Some(lost), Code::Aborted)).await;
    assert!(!aborted.accepted, "the cancelled run's Result was accepted");
    let result = output(&cell, "from the retry", 0).await;
    assert!(back.report(ran(Some(retry), &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
}
