//! What goes right reading history back: a restart's summary and rows, a lost chunk, the
//! instances of an invocation, runs live and durable, page tokens, and a server with no
//! history. Each test names the defect it catches.

use std::sync::Arc;

use crate::pages::{PAGES_ONLY, Server, digest, text};
use crate::world::{ONLY_A, World, cached_of, hit_of, key, ran, set_header};
use kbf_history::records::InvocationHeader;
use kbf_history::{IndexRoot, InvKey};
use serde_json::{Value, json};

/// The keys of `value` named in `keys`.
fn pick(value: &Value, keys: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for k in keys {
        out.insert(
            (*k).to_owned(),
            value.get(*k).cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(out)
}

/// The keys of a restored summary that are history's own: those that equal the live
/// record's. Not these, because history does not keep them: `tool`, `tool_version`,
/// `correlated_invocations_id`, `collisions`, and the times of the first and last call.
const SUMMARY: [&str; 10] = [
    "id",
    "instances",
    "state",
    "complete",
    "rows_capped",
    "actions",
    "counts",
    "phase_totals",
    "top_slowest",
    "links",
];

/// The keys of a restored row that equal the live row's. Not these: `target`,
/// `mnemonic`, `configuration`, `action_id` and `operation` (the client's hints and the
/// operation name), and `result` (a hit's listing).
const ROW: [&str; 8] = [
    "action",
    "instance",
    "how",
    "attempts",
    "outcome",
    "durations",
    "href",
    "shape",
];

/// Catches (a record read back wrong): a summary whose counters, phase totals or slowest
/// actions differ from the record it came from, rows missing or in another order, a
/// row's `how` lost (a cache hit made a run), attempt times (queued, granted, started,
/// ended), the REAPI times, the cgroup block or the run id of an attempt lost or
/// shifted, a non-zero exit made a success, and a restored record that does not say
/// it is one (while a live one does).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invocation_summary_and_actions_survive_a_restart() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = ran("build", "main", 1, 1, 0);
    events.extend(ran("build", "main", 2, 2, 2));
    events.extend([hit_of("build", 3), cached_of("build", 4)]);
    a.feed(events).await;
    let live_summary = a.ok("/v1/invocations/build").await;
    let live_rows = a.ok("/v1/invocations/build/actions").await;
    assert!(live_summary.get("restored").is_none(), "{live_summary}");
    assert_eq!(live_summary["actions"], 4);
    world.flushed("build", 4).await;
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let summary = b.ok("/v1/invocations/build").await;
    let rows = b.ok("/v1/invocations/build/actions").await;
    assert_eq!(summary["restored"], true, "{summary}");
    assert_eq!(pick(&summary, &SUMMARY), pick(&live_summary, &SUMMARY));
    // The counters that matter, spelt out: two runs, a cached Execute, a hit, one failure.
    assert_eq!(summary["counts"]["ran"], 2);
    assert_eq!(summary["counts"]["cache_hit_execute"], 1);
    assert_eq!(summary["counts"]["ac_hit"], 1);
    assert_eq!(summary["counts"]["failed"], 1);
    let (live, restored) = (
        live_rows["actions"].as_array().expect("rows"),
        rows["actions"].as_array().expect("rows"),
    );
    assert_eq!(live.len(), restored.len());
    for (l, r) in live.iter().zip(restored) {
        assert_eq!(pick(r, &ROW), pick(l, &ROW), "{}", l["action"]);
    }
    // The attempts, spelt out for the first row: its times, its run id, its usage.
    let attempt = &restored[0]["attempts"][0];
    assert_eq!(attempt["queued"], live[0]["attempts"][0]["queued"]);
    assert!(attempt["ended"].is_string() && attempt["started"].is_string());
    assert!(attempt["run_id"].is_string());
    assert_eq!(
        attempt["run_href"],
        format!("/v1/runs/{}", attempt["run_id"].as_str().unwrap())
    );
    assert_eq!(attempt["usage"]["memory_peak_bytes"], 777);
    assert_eq!(restored[1]["outcome"], json!({"ok": {"exit_code": 2}}));
    assert_eq!(restored[2]["how"], "cache_hit");
    assert_eq!(restored[3]["how"], "cache_hit_execute");
    b.stop().await;
}

/// Catches: a chunk that was never written read back as something (a record made of
/// another invocation's rows, or of nothing: a `200` with no actions), and a server that
/// answers no invocation at all from history: the invocation whose chunk did land is the
/// control.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_before_the_chunk_put_is_no_record_not_a_wrong_record() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    a.feed(ran("kept", "main", 1, 1, 0)).await;
    world.flushed("kept", 1).await;
    // From here the object store refuses every write: the next build never reaches it.
    world.objects.fail_puts(true);
    a.feed(ran("lost", "main", 2, 2, 0)).await;
    a.stop().await;
    assert!(
        world
            .rows(&IndexRoot::default().ichunk_prefix(&InvKey::from_record_id("lost")))
            .is_empty()
    );

    let b = world.start(PAGES_ONLY);
    let lost = b.get("/v1/invocations/lost").await;
    assert_eq!(lost.code, 404, "{}", lost.json);
    assert_eq!(lost.json["error"], "no_record");
    let kept = b.ok("/v1/invocations/kept").await;
    assert_eq!(
        kept["actions"], 1,
        "only the build whose chunk landed: {kept}"
    );
    b.stop().await;
}

