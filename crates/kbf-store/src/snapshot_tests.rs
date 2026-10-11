//! The snapshot file and compaction over [`FaultFs`]: what a snapshot hides and
//! removes, a power cut at every operation of a snapshot write with every subset of its
//! name changes, an error at every operation, and damage at every byte.

use std::ops::RangeInclusive;

use kbf_raft::{Entry, LogId, LogIndex, Payload, Snapshot, Storage, Term};

use crate::{Fault, FaultFs, Fs, Options, Recovered, SNAPSHOT, SNAPSHOT_TMP, Store, StoreError};

/// Every persist call after the first goes to a new segment.
const ONE_PER_CALL: Options = Options { segment_bytes: 1 };

fn entry(index: u64) -> Entry {
    Entry {
        id: LogId::new(Term(1), LogIndex(index)),
        payload: Payload::Command(index.to_le_bytes().to_vec()),
    }
}

fn run_of(range: RangeInclusive<u64>) -> Vec<Entry> {
    range.map(entry).collect()
}

fn snap(index: u64) -> Snapshot {
    Snapshot {
        base: LogId::new(Term(1), LogIndex(index)),
        state: format!("state through {index}").into_bytes(),
    }
}

fn indexes(rec: &Recovered) -> Vec<u64> {
    rec.entries.iter().map(|e| e.id.index.0).collect()
}

/// The first index of each segment, in name (sequence) order.
fn firsts(fs: &FaultFs) -> Vec<u64> {
    let mut names: Vec<String> = fs
        .list()
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".log"))
        .collect();
    names.sort();
    names.iter().map(|n| n[21..41].parse().unwrap()).collect()
}

/// A store holding entries 1 to 9 in three segments, starting at 1, 4 and 7.
fn three_segments() -> (FaultFs, Store<FaultFs>) {
    let fs = FaultFs::new();
    let (mut store, _) = Store::open(fs.clone(), ONE_PER_CALL).unwrap();
    for run in [1..=3, 4..=6, 7..=9] {
        store.persist_entries(&run_of(run)).unwrap();
    }
    assert_eq!(firsts(&fs), [1, 4, 7]);
    (fs, store)
}

fn reopen(fs: &FaultFs) -> Recovered {
    Store::open(fs.clone(), ONE_PER_CALL).unwrap().1
}

/// Catches: a log after a snapshot at base k that still returns entry k (an
/// off-by-one in what the base hides) or loses entry k + 1, in `load` or at a reopen
/// after a power cut, for a base at the start, the middle and the end of a segment
/// and past the log's end; and a store that cannot go on appending right after the
/// base (the next index must be the one after the base, even past the log's end),
/// whether it compacted the log itself (a stale handle on a removed last segment
/// loses the append) or reopened a disk where the removed segments came back (an
/// open that keeps a last segment holding nothing after the base appends behind it,
/// out of order).
///
/// The store runs with 64 MiB segments after the three are written, so an append
/// goes to the last segment whenever there is one to go to.
#[test]
fn after_a_snapshot_at_k_the_log_is_exactly_k_plus_one_to_n() {
    for k in 1..=11 {
        let (fs, written) = three_segments();
        drop(written);
        let (mut store, _) = Store::open(fs.clone(), Options::default()).unwrap();
        store.persist_snapshot(&snap(k)).unwrap();
        let want: Vec<u64> = (k + 1..=9).collect();
        let loaded = store.load().unwrap();
        assert_eq!(indexes(&loaded), want, "load after a snapshot at {k}");
        assert_eq!(loaded.snapshot, Some(snap(k)));
        assert_eq!(store.last_index(), k.max(9));
        let next = k.max(9) + 1;
        let mut with_next = want.clone();
        with_next.push(next);

        let cut = fs.crash(0);
        let (mut reopened, rec) = Store::open(cut.clone(), Options::default()).unwrap();
        assert_eq!(rec, loaded, "reopen after a power cut, base {k}");
        assert_eq!(reopened.last_index(), k.max(9));
        reopened.persist_entries(&[entry(next)]).unwrap();
        let rec = reopen(&cut.crash(0));
        assert_eq!(indexes(&rec), with_next, "append after a reopen, base {k}");

        store.persist_entries(&[entry(next)]).unwrap();
        let (mut again, rec) = Store::open(fs.crash(0), ONE_PER_CALL).unwrap();
        assert_eq!(indexes(&rec), with_next, "append after a snapshot at {k}");
        assert_eq!(again.load().unwrap(), rec);
        assert_eq!(again.last_index(), next);
    }
}

