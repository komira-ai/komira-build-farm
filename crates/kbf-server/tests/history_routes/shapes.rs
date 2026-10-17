//! The shape catalog and the tests, read from index rows after a restart. Each test names
//! the defect it catches.

use kbf_history::records::TestStatus;
use kbf_history::{IndexRoot, RingRun, StatusRow};
use kbf_state::{Cond, Store as _, Txn};
use prost::Message as _;
use serde_json::{Value, json};

use crate::kit::{indexed, run, shape, status, until};
use crate::pages::PAGES_ONLY;
use crate::world::World;

const FAILING: u64 = 0x1111_1111_1111_1111;
const PASSING: u64 = 0x2222_2222_2222_2222;
const COMPILE: u64 = 0x3333_3333_3333_3333;

/// The `key`s of the rows of a list answer, in order.
fn keys(answer: &Value) -> Vec<String> {
    answer["shapes"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|s| s["key"].as_str().expect("a key").to_owned())
        .collect()
}

/// Catches (ui-data 4.3, 5.5, 6.2): a shape list, a test list, a status filter, a shape's
/// detail or its ring of runs that reads an object of the object store (a chunk or a tail,
/// where the index rows answer: the count of reads the store was asked for is the same
/// before and after every request), a row that loses a field the contract names, a test
/// whose status or first failure is told from the wrong run, a catalog row listed as a test
/// (or a test as another shape), and a status filter that lists a test under another
/// status. A restart is between the build and every read: nothing is held in memory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shapes_and_tests_answer_after_a_restart_from_index_only() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = run(
        "b1",
        "main",
        shape(FAILING, "cfgA", true, Some("komira//t:fail")),
        1,
        2,
    );
    events.extend(run("b1", "main", shape(PASSING, "cfgA", true, None), 2, 0));
    events.extend(run("b1", "main", shape(COMPILE, "-", false, None), 3, 0));
    a.feed(events).await;
    world.flushed("b1", 3).await;
    indexed(&world, 3, 2).await;
    until("both by-status rows", || {
        world.rows("ui/statusx/failing/").len() == 1 && world.rows("ui/statusx/passing/").len() == 1
    })
    .await;
    a.stop().await;
    let before = world.objects.gets();

    let b = world.start(PAGES_ONLY);
    let all = b.ok("/v1/shapes").await;
    assert_eq!(
        keys(&all),
        ["1111111111111111", "2222222222222222", "3333333333333333"]
    );
    let first = &all["shapes"][0];
    assert_eq!(first["instance"], "main");
    assert_eq!(first["cfg"], "cfgA");
    assert_eq!(first["kind"], "test");
    assert_eq!(first["target"], "komira//t:fail");
    assert_eq!(first["status"], "failing");
    assert_eq!(
        first["flaky"],
        json!({"proven": false, "proof_digest": null, "since": null})
    );
    assert_eq!(first["last_pass"], Value::Null);
    let fail_run = first["first_fail"]["run_id"].as_str().expect("a run id");
    assert_eq!(fail_run.len(), 24);
    assert_eq!(
        first["first_fail"]["run_href"],
        format!("/v1/runs/{fail_run}")
    );
    assert!(
        first["first_seen"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z'))
    );
    assert!(
        first["last_seen"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z'))
    );
    assert_eq!(
        first["href"],
        "/v1/shapes/1111111111111111?instance=main&cfg=cfgA"
    );
    assert_eq!(
        first["runs_href"],
        "/v1/shapes/1111111111111111/runs?instance=main&cfg=cfgA"
    );
    assert_eq!(all["shapes"][1]["status"], "passing");
    assert_eq!(all["shapes"][1]["target"], Value::Null);
    // The compile is a shape that is not a test: no status, no flake state.
    let compile = &all["shapes"][2];
    assert_eq!(compile["kind"], "other");
    assert!(compile.get("status").is_none() && compile.get("flaky").is_none());

    assert_eq!(
        keys(&b.ok("/v1/tests").await),
        ["1111111111111111", "2222222222222222"]
    );
    assert_eq!(
        keys(&b.ok("/v1/shapes?kind=other").await),
        ["3333333333333333"]
    );
    assert_eq!(
        keys(&b.ok("/v1/shapes?kind=test").await),
        ["1111111111111111", "2222222222222222"]
    );
    assert_eq!(
        keys(&b.ok("/v1/tests?status=failing").await),
        ["1111111111111111"]
    );
    assert_eq!(
        keys(&b.ok("/v1/shapes?status=passing").await),
        ["2222222222222222"]
    );
    assert!(keys(&b.ok("/v1/tests?status=flaky").await).is_empty());
    assert!(keys(&b.ok("/v1/tests?status=oom_killed").await).is_empty());
    assert!(keys(&b.ok("/v1/tests?instance=elsewhere").await).is_empty());
    // The failing test's row by status carries the catalog's target too.
    let failing = b.ok("/v1/tests?status=failing&instance=main").await;
    assert_eq!(failing["shapes"][0]["target"], "komira//t:fail");

    // One row by instance and configuration, and the rows of a key.
    let one = b
        .ok("/v1/shapes/1111111111111111?instance=main&cfg=cfgA")
        .await;
    assert_eq!(one["key"], "1111111111111111");
    assert!(
        one.get("previous_keys").is_some_and(Value::is_null),
        "the key is there, and null: no report maps keys yet: {one}"
    );
    assert_eq!(one["status"], "failing");
    let rows = b.ok("/v1/shapes/1111111111111111").await;
    assert_eq!(keys(&rows), ["1111111111111111"]);
    assert!(rows.get("next_page").is_none());
    assert_eq!(
        keys(&b.ok("/v1/shapes/1111111111111111?cfg=cfgA").await),
        ["1111111111111111"]
    );
    assert_eq!(
        b.get("/v1/shapes/1111111111111111?cfg=other").await.code,
        404
    );
    assert_eq!(b.get("/v1/shapes/4444444444444444").await.code, 404);
    let unknown = b
        .get("/v1/shapes/4444444444444444?instance=main&cfg=cfgA")
        .await;
    assert_eq!(
        (unknown.code, &unknown.json["error"]),
        (404, &json!("no_shape"))
    );

    let no_ring = b
        .get("/v1/shapes/4444444444444444/runs?instance=main&cfg=cfgA")
        .await;
    assert_eq!(
        (no_ring.code, &no_ring.json["error"]),
        (404, &json!("no_shape"))
    );
    let ring = b
        .ok("/v1/shapes/1111111111111111/runs?instance=main&cfg=cfgA")
        .await;
    assert_eq!(ring["runs"].as_array().map(Vec::len), Some(1));
    assert_eq!(ring["runs"][0]["class"], "fail");
    assert_eq!(ring["runs"][0]["run_id"], fail_run);
    assert_eq!(
        b.ok("/v1/shapes/3333333333333333/runs?instance=main&cfg=-")
            .await["runs"],
        json!([])
    );
    assert_eq!(
        world.objects.gets(),
        before,
        "no request read an object: the index answers"
    );
    b.stop().await;
}

/// Catches (ui-data 4.3 "flaky, proven"): a digest seen failing and then passing listed
/// as passing (the `flaky` status lost, or the proof not kept), a proof that names no
/// digest or the wrong run as the one that completed it, and a link to a run that does
/// not resolve: every `run_href` of the list, the ring and the marks answers
/// `GET /v1/runs/{id}` after a restart with the run it names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flaky_test_is_listed_with_its_proof_digest_and_the_run_links_resolve_via_runs_by_id() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let flaky = || shape(FAILING, "cfgA", true, Some("komira//t:flaky"));
    // The same action (digest 1) fails in one build and passes in the next.
    a.feed(run("fl1", "main", flaky(), 1, 1)).await;
    world.flushed("fl1", 1).await;
    a.feed(run("fl2", "main", flaky(), 1, 0)).await;
    world.flushed("fl2", 1).await;
    until("the proof in the status row", || {
        status(&world, "main", FAILING, "cfgA").is_some_and(|s| s.proven && s.ring.len() == 2)
    })
    .await;
    until("the flaky row", || {
        world.rows("ui/statusx/flaky/").len() == 1 && world.rows("ui/statusx/failing/").is_empty()
    })
    .await;
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let list = b.ok("/v1/tests?status=flaky").await;
    assert_eq!(keys(&list), ["1111111111111111"]);
    let test = &list["shapes"][0];
    assert_eq!(test["status"], "flaky");
    assert_eq!(test["flaky"]["proven"], true);
    assert_eq!(test["flaky"]["proof_digest"], "0000000000000001");
    let since = test["flaky"]["since"]["run_id"].as_str().expect("a run id");
    assert_eq!(
        test["last_pass"]["run_id"], since,
        "the pass completed the proof"
    );
    assert_eq!(
        test["first_fail"],
        Value::Null,
        "a pass since clears the first failure"
    );
    // Every link resolves to the run it names, in the invocation that made it.
    let ring = b.ok(test["runs_href"].as_str().expect("a link")).await;
    let runs = ring["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["class"], "pass", "newest first: {ring}");
    assert_eq!(runs[1]["class"], "fail");
    assert_eq!(runs[0]["digest16"], "0000000000000001");
    for r in runs {
        let resolved = b.ok(r["run_href"].as_str().expect("a link")).await;
        assert_eq!(resolved["run_id"], r["run_id"]);
        assert_eq!(resolved["instance"], "main");
        assert_eq!(resolved["shape"]["key"], "1111111111111111");
    }
    let href = test["last_pass"]["run_href"].as_str().expect("a link");
    assert_eq!(b.ok(href).await["class"], "ok");
    let failed = b.ok(runs[1]["run_href"].as_str().expect("a link")).await;
    assert_eq!(failed["class"], "failed");
    assert_ne!(failed["run_id"], since);
    b.stop().await;
}

/// Catches (ui-data 4.3 "ring of the last 16 runs"): a ring that answers more or fewer
/// than the last 16 runs, the oldest first, or not the runs that were last (compared with
/// the run ids the build page names for its 20 actions), and an answer that claims to be
/// the whole history: `complete` is false, `covers` says what it is and `needs` names what
/// would answer the rest. A shape that is not a test has no ring, and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ring_answers_16_runs_and_says_complete_false() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = Vec::new();
    for n in 1..=20u8 {
        events.extend(run(
            "ring",
            "main",
            shape(PASSING, "cfgA", true, None),
            n,
            0,
        ));
    }
    events.extend(run("ring", "main", shape(COMPILE, "-", false, None), 21, 0));
    a.feed(events).await;
    world.flushed("ring", 21).await;
    until("a full ring", || {
        status(&world, "main", PASSING, "cfgA").is_some_and(|s| s.ring.len() == 16)
    })
    .await;
    let rows = a.ok("/v1/invocations/ring/actions").await;
    let ids: Vec<String> = rows["actions"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| {
            r["attempts"][0]["run_id"]
                .as_str()
                .expect("a run id")
                .to_owned()
        })
        .collect();
    assert_eq!(ids.len(), 21);
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let ring = b
        .ok("/v1/shapes/2222222222222222/runs?instance=main&cfg=cfgA")
        .await;
    let got: Vec<&str> = ring["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .map(|r| r["run_id"].as_str().expect("a run id"))
        .collect();
    let last_16_newest_first: Vec<&str> = ids[..20]
        .iter()
        .rev()
        .take(16)
        .map(String::as_str)
        .collect();
    assert_eq!(got, last_16_newest_first);
    assert_eq!(ring["complete"], false);
    assert_eq!(ring["covers"], "recent_16");
    assert_eq!(ring["needs"], "day_index");
    assert!(ring["runs"][0]["source"] == "run" && ring["runs"][0]["execution_ms"].is_number());

    let compile = b
        .ok("/v1/shapes/3333333333333333/runs?instance=main&cfg=-")
        .await;
    assert_eq!(compile["runs"], json!([]));
    assert_eq!(compile["complete"], false);
    assert_eq!(compile["covers"], "none");
    assert_eq!(compile["needs"], "day_index");
    b.stop().await;
}

/// Catches (paging): a page of shapes that repeats or skips a row at an instance's end,
/// a token that names its place in an instance by something that breaks on a name with a
/// `:` or a `/`, a filter that leaves a short page and no way to go on, and a next link
/// that drops the filter it was made under. Five rows over two instances, one row a page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shape_pages_walk_instances_and_configurations_without_repeats() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = Vec::new();
    let mut n = 0u8;
    // Instance `c` has a shape the key walk below never meets.
    events.extend(run("p3", "c", shape(0x4444, "c1", false, None), 6, 0));
    for (id, instance) in [("p1", "a:1/x"), ("p2", "b")] {
        for (key, cfg, test) in [
            (FAILING, "c1", true),
            (FAILING, "c2", true),
            (COMPILE, "-", false),
        ] {
            if instance == "b" && cfg == "c2" {
                continue;
            }
            n += 1;
            events.extend(run(id, instance, shape(key, cfg, test, None), n, 0));
        }
    }
    a.feed(events).await;
    world.flushed("p1", 3).await;
    world.flushed("p2", 2).await;
    world.flushed("p3", 1).await;
    indexed(&world, 6, 3).await;
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let mut seen = Vec::new();
    let mut next = Some("/v1/shapes?page_size=1".to_owned());
    while let Some(link) = next {
        let page = b.ok(&link).await;
        assert!(page["shapes"].as_array().expect("a list").len() <= 1);
        for s in page["shapes"].as_array().expect("a list") {
            seen.push((
                s["instance"].as_str().expect("an instance").to_owned(),
                s["key"].as_str().expect("a key").to_owned(),
                s["cfg"].as_str().expect("a cfg").to_owned(),
            ));
        }
        next = page["next_page"].as_str().map(str::to_owned);
        assert!(seen.len() <= 6, "a page repeats: {seen:?}");
    }
    let row = |i: &str, k: u64, c: &str| (i.to_owned(), format!("{k:016x}"), c.to_owned());
    assert_eq!(
        seen,
        [
            row("a:1/x", FAILING, "c1"),
            row("a:1/x", FAILING, "c2"),
            row("a:1/x", COMPILE, "-"),
            row("b", FAILING, "c1"),
            row("b", COMPILE, "-"),
            row("c", 0x4444, "c1"),
        ]
    );
    // A filter that matches one row in three: pages keep the filter and the walk ends.
    let mut tests = 0;
    let mut next = Some("/v1/tests?page_size=1".to_owned());
    while let Some(link) = next {
        let page = b.ok(&link).await;
        tests += keys(&page).len();
        next = page["next_page"].as_str().map(str::to_owned);
        assert!(tests <= 3, "a page repeats");
    }
    assert_eq!(tests, 3);
    // The rows of one key, a page of one: both configurations, then the end.
    let first = b
        .ok("/v1/shapes/1111111111111111?instance=a%3A1%2Fx&page_size=1")
        .await;
    assert_eq!(first["shapes"][0]["cfg"], "c1");
    let second = b
        .ok(first["next_page"].as_str().expect("a next link"))
        .await;
    assert_eq!(second["shapes"][0]["cfg"], "c2");
    // A page that fills exactly where the key's rows end: the next page is empty, and
    // says it is the last.
    let all_rows = b.ok("/v1/shapes/1111111111111111?page_size=3").await;
    assert_eq!(keys(&all_rows).len(), 3);
    let last = b
        .ok(all_rows["next_page"].as_str().expect("a next link"))
        .await;
    assert_eq!(last["shapes"], json!([]));
    assert!(last.get("next_page").is_none());
    let by_status = b.ok("/v1/tests?status=passing&page_size=1").await;
    let next = by_status["next_page"].as_str().expect("a next link");
    assert!(next.contains("status=passing"), "{next}");
    let rest = b.ok(next).await;
    assert_eq!(keys(&rest).len(), 1);
    b.stop().await;
}