/// Catches: an invocation read back as complete when a later chunk was never written (its
/// header still open, as a crash leaves it), and a hole with no time: `gap_from` is when
/// the last chunk that did land was put, which is where a reader should look for what
/// is missing after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_second_chunk_says_complete_false_with_gap_from() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    a.feed(ran("holed", "main", 1, 1, 0)).await;
    world.flushed("holed", 1).await;
    let first = world.header("holed");
    assert!(first.last_chunk_ms > 0);
    world.objects.fail_puts(true);
    a.feed(ran("holed", "main", 2, 2, 0)).await;
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let summary = b.ok("/v1/invocations/holed").await;
    assert_eq!(summary["complete"], false, "{summary}");
    assert_eq!(
        summary["actions"], 1,
        "the lost chunk's action is not there"
    );
    assert_eq!(summary["gap_from"], rfc3339_ms(first.last_chunk_ms));
    b.stop().await;
}

/// `ms` milliseconds after the epoch as the API writes a time.
fn rfc3339_ms(ms: u64) -> String {
    // Seconds into the day, then the day, from the epoch (the tests' times are all after
    // 2000, and the writer's clock is the wall clock).
    let (secs, frac) = (ms / 1_000, ms % 1_000);
    let (days, day) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:06}Z",
        day / 3_600,
        day % 3_600 / 60,
        day % 60,
        frac * 1_000
    )
}

/// Catches: an index row that names a chunk the object store does not hold (a read of it
/// would be an error for ever), and a failed write that still leaves a stream, header or
/// chunk row. With writes working, the same build leaves them and is read back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_index_row_names_an_object_that_was_never_written() {
    let failing = World::new();
    failing.objects.fail_puts(true);
    let a = failing.start(PAGES_ONLY);
    a.feed(ran("never", "main", 1, 1, 0)).await;
    a.stop().await;
    assert_eq!(failing.objects.puts(), 0);
    for prefix in ["ui/ichunk/", "ui/invid/", "ui/stream/"] {
        assert!(
            failing.rows(prefix).is_empty(),
            "{prefix}: {:?}",
            failing.rows(prefix)
        );
    }
    let b = failing.start(PAGES_ONLY);
    assert_eq!(b.get("/v1/invocations/never").await.code, 404);
    b.stop().await;

    let working = World::new();
    let a = working.start(PAGES_ONLY);
    a.feed(ran("always", "main", 1, 1, 0)).await;
    working.flushed("always", 1).await;
    a.stop().await;
    assert!(working.objects.puts() > 0);
    for prefix in ["ui/ichunk/", "ui/invid/", "ui/stream/"] {
        assert!(!working.rows(prefix).is_empty(), "{prefix}");
    }
    let b = working.start(PAGES_ONLY);
    assert_eq!(b.ok("/v1/invocations/always").await["actions"], 1);
    b.stop().await;
}

