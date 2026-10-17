//! A failed run keeps its logs (`docs/design/ui-data.md` 5.3 and Q21, phase U3): the last
//! 16 KiB of each log with its record, and a copy of the logs and the ActionResult that
//! outlives the CAS blobs, read back through `GET /v1/runs/{id}/logs/{log}`. Servers are
//! started one after another over the same object store and state store, as a restart
//! leaves them. Each test names the defect it catches.

use std::time::Duration;

use kbf_invocation::{Event, Outcome};
use kbf_types::{Digest, FarmTime};
use serde_json::Value;

use crate::pages::{OPEN, Server, finished, granted, later, listing, queued, started};
use crate::world::{ONLY_A, World, key, measured};

/// The text of a log long enough for a tail to be shorter than it: 20,000 ASCII bytes that
/// tell any two places apart (a tail is shown as text, and text must read back as it was).
fn long_log() -> Vec<u8> {
    (0..20_000)
        .map(|i| b'a' + ((i * 7 + i / 251) % 26) as u8)
        .collect()
}

/// Farm time `n` days after the epoch.
fn days(n: u64) -> FarmTime {
    FarmTime::from_millis(n * 24 * 60 * 60 * 1000)
}

/// The events of a command that exited 2 in invocation `id`, asked on `instance`: queued as
/// operation `op`, run on a node, with `stdout` and `stderr` for its logs and `result` for
/// its ActionResult.
fn failed_run(
    id: &str,
    instance: &str,
    op: u64,
    logs: (Option<Digest>, Option<Digest>),
    result: Digest,
) -> Vec<Event> {
    let t = op * 1_000;
    let l = listing(result, 2, &[], logs);
    vec![
        queued(id, instance, later(t), crate::pages::digest(op), op),
        granted(op, 1, "node-a", later(t + 10)),
        started(op, 1, later(t + 12)),
        Event::Ran {
            operation: kbf_types::OperationId(op),
            lease: crate::pages::lease(1),
            run: Box::new(measured(t)),
        },
        finished(op, 1, Outcome::Ok { exit_code: 2 }, Some(l), later(t + 50)),
    ]
}

/// The run id, as text, of the first attempt of `inv`'s first action.
fn run_id(server: &Server, inv: &str) -> String {
    let record = server.recorder.get(&key(inv)).expect("a record");
    hex::encode(record.rows()[0].attempts[0].run_id.expect("an id"))
}

