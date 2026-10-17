//! The runs of today by action digest and by node, from the writer's index in memory.
//! Each test names the defect it catches.

use serde_json::Value;

use crate::kit::run;
use crate::kit::shape;
use crate::pages::{PAGES_ONLY, digest, text};
use crate::world::{World, ran, ran_action};

/// The `run_id`s of an answer, in order.
fn run_ids(answer: &Value) -> Vec<String> {
    answer["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .map(|r| r["run_id"].as_str().expect("a run id").to_owned())
        .collect()
}

/// Catches (ui-data 5.5: a restart empties the in-memory index): an answer after a restart
/// that lists a run from before it (the writer's memory is the whole index, so a build
/// that ran before is not there) or one that does not say so: `partial` is true and
/// `covers_from` is when the index began. The action ran in `d1` before the restart and in
/// `d2` after; before the restart the first server lists `d1`'s run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_runs_after_a_restart_say_partial_and_covers_from() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    a.feed(ran("d1", "main", 1, 1, 0)).await;
    world.flushed("d1", 1).await;
    let route = format!("/v1/actions/{}/runs", text(&digest(1)));
    let before = a.ok(&route).await;
    let first = run_ids(&before);
    assert_eq!(first.len(), 1, "{before}");
    assert_eq!(before["partial"], true);
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let empty = b.ok(&route).await;
    assert_eq!(
        run_ids(&empty),
        Vec::<String>::new(),
        "the run before the restart is not there"
    );
    assert_eq!(empty["partial"], true);
    b.feed(ran_action("d2", "main", 1, 2, 2, 0)).await;
    world.flushed("d2", 1).await;
    let after = b.ok(&route).await;
    let second = run_ids(&after);
    assert_eq!(second.len(), 1, "{after}");
    assert_ne!(first, second);
    assert_eq!(after["partial"], true);
    assert_eq!(after["digest"], text(&digest(1)));
    let covers = after["covers_from"].as_str().expect("a time");
    assert!(covers.ends_with('Z'));
    let item = &after["runs"][0];
    assert_eq!(item["instance"], "main");
    assert_eq!(item["worker"], "node-a");
    assert_eq!(item["class"], "ok");
    assert_eq!(item["shape"], Value::Null);
    assert_eq!(item["run_href"], format!("/v1/runs/{}", second[0]));
    assert!(
        covers < item["started"].as_str().expect("a time"),
        "{covers} {item}"
    );
    // The run link resolves.
    assert_eq!(
        b.ok(item["run_href"].as_str().unwrap()).await["run_id"],
        second[0]
    );
    b.stop().await;
}

/// Catches: runs on a node listed in arrival order or with a window that misses the runs
/// that overlap it; a class or instance filter ignored; a page that repeats or skips a run;
/// a run of another node; and the shape of a run left out of its item. Five runs on `node-a`
/// (`ran` puts every run there) at 1 ms apart in the year 2030, newest first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_runs_filter_by_window_class_and_instance_and_page_newest_first() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = Vec::new();
    for n in 1..=5u8 {
        let instance = if n % 2 == 0 { "main" } else { "other" };
        let exit = i32::from(n == 3);
        events.extend(run(
            &format!("n{n}"),
            instance,
            shape(0xab, "c", true, None),
            n,
            exit,
        ));
    }
    a.feed(events).await;
    for n in 1..=5 {
        world.flushed(&format!("n{n}"), 1).await;
    }
    let all = a.ok("/v1/runs?node=node-a").await;
    let ids = run_ids(&all);
    assert_eq!(ids.len(), 5);
    assert_eq!(all["partial"], true);
    // Newest first: the run ids order by time.
    let mut sorted = ids.clone();
    sorted.sort_by(|x, y| y.cmp(x));
    assert_eq!(ids, sorted);
    assert_eq!(
        all["runs"][0]["shape"],
        serde_json::json!({"key": "00000000000000ab", "cfg": "c"})
    );
    assert!(
        a.ok("/v1/runs?node=nowhere").await["runs"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Filters.
    assert_eq!(
        run_ids(&a.ok("/v1/runs?node=node-a&instance=main").await).len(),
        2
    );
    let failed = a.ok("/v1/runs?node=node-a&class=failed").await;
    assert_eq!(run_ids(&failed), [ids[2].clone()], "the third run failed");
    assert_eq!(
        run_ids(&a.ok("/v1/runs?node=node-a&class=ok").await).len(),
        4
    );
    // The window: runs run from 2030-01-01T00:00:00.00n, each a few microseconds long.
    let from = |ms: u32| format!("2030-01-01T00:00:00.00{ms}Z");
    let window = a
        .ok(&format!(
            "/v1/runs?node=node-a&from={}&to={}",
            from(2),
            from(3)
        ))
        .await;
    assert_eq!(run_ids(&window), [ids[2].clone(), ids[3].clone()]);
    let after = a.ok("/v1/runs?node=node-a&from=2031-01-01T00:00:00Z").await;
    assert!(run_ids(&after).is_empty());
    let before = a.ok("/v1/runs?node=node-a&to=2029-01-01T00:00:00Z").await;
    assert!(run_ids(&before).is_empty());

    // Pages of two: every run once, newest first.
    let mut got = Vec::new();
    let mut next = Some("/v1/runs?node=node-a&page_size=2".to_owned());
    while let Some(link) = next {
        let page = a.ok(&link).await;
        assert!(page["runs"].as_array().unwrap().len() <= 2);
        got.extend(run_ids(&page));
        next = page["next_page"].as_str().map(str::to_owned);
        assert!(got.len() <= 5, "a page repeats");
    }
    assert_eq!(got, ids);
    a.stop().await;
}