/// Catches (security, mutant "authorize only the first instance"): an invocation that
/// used `a` and `b` read back, after a restart, by a caller allowed on `a` alone; and one
/// whose instances are all allowed refused. The refusal is the live one's: `403` and the
/// same text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spanning_invocation_is_all_must_allow_after_restart() {
    let world = World::new();
    let a = world.start(ONLY_A);
    let mut events = ran("span", "a/main", 1, 1, 0);
    // `a/main` sorts before `b`: the first name is the allowed one.
    events.extend(ran("span", "b", 2, 2, 0));
    events.extend(ran("only-a", "a/main", 3, 3, 0));
    a.feed(events).await;
    let live = a.get("/v1/invocations/span").await;
    assert_eq!(live.code, 403, "{}", live.json);
    assert_eq!(a.get("/v1/invocations/only-a").await.code, 200);
    world.flushed("span", 2).await;
    world.flushed("only-a", 1).await;
    a.stop().await;

    let b = world.start(ONLY_A);
    for route in ["", "/actions", "/metrics", "/logs", "/artifacts"] {
        let answer = b.get(&format!("/v1/invocations/span{route}")).await;
        assert_eq!(answer.code, 403, "{route}: {}", answer.json);
        assert_eq!(answer.json["error"], live.json["error"], "{route}");
        assert!(answer.json.get("actions").is_none() && answer.json.get("id").is_none());
    }
    assert_eq!(b.ok("/v1/invocations/only-a").await["restored"], true);
    b.stop().await;
}

/// Catches (mutant "the restore path ignores `truncated`"): a header whose instance list
/// the writer cut at its bound read as whole, so a caller allowed on the names listed
/// reads an invocation that used others. Nobody is answered, whoever they are, and an
/// invocation whose header lists none is the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_truncated_instance_list_answers_no_caller() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = ran("wide", "main", 1, 1, 0);
    events.extend(ran("bare", "main", 2, 2, 0));
    events.extend(ran("fine", "main", 3, 3, 0));
    a.feed(events).await;
    for id in ["wide", "bare", "fine"] {
        world.flushed(id, 1).await;
    }
    a.stop().await;
    let wide = InvocationHeader {
        instances_truncated: true,
        ..world.header("wide")
    };
    world.set_header("wide", &wide);
    let bare = InvocationHeader {
        instances: Vec::new(),
        ..world.header("bare")
    };
    world.set_header("bare", &bare);

    let b = world.start(PAGES_ONLY);
    for id in ["wide", "bare"] {
        let answer = b.get(&format!("/v1/invocations/{id}")).await;
        assert_eq!(answer.code, 404, "{id}: {}", answer.json);
        assert_eq!(answer.json["error"], "no_record", "{id}");
    }
    assert_eq!(b.ok("/v1/invocations/fine").await["actions"], 1);
    b.stop().await;
}

