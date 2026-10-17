//! What goes wrong when history is read back (`read.rs` is what goes right): a
//! store that fails, a caller the policy refuses, rows that are not the ones a record was
//! written with, and rows that change between the two phases of an authorized read. Each
//! test names the defect it catches.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::pages::PAGES_ONLY;
use crate::world::{ONLY_A, STICKY, World, key, ran, set_header};
use kbf_history::records::InvocationHeader;
use kbf_history::{IndexRoot, InvKey};
use kbf_state::Store as _;

/// Catches: an object store or state store that fails answered as a record that is not
/// there (`404`, which a client takes for "your build was never recorded"), the cause
/// of the failure sent to the caller, and a failed read that is cached or fails the
/// server. Each read that touches history answers `503` while it cannot be read, and a
/// read after it works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_history_that_cannot_be_read_is_503_and_names_no_cause() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    a.feed(ran("down", "main", 1, 1, 0)).await;
    world.flushed("down", 1).await;
    a.recorder.caught_up().await;
    let run = hex::encode(
        a.recorder.get(&key("down")).expect("a record").rows()[0].attempts[0]
            .run_id
            .expect("an id"),
    );
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let routes = ["/v1/invocations/down".to_owned(), format!("/v1/runs/{run}")];
    // The index does not answer a read of the header.
    world.state.fail_gets.store(STICKY, Ordering::SeqCst);
    let answer = b.get(&routes[0]).await;
    world.state.fail_gets.store(0, Ordering::SeqCst);
    assert_eq!(answer.code, 503, "{}", answer.json);
    assert_eq!(
        answer.json["error"],
        "build history cannot be read right now"
    );
    // The object store does not answer: the summary chunk, the records and the run block.
    world.objects.fail_gets(true);
    for route in &routes {
        let answer = b.get(route).await;
        assert_eq!(answer.code, 503, "{route}: {}", answer.json);
        let text = answer.json.to_string();
        assert!(
            !text.contains("planted"),
            "{route}: the cause is logged, not sent: {text}"
        );
    }
    world.objects.fail_gets(false);
    assert_eq!(b.ok(&routes[0]).await["restored"], true);
    assert_eq!(b.ok(&routes[1]).await["class"], "ok");
    b.stop().await;
}

/// Catches: a caller the policy does not accept let in to a run (`401`, as to every other
/// route), and the run route answering when the policy turns the build pages off: `404`
/// with the `off` body of the pages, and never `no_run`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_run_route_is_authenticated_and_off_with_the_pages() {
    let world = World::new();
    let id = "0a0b0c0d0e0f0a0b0c0d0e0f";
    let denied = world.start(
        r#"{"authenticationPolicy": {"deny": "who are you"},
            "invocations": {"getAuthorizer": {"allow": {}}}}"#,
    );
    let answer = denied.get(&format!("/v1/runs/{id}")).await;
    assert_eq!(answer.code, 401, "{}", answer.json);
    denied.stop().await;
    let off = world.start(r#"{"authenticationPolicy": {"allow": {}}}"#);
    let answer = off.get(&format!("/v1/runs/{id}")).await;
    assert_eq!(answer.code, 404, "{}", answer.json);
    assert_eq!(answer.json["off"], "invocations");
    off.stop().await;
}

/// Catches: a header under the hash of another invocation's id (a collision, or a row
/// planted) read as this one's records, and a header whose chunks are gone from the
/// index (only a close with nothing to write leaves one) answered as an empty record:
/// both are no record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_header_of_another_id_or_with_no_chunk_is_no_record() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = ran("swapped", "main", 1, 1, 0);
    events.extend(ran("hollow", "main", 2, 2, 0));
    events.extend(ran("fine", "main", 3, 3, 0));
    a.feed(events).await;
    for id in ["swapped", "hollow", "fine"] {
        world.flushed(id, 1).await;
    }
    a.stop().await;
    let other = InvocationHeader {
        record_id: "someone-else".to_owned(),
        ..world.header("swapped")
    };
    world.set_header("swapped", &other);
    let prefix = IndexRoot::default().ichunk_prefix(&InvKey::from_record_id("hollow"));
    for row in world.rows(&prefix) {
        let delete = kbf_state::Txn::new().delete(row, kbf_state::Cond::Any);
        world
            .state
            .fake
            .txn(delete)
            .expect("a write")
            .expect("no conflict");
    }

    let b = world.start(PAGES_ONLY);
    for id in ["swapped", "hollow"] {
        let answer = b.get(&format!("/v1/invocations/{id}")).await;
        assert_eq!(answer.code, 404, "{id}: {}", answer.json);
        assert_eq!(answer.json["error"], "no_record", "{id}");
    }
    assert_eq!(b.ok("/v1/invocations/fine").await["actions"], 1);
    b.stop().await;
}