/// Waits until `n` failure-log rows are in the index.
async fn copies(world: &World, n: usize) {
    for _ in 0..500 {
        if world.rows("ui/obj/flog/").len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{n} failure logs never reached the index");
}

/// The run `id` of a build whose chunk is in the stores, once its failed section is: a
/// run page that shows its tails.
async fn with_tails(server: &Server, id: &str) -> Value {
    for _ in 0..500 {
        let page = server.ok(&format!("/v1/runs/{id}")).await;
        if page["logs"]["stdout"]["tail"].is_string() {
            return page;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("run {id} never showed a tail");
}

/// Catches (the Q21 promise): a failed run's logs that die with their CAS blobs. The blobs
/// are collected; the run page still shows the tails and now links the copy, the copy
/// serves the original bytes of each log and of the ActionResult, and a restarted server
/// (whose cache holds nothing) serves the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_runs_logs_outlive_the_cas_blob() {
    let world = World::new();
    let a = world.start(OPEN);
    let out = long_log();
    let (stdout, stderr) = (a.store(&out).await, a.store(b"error: boom\n").await);
    let result = a.store(b"an ActionResult").await;
    a.feed(failed_run(
        "build",
        "main",
        1,
        (Some(stdout), Some(stderr)),
        result,
    ))
    .await;
    copies(&world, 1).await;
    world.flushed("build", 1).await;
    let id = run_id(&a, "build");
    let page = with_tails(&a, &id).await;
    // While the blobs are held: the page links the blob route, with the tails beside.
    let logs = &page["logs"];
    assert_eq!(logs["stderr"]["available"], true, "{page}");
    assert!(
        logs["stderr"]["href"]
            .as_str()
            .unwrap_or_default()
            .starts_with("/v1/blobs/"),
        "{page}"
    );
    assert_eq!(logs["stderr"]["tail"], "error: boom\n");
    assert_eq!(logs["stderr"]["total_bytes"], 12);
    assert_eq!(logs["stdout"]["total_bytes"], 20_000);
    let tail = logs["stdout"]["tail"].as_str().expect("a tail");
    assert_eq!(
        tail.as_bytes(),
        &out[out.len() - 16 * 1024..],
        "the last 16 KiB"
    );
    assert_eq!(
        logs["stdout"]["tail_redacted"], false,
        "no redaction exists, and it says so"
    );
    assert_eq!(
        page["command_rendering"],
        Value::Null,
        "this run has no command blob"
    );
    // The blobs are collected.
    a.cache.tick(days(100)).await.expect("tick");
    a.cache.collect().await.expect("collect");
    let missing = a
        .cache
        .find_missing(&[stdout, stderr, result])
        .await
        .expect("find");
    assert_eq!(missing.len(), 3, "the CAS holds none of them any more");
    let page = a.ok(&format!("/v1/runs/{id}")).await;
    for log in ["stdout", "stderr", "result"] {
        assert_eq!(page["logs"][log]["available"], false, "{log}: {page}");
        assert_eq!(
            page["logs"][log]["href"],
            format!("/v1/runs/{id}/logs/{log}"),
            "{log}"
        );
    }
    assert_eq!(
        page["logs"]["stderr"]["tail"], "error: boom\n",
        "the tail outlives it"
    );
    for (log, bytes) in [
        ("stdout", out.as_slice()),
        ("stderr", b"error: boom\n"),
        ("result", b"an ActionResult"),
    ] {
        let answer = a.get(&format!("/v1/runs/{id}/logs/{log}")).await;
        assert_eq!(answer.code, 200, "{log}: {}", answer.head);
        assert!(answer.body == bytes, "{log}: the original bytes");
        let head = answer.head.to_lowercase();
        assert!(
            head.contains("content-type: application/octet-stream"),
            "{head}"
        );
        assert!(head.contains("x-content-type-options: nosniff"), "{head}");
        assert!(head.contains("content-security-policy: sandbox"), "{head}");
    }
    a.stop().await;

    // A restarted server, with a cache of its own that holds nothing.
    let b = world.start(OPEN);
    let page = b.ok(&format!("/v1/runs/{id}")).await;
    assert_eq!(
        page["logs"]["stdout"]["href"],
        format!("/v1/runs/{id}/logs/stdout")
    );
    assert_eq!(page["logs"]["stderr"]["tail"], "error: boom\n");
    let answer = b.get(&format!("/v1/runs/{id}/logs/stdout")).await;
    assert_eq!(answer.code, 200, "{}", answer.head);
    assert!(answer.body == out, "the whole log, after a restart");
    // One range of it: only that part is sent.
    let answer = b
        .get_with(
            &format!("/v1/runs/{id}/logs/stdout"),
            "Range: bytes=100-199\r\n",
        )
        .await;
    assert_eq!(answer.code, 206, "{}", answer.head);
    assert!(
        answer
            .head
            .to_lowercase()
            .contains("content-range: bytes 100-199/20000")
    );
    assert!(answer.body == out[100..200]);
    b.stop().await;
}

/// Catches (mutant "copy successful runs too"): a copy and a row for a run that passed (the
/// PUTs of a farm that mostly passes would be doubled), or a failed run that wrote no
/// object (no copy). Counted in PUTs: one chunk for the passing build; a chunk and a copy
/// for the failing one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_run_writes_no_flog_object() {
    let world = World::new();
    let a = world.start(OPEN);
    let (stdout, result) = (a.store(b"fine").await, a.store(b"result").await);
    // A passing run with logs of its own, as the command wrote them.
    let l = listing(result, 0, &[], (Some(stdout), None));
    let events = [
        queued("pass", "main", later(1_000), crate::pages::digest(1), 1),
        granted(1, 1, "node-a", later(1_010)),
        started(1, 1, later(1_012)),
        Event::Ran {
            operation: kbf_types::OperationId(1),
            lease: crate::pages::lease(1),
            run: Box::new(measured(1_000)),
        },
        finished(1, 1, Outcome::Ok { exit_code: 0 }, Some(l), later(1_050)),
    ];
    a.feed(events).await;
    world.flushed("pass", 1).await;
    assert_eq!(world.objects.puts(), 1, "the chunk, and nothing else");
    assert!(world.rows("ui/obj/").is_empty());
    a.feed(failed_run("fail", "main", 2, (Some(stdout), None), result))
        .await;
    copies(&world, 1).await;
    world.flushed("fail", 1).await;
    assert_eq!(
        world.objects.puts(),
        3,
        "a chunk and a copy for the failing build"
    );
    assert_eq!(world.rows("ui/obj/flog/").len(), 1);
    a.stop().await;
}

/// Catches (security, mutant "skip the instance check on the logs route"): a copy served to
/// a caller authorized on another instance than the run's, live or after a restart, and
/// the instance of the run named in the refusal or its bytes in it. The run of the allowed
/// instance is served, so it is the route that refuses and not the route that is off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_on_another_instance_is_refused_the_log_route() {
    let world = World::new();
    let a = world.start(ONLY_A);
    let (open, secret) = (
        a.store(b"a log anyone may read").await,
        a.store(b"hunter2").await,
    );
    let result = a.store(b"r").await;
    let mut events = failed_run("mine", "a/main", 1, (Some(open), None), result);
    events.extend(failed_run("theirs", "b", 2, (Some(secret), None), result));
    a.feed(events).await;
    copies(&world, 2).await;
    world.flushed("mine", 1).await;
    world.flushed("theirs", 1).await;
    let (mine, theirs) = (run_id(&a, "mine"), run_id(&a, "theirs"));
    let ok = a.get(&format!("/v1/runs/{mine}/logs/stdout")).await;
    assert_eq!(ok.code, 200, "{}", ok.head);
    assert!(ok.body == b"a log anyone may read");
    let refused = a.get(&format!("/v1/runs/{theirs}/logs/stdout")).await;
    assert_eq!(refused.code, 403, "{}", refused.head);
    assert!(!String::from_utf8_lossy(&refused.body).contains("hunter2"));
    assert!(
        refused
            .head
            .to_lowercase()
            .contains("x-content-type-options: nosniff")
    );
    a.stop().await;
    let b = world.start(ONLY_A);
    let refused = b.get(&format!("/v1/runs/{theirs}/logs/stdout")).await;
    assert_eq!(refused.code, 403, "after a restart: {}", refused.head);
    assert!(!String::from_utf8_lossy(&refused.body).contains("hunter2"));
    assert_eq!(
        b.get(&format!("/v1/runs/{mine}/logs/stdout")).await.code,
        200
    );
    b.stop().await;
}

