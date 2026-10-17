//! Today's runs in memory: bounded, newest first, scoped by instance, fed by the writer.
//! Each test names the defect it catches.

use kbf_history::records::{RunOutcome, RunRecord};
use kbf_history::{InvRef, Limits, MAX_EXAMINED, RunId, StreamId, Today, TodayQuery, TodayRun};

use crate::support::{Life, T, all, run_record};

/// A run `n` that ended at `ended` on `node`, of the action whose digest is `n`.
fn run(n: u8, instance: &str, node: &str, ended: u64) -> TodayRun {
    TodayRun {
        instance: instance.into(),
        run_id: RunId::new(ended, StreamId(1), u32::from(n)),
        shape: 0,
        cfg: "-".into(),
        outcome: RunOutcome::Ok,
        node: node.into(),
        started_ms: ended - 5,
        ended_ms: ended,
        action: Some((vec![n; 32], u64::from(n))),
    }
}

fn action(n: u8) -> (Vec<u8>, u64) {
    (vec![n; 32], u64::from(n))
}

fn ids(runs: &[TodayRun]) -> Vec<u32> {
    runs.iter().map(|r| r.run_id.seq()).collect()
}

#[test]
fn today_index_is_bounded_and_evicts_oldest() {
    // Catches (mutant "a today index that never evicts"): an index that grows with the
    // day (it holds every run, and the process the memory); an eviction that forgets the
    // newest instead of the oldest; one that leaves the forgotten run in an index (a
    // digest, a node or an instance still naming it); a bound counted in entries, not
    // bytes; and a start of coverage that does not move when runs are forgotten (an answer
    // claiming a completeness it no longer has).
    let probe = Today::new(usize::MAX);
    probe.push(run(1, "main", "a", 1_000));
    let one = probe.bytes();
    let today = Today::new(3 * one + one / 2);
    today.begin(100);
    assert_eq!(today.covers_from_ms(), Some(100));
    for n in 1..=10u8 {
        let instance = if n <= 2 { "other" } else { "main" };
        let node = if n % 2 == 1 { "a" } else { "b" };
        today.push(run(n, instance, node, 1_000 + u64::from(n)));
    }
    assert_eq!(today.len(), 3);
    assert!(
        today.bytes() <= 3 * one + one / 2,
        "{} > {}",
        today.bytes(),
        3 * one
    );
    let q = TodayQuery::newest(100);
    for n in 1..=7 {
        assert!(
            today.of_action(&action(n), &q, &all).runs.is_empty(),
            "run {n} was forgotten"
        );
    }
    for n in 8..=10 {
        assert_eq!(
            ids(&today.of_action(&action(n), &q, &all).runs),
            [u32::from(n)]
        );
    }
    assert_eq!(ids(&today.on_node("a", &q, &all).runs), [9]);
    assert_eq!(
        ids(&today.on_node("b", &q, &all).runs),
        [10, 8],
        "newest first"
    );
    assert_eq!(
        today.instances(),
        ["main"],
        "an instance with no run left is gone"
    );
    // Coverage starts at the oldest run kept, which is after the writer's start.
    assert_eq!(today.covers_from_ms(), Some(1_008));
    assert_eq!(
        today.on_node("nobody", &q, &all).covers_from_ms,
        Some(1_008)
    );
    // One more run: still three, still the newest three.
    today.push(run(11, "main", "a", 1_011));
    assert_eq!(today.len(), 3);
    assert_eq!(ids(&today.on_node("a", &q, &all).runs), [11, 9]);
    assert_eq!(today.covers_from_ms(), Some(1_009));
    // A run larger than the whole bound is not kept, and nothing else is either.
    let tiny = Today::new(one / 2);
    tiny.push(run(1, "main", "a", 1_000));
    assert!(tiny.is_empty());
    // A run with no action digest is forgotten from the node and the instance.
    let mut unnamed = run(2, "main", "a", 1_001);
    unnamed.action = None;
    tiny.push(unnamed);
    assert!(tiny.is_empty());
    assert!(tiny.instances().is_empty());
    assert!(
        tiny.on_node("a", &TodayQuery::newest(5), &all)
            .runs
            .is_empty()
    );
    assert_eq!(tiny.bytes(), 0);
    assert_eq!(
        tiny.covers_from_ms(),
        Some(1_000),
        "the writer was sent a run at 1,000"
    );
}

