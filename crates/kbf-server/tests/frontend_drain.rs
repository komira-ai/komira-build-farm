//! The frontend's drain endpoint (`GET` and `POST /v1/frontend/drain`), in one process:
//! its status, its gates, what a drain does and keeps, the escalation, and the node
//! verbs the `kbf cluster` client sends. The binary's signals are in `shutdown.rs`.

mod support;

use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_server::api::{FRONTEND_DRAIN, NODES};
use kbf_server::api_client::{OperatorClient, Reply};
use serde_json::{Value, json};
use support::{API_TOKEN, Cell, Job, PROMPT, done, output, ran, stage};
use tonic::Code;

/// A call to the cell's operator API, off the async threads: the client blocks.
async fn call(
    cell: &Cell,
    token: bool,
    f: impl FnOnce(&OperatorClient) -> Reply + Send + 'static,
) -> Reply {
    let api = cell.api.expect("a cell with an operator API");
    let token = token.then(|| API_TOKEN.to_owned());
    tokio::task::spawn_blocking(move || f(&OperatorClient::new(api, token)))
        .await
        .expect("the call runs")
}

async fn status(cell: &Cell) -> Value {
    let reply = call(cell, false, |c| c.get(FRONTEND_DRAIN).expect("a reply")).await;
    assert_eq!(reply.status, 200, "{reply:?}");
    serde_json::from_str(&reply.body).expect("JSON")
}

async fn begin(cell: &Cell, token: bool, body: &'static str) -> Reply {
    call(cell, token, move |c| {
        c.post(FRONTEND_DRAIN, body).expect("a reply")
    })
    .await
}

/// Catches the endpoint's whole contract on a frontend with one open stream and its
/// operation queued with no worker to run it:
/// - the status names `draining`, `open_executions` and `queued_operations`, as a
///   count of each (an orchestrator tells idle from queued-with-no-worker by them);
/// - a `POST` without the token (401) or with a body that is not `{}` (400) changes
///   nothing (mutant: the gate left out of `begin_drain`);
/// - a `POST` with the token begins the drain and answers its status, again and again
///   (idempotent): `/readyz` is 503, a new Execute is refused, and the
///   stream opened before it still gets its operation's end, from a daemon that
///   registers after the drain (mutant: the stream ended by the drain).
///   The REAPI connection is sent GOAWAY (`Drain::begin`'s unit tests pin each effect).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drain_by_the_endpoint_keeps_the_open_stream_and_refuses_the_next() {
    let cell = Cell::start_with_api().await;
    let job = Job::new("drained by the endpoint", &[]);
    cell.upload(&job.blobs()).await;
    // Everything the daemon will report is uploaded now: the drain closes the REAPI
    // listener to new connections.
    let result = output(&cell, "the build output", 0).await;
    assert_eq!(
        status(&cell).await,
        json!({"draining": false, "open_executions": 0, "queued_operations": 0})
    );

    let mut ops = cell.execute(&job.action).await;
    let queued = ops.message().await.expect("healthy").expect("QUEUED");
    assert_eq!(stage(&queued), ExecStage::Queued as i32);
    assert_eq!(
        status(&cell).await,
        json!({"draining": false, "open_executions": 1, "queued_operations": 1}),
        "an open stream whose operation waits for a worker"
    );

    let refused = begin(&cell, false, "").await;
    assert_eq!(refused.status, 401, "{refused:?}");
    let malformed = begin(&cell, true, r#"{"deadline_secs": 5}"#).await;
    assert_eq!(malformed.status, 400, "{malformed:?}");
    assert_eq!(
        status(&cell).await["draining"],
        false,
        "a refused POST drained"
    );

    let begun = begin(&cell, true, "").await;
    assert_eq!(begun.status, 200, "{begun:?}");
    let want = json!({"draining": true, "open_executions": 1, "queued_operations": 1});
    assert_eq!(
        serde_json::from_str::<Value>(&begun.body).expect("JSON"),
        want
    );
    let again = begin(&cell, true, "{}").await;
    assert_eq!(again.status, 200, "{again:?}");
    assert_eq!(
        serde_json::from_str::<Value>(&again.body).expect("JSON"),
        want
    );
    assert_eq!(status(&cell).await, want);

    let ready = call(&cell, false, |c| c.get("/readyz").expect("a reply")).await;
    assert_eq!(ready.status, 503, "{ready:?}");
    assert!(ready.body.contains("stopping"), "{ready:?}");
    // Refused, as UNAVAILABLE by the service or as a transport error once the
    // connection has been sent GOAWAY: either way not taken (`kbf-front`'s tests pin the
    // UNAVAILABLE; here the scheduler must still hold the one operation).
    let next = cell.execute_with(&job.action, false).await;
    next.expect_err("a new Execute during the drain");
    assert_eq!(status(&cell).await, want, "the refused Execute was queued");

    // The worker listener is not drained: a daemon registers now, takes the lease and
    // reports, and the stream opened before the drain ends with its result.
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let start = daemon.start().await;
    let ack = daemon.report(ran(start.lease_id, &result)).await;
    assert!(ack.accepted, "{ack:?}");
    let last = done(&mut ops).await;
    assert_eq!(stage(&last), ExecStage::Completed as i32);
}