/// Catches (mutant "skip the second-phase check"): an instance added to the invocation
/// between the authorization of its names and the read of its records served to a caller
/// who was never asked about it. The request answers `503`, to be sent again, as a live
/// record's does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_instance_added_between_the_phases_is_503_changed() {
    let world = World::new();
    let a = world.start(ONLY_A);
    a.feed(ran("grow", "a/main", 1, 1, 0)).await;
    world.flushed("grow", 1).await;
    a.stop().await;
    let b = world.start(ONLY_A);
    assert_eq!(b.ok("/v1/invocations/grow").await["restored"], true);

    // Another writer adds instance `b` to the header right after the names were read.
    let grown = InvocationHeader {
        instances: vec!["a/main".to_owned(), "b".to_owned()],
        ..world.header("grow")
    };
    let state = Arc::clone(&world.state);
    world
        .state
        .after_get("ui/invid/", move || set_header(&state, "grow", &grown));
    let answer = b.get("/v1/invocations/grow").await;
    assert_eq!(answer.code, 503, "{}", answer.json);
    let why = answer.json["error"].as_str().unwrap_or_default();
    assert!(why.contains("gained an instance name"), "{why}");
    // Sent again, it is a plain refusal: `b` is not allowed.
    assert_eq!(b.get("/v1/invocations/grow").await.code, 403);
    b.stop().await;
}

/// Catches: a run whose JSON differs before and after the server restarts (a field the
/// live rendering has and the record does not carry, or the reverse), a run id that
/// resolves live and not from history, and one that resolves from history with another
/// run's fields. A's object store takes no writes while it is read, so its answer can
/// only be the live one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_by_id_live_equals_run_by_id_durable() {
    let world = World::new();
    world.objects.fail_puts(true);
    let a = world.start(PAGES_ONLY);
    let mut events = ran("runs", "main", 1, 1, 0);
    events.extend(ran("runs", "main", 2, 2, 7));
    a.feed(events).await;
    let rows = a.ok("/v1/invocations/runs/actions").await;
    let ids: Vec<String> = rows["actions"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| {
            r["attempts"][0]["run_id"]
                .as_str()
                .expect("an id")
                .to_owned()
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(world.objects.puts(), 0, "nothing durable yet");
    let mut live = Vec::new();
    for id in &ids {
        live.push(a.ok(&format!("/v1/runs/{id}")).await);
    }
    assert_eq!(live[0]["class"], "ok");
    assert_eq!(live[1]["class"], "failed");
    assert_eq!(live[1]["exit_code"], 7);
    assert_ne!(live[0]["action"], live[1]["action"]);
    // The writes work again: the writer puts what it held, and a stop flushes the rest.
    world.objects.fail_puts(false);
    world.flushed("runs", 2).await;
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    for (id, live) in ids.iter().zip(&live) {
        let durable = b.ok(&format!("/v1/runs/{id}")).await;
        assert_eq!(&durable, live, "{id}");
        assert_eq!(durable["run_id"], id.as_str());
        assert_eq!(durable["href"], format!("/v1/runs/{id}"));
    }
    // A run history does not hold, and text that is not a run id.
    let unknown = b.get("/v1/runs/0a0b0c0d0e0f0a0b0c0d0e0f").await;
    assert_eq!(unknown.code, 404);
    assert_eq!(unknown.json["error"], "no_run");
    for bad in [
        "nothing",
        "0A0B0C0D0E0F0A0B0C0D0E0F",
        "0a0b0c0d0e0f0a0b0c0d0e0",
    ] {
        let answer = b.get(&format!("/v1/runs/{bad}")).await;
        assert_eq!(answer.code, 404, "{bad}");
        assert_ne!(answer.json["error"], "no_run", "{bad}");
    }
    b.stop().await;
}

/// Catches (security, mutant "the run lookup ignores the instance"): a run asked under
/// an instance the caller may not read answered, live or after a restart, to a caller
/// allowed on another one; and a run of the allowed instance refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_of_an_unallowed_instance_is_refused() {
    let world = World::new();
    let a = world.start(ONLY_A);
    let mut events = ran("mixed", "a/main", 1, 1, 0);
    events.extend(ran("secret-build", "b", 2, 2, 0));
    a.feed(events).await;
    a.recorder.caught_up().await;
    let id_of = |build: &str| {
        let record = a.recorder.get(&key(build)).expect("a record");
        let id = record.rows()[0].attempts[0].run_id.expect("an id");
        hex::encode(id)
    };
    let (allowed, secret) = (id_of("mixed"), id_of("secret-build"));
    assert_eq!(a.get(&format!("/v1/runs/{allowed}")).await.code, 200);
    let live = a.get(&format!("/v1/runs/{secret}")).await;
    assert_eq!(live.code, 403, "{}", live.json);
    world.flushed("mixed", 1).await;
    world.flushed("secret-build", 1).await;
    a.stop().await;

    let b = world.start(ONLY_A);
    assert_eq!(
        b.ok(&format!("/v1/runs/{allowed}")).await["instance"],
        "a/main"
    );
    let durable = b.get(&format!("/v1/runs/{secret}")).await;
    assert_eq!(durable.code, 403, "{}", durable.json);
    assert_eq!(durable.json["error"], live.json["error"]);
    assert!(durable.json.get("action").is_none() && durable.json.get("worker").is_none());
    b.stop().await;
}

