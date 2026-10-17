//! Writer fencing: the generation row, the handle that checks it on every write, and
//! `demote` handing a SQLite file to a second process's store.
//!
//! Every test names the defect it catches.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::scratch;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::sqlite::{Options, SqliteError, SqliteStore};
use kbf_state::{Clock, Cond, Fenced, GENERATION, ManualClock, Store, StoreError, TakeError, Txn};

fn fake() -> Arc<dyn Store> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn open(dir: &std::path::Path) -> Result<SqliteStore, SqliteError> {
    let clock: Arc<dyn Clock> = Arc::new(kbf_state::SystemClock::default());
    SqliteStore::open(&dir.join("state.db"), clock, Options::default())
}

// Catches a generation that does not count up from the store's own row: the first writer
// of an empty store holds 1, the next holds 2.
#[test]
fn each_taker_holds_the_next_generation() {
    let store = fake();
    let first = Fenced::take(Arc::clone(&store), None).expect("take");
    assert_eq!(first.generation(), 1);
    let second = Fenced::take(Arc::clone(&store), Some(1)).expect("take");
    assert_eq!(second.generation(), 2);
}

// Catches a successor that takes over a file that is not the one it was told about (a
// restored backup, a swapped file): the expectation is checked and nothing is written.
#[test]
fn a_taker_that_expects_another_generation_is_refused_and_writes_nothing() {
    let store = fake();
    Fenced::take(Arc::clone(&store), None).expect("take");
    let before = store.get(GENERATION).expect("get").expect("row");
    let refused = Fenced::take(Arc::clone(&store), Some(7)).expect_err("another generation");
    assert_eq!(
        refused,
        TakeError::Mismatch {
            expected: 7,
            found: 1
        }
    );
    assert_eq!(store.get(GENERATION).expect("get"), Some(before));
    let corrupt = fake();
    corrupt
        .txn(Txn::new().put(GENERATION, "x", Cond::Any))
        .expect("store")
        .expect("put");
    assert_eq!(
        Fenced::take(corrupt, None).expect_err("not a number"),
        TakeError::Corrupt
    );
}

// Catches a lost update: when many handles take the generation of one store at the same
// moment, exactly one write wins each round, so no two hold the same generation.
#[test]
fn concurrent_takers_never_hold_the_same_generation() {
    let store = fake();
    let takers: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || Fenced::take(store, None))
        })
        .collect();
    let mut held: Vec<u64> = takers
        .into_iter()
        .filter_map(|t| t.join().expect("join").ok())
        .map(|f| f.generation())
        .collect();
    held.sort_unstable();
    let distinct = held.len();
    held.dedup();
    assert_eq!(held.len(), distinct, "two handles hold one generation");
    assert!(!held.is_empty(), "at least one taker won");
}

// Catches a write path that bypasses the fence: once another handle takes the next
// generation, a put, a delete, a time-to-live put, a counter and a check-only transaction
// of the old handle each fail Fenced and change nothing.
#[test]
fn a_superseded_handle_fails_every_kind_of_write() {
    let store = fake();
    let old = Fenced::take(Arc::clone(&store), None).expect("take");
    old.txn(Txn::new().put("k", "v", Cond::Any))
        .expect("the holder writes")
        .expect("commits");
    let new = Fenced::take(Arc::clone(&store), Some(1)).expect("take");
    let writes = [
        Txn::new().put("k", "stale", Cond::Any),
        Txn::new().delete("k", Cond::Any),
        Txn::new().put_ttl("t", "v", Duration::from_secs(5), Cond::Any),
        Txn::new().next("counter"),
        Txn::new().check("k", Cond::Any),
        Txn::new(),
    ];
    for txn in writes {
        assert_eq!(old.txn(txn.clone()).unwrap_err(), StoreError::Fenced);
        new.txn(txn)
            .expect("the new holder writes")
            .expect("commits");
    }
    assert_eq!(
        old.apply_batch(vec![Txn::new().put("a", "1", Cond::Any)])
            .unwrap_err(),
        StoreError::Fenced
    );
    assert_eq!(old.reap_expired().unwrap_err(), StoreError::Fenced);
    assert_eq!(
        old.get("a").expect("reads are not fenced"),
        None,
        "the fenced batch wrote nothing"
    );
    assert_eq!(new.reap_expired(), Ok(0));
}

