//! The change feed read while a writer commits: a poller paging the feed must see every
//! group whole, in order, once, and a cursor that never runs past a row it has not
//! returned.
//!
//! The conformance clause C5 cannot interleave commits on a single-writer store, so this
//! is where the SQLite store's feed is held to it: the writer threads commit groups of
//! several transactions each (one `apply_batch` is one group's worth, and the writer
//! merges concurrent calls into larger ones) while pollers page the feed.

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use common::{open_manual, scratch};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{Changes, Cond, Revision, Store, Txn};

const WRITERS: usize = 3;
const BATCHES: usize = 60;
const PER_BATCH: usize = 5;
const ROWS: u64 = (WRITERS * BATCHES * PER_BATCH) as u64;

/// What a poller has seen: the last revision, and how many rows of each batch.
#[derive(Default)]
struct Seen {
    last: u64,
    per_batch: BTreeMap<String, usize>,
}

impl Seen {
    /// Takes one page from a poller that asked after `after` (the cursor of the page
    /// before), checking it against all that came before it.
    fn take(&mut self, after: Revision, page: &Changes, whole_batches: bool) {
        let mut previous = after.0;
        for row in &page.rows {
            assert_eq!(
                row.revision.0,
                previous + 1,
                "the feed has a hole or a repeat after revision {previous}"
            );
            previous = row.revision.0;
            let batch = row.key.rsplit_once('/').map_or("", |(batch, _)| batch);
            *self.per_batch.entry(batch.to_owned()).or_default() += 1;
        }
        let expected = page.rows.last().map_or(after, |row| row.revision);
        assert_eq!(
            page.cursor, expected,
            "the cursor must be the last row returned: past it, a row is skipped; behind it, repeated"
        );
        self.last = page.cursor.0;
        if whole_batches {
            // A page that is not cut by its limit ends at a commit, so a batch is in it
            // whole or not at all.
            for (batch, count) in &self.per_batch {
                assert_eq!(
                    *count, PER_BATCH,
                    "batch {batch} was seen in part ({count} of {PER_BATCH} rows)"
                );
            }
        }
    }
}

fn poll(
    store: &SqliteStore,
    limit: usize,
    whole_batches: bool,
    writers_done: &AtomicUsize,
) -> Seen {
    let mut seen = Seen::default();
    loop {
        // Read the flag first: a row committed before it is set is in the page that follows.
        let done = writers_done.load(Ordering::SeqCst) == WRITERS;
        let after = Revision(seen.last);
        let page = store.changes(after, limit).expect("the store is reachable");
        let empty = page.rows.is_empty();
        seen.take(after, &page, whole_batches);
        if done && empty {
            return seen;
        }
        if empty {
            thread::yield_now();
        }
    }
}

// Catches a feed read that is not one snapshot (the head read apart from the rows), a cursor
// that passes a row it did not return or falls behind one it did, a row skipped or repeated
// when a poller resumes, and a commit group that shows in part. One poller reads pages as
// large as the feed (so each page ends at a commit and whole batches are checked); the other
// pages by a small limit, so the full-page cursor is exercised while commits land.
#[test]
fn pollers_see_every_group_whole_and_in_order_while_writers_commit() {
    let dir = scratch("feed-pollers");
    let (store, _) = open_manual(&dir, Options::default());
    let store = Arc::new(store);
    let writers_done = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));

    let pollers: Vec<_> = [(10_000, true), (7, false)]
        .into_iter()
        .map(|(limit, whole)| {
            let store = Arc::clone(&store);
            let done = Arc::clone(&writers_done);
            let results = Arc::clone(&results);
            thread::spawn(move || {
                let seen = poll(&store, limit, whole, &done);
                results.lock().expect("lock").push(seen);
            })
        })
        .collect();
    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let store = Arc::clone(&store);
            let done = Arc::clone(&writers_done);
            thread::spawn(move || {
                for batch in 0..BATCHES {
                    let txns: Vec<Txn> = (0..PER_BATCH)
                        .map(|n| {
                            Txn::new().put(format!("w{writer}b{batch:03}/{n}"), "v", Cond::Any)
                        })
                        .collect();
                    let results = store.apply_batch(txns).expect("the store is reachable");
                    assert!(results.iter().all(Result::is_ok), "every put commits");
                }
                done.fetch_add(1, Ordering::SeqCst);
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("a writer finished");
    }
    for poller in pollers {
        poller.join().expect("a poller finished");
    }

    let results = results.lock().expect("lock");
    assert_eq!(results.len(), 2);
    for seen in results.iter() {
        assert_eq!(seen.last, ROWS, "a poller saw the whole feed");
        assert_eq!(
            seen.per_batch.len(),
            WRITERS * BATCHES,
            "every batch was seen"
        );
        assert!(seen.per_batch.values().all(|count| *count == PER_BATCH));
    }
}