/// Catches: a page token made before a restart read against the record made from history
/// (its position names a row of a record that was another one): it answers `no_record`
/// as a token of a record removed at the bound does. A token made after the restart
/// pages the restored record, and the pages together are every row once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_from_before_a_restart_is_gone() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = Vec::new();
    for n in 1..=3 {
        events.extend(ran("paged", "main", n, u64::from(n), 0));
    }
    a.feed(events).await;
    let first = a.ok("/v1/invocations/paged/actions?page_size=1").await;
    let old = first["next_page_token"]
        .as_str()
        .expect("a token")
        .to_owned();
    world.flushed("paged", 3).await;
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let gone = b
        .get(&format!(
            "/v1/invocations/paged/actions?page_size=1&page_token={old}"
        ))
        .await;
    assert_eq!(gone.code, 404, "{}", gone.json);
    assert_eq!(gone.json["error"], "no_record");
    let mut seen = Vec::new();
    let mut next = "/v1/invocations/paged/actions?page_size=1".to_owned();
    loop {
        let page = b.ok(&next).await;
        seen.extend(
            page["actions"]
                .as_array()
                .expect("rows")
                .iter()
                .map(|r| r["action"].clone()),
        );
        match page["next_page"].as_str() {
            Some(link) => next = link.to_owned(),
            None => break,
        }
    }
    let want: Vec<Value> = (1..=3).map(|n| json!(text(&digest(n)))).collect();
    assert_eq!(seen, want);
    b.stop().await;
}

/// Catches: history leaking into a server that does not keep any: `/v1/runs/{id}`
/// answering (or answering unlike an unknown route), a `run_id` on the attempts of a live
/// record, a `restored` key, or a record the recorder does not hold answered with
/// anything but the `no_record` it always was. Without `--history-objects` the pages are
/// what they were.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_history_the_pages_are_what_they_were() {
    let server = Server::with(crate::pages::OPEN);
    server.feed(ran("plain", "main", 1, 1, 0)).await;
    let summary = server.ok("/v1/invocations/plain").await;
    assert!(summary.get("restored").is_none() && summary.get("gap_from").is_none());
    let rows = server.ok("/v1/invocations/plain/actions").await;
    let attempt = &rows["actions"][0]["attempts"][0];
    assert!(attempt.get("run_id").is_none() && attempt.get("run_href").is_none());
    let run = server.get("/v1/runs/0a0b0c0d0e0f0a0b0c0d0e0f").await;
    let nothing = server.get("/v1/nothing/at/all").await;
    assert_eq!((run.code, &run.body), (nothing.code, &nothing.body));
    let missing = server.get("/v1/invocations/missing").await;
    assert_eq!(missing.code, 404);
    assert_eq!(missing.json["error"], "no_record");
    assert_eq!(missing.json.as_object().expect("an object").len(), 3);
    server.stop().await;
}
