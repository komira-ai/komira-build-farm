//! The store-commits gate: the SQLite store must carry the target write rate
//! ([`kbf_state::TARGET_WRITES_PER_SECOND`], 1500 durable writes a second) on far fewer
//! commits than writes. The bound is a count of commits, which does not depend on how fast
//! the machine syncs. No test here asserts a latency: speed depends on the hardware.

mod common;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use common::{open_manual, scratch};
use kbf_state::sqlite::Options;
use kbf_state::{Cond, Store, TARGET_WRITES_PER_SECOND, Txn};

const WRITERS: usize = 50;

// Catches a writer that syncs per write, or one that does not group concurrent writers:
// 1500 writes from 50 callers, each waiting for its acknowledgement before the next, must
// take exactly one commit per round of 50. The window is far longer than the test needs, so
// a group closes only because it is full, never because time ran out.
#[test]
fn the_target_rate_takes_one_commit_per_group() {
    let writes = usize::try_from(TARGET_WRITES_PER_SECOND).expect("the rate fits a usize");
    let rounds = writes / WRITERS;
    assert_eq!(
        rounds * WRITERS,
        writes,
        "the writers divide the rate evenly"
    );

    let dir = scratch("store-commits");
    let options = Options {
        window: Duration::from_secs(30),
        max_group: WRITERS,
        ..Options::default()
    };
    let (store, _clock) = open_manual(&dir, options);
    let store = Arc::new(store);
    let before = store.commit_stats();

    // The callers and this thread meet after every round, so a group that did not close
    // when it filled is caught at the end of its round, not by a test that never finishes.
    let barrier = Arc::new(Barrier::new(WRITERS + 1));
    let callers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                for round in 0..rounds {
                    let key = format!("gate/{writer:02}/{round:03}");
                    store
                        .txn(Txn::new().put(key, "x", Cond::Any))
                        .expect("the store is reachable")
                        .expect("an unconditional put commits");
                    // Once everyone has its acknowledgement, and again once the checks
                    // of the round are made: no write of the next round is in flight
                    // while they are.
                    barrier.wait();
                    barrier.wait();
                }
            })
        })
        .collect();
    for round in 0..rounds {
        barrier.wait();
        let metrics = store.commit_metrics();
        assert_eq!(
            metrics.groups,
            round as u64 + 1,
            "round {round}: every round of {WRITERS} callers is one group"
        );
        barrier.wait();
    }
    for caller in callers {
        caller.join().expect("a caller finished");
    }

    let stats = store.commit_stats();
    assert_eq!(
        stats.txns - before.txns,
        writes as u64,
        "every write committed"
    );
    let commits = stats.syncs - before.syncs;
    assert_eq!(
        commits, rounds as u64,
        "{writes} writes took {commits} commits; one per round of {WRITERS} is the bound"
    );
    assert!(
        commits * 10 <= writes as u64,
        "grouping must cut commits by at least ten times"
    );
    let metrics = store.commit_metrics();
    assert_eq!(metrics.groups, rounds as u64);
    assert_eq!(metrics.largest_group, WRITERS as u64);
    assert_eq!(
        store.scan_prefix("gate/").expect("scan").len(),
        writes,
        "every acknowledged write is readable"
    );
}