/// Catches: a compaction that removes a segment because it starts at or below the
/// base (the entries after the base it holds are lost: a reopen finds a gap), one
/// that keeps segments holding nothing after the base, and an open that returns the
/// hidden entries of a kept segment.
#[test]
fn a_segment_holding_the_base_is_kept_and_hides_its_early_entries() {
    let (fs, mut store) = three_segments();
    store.persist_snapshot(&snap(5)).unwrap();
    assert_eq!(firsts(&fs), [4, 7], "the segment from 4 holds 6");
    assert_eq!(indexes(&store.load().unwrap()), [6, 7, 8, 9]);
    assert_eq!(indexes(&reopen(&fs.crash(0))), [6, 7, 8, 9]);
    store.persist_snapshot(&snap(6)).unwrap();
    assert_eq!(firsts(&fs), [7], "the segment from 4 ends at the base");
    store.persist_snapshot(&snap(8)).unwrap();
    assert_eq!(firsts(&fs), [7]);
    assert_eq!(indexes(&reopen(&fs)), [9]);
    store.persist_snapshot(&snap(9)).unwrap();
    assert!(firsts(&fs).is_empty(), "the last segment ends at the base");
    assert_eq!(indexes(&reopen(&fs)), Vec::<u64>::new());
}

/// Which of the two states a reopen found.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    Old,
    New,
}

fn old_or_new(rec: &Recovered, why: &str) -> Found {
    let found = if rec.snapshot == Some(snap(2)) {
        Found::Old
    } else {
        Found::New
    };
    let (base, after) = match found {
        Found::Old => (2, 3..=9),
        Found::New => (7, 8..=9),
    };
    assert_eq!(
        (rec.snapshot.clone(), indexes(rec)),
        (Some(snap(base)), after.collect()),
        "{why}: neither the old snapshot and its log nor the new"
    );
    found
}

/// Catches: a snapshot write that skips the directory sync after its rename (it
/// returns before the new snapshot is durable, and a power cut that keeps the
/// segment removals but not the rename loses both snapshots' entries), one that
/// removes segments before the snapshot is durable, and one that reports success
/// for a snapshot a power cut can take back.
///
/// The store holds an old snapshot at 2 and entries 3 to 9; a snapshot at 7 is
/// written with a crash at each of its operations, or none. Each crash becomes a
/// power cut with every subset of the name changes since the last directory sync and
/// every count of torn bytes kept. Every reopen finds the old snapshot and every
/// entry after it, or the new one and every entry after it; once the write returned
/// `Ok`, only the new.
#[test]
fn a_power_cut_during_a_snapshot_write_keeps_the_old_or_the_new() {
    let (fs, mut store) = three_segments();
    store.persist_snapshot(&snap(2)).unwrap();
    drop(store);
    let disk = fs.crash(0);
    let mut cases = 0;
    let mut seen_new_after_crash = false;
    for at in 0.. {
        let fs = disk.crash(0);
        let (mut store, rec) = Store::open(fs.clone(), ONE_PER_CALL).unwrap();
        assert_eq!(old_or_new(&rec, "setup"), Found::Old);
        fs.fail_at(fs.ops() + at, Fault::Crash);
        let returned = store.persist_snapshot(&snap(7));
        for mask in 0..1u64 << fs.pending_names() {
            for keep in 0..=fs.unsynced() {
                let why = format!("crash at op {at} of the write, names {mask:b}, {keep} kept");
                let cut = fs.crash_reordered(keep, mask);
                let rec = Store::open(cut.clone(), ONE_PER_CALL)
                    .unwrap_or_else(|e| panic!("{why}: open failed: {e}"))
                    .1;
                let found = old_or_new(&rec, &why);
                if returned.is_ok() {
                    assert_eq!(found, Found::New, "{why}: the write had returned Ok");
                } else if found == Found::New {
                    seen_new_after_crash = true;
                }
                cases += 1;
            }
        }
        if returned.is_ok() {
            break;
        }
    }
    assert!(
        seen_new_after_crash,
        "no crash fell after the snapshot was durable"
    );
    assert!(cases > 20, "only {cases} cases");
}

