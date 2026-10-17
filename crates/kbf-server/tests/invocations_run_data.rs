//! What the build pages add for each action and attempt (`docs/design/ui-data.md` 4.1,
//! 4.2 and 6.1 to 6.3, phase U1), over a real listener with the records fed to the
//! recorder directly: what a lease's run measured, the shape and the target the server
//! read from the Command, `sort=` and the new filters on `/actions`, and the
//! summary's slowest actions and phase totals. Each test names the defect it catches.

mod pages;

use kbf_invocation::{
    ActionShape, Event, LeaseEnd, NumaBytes, Outcome, ReapiTimes, Run, RunUsage, Target,
    TargetKind, Text,
};
use kbf_sched::RequeueReason;
use kbf_types::{Digest, Failure, OperationId};
use pages::{
    OPEN, Server, at, digest, ended, finished, granted, hit, lease, listing, queued, requeued,
    started, text,
};
use serde_json::{Value, json};

const GIB: u64 = 1 << 30;

/// Lease `n` of operation `op` measured `run`.
fn ran(op: u64, n: u64, run: Run) -> Event {
    Event::Ran {
        operation: OperationId(op),
        lease: lease(n),
        run: Box::new(run),
    }
}

/// A run whose `execution_start` and `execution_completed` are `from` and `to`
/// microseconds after the tests' base time.
fn executed(from: u64, to: u64) -> Run {
    Run {
        times: ReapiTimes {
            execution_start: Some(at(from)),
            execution_completed: Some(at(to)),
            ..ReapiTimes::default()
        },
        ..Run::default()
    }
}

/// An action of invocation `id` queued as operation `op` at `from` (microseconds),
/// whose one lease (granted at once) ended accepted at `to`, its run measuring
/// `run`.
fn one_run(id: &str, action: Digest, op: u64, (from, to): (u64, u64), run: Run) -> Vec<Event> {
    vec![
        queued(id, "main", at(from), action, op),
        granted(op, 1, "node-a", at(from)),
        ran(op, 1, run),
        finished(op, 1, Outcome::Ok { exit_code: 0 }, None, at(to)),
    ]
}

