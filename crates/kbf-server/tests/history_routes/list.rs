//! The invocation list, read from index rows across a restart. Each test names the defect
//! it catches.

use kbf_history::records::InvState;
use kbf_history::{IndexRoot, InvKey};
use kbf_state::{Cond, Store as _, Txn};
use serde_json::Value;

use crate::kit::header;
use crate::pages::PAGES_ONLY;
use crate::world::{World, hit_of, ran};

/// The `id`s of the invocations of a list answer, in order.
fn ids(answer: &Value) -> Vec<String> {
    answer["invocations"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|i| i["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// Catches (ui-data 6.2 `GET /v1/invocations`): a page that repeats or skips an invocation
/// at its edge, a token that does not survive a restart (the first page is read from one
/// server and the rest from the next), an invocation that used two instances listed twice
/// (it is under each instance's list), and the merge of instances' lists out of order: the
/// newest invocation is first whatever instance it is in. Seven invocations on two
/// instances, three to a page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invocation_list_pages_across_a_restart_without_repeats() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = Vec::new();
    for n in 1..=5u8 {
        events.extend(ran(&format!("i{n}"), "main", n, u64::from(n), 0));
    }
    events.extend(ran("s2", "other", 6, 6, 0));
    // One build on both instances, its first record the newest of all.
    events.extend(ran("span", "main", 7, 7, 0));
    events.extend(ran("span", "other", 8, 8, 0));
    events.push(hit_of("i1", 9));
    a.feed(events).await;
    for (id, records) in [
        ("i1", 2),
        ("i2", 1),
        ("i3", 1),
        ("i4", 1),
        ("i5", 1),
        ("s2", 1),
        ("span", 2),
    ] {
        world.flushed(id, records).await;
    }
    let first = a.ok("/v1/invocations?page_size=3").await;
    assert_eq!(ids(&first), ["span", "s2", "i5"]);
    let token = first["next_page"].as_str().expect("a next link").to_owned();
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let second = b.ok(&token).await;
    assert_eq!(ids(&second), ["i4", "i3", "i2"]);
    let third = b
        .ok(second["next_page"].as_str().expect("a next link"))
        .await;
    assert_eq!(ids(&third), ["i1"]);
    assert!(third.get("next_page").is_none(), "{third}");
    // The item: what the header says, and the link to the build's page.
    let item = &third["invocations"][0];
    // A server that stops closes the builds it holds open: they are whole.
    assert_eq!(item["state"], "closed");
    assert_eq!(item["complete"], true);
    assert_eq!(
        item["counts"],
        serde_json::json!({"ran": 1, "hits": 1, "failed": 0})
    );
    assert_eq!(item["instances"], serde_json::json!(["main"]));
    assert_eq!(item["href"], "/v1/invocations/i1");
    assert!(item["first_seen"].is_string() && item["last_seen"].is_string());
    let span = &first["invocations"][0];
    assert_eq!(span["instances"], serde_json::json!(["main", "other"]));
    // The same list for one instance: the spanning build is in it, and the other's is not.
    assert_eq!(
        ids(&b.ok("/v1/invocations?instance=main").await),
        ["span", "i5", "i4", "i3", "i2", "i1"]
    );
    assert_eq!(
        ids(&b.ok("/v1/invocations?instance=other").await),
        ["span", "s2"]
    );
    b.stop().await;
}

/// Catches: `since`, `until` and `state` that filter nothing or the wrong side, and a
/// closed invocation that reads as open (`complete` and `state` come from the header the
/// writer put when it closed the build, and an open one is never complete). The window is told from the times of the
/// invocations' own headers, so it holds whatever the test clock is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_list_filters_by_time_and_state() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    a.feed(ran("old", "main", 1, 1, 0)).await;
    a.feed(ran("new", "main", 2, 2, 0)).await;
    world.flushed("old", 1).await;
    world.flushed("new", 1).await;
    let (old, new) = (
        header(&world, "old").expect("a header").list_ms,
        header(&world, "new").expect("a header").list_ms,
    );
    assert!(old < new);
    // A server that stops closes the builds it holds open; the header of one is opened by
    // hand, as a crash leaves it, and the other stays closed.
    a.stop().await;
    let mut open = header(&world, "new").expect("a header");
    assert_eq!(open.state(), InvState::Closed);
    open.state = InvState::Open.into();
    world.set_header("new", &open);

    let b = world.start(PAGES_ONLY);
    let at = |ms: u64| {
        let secs = ms / 1000;
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
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
            day / 3_600,
            day % 3_600 / 60,
            day % 60,
            ms % 1000
        )
    };
    let route = |q: &str| format!("/v1/invocations?{q}");
    assert_eq!(
        ids(&b.ok(&route(&format!("since={}", at(new)))).await),
        ["new"]
    );
    assert_eq!(
        ids(&b.ok(&route(&format!("until={}", at(old)))).await),
        ["old"]
    );
    assert_eq!(
        ids(&b
            .ok(&route(&format!("since={}&until={}", at(old), at(new))))
            .await),
        ["new", "old"]
    );
    let closed = b.ok(&route("state=closed")).await;
    assert_eq!(ids(&closed), ["old"]);
    assert_eq!(closed["invocations"][0]["state"], "closed");
    assert_eq!(closed["invocations"][0]["complete"], true);
    let open = b.ok(&route("state=open")).await;
    assert_eq!(ids(&open), ["new"]);
    assert_eq!(open["invocations"][0]["complete"], false);
    b.stop().await;
}

/// Catches (a merge that passes what a scan has not reached): a page that lists an older
/// build of one instance while another instance's scan stopped at its bound short of a
/// newer one, so the newer build comes on a later page: the list is no longer newest
/// first, and a client that stops at the first page has missed it. The instance `heavy` has
/// more rows newer than its one real build than a page of the list scans (the rows name
/// builds with no header, as a build forgotten leaves); `main` has an older real build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_does_not_pass_what_a_scan_has_not_reached() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = ran("early", "main", 1, 1, 0);
    events.extend(ran("late", "heavy", 2, 2, 0));
    a.feed(events).await;
    world.flushed("early", 1).await;
    world.flushed("late", 1).await;
    let late = header(&world, "late").expect("a header").list_ms;
    let root = IndexRoot::default();
    let ghost = InvKey::from_record_id("ghost");
    let rows = kbf_history::MAX_SCAN as u64 + 5;
    for from in (0..rows).step_by(1_000) {
        let mut puts = Txn::new();
        for t in from..(from + 1_000).min(rows) {
            puts = puts.put(
                root.inv_list("heavy", late + 1 + t, &ghost),
                vec![1],
                Cond::Any,
            );
        }
        world
            .state
            .fake
            .txn(puts)
            .expect("a write")
            .expect("no conflict");
    }
    let first = a.ok("/v1/invocations").await;
    assert_eq!(
        ids(&first),
        Vec::<String>::new(),
        "nothing is passed unscanned: {first}"
    );
    let mut seen = Vec::new();
    let mut next = first["next_page"].as_str().map(str::to_owned);
    while let Some(link) = next {
        let page = a.ok(&link).await;
        seen.extend(ids(&page));
        next = page["next_page"].as_str().map(str::to_owned);
        assert!(seen.len() <= 2, "a page repeats: {seen:?}");
    }
    assert_eq!(seen, ["late", "early"]);
    a.stop().await;
}