/// Catches (mutant "the records are not checked against the names"): an instance added
/// after the header was read for the summary and before the records were, served to a
/// caller who was never asked about it. The records are read under the same check as the
/// summary, and the request answers `503`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_instance_added_between_the_header_and_the_records_is_503_changed() {
    let world = World::new();
    let a = world.start(ONLY_A);
    a.feed(ran("late", "a/main", 1, 1, 0)).await;
    world.flushed("late", 1).await;
    a.stop().await;
    let b = world.start(ONLY_A);

    let grown = InvocationHeader {
        instances: vec!["a/main".to_owned(), "b".to_owned()],
        ..world.header("late")
    };
    // After the names are read, wait for the summary's read of the header, and add the
    // instance after that.
    let first = Arc::clone(&world.state);
    world.state.after_get("ui/invid/", move || {
        let second = Arc::clone(&first);
        first.after_get("ui/invid/", move || set_header(&second, "late", &grown));
    });
    let answer = b.get("/v1/invocations/late").await;
    assert_eq!(answer.code, 503, "{}", answer.json);
    let why = answer.json["error"].as_str().unwrap_or_default();
    assert!(why.contains("gained an instance name"), "{why}");
    b.stop().await;
}

/// Catches: a whole-chunk read that fails (the summary read, which asks for the footer
/// only, worked) answered as a record with no rows, or with the cause sent: `503`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chunk_that_cannot_be_read_whole_is_503() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    a.feed(ran("chunky", "main", 1, 1, 0)).await;
    world.flushed("chunky", 1).await;
    a.stop().await;
    let b = world.start(PAGES_ONLY);
    world.objects.fail_chunk_reads(true);
    let answer = b.get("/v1/invocations/chunky").await;
    assert_eq!(answer.code, 503, "{}", answer.json);
    assert!(!answer.json.to_string().contains("planted"));
    world.objects.fail_chunk_reads(false);
    assert_eq!(b.ok("/v1/invocations/chunky").await["actions"], 1);
    b.stop().await;
}

/// Catches: a header that lags the chunk it was written with (it lists fewer instances
/// than the newest chunk's summary) served to a caller who was asked about the header's
/// names only: the summary names `b` as well, so the invocation is not held for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_header_that_lags_its_newest_chunk_is_not_held() {
    let world = World::new();
    let a = world.start(ONLY_A);
    let mut events = ran("lag", "a/main", 1, 1, 0);
    events.extend(ran("lag", "b", 2, 2, 0));
    a.feed(events).await;
    world.flushed("lag", 2).await;
    a.stop().await;
    let lagging = InvocationHeader {
        instances: vec!["a/main".to_owned()],
        ..world.header("lag")
    };
    world.set_header("lag", &lagging);

    let b = world.start(ONLY_A);
    let answer = b.get("/v1/invocations/lag").await;
    assert_eq!(answer.code, 404, "{}", answer.json);
    assert_eq!(answer.json["error"], "no_record");
    b.stop().await;
}

/// Catches: an index that fails while the reason a read stopped answering is looked up
/// answered as "gone" or "changed" (a client that retries a thing that cannot work, or
/// gives up on one that can): `503`, as any failed read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_index_that_fails_after_a_change_is_503_not_changed() {
    let world = World::new();
    let a = world.start(ONLY_A);
    a.feed(ran("flaky", "a/main", 1, 1, 0)).await;
    world.flushed("flaky", 1).await;
    a.stop().await;
    let b = world.start(ONLY_A);
    let grown = InvocationHeader {
        instances: vec!["a/main".to_owned(), "b".to_owned()],
        ..world.header("flaky")
    };
    // After the names are read the header grows, and after the summary's read of it the
    // index stops answering: the look-up of why the summary was refused fails.
    let first = Arc::clone(&world.state);
    world.state.after_get("ui/invid/", move || {
        set_header(&first, "flaky", &grown);
        let second = Arc::clone(&first);
        first.after_get("ui/invid/", move || {
            second.fail_gets.store(STICKY, Ordering::SeqCst);
        });
    });
    let answer = b.get("/v1/invocations/flaky").await;
    world.state.fail_gets.store(0, Ordering::SeqCst);
    assert_eq!(answer.code, 503, "{}", answer.json);
    assert_eq!(
        answer.json["error"],
        "build history cannot be read right now"
    );
    b.stop().await;
}