/// Catches: a log that was never copied answered `200` empty, or `500`; a run that is not
/// held answered like one that is; a parameter or a log name the route does not have
/// accepted (the first `400`, the second `404`); and the hardening headers missing from an
/// error (a refusal is as much a response as bytes).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_is_not_a_log_of_a_run_is_an_error_that_is_still_not_a_page() {
    let world = World::new();
    let a = world.start(OPEN);
    let result = a.store(b"r").await;
    a.feed(failed_run("only-result", "main", 1, (None, None), result))
        .await;
    copies(&world, 1).await;
    let id = run_id(&a, "only-result");
    let none = a.get(&format!("/v1/runs/{id}/logs/stdout")).await;
    assert_eq!(none.code, 404, "{}", none.head);
    assert_eq!(none.json["error"], "no_log");
    assert_eq!(a.get(&format!("/v1/runs/{id}/logs/result")).await.code, 200);
    let unknown = a.get("/v1/runs/0a0b0c0d0e0f0a0b0c0d0e0f/logs/stdout").await;
    assert_eq!(
        (unknown.code, unknown.json["error"].clone()),
        (404, "no_run".into())
    );
    let query = a.get(&format!("/v1/runs/{id}/logs/result?x=1")).await;
    assert_eq!(query.code, 400, "{}", query.head);
    let name = a.get(&format!("/v1/runs/{id}/logs/stdin")).await;
    assert_eq!(name.code, 404, "{}", name.head);
    for answer in [&none, &unknown, &query, &name] {
        let head = answer.head.to_lowercase();
        assert!(head.contains("x-content-type-options: nosniff"), "{head}");
        assert!(head.contains("content-security-policy: sandbox"), "{head}");
    }
    a.stop().await;
}

/// Catches: the route, or the tails, existing on a server that keeps no history (it must
/// run as it did): the logs path answers as any unknown path does, a run's page is not
/// served, and a build with a failed run shows what it always showed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_with_no_history_has_no_log_route() {
    let server = Server::with(OPEN);
    let result = server.store(b"r").await;
    let out = server.store(b"out").await;
    server
        .feed(failed_run("plain", "main", 1, (Some(out), None), result))
        .await;
    // No tap mints run ids on a server with no history: any id is one it does not hold.
    let id = "0a0b0c0d0e0f0a0b0c0d0e0f";
    let logs = server.get(&format!("/v1/runs/{id}/logs/stdout")).await;
    let nothing = server.get(&format!("/v1/runs/{id}/nothing")).await;
    assert_eq!((logs.code, &logs.body), (nothing.code, &nothing.body));
    assert_eq!(logs.code, 404);
    assert!(
        logs.json.get("error").is_none(),
        "no route, not no_log: {}",
        logs.json
    );
    let run = server.get(&format!("/v1/runs/{id}")).await;
    assert_eq!(run.code, 404, "the run page is history's");
    // The build page is what it was: no `logs` object of tails, and no recorded run id.
    let page = server.ok("/v1/invocations/plain/actions").await;
    assert_eq!(
        page["actions"][0]["attempts"][0].get("run_id"),
        None,
        "{page}"
    );
    server.stop().await;
}