/// The rows' actions a page of `/actions` lists, in order.
fn listed(page: &Value) -> Vec<String> {
    page["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .map(|r| r["action"].as_str().expect("an action").to_owned())
        .collect()
}

/// Catches (ui-data 4.2, 6.3): an attempt's answer that drops what its lease's run
/// measured: the REAPI times, a cgroup counter (`memory_peak_bytes` above all), the
/// parallelism derived from them, the NUMA bytes; a counter not measured written as
/// zero rather than left out; the memory kill read wrong (an own-limit kill, a busy
/// node's, none) or given to a held lease; the booking and cap of the kill that ended
/// the operation lost; `raised_memory_bytes` on anything but an own-limit requeue;
/// and a lease with no run given made-up times.
#[tokio::test]
async fn an_attempt_answers_what_its_run_measured() {
    let server = Server::with(OPEN);
    let (killed, held, busy) = (digest(1), digest(2), digest(3));
    let measured = Run {
        times: ReapiTimes {
            queued: Some(at(0)),
            worker_start: Some(at(15)),
            execution_start: Some(at(16)),
            execution_completed: Some(at(29)),
            ..ReapiTimes::default()
        },
        usage: Some(RunUsage {
            wall_micros: 2_000_000,
            cpu_usage_micros: Some(3_000_000),
            cpu_user_micros: Some(2_000_000),
            cpu_system_micros: Some(1_000_000),
            memory_peak_bytes: Some(777),
            numa: vec![
                NumaBytes { node: 0, bytes: 6 },
                NumaBytes { node: 1, bytes: 4 },
            ],
            ..RunUsage::default()
        }),
        booked_memory_bytes: Some(GIB),
        cap_bytes: Some(2 * GIB),
    };
    let last = Run {
        booked_memory_bytes: Some(2 * GIB),
        cap_bytes: Some(2 * GIB),
        ..Run::default()
    };
    let oom = RequeueReason::OutOfMemory {
        booked: GIB,
        raised: 2 * GIB,
    };
    let gave_up = Outcome::Failed {
        code: 9,
        message: "needs more memory".to_owned(),
    };
    let pressure = Run {
        booked_memory_bytes: Some(GIB),
        usage: Some(RunUsage {
            wall_micros: 5,
            ..RunUsage::default()
        }),
        ..Run::default()
    };
    server
        .feed([
            queued("b", "main", at(0), killed, 1),
            granted(1, 1, "node-a", at(10)),
            started(1, 1, at(15)),
            ran(1, 1, measured),
            requeued(1, 1, oom, at(30)),
            granted(1, 2, "node-a", at(40)),
            ran(1, 2, last),
            ended(
                1,
                Some((
                    2,
                    LeaseEnd::Failed(Failure::OutOfMemory, "killed".to_owned()),
                )),
                gave_up,
                at(50),
            ),
            queued("b", "main", at(60), held, 2),
            granted(2, 1, "node-b", at(70)),
            queued("b", "main", at(80), busy, 3),
            granted(3, 1, "node-c", at(90)),
            ran(3, 1, pressure),
            requeued(3, 1, RequeueReason::NodeMemoryPressure, at(95)),
        ])
        .await;
    let base = "/v1/invocations/b/actions";
    let row = server.ok(&format!("{base}/{}", text(&killed))).await;
    let [first, second] = row["attempts"].as_array().expect("attempts").as_slice() else {
        panic!("two attempts: {row}");
    };
    assert_eq!(first["memory_kill"], "own_limit");
    assert_eq!(
        (
            &first["booked_memory_bytes"],
            &first["raised_memory_bytes"],
            &first["cap_bytes"]
        ),
        (&json!(GIB), &json!(2 * GIB), &json!(2 * GIB))
    );
    assert_eq!(
        first["usage"],
        json!({
            "wall_us": 2_000_000, "cpu_usage_us": 3_000_000, "cpu_user_us": 2_000_000,
            "cpu_system_us": 1_000_000, "memory_peak_bytes": 777, "parallelism": 1.5,
            "numa_bytes": [{"node": 0, "bytes": 6}, {"node": 1, "bytes": 4}],
        })
    );
    assert_eq!(
        first["reapi_times"],
        json!({
            "queued": "2026-10-11T09:00:00.000000Z",
            "worker_start": "2026-10-11T09:00:00.000015Z",
            "execution_start": "2026-10-11T09:00:00.000016Z",
            "execution_completed": "2026-10-11T09:00:00.000029Z",
        })
    );
    // The kill that ended the operation: its booking and cap, nothing raised.
    assert_eq!(second["memory_kill"], "own_limit");
    assert_eq!(
        (&second["booked_memory_bytes"], &second["cap_bytes"]),
        (&json!(2 * GIB), &json!(2 * GIB))
    );
    assert!(second.get("raised_memory_bytes").is_none(), "{second}");
    assert!(second.get("usage").is_none(), "no usage reported: {second}");
    assert_eq!(second["reapi_times"], json!({}));

    let rows = server.ok(base).await;
    let held_attempt = &rows["actions"][1]["attempts"][0];
    for key in ["memory_kill", "reapi_times", "usage", "booked_memory_bytes"] {
        assert!(held_attempt.get(key).is_none(), "{key}: {held_attempt}");
    }
    let busy_attempt = &rows["actions"][2]["attempts"][0];
    assert_eq!(busy_attempt["memory_kill"], "node_pressure");
    assert_eq!(busy_attempt["booked_memory_bytes"], GIB);
    for key in ["cap_bytes", "raised_memory_bytes"] {
        assert!(busy_attempt.get(key).is_none(), "{key}: {busy_attempt}");
    }
    assert_eq!(
        busy_attempt["usage"],
        json!({"wall_us": 5}),
        "no counter, parallelism or NUMA list made up"
    );
    assert_eq!(
        rows["actions"][0]["attempts"][0]["usage"]["memory_peak_bytes"], 777,
        "a page of rows lists the same attempts"
    );
    server.stop().await;
}

/// Catches (issue #796, ui-data 6.3): a row without the shape the server read (its
/// key, cfg and target), the target's kind or label misread, a label past 512 bytes
/// not cut and marked, the client's `target_id` (the row's `target`) replaced by the
/// derived one, and a row the server read no shape for given one.
#[tokio::test]
async fn a_row_answers_its_shape_and_target() {
    let server = Server::with(OPEN);
    let (shaped, bare, untargeted) = (digest(10), digest(11), digest(12));
    let long = format!("komira//src/{}:x", "p/".repeat(300));
    let shape = ActionShape {
        key: 0x0123_4567_89ab_cdef,
        cfg: Some("7a3c1e9f0b2d4c68".to_owned()),
        target: Some(Target {
            kind: TargetKind::Anon,
            label: Text::sent(long.clone()),
        }),
        test: false,
    };
    let command = |action, shape| Event::Command {
        id: kbf_invocation::InvocationId::new("s"),
        owner: kbf_invocation::Owner::default(),
        action,
        command: digest(99),
        shape,
    };
    server
        .feed([
            pages::queued_target("s", "//client:says", at(0), shaped, 1),
            command(shaped, Some(Box::new(shape))),
            queued("s", "main", at(1), bare, 2),
            command(bare, None),
            queued("s", "main", at(2), untargeted, 3),
            command(
                untargeted,
                Some(Box::new(ActionShape {
                    key: 1,
                    cfg: None,
                    target: None,
                    test: false,
                })),
            ),
        ])
        .await;
    let rows = server.ok("/v1/invocations/s/actions").await;
    let row = &rows["actions"][0];
    assert_eq!(row["shape"]["key"], "0123456789abcdef");
    assert_eq!(row["shape"]["cfg"], "7a3c1e9f0b2d4c68");
    let target = &row["shape"]["target"];
    assert_eq!(target["kind"], "anon");
    assert_eq!(target["truncated"], true);
    let label = target["label"].as_str().expect("a label");
    assert_eq!(label.len(), 512);
    assert!(long.starts_with(label));
    assert_eq!(
        row["target"],
        json!({"value": "//client:says", "truncated": false}),
        "the client's hint, as built"
    );
    assert_eq!(rows["actions"][1]["shape"], Value::Null);
    assert_eq!(
        rows["actions"][2]["shape"],
        json!({"key": "0000000000000001", "cfg": null, "target": null}),
        "paths that name no target: null, not a guess"
    );
    server.stop().await;
}

/// Catches (ui-data 4.1, 6.2): `sort=` that does not list longest first, breaks ties
/// otherwise than by row order, lists a row that has not measured the phase (one in
/// flight), loses its order on the next page (a page token that does not carry the
/// sort, or a `next_page` without it), drops or repeats a row tied with the last of
/// the page before (a cursor that compares the duration alone: page size 1 walks
/// through the tie), reads `execution_ms` from anything but the
/// run's REAPI times; a row's `durations` missing or not in milliseconds; and a sort
/// the route does not know, or a sorted page's token sent without the sort, answered
/// anything but `400`.
#[tokio::test]
async fn actions_sort_longest_first_and_page_on() {
    let server = Server::with(OPEN);
    let (a, b, c, d, flight) = (digest(1), digest(2), digest(3), digest(4), digest(5));
    let mut events = Vec::new();
    // Wall 30, 50, 50 and 10 ms; executions 1, 2, 9 and 4 ms.
    events.extend(one_run("t", a, 1, (0, 30_000), executed(0, 1_000)));
    events.extend(one_run("t", b, 2, (0, 50_000), executed(0, 2_000)));
    events.extend(one_run("t", c, 3, (0, 50_000), executed(0, 9_000)));
    events.extend(one_run("t", d, 4, (0, 10_000), executed(0, 4_000)));
    events.push(queued("t", "main", at(0), flight, 5));
    events.push(granted(5, 1, "node-a", at(1)));
    server.feed(events).await;

    let base = "/v1/invocations/t/actions";
    let first = server.ok(&format!("{base}?sort=wall_ms&page_size=2")).await;
    assert_eq!(listed(&first), [text(&b), text(&c)]);
    assert_eq!(first["actions"][0]["durations"]["wall_ms"], 50);
    let next = first["next_page"].as_str().expect("a next page");
    assert!(next.contains("sort=wall_ms"), "{next}");
    let second = server.ok(next).await;
    assert_eq!(listed(&second), [text(&a), text(&d)]);
    assert!(second.get("next_page").is_none(), "{second}");

    // One row a page, through the tie of b and c at 50 ms: b, then c, then a.
    let mut walked = Vec::new();
    let mut page = server.ok(&format!("{base}?sort=wall_ms&page_size=1")).await;
    for _ in 0..3 {
        walked.extend(listed(&page));
        // The token alone, sent with the same query: what a client holding it does.
        let token = page["next_page_token"]
            .as_str()
            .expect("a token")
            .to_owned();
        page = server
            .ok(&format!(
                "{base}?sort=wall_ms&page_size=1&page_token={token}"
            ))
            .await;
    }
    walked.extend(listed(&page));
    assert_eq!(walked, [text(&b), text(&c), text(&a), text(&d)]);
    assert!(page.get("next_page").is_none(), "{page}");

    let by_execution = server.ok(&format!("{base}?sort=execution_ms")).await;
    assert_eq!(
        listed(&by_execution),
        [text(&c), text(&d), text(&b), text(&a)]
    );
    assert_eq!(
        by_execution["actions"][0]["durations"],
        json!({"wall_ms": 50, "execution_ms": 9, "queue_ms": 0})
    );
    let rows = server.ok(base).await;
    assert_eq!(listed(&rows).len(), 5, "unsorted, every row");
    assert_eq!(rows["actions"][4]["durations"], json!({"queue_ms": 0}));

    let empty = server.ok(&format!("{base}?sort=input_fetch_ms")).await;
    assert_eq!(listed(&empty), Vec::<String>::new(), "no row measured it");
    assert!(empty.get("next_page").is_none(), "{empty}");

    let token = first["next_page_token"].as_str().expect("a token");
    // The same token, its position past the record's rows.
    let decoded = String::from_utf8(hex::decode(token).expect("hex")).expect("text");
    let mut fields: Vec<&str> = decoded.split(':').collect();
    fields[4] = "99";
    let past = hex::encode(fields.join(":"));
    for bad in [
        format!("{base}?sort=fast"),
        format!("{base}?page_token={token}"),
        format!("{base}?sort=execution_ms&page_token={token}"),
        format!("{base}?sort=wall_ms&page_token={past}"),
        "/v1/invocations/t/artifacts?sort=wall_ms".to_owned(),
        "/v1/invocations/t/logs?requeue_reason=silent".to_owned(),
        "/v1/invocations/t/artifacts?memory_kill=none".to_owned(),
    ] {
        assert_eq!(server.get(&bad).await.code, 400, "{bad}");
    }
    server.stop().await;
}

/// Catches (review of ui-data 6.2): a sorted page cut to its first rows in record
/// order before it is sorted (`select_nth_unstable_by` dropped from `sorted`), which
/// leaves out the longest rows when they come late in the record: here each row is
/// longer than the one before, so that cut lists 20 ms first at page size 1 and 30, 20
/// at page size 2.
#[tokio::test]
async fn a_sorted_page_finds_the_longest_rows_late_in_the_record() {
    let server = Server::with(OPEN);
    let rows = [digest(1), digest(2), digest(3), digest(4)];
    let mut events = Vec::new();
    for (op, row) in (1_u64..).zip(rows) {
        events.extend(one_run("t", row, op, (0, 10_000 * op), executed(0, 1_000)));
    }
    server.feed(events).await;
    let longest_first: Vec<String> = rows.iter().rev().map(text).collect();

    let base = "/v1/invocations/t/actions?sort=wall_ms";
    for size in [1, 2] {
        let mut walked = Vec::new();
        let mut page = server.ok(&format!("{base}&page_size={size}")).await;
        assert_eq!(listed(&page)[0], longest_first[0], "page size {size}");
        loop {
            walked.extend(listed(&page));
            let Some(next) = page["next_page"].as_str().map(str::to_owned) else {
                break;
            };
            page = server.ok(&next).await;
        }
        assert_eq!(walked, longest_first, "page size {size}");
    }
    server.stop().await;
}

/// Catches (ui-data 9, U1 red, and 6.2): `?requeue_reason=out_of_memory` missing an
/// action whose kill was run again and ended OK, or keeping one requeued for another
/// reason; `?memory_kill=` keeping the wrong rows (`none` keeping a killed row, a
/// kind keeping another kind's, `none` dropping a cache-hit row, which has no lease); a
/// `next_page` that drops the filter; and a value the route does not know answered
/// anything but `400`.
#[tokio::test]
async fn filters_keep_rows_by_requeue_reason_and_memory_kill() {
    let server = Server::with(OPEN);
    let (oom, silent, busy, plain) = (digest(1), digest(2), digest(3), digest(4));
    let hit_row = digest(5);
    let mut events = Vec::new();
    for (op, action, reason) in [
        (
            1,
            oom,
            Some(RequeueReason::OutOfMemory {
                booked: GIB,
                raised: 2 * GIB,
            }),
        ),
        (2, silent, Some(RequeueReason::Silent)),
        (3, busy, Some(RequeueReason::NodeMemoryPressure)),
        (4, plain, None),
    ] {
        events.push(queued("f", "main", at(0), action, op));
        events.push(granted(op, 1, "node-a", at(1)));
        let last = match reason {
            Some(reason) => {
                events.push(requeued(op, 1, reason, at(2)));
                events.push(granted(op, 2, "node-a", at(3)));
                2
            }
            None => 1,
        };
        events.push(finished(
            op,
            last,
            Outcome::Ok { exit_code: 0 },
            None,
            at(4),
        ));
    }
    events.push(hit(
        "f",
        at(5),
        hit_row,
        listing(digest(59), 0, &[], (None, None)),
    ));
    server.feed(events).await;
    let base = "/v1/invocations/f/actions";
    for (query, want) in [
        ("requeue_reason=out_of_memory", vec![oom]),
        ("requeue_reason=silent", vec![silent]),
        ("requeue_reason=infra", vec![]),
        ("memory_kill=own_limit", vec![oom]),
        ("memory_kill=node_pressure", vec![busy]),
        ("memory_kill=none", vec![silent, plain, hit_row]),
        ("memory_kill=own_limit&requeue_reason=silent", vec![]),
    ] {
        let page = server.ok(&format!("{base}?{query}")).await;
        let want: Vec<String> = want.iter().map(text).collect();
        assert_eq!(listed(&page), want, "{query}");
    }
    let first = server
        .ok(&format!("{base}?memory_kill=none&page_size=1"))
        .await;
    let next = first["next_page"].as_str().expect("a next page");
    assert!(next.contains("memory_kill=none"), "{next}");
    assert_eq!(listed(&server.ok(next).await), [text(&plain)]);
    for bad in ["requeue_reason=bored", "memory_kill=some"] {
        assert_eq!(
            server.get(&format!("{base}?{bad}")).await.code,
            400,
            "{bad}"
        );
    }
    server.stop().await;
}

/// Catches (ui-data 4.1): `top_slowest` that lists more than 20, not longest first,
/// counts a joined row (its twin's time, counted where the twin ran), or links rows
/// that do not answer; phase totals that sum the wrong rows or units, leave out how
/// many rows measured each phase (a mean needs it), or write a phase no row measured
/// as zero.
#[tokio::test]
async fn the_summary_names_the_slowest_and_the_phase_totals() {
    let server = Server::with(OPEN);
    let mut events = Vec::new();
    for n in 1..=22_u64 {
        // Wall n ms; execution 23 - n ms (so the two lists differ).
        let run = executed(0, (23 - n) * 1_000);
        events.extend(one_run("s", digest(n), n, (0, n * 1_000), run));
    }
    // A twin joined to the slowest while it runs: it shares its leases, and is listed
    // nowhere in the summary.
    let finish = events.pop().expect("the slowest's end");
    events.push(pages::call(
        "s",
        "",
        "main",
        at(0),
        kbf_invocation::CallKind::Execute {
            action: digest(99),
            answer: kbf_invocation::ExecuteAnswer::Joined {
                name: "operations/1-22j".to_owned(),
                operation: OperationId(22),
            },
        },
    ));
    events.push(finish);
    server.feed(events).await;
    let summary = server.ok("/v1/invocations/s").await;
    let wall = summary["top_slowest"]["wall_ms"]
        .as_array()
        .expect("a list");
    assert_eq!(wall.len(), 20);
    let walls: Vec<u64> = wall
        .iter()
        .map(|e| e["wall_ms"].as_u64().expect("ms"))
        .collect();
    assert_eq!(walls, (3..=22).rev().collect::<Vec<u64>>());
    assert_eq!(wall[0]["action"], text(&digest(22)));
    let joined = server.ok("/v1/invocations/s/actions?how=joined").await;
    assert_eq!(
        joined["actions"][0]["durations"]["wall_ms"], 22,
        "the twin has the slowest's leases"
    );
    let execution = &summary["top_slowest"]["execution_ms"][0];
    assert_eq!(
        (&execution["action"], &execution["execution_ms"]),
        (&json!(text(&digest(1))), &json!(22))
    );
    let href = wall[0]["href"].as_str().expect("an href");
    assert_eq!(server.ok(href).await["action"], text(&digest(22)));
    // Executions 22 + 21 + ... + 1 ms over the 22 rows that ran; nothing queued; no
    // fetch or upload measured.
    assert_eq!(
        summary["phase_totals"],
        json!({
            "execution_ms": {"sum": 253, "rows": 22},
            "queue_ms": {"sum": 0, "rows": 22},
        })
    );
    server.stop().await;
}

/// Catches (ui-data 6.1): a built key's form changed by the added ones: a page token
/// of the built filters written in another form (a client holding one breaks), or
/// `next_page` reordered; the built keys of a row and an attempt not as they were
/// (each one present under its built name: a renamed `started` goes red).
#[tokio::test]
async fn built_keys_and_links_read_as_before() {
    let server = Server::with(OPEN);
    let (a, b) = (digest(1), digest(2));
    let mut events = Vec::new();
    events.extend(one_run("k", a, 1, (0, 10), Run::default()));
    events.extend(one_run("k", b, 2, (0, 10), Run::default()));
    server.feed(events).await;
    let page = server
        .ok("/v1/invocations/k/actions?outcome=ok&page_size=1")
        .await;
    let token = page["next_page_token"].as_str().expect("a token");
    let decoded = String::from_utf8(hex::decode(token).expect("hex")).expect("text");
    let fields: Vec<&str> = decoded.split(':').collect();
    assert_eq!(
        (
            fields.len(),
            fields[0],
            fields[2],
            fields[3],
            fields[4],
            fields[5]
        ),
        (6, "1", "actions", "ok+-", "1", "0"),
        "{decoded}"
    );
    assert_eq!(
        page["next_page"],
        format!("/v1/invocations/k/actions?outcome=ok&page_size=1&page_token={token}")
    );
    let row = &page["actions"][0];
    let built = [
        "action",
        "instance",
        "target",
        "mnemonic",
        "configuration",
        "action_id",
        "how",
        "operation",
        "attempts",
        "outcome",
        "result",
        "command",
        "href",
    ];
    for key in built {
        assert!(row.get(key).is_some(), "{key}: {row}");
    }
    let attempt = &row["attempts"][0];
    for key in [
        "lease", "worker", "queued", "granted", "started", "ended", "end",
    ] {
        assert!(attempt.get(key).is_some(), "{key}: {attempt}");
    }
    assert_eq!(
        attempt["end"],
        json!({"result": "ok"}),
        "an attempt's end as built"
    );
    assert_eq!(attempt["lease"], "1.1");
    server.stop().await;
}

/// Catches (review of ui-data 4.2, 6.1): a row waiting to be granted again after an
/// own-limit requeue answered with a `wall_ms` from its first queueing to the requeue
/// (as if final) or the requeued lease's `execution_ms`, and so ranked by
/// `top_slowest` and `?sort=` on numbers that will change; the queueing so far
/// dropped with them.
#[tokio::test]
async fn a_row_waiting_after_a_requeue_has_no_wall_time_yet() {
    let server = Server::with(OPEN);
    let (done, waiting) = (digest(1), digest(2));
    let mut events = one_run("r", done, 1, (0, 1_000), executed(0, 500));
    events.extend([
        queued("r", "main", at(0), waiting, 2),
        granted(2, 1, "node-a", at(2_000)),
        ran(2, 1, executed(2_000, 90_000)),
        requeued(
            2,
            1,
            RequeueReason::OutOfMemory {
                booked: GIB,
                raised: 2 * GIB,
            },
            at(100_000),
        ),
    ]);
    server.feed(events).await;
    let row = server
        .ok(&format!("/v1/invocations/r/actions/{}", text(&waiting)))
        .await;
    assert_eq!(row["durations"], json!({"queue_ms": 2}));
    let summary = server.ok("/v1/invocations/r").await;
    for phase in ["wall_ms", "execution_ms"] {
        let actions: Vec<&Value> = summary["top_slowest"][phase]
            .as_array()
            .expect("a list")
            .iter()
            .map(|e| &e["action"])
            .collect();
        assert_eq!(actions, [&json!(text(&done))], "{phase}");
    }
    let sorted = server.ok("/v1/invocations/r/actions?sort=wall_ms").await;
    assert_eq!(listed(&sorted), [text(&done)]);
    server.stop().await;
}