/// Catches: a snapshot write that ignores a write or sync error, that hides which
/// error it was, or after which the store keeps writing or loading (and through the
/// kbf-raft `Storage` calls too); and a failed write after which a power cut leaves
/// anything but the old or the new snapshot.
#[test]
fn an_error_at_any_snapshot_operation_stops_the_store() {
    let (fs, mut store) = three_segments();
    store.persist_snapshot(&snap(2)).unwrap();
    drop(store);
    let disk = fs.crash(0);
    let ops = {
        let fs = disk.crash(0);
        let (mut store, _) = Store::open(fs.clone(), ONE_PER_CALL).unwrap();
        let start = fs.ops();
        store.persist_snapshot(&snap(7)).unwrap();
        fs.ops() - start
    };
    assert_eq!(
        ops, 7,
        "create, append, sync, rename, sync the directory, two removals"
    );
    for at in 0..ops {
        let fs = disk.crash(0);
        let (mut store, _) = Store::open(fs.clone(), ONE_PER_CALL).unwrap();
        let op = fs.ops() + at;
        fs.fail_at(op, Fault::Error);
        let err = store.persist_snapshot(&snap(7)).unwrap_err();
        let cause = std::error::Error::source(&err).map(ToString::to_string);
        assert_eq!(cause, Some(format!("injected error at op {op}")), "{err}");
        assert!(matches!(
            store.persist_snapshot(&snap(8)),
            Err(StoreError::Failed)
        ));
        assert!(matches!(store.load(), Err(StoreError::Failed)));
        assert!(matches!(Storage::sync(&mut store), Err(StoreError::Failed)));
        assert!(matches!(
            store.write_snapshot(&snap(8)),
            Err(StoreError::Failed)
        ));
        assert!(matches!(
            store.write_entries(&[entry(10)]),
            Err(StoreError::Failed)
        ));
        assert!(matches!(
            store.write_hard_state(kbf_raft::HardState::default()),
            Err(StoreError::Failed)
        ));
        old_or_new(&reopen(&fs.crash(0)), &format!("error at op {at}"));
    }
}

/// Catches: a snapshot at or before the current base, or at index 0, that is
/// written (it would take back compacted state), entries at or below the base that
/// are written (open would read them as a truncation of the hidden log), and a
/// refusal that stops the store.
#[test]
fn a_snapshot_or_entries_not_past_the_base_are_refused() {
    let (fs, mut store) = three_segments();
    assert!(matches!(
        store.persist_snapshot(&snap(0)),
        Err(StoreError::Invalid(_))
    ));
    store.persist_snapshot(&snap(5)).unwrap();
    for k in [4, 5] {
        assert!(matches!(
            store.persist_snapshot(&snap(k)),
            Err(StoreError::Invalid(_))
        ));
    }
    for first in [5, 2] {
        assert!(matches!(
            store.persist_entries(&[entry(first)]),
            Err(StoreError::Invalid(_))
        ));
    }
    store.persist_entries(&[entry(6)]).unwrap();
    assert_eq!(indexes(&reopen(&fs)), [6]);
    assert_eq!(Storage::sync(&mut store).ok(), Some(()));
}

