//! The drain sequence of a frontend over a state store (`POST /v1/frontend/drain` and a stop
//! signal, `kbf_server::sequence`): two frontends in this process over one real Postgres
//! database (the server `KBF_TEST_POSTGRES_URL` names; these tests fail without it), as the
//! tests of retire mode have them. A fake daemon and a client play the farm's node and Buck2.
//! `--drain-lb-wait` and `--drain-deadline` are set in seconds, as an operator sets them.

use kbf_proto::google::longrunning::Operation;
use kbf_server::api::FRONTEND_DRAIN;
use tonic::{Code, Status, Streaming};

use super::retire::{Pair, bearer, exchange, execute_refused, queued, readyz, until};
use super::*;

/// One call to the drain endpoint of the frontend at `api`; `auth` is the header lines to
/// send besides the content type.
async fn drain_call(api: SocketAddr, method: &str, auth: &str) -> (u16, Value) {
    let request = format!(
        "{method} {FRONTEND_DRAIN} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Type: application/json\r\n{auth}Content-Length: 0\r\n\r\n"
    );
    exchange(api, &request).await
}

/// `POST /v1/frontend/drain` with the token and no body, which answers 200.
async fn begin_drain(api: SocketAddr) -> Value {
    let (code, status) = drain_call(api, "POST", &bearer()).await;
    assert_eq!(code, 200, "{status}");
    status
}

/// The status `GET /v1/frontend/drain` answers, or none if the frontend does not answer
/// (it has exited, or a connection was closed in the moment its operator API was sent
/// GOAWAY).
async fn try_status(api: SocketAddr) -> Option<Value> {
    let request =
        format!("GET {FRONTEND_DRAIN} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    let exchanged = async {
        let mut stream = TcpStream::connect(api).await.ok()?;
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).await.ok()?;
        let (_, body) = response.split_once("\r\n\r\n")?;
        serde_json::from_str(body).ok()
    };
    tokio::time::timeout(Duration::from_secs(5), exchanged)
        .await
        .ok()
        .flatten()
}

/// Whether the frontend at `api` reports the phase `phase`.
async fn in_phase(api: SocketAddr, phase: &str) -> bool {
    let status = try_status(api).await;
    status.is_some_and(|status| status["phase"] == phase)
}

/// Whether the frontend at `api` reports `open` open executions.
async fn has_open(api: SocketAddr, open: u64) -> bool {
    let status = try_status(api).await;
    status.is_some_and(|status| status["open_executions"] == open)
}

/// A connection to `addr` that has said HTTP/2 without negotiating it (the connection
/// preface and an empty SETTINGS frame), as an h2c client does.
async fn h2(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    preface.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
    stream.write_all(&preface).await.expect("send the preface");
    stream
}

/// The types of the HTTP/2 frames in `bytes`: a frame is a head of nine bytes, the first
/// three the length of the payload and the fourth its type, and the payload.
fn frame_types(mut bytes: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    while let [a, b, c, kind, _, _, _, _, _, rest @ ..] = bytes {
        let length = usize::try_from(u32::from_be_bytes([0, *a, *b, *c])).unwrap_or(usize::MAX);
        types.push(*kind);
        bytes = rest.get(length..).unwrap_or_default();
    }
    types
}

/// The type of a GOAWAY frame.
const GOAWAY: u8 = 7;

/// Whether `stream` is sent a GOAWAY frame within `wait`.
async fn sees_goaway(stream: &mut TcpStream, wait: Duration) -> bool {
    let mut bytes = Vec::new();
    let reading = async {
        let mut chunk = [0_u8; 1024];
        while !frame_types(&bytes).contains(&GOAWAY) {
            let read = stream.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
    };
    let _ = tokio::time::timeout(wait, reading).await;
    frame_types(&bytes).contains(&GOAWAY)
}

/// The status an Execute stream ends with, within thirty seconds.
async fn stream_end(ops: &mut Streaming<Operation>) -> Status {
    let ending = async {
        loop {
            match ops.message().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the stream ended without an error"),
                Err(status) => return status,
            }
        }
    };
    let ended = tokio::time::timeout(Duration::from_secs(30), ending).await;
    ended.expect("the stream ends in time")
}