/// Catches an idle frontend whose drain answer is lost: with no REAPI connection the
/// listener finishes the moment it is sent GOAWAY, and the serving future then ends
/// the process, so the operator API must finish answering what it is answering first.
/// Repeated, since the loss is a race. Mutant: the serving future returning as soon as
/// the REAPI listener is done, with no wait for the operator API.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_frontend_answers_the_drain_that_ends_it() {
    for round in 0..25 {
        let cell = Cell::start_with_api().await;
        let begun = begin(&cell, true, "").await;
        assert_eq!(begun.status, 200, "round {round}: {begun:?}");
        let body: Value = serde_json::from_str(&begun.body).expect("JSON");
        assert_eq!(body["draining"], true, "round {round}");
    }
}

/// Catches an escalation that does not end the streams: a drain that began (by the
/// endpoint, as here, or by a signal) waits for the stream's action, and
/// `Drain::close`, which a second signal calls, ends it UNAVAILABLE now. Mutant:
/// `Drain::close` not closing (the read times out).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_ends_the_streams_of_a_drain() {
    let cell = Cell::start_with_api().await;
    let job = Job::new("forced by a close", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    ops.message().await.expect("healthy").expect("QUEUED");
    let begun = begin(&cell, true, "").await;
    assert_eq!(begun.status, 200, "{begun:?}");

    cell.drain.close();
    let ended = tokio::time::timeout(PROMPT, ops.message())
        .await
        .expect("the stream ends after a close")
        .expect_err("it ends with an error");
    assert_eq!(ended.code(), Code::Unavailable, "{ended:?}");
    assert!(ended.message().contains("shutting down"), "{ended:?}");
}

/// Catches the node verbs `kbf cluster` sends: `drain` and `undrain` (the API's
/// `:uncordon`) reach the node the client names, with a deadline when it gives one;
/// a node id with a `/`, a space and a `:` is one path segment (percent-encoded), so
/// it reaches the node and not a 404; an unknown node is a 404 the client returns as a
/// reply, not an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_drains_and_undrains_a_node_whose_id_needs_encoding() {
    let cell = Cell::start_with_api().await;
    let node = "rack 1/node:a";
    let _daemon = cell.daemon(node, 4, 8).await;
    let verb = |verb: &'static str, body: &'static str| {
        call(&cell, true, move |c| {
            c.node_action(node, verb, body).expect("a reply")
        })
    };

    let drained = verb("drain", r#"{"deadline_secs": 3600}"#).await;
    assert_eq!(drained.status, 200, "{drained:?}");
    let view: Value = serde_json::from_str(&drained.body).expect("JSON");
    assert_eq!(view["node_id"], node, "{view}");
    assert_ne!(view["placement"]["state"], "serving", "{view}");

    let back = verb("uncordon", "").await;
    assert_eq!(back.status, 200, "{back:?}");
    let view: Value = serde_json::from_str(&back.body).expect("JSON");
    assert_eq!(view["placement"]["state"], "serving", "{view}");

    let unknown = call(&cell, true, |c| {
        c.node_action("nobody", "drain", "").expect("a reply")
    })
    .await;
    assert_eq!(unknown.status, 404, "{unknown:?}");
    let listed = call(&cell, false, |c| c.get(NODES).expect("a reply")).await;
    assert_eq!(listed.status, 200);
}