#[test]
fn a_search_is_newest_first_filtered_scoped_and_paged() {
    // Catches: results in arrival order, a filter that matches the wrong side of the
    // window (a run that ended before it or started after it listed), an instance, class
    // or allow filter applied to the page instead of the search (a short page, a cursor
    // that skips rows), a cursor that repeats the row it names, and a run of no action
    // (it has an instance and a node, and is in no digest's list).
    let today = Today::new(usize::MAX);
    today.begin(0);
    // Runs 1 to 6 of one action on node `a`, ended 100 apart, started 50 before.
    for n in 1..=6u8 {
        let mut r = run(
            1,
            if n % 2 == 0 { "main" } else { "other" },
            "a",
            1_000 + 100 * u64::from(n),
        );
        r.run_id = RunId::new(r.ended_ms, StreamId(1), u32::from(n));
        r.started_ms = r.ended_ms - 50;
        r.outcome = if n == 3 {
            RunOutcome::Failed
        } else {
            RunOutcome::Ok
        };
        today.push(r);
    }
    let mut none = run(7, "main", "a", 2_000);
    none.action = None;
    today.push(none);

    let q = TodayQuery::newest(100);
    assert_eq!(
        ids(&today.on_node("a", &q, &all).runs),
        [7, 6, 5, 4, 3, 2, 1]
    );
    assert_eq!(
        ids(&today.of_action(&action(1), &q, &all).runs),
        [6, 5, 4, 3, 2, 1]
    );
    // The window: the run that ended at 1,300 (started 1,250) overlaps [1_250, 1_260] and
    // [1_300, 1_310], not [1_301, 1_349] (which falls between it and the next run's
    // start at 1,350).
    let window = |from_ms, to_ms| TodayQuery {
        from_ms,
        to_ms,
        ..q
    };
    assert_eq!(
        ids(&today.on_node("a", &window(1_250, 1_260), &all).runs),
        [3]
    );
    assert_eq!(
        ids(&today.on_node("a", &window(1_300, 1_310), &all).runs),
        [3]
    );
    assert!(
        today
            .on_node("a", &window(1_301, 1_349), &all)
            .runs
            .is_empty()
    );
    assert_eq!(
        ids(&today.on_node("a", &window(1_301, 1_350), &all).runs),
        [4]
    );
    // Instance, class and the caller's allow.
    let main = TodayQuery {
        instance: Some("main"),
        ..q
    };
    assert_eq!(ids(&today.on_node("a", &main, &all).runs), [7, 6, 4, 2]);
    let failed = TodayQuery {
        class: Some(RunOutcome::Failed),
        ..q
    };
    assert_eq!(ids(&today.on_node("a", &failed, &all).runs), [3]);
    let only_other = |i: &str| i == "other";
    assert_eq!(ids(&today.on_node("a", &q, &only_other).runs), [5, 3, 1]);
    assert!(today.on_node("a", &main, &only_other).runs.is_empty());
    // Pages of two, newest first, no row twice, the last page says it is the last.
    let mut got = Vec::new();
    let mut after = None;
    loop {
        let page = today.on_node(
            "a",
            &TodayQuery {
                limit: 2,
                after,
                ..q
            },
            &all,
        );
        assert!(page.runs.len() <= 2);
        got.extend(ids(&page.runs));
        assert!(got.len() <= 7, "a page repeats");
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(got, [7, 6, 5, 4, 3, 2, 1]);
    // Unknown keys have nothing, and still say what the index covers.
    let nothing = today.of_action(&action(99), &q, &all);
    assert!(nothing.runs.is_empty() && nothing.next.is_none());
    assert_eq!(nothing.covers_from_ms, Some(0));
    assert!(today.on_node("nobody", &q, &all).runs.is_empty());
    // Before the writer has stepped or been sent a run, there is nothing to cover.
    assert_eq!(Today::new(1).covers_from_ms(), None);
}

#[test]
fn a_search_stops_after_the_entries_it_may_examine_and_says_where() {
    // Catches: a filter that matches nothing making one request walk the whole index
    // under its lock with no way to go on (no cursor), and a cursor that skips the entry
    // the search stopped before.
    let today = Today::new(usize::MAX);
    let total = MAX_EXAMINED + 100;
    for n in 0..total {
        let mut r = run(1, "main", "a", 1_000 + n as u64);
        r.run_id = RunId::new(r.ended_ms, StreamId(1), u32::try_from(n).unwrap());
        // The oldest entry is the one that matches, and it is past the bound.
        r.outcome = if n == 0 {
            RunOutcome::Failed
        } else {
            RunOutcome::Ok
        };
        today.push(r);
    }
    let q = TodayQuery {
        class: Some(RunOutcome::Failed),
        ..TodayQuery::newest(5)
    };
    let first = today.on_node("a", &q, &all);
    assert!(first.runs.is_empty(), "the match is past the bound");
    let next = first.next.expect("a cursor to go on from");
    let second = today.on_node(
        "a",
        &TodayQuery {
            after: Some(next),
            ..q
        },
        &all,
    );
    assert_eq!(ids(&second.runs), [0]);
    assert_eq!(second.next, None);
}

#[test]
fn a_run_record_becomes_an_entry_with_its_times_and_action() {
    // Catches: a start time read in the wrong unit, a run with no end time (it has the
    // time of its id) given a start in the past, and a run with no action in a digest's
    // list.
    let mut r = run_record("main", RunOutcome::Failed, 5);
    r.set_run_id(RunId::new(7_000, StreamId(2), 3));
    r.shape = 9;
    r.cfg = "cfg".into();
    r.started_us = 6_500_000;
    r.ended_us = 6_900_123;
    let entry = TodayRun::of(&r);
    assert_eq!(
        (
            entry.started_ms,
            entry.ended_ms,
            entry.shape,
            entry.cfg.as_str()
        ),
        (6_500, 6_900, 9, "cfg")
    );
    assert_eq!(entry.action, Some((vec![1; 32], 1)));
    assert_eq!(entry.run_id, r.run_id());
    assert_eq!(
        (entry.instance.as_str(), entry.node.as_str(), entry.outcome),
        ("main", "node-1", RunOutcome::Failed)
    );
    let unstamped = RunRecord {
        started_us: 0,
        ended_us: 0,
        action: None,
        ..r
    };
    let entry = TodayRun::of(&unstamped);
    assert_eq!((entry.started_ms, entry.ended_ms), (7_000, 7_000));
    assert_eq!(entry.action, None);
}

#[test]
fn the_writer_feeds_today_from_the_runs_it_drains() {
    // Catches: a writer that never feeds the index (every cross-build read empty), one
    // that feeds it a run it dropped or a hit, an index that ignores the writer's
    // bound, and a start of coverage that is not the writer's first step.
    let probe = Today::new(usize::MAX);
    probe.push(TodayRun::of(&run_record("main", RunOutcome::Ok, 1)));
    let one = probe.bytes();
    let mut life = Life::fresh(Limits {
        today_bytes: 2 * one + one / 2,
        ..Limits::default()
    });
    let today = life.writer.today();
    assert_eq!(today.covers_from_ms(), None);
    let inv = InvRef::named("b1");
    life.run(&inv, T + 1, "main", RunOutcome::Ok);
    life.run(&inv, T + 2, "other", RunOutcome::Failed);
    life.hit(&inv, T + 3, "main");
    life.run(&inv, T + 4, "main", RunOutcome::Ok);
    life.step(T + 10);
    assert_eq!(today.len(), 2, "the hit is no run, and the bound holds two");
    let q = TodayQuery::newest(10);
    let got = today.on_node("node-1", &q, &all).runs;
    assert_eq!(
        got.iter().map(|r| r.ended_ms).collect::<Vec<_>>(),
        [T + 4, T + 2]
    );
    assert_eq!(got[1].outcome, RunOutcome::Failed);
    assert_eq!(
        today.covers_from_ms(),
        Some(T + 10),
        "the writer's first step, which is after the oldest run kept"
    );
    assert_eq!(
        life.writer.today().len(),
        2,
        "one index, however it is asked for"
    );
    // The digest's list is the same runs.
    let by_action = today.of_action(&(vec![1; 32], 1), &q, &all).runs;
    assert_eq!(by_action, got);
}