/// Asserts that in the wait the frontend answers `/readyz` 503 at once and serves on: a
/// unary call is answered and a new Execute is taken. When the wait is over the phase is
/// `draining`, a new Execute is refused UNAVAILABLE, and the frontend takes no more work in
/// from the store (an operation queued on the other frontend then stays off its queue),
/// while the operation it holds is run by a daemon of the other frontend and its caller
/// gets the result. Then the frontend, with nothing open, exits 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wait_serves_everything_and_its_end_hands_the_work_to_the_other_frontend() {
    let flags = ["--drain-lb-wait=5", "--drain-deadline=60"];
    let pair = Pair::start_with("drain-wait", &flags).await;
    let (a, b) = (&pair.a, &pair.b);
    let on_a = Client::connect(a.reapi, a.worker).await;
    let on_b = Client::connect(b.reapi, b.worker).await;
    let (first, second, third) = (
        Job::new("taken in the wait", &[]),
        Job::new("refused after the wait", &[]),
        Job::new("queued on the other frontend", &[]),
    );
    for job in [&first, &second, &third] {
        on_a.upload(&job.blobs()).await;
    }
    let result = support::output(&on_b, "built while the frontend drains", 0).await;
    assert!(in_phase(a.api, "serving").await);

    let begun = begin_drain(a.api).await;
    assert_eq!(begun["phase"], "lb_wait", "{begun}");
    assert_eq!(begun["draining"], true, "{begun}");
    assert_eq!(readyz(a.api).await, 503, "ready in the wait");
    assert!(on_a.holds(&first.action).await, "a unary call in the wait");
    let mut ops = on_a.execute(&first.action).await;
    ops.message().await.expect("healthy").expect("QUEUED");

    until("the wait is over", async || {
        in_phase(a.api, "draining").await
    })
    .await;
    let refused = execute_refused(&on_a, &second).await;
    assert_eq!(
        refused,
        Some(Code::Unavailable),
        "a new Execute was not refused UNAVAILABLE after the wait"
    );

    let mut late = on_b.execute(&third.action).await;
    until("both operations are queued on the other", async || {
        queued(b.api).await == 2
    })
    .await;
    // Several intake polls of the drained frontend.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(queued(a.api).await, 1, "the drained frontend took work in");

    let mut daemon = on_b.daemon("node-b", 4, 8).await;
    for _ in 0..2 {
        let start = daemon.start().await;
        let ack = daemon.report(ran(start.lease_id, &result)).await;
        assert!(ack.accepted, "{ack:?}");
    }
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result.clone()));
    let last = done(&mut late).await;
    assert_eq!(response(&last).result, Some(result));

    until("the drained frontend exits", async || {
        pair.a.served.is_finished()
    })
    .await;
    drop((ops, late, daemon, on_a, on_b));
    pair.stop().await;
}