// Catches a precondition failure being mistaken for a fence (and the reverse): a lost
// compare-and-set of the live holder is a Conflict, not Fenced.
#[test]
fn the_holder_still_sees_its_own_conflicts() {
    let store = fake();
    let held = Fenced::take(store, None).expect("take");
    held.txn(Txn::new().put("k", "v", Cond::Absent))
        .expect("store")
        .expect("commits");
    let conflict = held
        .txn(Txn::new().put("k", "w", Cond::Absent))
        .expect("the store answers")
        .expect_err("the key exists");
    assert_eq!(conflict.key, "k");
}

// Catches a handle that forwards the wrong things: reads, the feed, the clock and the
// counters all come from the store underneath.
#[test]
fn reads_pass_through() {
    let store = fake();
    let held = Fenced::take(Arc::clone(&store), None).expect("take");
    held.txn(
        Txn::new()
            .put("p/a", "1", Cond::Any)
            .put("p/b", "2", Cond::Any),
    )
    .expect("store")
    .expect("commit");
    assert_eq!(held.scan_prefix("p/").expect("scan").len(), 2);
    let page = held
        .scan_range(kbf_state::ScanRange::new("p/", std::num::NonZeroUsize::MIN))
        .expect("range");
    assert_eq!(page.rows.len(), 1);
    assert!(
        held.changes(kbf_state::Revision::ZERO, 100)
            .expect("feed")
            .rows
            .len()
            >= 3
    );
    assert_eq!(held.now(), store.now());
    assert_eq!(held.commit_stats(), store.commit_stats());
    assert!(
        held.demote().is_err(),
        "the fake has no writer role to give up"
    );
}

// Catches a demote that leaves the lock or the writer thread behind: after it a second
// open of the file succeeds, the old handle still reads, and its writes fail Fenced.
#[test]
fn demote_frees_the_file_for_a_second_store_and_keeps_reads() {
    let dir = scratch("demote");
    let old = open(&dir).expect("open");
    old.txn(Txn::new().put("k", "v", Cond::Any))
        .expect("s")
        .expect("c");
    assert!(
        matches!(open(&dir), Err(SqliteError::Locked { .. })),
        "the lock is held until the demote"
    );
    old.demote().expect("demote");
    old.demote().expect("demoting twice is harmless");
    let new = open(&dir).expect("the file is free");
    new.txn(Txn::new().put("k2", "v2", Cond::Any))
        .expect("s")
        .expect("c");
    assert_eq!(old.get("k").expect("read").expect("row").value, b"v");
    assert_eq!(
        old.get("k2").expect("read").expect("row").value,
        b"v2",
        "the old handle sees the new writer's rows"
    );
    assert_eq!(
        old.txn(Txn::new().put("k3", "x", Cond::Any)).unwrap_err(),
        StoreError::Fenced
    );
    assert_eq!(old.reap_expired().unwrap_err(), StoreError::Fenced);
    assert_eq!(new.get("k3").expect("read"), None);
}

// Catches a demote that drops a write already acknowledged to its caller or still queued
// behind the writer: calls made before it complete, and are on disk for the successor.
#[test]
fn writes_made_before_a_demote_are_on_disk_for_the_next_store() {
    let dir = scratch("demote-writes");
    let old = Arc::new(open(&dir).expect("open"));
    let writers: Vec<_> = (0..16)
        .map(|n| {
            let old = Arc::clone(&old);
            std::thread::spawn(move || old.txn(Txn::new().put(format!("w{n}"), "v", Cond::Any)))
        })
        .collect();
    old.demote().expect("demote");
    let mut acknowledged = Vec::new();
    for (n, writer) in writers.into_iter().enumerate() {
        match writer.join().expect("join") {
            Ok(Ok(_)) => acknowledged.push(n),
            Err(StoreError::Fenced) => {}
            other => panic!("neither committed nor fenced: {other:?}"),
        }
    }
    let new = open(&dir).expect("open");
    for n in acknowledged {
        assert!(new.get(&format!("w{n}")).expect("read").is_some(), "w{n}");
    }
}

// Catches: a handle that cannot be printed (it is held in structs that derive Debug) or
// that prints without saying which generation it holds.
#[test]
fn a_handle_prints_its_generation() {
    let handle = Fenced::take(fake(), None).expect("take");
    let text = format!("{handle:?}");
    assert!(
        text.contains("Fenced") && text.contains("generation: 1"),
        "{text}"
    );
}
