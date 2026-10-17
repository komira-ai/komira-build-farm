//! The grid of shapes against builds, read from the builds' chunks. Each test names the
//! defect it catches.

use serde_json::{Value, json};

use crate::kit::{header, run, shape};
use crate::pages::PAGES_ONLY;
use crate::world::World;

const X: u64 = 0x1111_1111_1111_1111;
const Y: u64 = 0x2222_2222_2222_2222;
const Z: u64 = 0x3333_3333_3333_3333;

/// The status of each cell of the row for `key`, in column order.
fn row(grid: &Value, key: &str) -> Vec<String> {
    let rows = grid["rows"].as_array().expect("rows");
    let row = rows
        .iter()
        .find(|r| r["key"] == key)
        .unwrap_or_else(|| panic!("no row {key}: {grid}"));
    row["cells"]
        .as_array()
        .expect("cells")
        .iter()
        .map(|c| c["status"].as_str().expect("a status").to_owned())
        .collect()
}

/// Catches (mutant: a build without the shape given another build's cell): the grid of
/// three builds after a restart. `g1` passed X and then failed it (the later run is the
/// cell), failed Y; `g2` failed X and has no Y; `g3` ran only Z. A cell is `absent` where
/// a build has no run of the row, a row is only made for what some build ran, and the
/// cells link to the runs they name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_grid_marks_an_invocation_without_the_shape_absent() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = run("g1", "main", shape(X, "c", true, None), 1, 0);
    events.extend(run("g1", "main", shape(X, "c", true, None), 5, 2));
    events.extend(run("g1", "main", shape(Y, "c", true, None), 2, 2));
    events.extend(run("g2", "main", shape(X, "c", true, None), 3, 2));
    events.extend(run("g3", "main", shape(Z, "c", true, None), 4, 0));
    a.feed(events).await;
    world.flushed("g1", 3).await;
    world.flushed("g2", 1).await;
    world.flushed("g3", 1).await;
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let grid = b
        .ok(&format!(
            "/v1/grid?shapes={X:016x},{Y:016x}&invocations=g1,g2,g3"
        ))
        .await;
    assert_eq!(
        grid["columns"]
            .as_array()
            .expect("columns")
            .iter()
            .map(|c| c["id"].as_str().expect("an id"))
            .collect::<Vec<_>>(),
        ["g1", "g2", "g3"]
    );
    assert_eq!(grid["columns"][0]["href"], "/v1/invocations/g1");
    assert_eq!(grid["columns"][0]["complete"], true);
    assert!(grid["columns"][0]["first_seen"].is_string());
    assert_eq!(row(&grid, "1111111111111111"), ["fail", "fail", "absent"]);
    assert_eq!(row(&grid, "2222222222222222"), ["fail", "absent", "absent"]);
    assert_eq!(
        grid["rows"].as_array().map(Vec::len),
        Some(2),
        "no row for Z: {grid}"
    );
    let x = &grid["rows"][0];
    assert_eq!(
        (x["instance"].clone(), x["cfg"].clone()),
        (json!("main"), json!("c"))
    );
    assert_eq!(x["href"], "/v1/shapes/1111111111111111?instance=main&cfg=c");
    assert_eq!(
        grid["shapes"],
        json!(["1111111111111111", "2222222222222222"])
    );
    // The cell names the run, which the run route resolves.
    let cell = &x["cells"][0];
    let resolved = b.ok(cell["run_href"].as_str().expect("a link")).await;
    assert_eq!(resolved["run_id"], cell["run_id"]);
    assert_eq!(resolved["class"], "failed");
    assert!(x["cells"][2].get("run_id").is_none());

    // The same builds by `last=`: the newest first.
    let last = b.ok(&format!("/v1/grid?shapes={X:016x}&last=2")).await;
    let ids: Vec<&str> = last["columns"]
        .as_array()
        .expect("columns")
        .iter()
        .map(|c| c["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids, ["g3", "g2"]);
    assert_eq!(row(&last, "1111111111111111"), ["absent", "fail"]);
    // One shape the builds never ran: no rows, and still a grid.
    let none = b
        .ok(&format!("/v1/grid?shapes={Z:016x}&invocations=g1"))
        .await;
    assert_eq!(none["rows"], json!([]));
    b.stop().await;
}

/// Catches: a passing run shown as failed, and the pass of an earlier build shown in a
/// later one: the cells of two builds of one test that passes and fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_grid_tells_a_pass_from_a_failure_build_by_build() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let mut events = run("p1", "main", shape(X, "c", true, None), 1, 0);
    events.extend(run("p2", "main", shape(X, "c", true, None), 2, 1));
    a.feed(events).await;
    world.flushed("p1", 1).await;
    world.flushed("p2", 1).await;
    let grid = a
        .ok(&format!(
            "/v1/grid?shapes={X:016x}&invocations=p1,p2&columns=invocation"
        ))
        .await;
    assert_eq!(row(&grid, "1111111111111111"), ["pass", "fail"]);
    a.stop().await;
}

/// Catches: a column read from a header that is not for the build asked for (another
/// build's header row under this key's hash), and one for a header with no chunk (a
/// crash before the first was written): each is the answer for a build that is not held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_header_that_is_not_the_builds_or_has_no_chunk_is_no_record() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    a.feed(run("held", "main", shape(X, "c", true, None), 1, 0))
        .await;
    world.flushed("held", 1).await;
    a.stop().await;
    let mut other = header(&world, "held").expect("a header");
    other.record_id = "someone-else".to_owned();
    world.set_header("held", &other);
    let mut bare = other.clone();
    bare.record_id = "bare".to_owned();
    world.set_header("bare", &bare);

    let b = world.start(PAGES_ONLY);
    for id in ["held", "bare"] {
        let answer = b
            .get(&format!("/v1/grid?shapes={X:016x}&invocations={id}"))
            .await;
        assert_eq!(answer.code, 404, "{id}: {}", answer.json);
        assert_eq!(answer.json["error"], "no_record", "{id}");
    }
    b.stop().await;
}