/// A disk with a snapshot at 5 whose open has everything to clean: a covered
/// segment back after its removal was lost, a leftover temporary snapshot, and a
/// torn tail.
fn messy() -> FaultFs {
    let (fs, mut store) = three_segments();
    store.persist_snapshot(&snap(5)).unwrap();
    drop(store);
    let disk = fs.crash(0);
    assert_eq!(firsts(&disk), [1, 4, 7], "the removal was never synced");
    disk.put(SNAPSHOT_TMP, b"half a snapshot");
    let last = segment_from(&disk, 7);
    let mut torn = disk.contents(&last).unwrap();
    torn.extend_from_slice(&[9, 0, 0]);
    disk.put(&last, &torn);
    disk
}

/// The name of the segment that starts at `first`.
fn segment_from(fs: &FaultFs, first: u64) -> String {
    let suffix = format!("-{first:020}.log");
    fs.list()
        .unwrap()
        .into_iter()
        .find(|n| n.ends_with(&suffix))
        .unwrap()
}

/// Catches: an open over a snapshot that keeps a covered segment or a leftover
/// temporary snapshot, misses the torn tail after a straddling segment, or swallows
/// an error at any of its operations; and an open whose own crash leaves a disk that
/// opens differently.
#[test]
fn an_open_over_a_snapshot_cleans_up_and_returns_its_errors() {
    let disk = messy();
    let (ops, clean) = {
        let fs = disk.crash(0);
        let (_, rec) = Store::open(fs.clone(), ONE_PER_CALL).unwrap();
        let ops = fs.ops();
        assert_eq!(
            ops, 10,
            "list, three reads, open, truncate, two syncs, two removals"
        );
        assert_eq!(firsts(&fs), [4, 7]);
        assert_eq!(fs.contents(SNAPSHOT_TMP), None);
        assert_eq!(indexes(&rec), [6, 7, 8, 9]);
        assert_eq!(rec.snapshot, Some(snap(5)));
        (ops, rec)
    };
    for at in 0..ops {
        let fs = disk.crash(0);
        fs.fail_at(at, Fault::Error);
        assert!(
            matches!(
                Store::open(fs.clone(), ONE_PER_CALL),
                Err(StoreError::Io(_))
            ),
            "op {at}"
        );
        let fs = disk.crash(0);
        fs.fail_at(at, Fault::Crash);
        assert!(Store::open(fs.clone(), ONE_PER_CALL).is_err());
        for mask in 0..1u64 << fs.pending_names() {
            assert_eq!(
                reopen(&fs.crash_reordered(0, mask)),
                clean,
                "open crashed at {at}, names {mask:b}"
            );
        }
    }
}

/// Catches: an open that accepts a damaged snapshot file (a flipped byte anywhere,
/// including in the state) instead of refusing it, and one that accepts a log that
/// does not reach the entry after the base.
#[test]
fn damage_in_the_snapshot_or_a_gap_after_its_base_refuses_open() {
    let (fs, mut store) = three_segments();
    store.persist_snapshot(&snap(5)).unwrap();
    drop(store);
    let disk = fs.crash(0);
    let len = disk.contents(SNAPSHOT).unwrap().len();
    for offset in 0..len {
        let fs = disk.crash(0);
        fs.flip(SNAPSHOT, offset);
        assert!(
            matches!(
                Store::open(fs, ONE_PER_CALL),
                Err(StoreError::Corrupt { .. })
            ),
            "snapshot byte {offset} opened"
        );
    }
    let gap = FaultFs::new();
    gap.put(SNAPSHOT, &disk.contents(SNAPSHOT).unwrap());
    let seven = segment_from(&disk, 7);
    gap.put(&seven, &disk.contents(&seven).unwrap());
    assert!(
        matches!(
            Store::open(gap, ONE_PER_CALL),
            Err(StoreError::Corrupt { .. })
        ),
        "a log from 7 after a base at 5 opened"
    );
}