/// Catches (mutant "write the obj row before the PUT"): a row naming an object that was not
/// written, which every read of the copy would fail on for as long as it is kept, and a copy
/// that could not be written failing the run (it is the log that is lost, never the run):
/// the run reads back with its tails, links the blob, says there is no copy, and the failed
/// PUT is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_flog_row_names_an_object_not_written() {
    let world = World::new();
    let a = world.start(OPEN);
    let (out, result) = (a.store(b"out of a run").await, a.store(b"r").await);
    world.objects.fail_puts(true);
    a.feed(failed_run("flaky", "main", 1, (Some(out), None), result))
        .await;
    let id = run_id(&a, "flaky");
    // The copy's write fails; the chunk's is retried, and lands once the store works.
    for _ in 0..100 {
        let metrics = a.get("/metrics").await;
        let text = String::from_utf8_lossy(&metrics.body).into_owned();
        if text.contains("kbf_history_dropped_total{stage=\"put\"} 0\n") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        } else {
            break;
        }
    }
    assert!(
        world.rows("ui/obj/").is_empty(),
        "no row for an object that was not written"
    );
    world.objects.fail_puts(false);
    world.flushed("flaky", 1).await;
    assert!(world.rows("ui/obj/").is_empty(), "nor later");
    let page = with_tails(&a, &id).await;
    assert_eq!(page["logs"]["stdout"]["tail"], "out of a run");
    assert!(
        page["logs"]["stdout"]["href"]
            .as_str()
            .unwrap_or_default()
            .starts_with("/v1/blobs/"),
        "{page}"
    );
    let none = a.get(&format!("/v1/runs/{id}/logs/stdout")).await;
    assert_eq!(
        (none.code, none.json["error"].clone()),
        (404, "no_log".into())
    );
    let metrics = a.get("/metrics").await;
    let text = String::from_utf8_lossy(&metrics.body).into_owned();
    assert!(
        !text.contains("kbf_history_dropped_total{stage=\"put\"} 0\n"),
        "the failed copy is counted: {text}"
    );
    a.stop().await;
}

/// Catches: a caller the policy does not accept let in to a log (`401`, as to every other
/// route), and the log route answering when the policy turns the build pages off: `404`
/// with the `off` body of the pages, and never `no_run`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_log_route_is_authenticated_and_off_with_the_pages() {
    let world = World::new();
    let path = "/v1/runs/0a0b0c0d0e0f0a0b0c0d0e0f/logs/stdout";
    let denied = world.start(
        r#"{"authenticationPolicy": {"deny": "who are you"},
            "invocations": {"getAuthorizer": {"allow": {}}}}"#,
    );
    let answer = denied.get(path).await;
    assert_eq!(answer.code, 401, "{}", answer.json);
    assert!(
        answer
            .head
            .to_lowercase()
            .contains("x-content-type-options: nosniff")
    );
    denied.stop().await;
    let off = world.start(r#"{"authenticationPolicy": {"allow": {}}}"#);
    let answer = off.get(path).await;
    assert_eq!(answer.code, 404, "{}", answer.json);
    assert_eq!(answer.json["off"], "invocations");
    off.stop().await;
}

/// Catches: a run the recorder holds failing to answer because history cannot be read (its
/// tails are the only thing missing: the page is the recorder's), the cause of a store's
/// failure sent to a caller of the log route, and a copy that cannot be read answered as one
/// that does not exist (`404`, which says there never was one).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_history_that_cannot_be_read_costs_a_live_run_its_tails_and_a_log_its_answer() {
    let world = World::new();
    let a = world.start(OPEN);
    let (out, result) = (a.store(b"out").await, a.store(b"r").await);
    a.feed(failed_run("live", "main", 1, (Some(out), None), result))
        .await;
    copies(&world, 1).await;
    world.flushed("live", 1).await;
    let id = run_id(&a, "live");
    world.objects.fail_gets(true);
    let page = a.ok(&format!("/v1/runs/{id}")).await;
    assert_eq!(page["class"], "failed", "{page}");
    assert!(
        page["logs"]["stdout"].get("tail").is_none(),
        "no tails without history: {page}"
    );
    let log = a.get(&format!("/v1/runs/{id}/logs/stdout")).await;
    assert_eq!(log.code, 503, "{}", log.head);
    assert!(!log.json.to_string().contains("planted"), "{}", log.json);
    world.objects.fail_gets(false);
    assert_eq!(a.get(&format!("/v1/runs/{id}/logs/stdout")).await.code, 200);
    a.stop().await;
    // After a restart the run is history's alone, and its failure is the answer.
    let b = world.start(OPEN);
    world.objects.fail_gets(true);
    let log = b.get(&format!("/v1/runs/{id}/logs/stdout")).await;
    assert_eq!(log.code, 503, "{}", log.head);
    world.objects.fail_gets(false);
    b.stop().await;
}
