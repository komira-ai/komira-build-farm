//! C8 page snapshot: one page is read from one snapshot of committed state, so a commit
//! that lands while a page is being read is in all of its rows or in none.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use super::{Backend, keys_of, run, scan_all};
use crate::clock::ManualClock;
use crate::types::{Cond, ScanRange, Txn};

/// The prefix of C8's page-snapshot check. The fake's per-row read flaw
/// ([`crate::fake::Flaw::ScanReadsPerRow`]) applies only under it.
pub const C8_SNAPSHOT_PREFIX: &str = "c8snap/";

/// How many keys the racer rewrites in each of its transactions.
const KEYS: usize = 24;

/// The page size: large enough that a page spans many rows, small enough that a scan
/// has several pages and so several tokens.
const LIMIT: usize = 10;

/// The fewest whole scans the reader pages through.
const SCANS: usize = 20;

/// The fewest transactions the racer commits. The reader keeps scanning until the racer
/// is done, so on a backend whose commits are slow next to a page read the check still
/// gets this many chances for a commit to land inside a page.
const GENERATIONS: u64 = 400;

/// The most transactions the racer commits, so the check ends whatever the reader does.
const MAX_GENERATIONS: u64 = 100_000;

fn key(index: usize) -> String {
    format!("{C8_SNAPSHOT_PREFIX}{index:02}")
}

/// One transaction that writes `generation` into every key.
fn generation(generation: u64) -> Txn {
    (0..KEYS).fold(Txn::new(), |txn, index| {
        txn.put(key(index), generation.to_string(), Cond::Any)
    })
}

/// Sets its flag when dropped: the reader's tells the racer to stop when the reader is
/// done or an assertion unwinds it, and the racer's tells the reader it is done (or
/// failed).
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// C8 page snapshot: a racer commits one transaction after another, each rewriting every
/// key under [`C8_SNAPSHOT_PREFIX`] to its own generation, while the reader pages through
/// the prefix. Every row of a page must carry one generation, a page with a token must
/// be full, and each scan returns every key once, in order.
pub(super) fn c8_page_snapshot<B: Backend>(backend: &B) {
    let store = backend.open(Arc::new(ManualClock::default()));
    run(&store, generation(0)).expect("seed the C8 snapshot keys");
    let expected: Vec<String> = (0..KEYS).map(key).collect();
    let limit = NonZeroUsize::new(LIMIT).expect("positive");
    let stop = AtomicBool::new(false);
    let scanned = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let _done = SetOnDrop(&done);
            // The racer outlives the reader's first scans, so every page of those is read
            // while commits keep landing.
            for next in 1..=MAX_GENERATIONS {
                let enough = next > GENERATIONS && scanned.load(Ordering::Relaxed);
                if enough || stop.load(Ordering::Relaxed) {
                    break;
                }
                run(&store, generation(next)).expect("a racer's unconditional commit applies");
            }
        });
        let _stop = SetOnDrop(&stop);
        let mut scans = 0;
        while !done.load(Ordering::Relaxed) {
            scans += 1;
            if scans >= SCANS {
                scanned.store(true, Ordering::Relaxed);
            }
            let pages = scan_all(&store, ScanRange::new(C8_SNAPSHOT_PREFIX, limit));
            for page in &pages {
                let first = page.rows.first().map(|(_, row)| &row.value);
                assert!(
                    page.rows.iter().all(|(_, row)| Some(&row.value) == first),
                    "C8 page snapshot: every row of a page is from one commit, though each commit rewrites them all ({:?})",
                    page.rows
                        .iter()
                        .map(|(_, row)| String::from_utf8_lossy(&row.value))
                        .collect::<Vec<_>>()
                );
                if page.resume.is_some() {
                    assert_eq!(
                        page.rows.len(),
                        LIMIT,
                        "C8 page snapshot: a page with a token is full while keys are only rewritten"
                    );
                }
            }
            let keys: Vec<&str> = pages.iter().flat_map(keys_of).collect();
            assert_eq!(
                keys, expected,
                "C8 page snapshot: a scan under rewrites returns each key once, in order"
            );
        }
    });
}