/// Catches: a test whose catalog row is gone (the catalog is never rewritten by a later
/// run, so a lost row is not restored by one) that has no ring and no listing: the status
/// row is the test's, and answers by itself: its ring, and its place in the status list
/// with the kind a test has and no target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_status_row_without_its_catalog_row_still_answers() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    // The writer's own rows are not needed: one status row, with its by-status entry.
    a.feed(run("only", "main", shape(PASSING, "c", true, None), 1, 0))
        .await;
    world.flushed("only", 1).await;
    let root = IndexRoot::default();
    let lone = StatusRow {
        last_outcome: TestStatus::Pass.into(),
        ring: vec![RingRun {
            run_id: vec![7; 12],
            status: TestStatus::Pass.into(),
            execution_ms: 5,
            digest16: 0,
        }],
        last_seen_ms: 9_000,
        ..StatusRow::default()
    };
    let key = 0x5555;
    let put = Txn::new()
        .put(
            root.status("main", key, "c"),
            lone.encode_to_vec(),
            Cond::Any,
        )
        .put(
            root.statusx("passing", "main", key, "c"),
            vec![1],
            Cond::Any,
        );
    world
        .state
        .fake
        .txn(put)
        .expect("a write")
        .expect("no conflict");

    let ring = a
        .ok("/v1/shapes/0000000000005555/runs?instance=main&cfg=c")
        .await;
    assert_eq!(ring["covers"], "recent_16");
    assert_eq!(ring["runs"][0]["run_id"], "07".repeat(12));
    assert!(
        ring["runs"][0].get("digest16").is_none(),
        "a run with no digest shows none"
    );
    let list = a.ok("/v1/tests?status=passing&instance=main").await;
    let lone = list["shapes"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|s| s["key"] == "0000000000005555")
        .expect("the lone status row is listed");
    assert_eq!(lone["kind"], "test");
    assert_eq!(lone["target"], Value::Null);
    assert!(
        lone.get("first_seen").is_none(),
        "no catalog row, no first sight"
    );
    assert_eq!(lone["last_seen"], "1970-01-01T00:00:09.000000Z");
    a.stop().await;
}