/// Asserts that an execution that is running on a node of the drained frontend finishes
/// there: the node's result is taken after the wait, its caller gets it, the node's stream
/// ends UNAVAILABLE once the execution is gone (its daemon dials another frontend), and the
/// frontend exits 0. The wait is zero here (`--drain-lb-wait=0`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_execution_finishes_before_the_frontend_exits() {
    let flags = ["--drain-lb-wait=0", "--drain-deadline=60"];
    let pair = Pair::start_with("drain-finish", &flags).await;
    let a = &pair.a;
    let client = Client::connect(a.reapi, a.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("finishes while the frontend drains", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;
    let result = support::output(&client, "built while the frontend drains", 0).await;

    let begun = begin_drain(a.api).await;
    assert_eq!(begun["draining"], true, "{begun}");
    until("the drain refuses new work", async || {
        in_phase(a.api, "draining").await
    })
    .await;
    until("the status shows the open execution", async || {
        has_open(a.api, 1).await
    })
    .await;
    assert!(
        !pair.a.served.is_finished(),
        "exited with an execution open"
    );

    let ack = daemon.report(ran(start.lease_id, &result)).await;
    assert!(ack.accepted, "{ack:?}");
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    until("the frontend exits", async || pair.a.served.is_finished()).await;
    drop((ops, daemon, client));
    pair.stop().await;
}

/// Asserts that with an execution that does not end, the phase goes from draining to
/// closing at the deadline, the caller's stream ends UNAVAILABLE, the node's stream ends
/// UNAVAILABLE, and the frontend exits 0. The daemon then dials the other frontend, which
/// keeps the lease the node held: its heartbeat is acknowledged, its result is accepted,
/// and the store holds one result and no operation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_the_deadline_the_streams_end_and_the_daemon_keeps_its_lease_on_the_other() {
    let flags = ["--drain-lb-wait=0", "--drain-deadline=2"];
    let pair = Pair::start_with("drain-deadline", &flags).await;
    let (a, b) = (&pair.a, &pair.b);
    let client = Client::connect(a.reapi, a.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("cut by the deadline", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let result = support::output(&client, "built across a drain", 0).await;

    begin_drain(a.api).await;
    until("the deadline passes", async || {
        in_phase(a.api, "closing").await
    })
    .await;
    let ended = stream_end(&mut ops).await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    until("the frontend exits", async || pair.a.served.is_finished()).await;

    let reached = Client::connect(b.reapi, b.worker).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert_eq!(daemon.epoch, lease.term, "the epoch was kept");
    assert!(
        daemon.heartbeat(&[lease]).await,
        "the other frontend did not keep the lease"
    );
    let ack = daemon.report(ran(Some(lease), &result)).await;
    assert!(ack.accepted, "the other frontend did not take the lease");
    assert_eq!(pair.count("done/"), 1);
    assert_eq!((pair.count("op/"), pair.count("lease/")), (0, 0));
    drop((ops, daemon, reached, client));
    pair.stop().await;
}

/// Asserts that a stop signal runs the same sequence: `/readyz` answers 503 at once while a
/// unary call is answered and a new Execute is taken, the phase is `draining` after the
/// wait and a new Execute is refused, and the frontend exits 0 once its executions are
/// gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_signal_runs_the_same_sequence() {
    let flags = ["--drain-lb-wait=3", "--drain-deadline=60"];
    let mut pair = Pair::start_with("drain-signal", &flags).await;
    let a = &pair.a;
    let client = Client::connect(a.reapi, a.worker).await;
    let (first, second, third) = (
        Job::new("open before the signal", &[]),
        Job::new("taken in the wait", &[]),
        Job::new("refused after the wait", &[]),
    );
    for job in [&first, &second, &third] {
        client.upload(&job.blobs()).await;
    }
    let mut before = client.execute(&first.action).await;
    before.message().await.expect("healthy").expect("QUEUED");

    pair.a.signal();
    let a = &pair.a;
    // The signal is taken by the serving task, which answers not ready before anything else.
    until("not ready after the signal", async || {
        readyz(a.api).await == 503
    })
    .await;
    assert!(in_phase(a.api, "lb_wait").await);
    assert!(
        client.holds(&second.action).await,
        "a unary call in the wait"
    );
    let mut in_the_wait = client.execute(&second.action).await;
    in_the_wait
        .message()
        .await
        .expect("healthy")
        .expect("QUEUED");

    until("the wait is over", async || {
        in_phase(a.api, "draining").await
    })
    .await;
    let refused = execute_refused(&client, &third).await;
    assert_eq!(
        refused,
        Some(Code::Unavailable),
        "a new Execute was not refused UNAVAILABLE after the wait"
    );
    assert!(!pair.a.served.is_finished(), "exited with executions open");

    drop((before, in_the_wait));
    until("the frontend exits", async || pair.a.served.is_finished()).await;
    drop(client);
    pair.stop().await;
}

/// Asserts that every listener is sent GOAWAY, and none before the wait is over: an HTTP/2
/// client on the REAPI listener, one on the worker listener and one on the operator API see
/// no GOAWAY in the wait and one when it ends, and the operator API goes on answering after
/// it, with the phase `draining`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_listener_is_sent_goaway_when_the_wait_ends() {
    let flags = ["--drain-lb-wait=4", "--drain-deadline=60"];
    let pair = Pair::start_with("drain-goaway", &flags).await;
    let a = &pair.a;
    let client = Client::connect(a.reapi, a.worker).await;
    let job = Job::new("holds the drain open", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    ops.message().await.expect("healthy").expect("QUEUED");
    let mut open = [h2(a.reapi).await, h2(a.worker).await, h2(a.api).await];

    begin_drain(a.api).await;
    let [reapi, worker, operator] = &mut open;
    let wait = Duration::from_millis(500);
    let early = tokio::join!(
        sees_goaway(reapi, wait),
        sees_goaway(worker, wait),
        sees_goaway(operator, wait)
    );
    assert_eq!(early, (false, false, false), "GOAWAY in the wait");

    let wait = Duration::from_secs(15);
    let late = tokio::join!(
        sees_goaway(reapi, wait),
        sees_goaway(worker, wait),
        sees_goaway(operator, wait)
    );
    assert_eq!(late, (true, true, true), "REAPI, worker and operator API");
    until("the operator API reports the drain", async || {
        in_phase(a.api, "draining").await
    })
    .await;

    drop((ops, open));
    until("the frontend exits", async || pair.a.served.is_finished()).await;
    drop(client);
    pair.stop().await;
}
