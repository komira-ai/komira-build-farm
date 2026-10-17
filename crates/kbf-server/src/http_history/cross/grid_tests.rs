//! The cells of a grid. Each test names the defect it catches.

use std::collections::BTreeSet;

use kbf_history::records::{MemoryKill, RunOutcome, RunRecord};
use kbf_history::{RunId, StreamId};
use serde_json::{Value, json};

use super::{Column, cells, grid, ids, word};
use kbf_history::records::TestStatus;

fn run(
    instance: &str,
    shape: u64,
    cfg: &str,
    time_ms: u64,
    seq: u32,
    outcome: RunOutcome,
) -> RunRecord {
    let mut r = RunRecord {
        instance: instance.into(),
        shape,
        cfg: cfg.into(),
        outcome: outcome.into(),
        ..RunRecord::default()
    };
    r.set_run_id(RunId::new(time_ms, StreamId(1), seq));
    r
}

fn status_of(runs: &[RunRecord], shape: u64) -> Vec<(String, String, &'static str)> {
    let wanted = BTreeSet::from([shape]);
    cells(runs, &wanted)
        .into_iter()
        .map(|((instance, _, cfg), cell)| (instance, cfg, cell.status))
        .collect()
}

/// Catches: a cell told from the first run of a row, or from the run with the larger
/// sequence when another ended later; runs of other shapes in a row; a cancelled run, or a
/// kill another attempt followed, shown as a result; and instances or configurations of one
/// shape merged into one row.
#[test]
fn a_cell_is_the_last_result_of_its_row() {
    let ok = RunOutcome::Ok;
    let failed = RunOutcome::Failed;
    let runs = [
        run("main", 1, "c", 100, 9, failed),
        run("main", 1, "c", 200, 1, ok),
        // The same time, a larger id: it is the later.
        run("main", 1, "c", 200, 2, failed),
        // Not results: they do not replace what is there.
        run("main", 1, "c", 300, 1, RunOutcome::Cancelled),
        run("main", 1, "c", 300, 2, RunOutcome::Unspecified),
        // Other instance, other configuration, other shape.
        run("other", 1, "c", 50, 1, ok),
        run("main", 1, "d", 50, 1, ok),
        run("main", 2, "c", 50, 1, ok),
    ];
    assert_eq!(
        status_of(&runs, 1),
        [
            ("main".to_owned(), "c".to_owned(), "fail"),
            ("main".to_owned(), "d".to_owned(), "pass"),
            ("other".to_owned(), "c".to_owned(), "pass"),
        ]
    );
    // A run that is older than the one held does not replace it, whatever the order.
    let reversed = [
        run("main", 1, "c", 200, 2, failed),
        run("main", 1, "c", 100, 9, ok),
    ];
    assert_eq!(status_of(&reversed, 1)[0].2, "fail");
    // A kill at the action's own limit is a result unless a rung followed it.
    let mut killed = run("main", 1, "c", 10, 1, RunOutcome::MemoryKilled);
    killed.memory_kill = MemoryKill::OwnLimit.into();
    assert_eq!(status_of(&[killed.clone()], 1)[0].2, "oom_killed");
    killed.raised_memory_bytes = 1;
    assert!(status_of(&[killed.clone()], 1).is_empty());
    killed.raised_memory_bytes = 0;
    killed.memory_kill = MemoryKill::NodePressure.into();
    assert_eq!(status_of(&[killed], 1)[0].2, "infra");
    assert_eq!(
        status_of(&[run("main", 1, "c", 1, 1, RunOutcome::Lost)], 1)[0].2,
        "infra"
    );
    assert_eq!(
        status_of(&[run("main", 1, "c", 1, 1, RunOutcome::TimedOut)], 1)[0].2,
        "fail"
    );
}

/// Catches: a test status named as another in the grid.
#[test]
fn a_status_has_the_grids_word() {
    for (status, name) in [
        (TestStatus::Pass, "pass"),
        (TestStatus::Fail, "fail"),
        (TestStatus::Timeout, "fail"),
        (TestStatus::OomKilled, "oom_killed"),
        (TestStatus::Infra, "infra"),
        (TestStatus::Unspecified, "infra"),
    ] {
        assert_eq!(word(status), name);
    }
}

/// Catches: a build without a row's shape given a cell of another build's, rows out of
/// order, a column's `complete` lost, and a grid with no runs that is not an empty one.
#[test]
fn a_grid_has_a_cell_for_every_build_and_marks_a_missing_run_absent() {
    let shapes = BTreeSet::from([1, 2]);
    let column = |id: &str, runs: Vec<RunRecord>, complete| Column {
        id: id.to_owned(),
        first_seen_ms: 1_000,
        complete,
        runs,
    };
    let columns = [
        column("a", vec![run("main", 2, "c", 1, 1, RunOutcome::Ok)], true),
        column(
            "b",
            vec![run("main", 1, "c", 1, 1, RunOutcome::Failed)],
            false,
        ),
        column("c", vec![], true),
    ];
    let g = grid(&columns, &shapes);
    assert_eq!(g["shapes"], json!(["0000000000000001", "0000000000000002"]));
    let ids: Vec<&Value> = g["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["id"])
        .collect();
    assert_eq!(ids, ["a", "b", "c"]);
    assert_eq!(g["columns"][1]["complete"], false);
    assert_eq!(g["columns"][0]["href"], "/v1/invocations/a");
    let rows = g["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["key"], "0000000000000001");
    assert_eq!(
        rows[0]["href"],
        "/v1/shapes/0000000000000001?instance=main&cfg=c"
    );
    let statuses = |row: &Value| -> Vec<String> {
        row["cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["status"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(statuses(&rows[0]), ["absent", "fail", "absent"]);
    assert_eq!(statuses(&rows[1]), ["pass", "absent", "absent"]);
    assert!(rows[0]["cells"][0].get("run_id").is_none());
    assert_eq!(
        rows[1]["cells"][0]["run_href"]
            .as_str()
            .map(|h| h.starts_with("/v1/runs/")),
        Some(true)
    );
    let none = grid(&[column("a", vec![], true)], &shapes);
    assert_eq!(none["rows"], json!([]));
}

/// Catches: a list of ids with an empty id, a repeated id (two columns of one build), or
/// more builds than a grid has.
#[test]
fn the_builds_of_a_grid_are_different_nonempty_ids() {
    assert_eq!(ids("a,b").ok(), Some(vec!["a".to_owned(), "b".to_owned()]));
    assert!(ids("a,a").is_err());
    assert!(ids("a,,b").is_err());
    assert!(ids("").is_err());
    let many: Vec<String> = (0..51).map(|i| format!("i{i}")).collect();
    assert!(ids(&many.join(",")).is_err());
    assert!(ids(&many[..50].join(",")).is_ok());
}
