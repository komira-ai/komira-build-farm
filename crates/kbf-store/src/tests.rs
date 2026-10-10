//! The store over [`FaultFs`]: a crash at every operation with every torn write, an
//! error at every operation, and damage at every byte.

use kbf_raft::{Entry, HardState, LogId, LogIndex, Payload, ServerId, Term};

use crate::{
    Fault, FaultFs, Fs, HARD_STATE, HARD_STATE_TMP, Options, Recovered, Store, StoreError,
};

/// Small segments, so the script below rotates.
const OPTS: Options = Options { segment_bytes: 80 };

fn entry(index: u64, term: u64) -> Entry {
    let len = usize::try_from(index % 4).unwrap() * 3;
    let payload = if index.is_multiple_of(5) {
        Payload::Blank
    } else {
        Payload::Command(vec![u8::try_from(index * 7 % 251).unwrap(); len + 1])
    };
    Entry {
        id: LogId::new(Term(term), LogIndex(index)),
        payload,
    }
}

fn entries(range: std::ops::RangeInclusive<u64>, term: u64) -> Vec<Entry> {
    range.map(|i| entry(i, term)).collect()
}

fn hard(term: u64, vote: Option<u64>) -> HardState {
    HardState {
        term: Term(term),
        voted_for: vote.map(ServerId),
    }
}

#[derive(Clone, Debug)]
enum Step {
    Hard(HardState),
    Entries(Vec<Entry>),
}

/// Appends, a rotation, a truncation that replaces a whole segment, one that cuts into
/// the first segment, and hard states between.
fn script() -> Vec<Step> {
    vec![
        Step::Hard(hard(1, Some(1))),
        Step::Entries(entries(1..=3, 1)),
        Step::Entries(entries(4..=4, 1)),
        Step::Entries(entries(5..=6, 1)),
        Step::Entries(entries(7..=7, 1)),
        Step::Hard(hard(2, Some(2))),
        Step::Entries(entries(5..=7, 2)),
        Step::Entries(entries(3..=3, 2)),
        Step::Entries(entries(4..=5, 2)),
        Step::Hard(hard(3, None)),
    ]
}

/// What the script has had acknowledged, and the step that was running when it
/// stopped.
#[derive(Clone, Debug, Default)]
struct Model {
    hard: HardState,
    entries: Vec<Entry>,
    in_flight: Option<Step>,
}

fn apply(log: &mut Vec<Entry>, new: &[Entry]) {
    log.truncate(usize::try_from(new[0].id.index.0 - 1).unwrap());
    log.extend_from_slice(new);
}

impl Model {
    /// Whether `rec` is what a crash could leave: every acknowledged write, and of the
    /// write in flight, nothing, all, or (for entries) a prefix of it.
    fn admits(&self, rec: &Recovered) -> bool {
        let hard_ok =
            rec.hard == self.hard || matches!(self.in_flight, Some(Step::Hard(h)) if h == rec.hard);
        let entries_ok = rec.entries == self.entries
            || match &self.in_flight {
                Some(Step::Entries(new)) => (0..=new.len()).any(|j| {
                    let mut log = self.entries.clone();
                    log.truncate(usize::try_from(new[0].id.index.0 - 1).unwrap());
                    log.extend_from_slice(&new[..j]);
                    rec.entries == log
                }),
                _ => false,
            };
        hard_ok && entries_ok
    }
}

/// Opens a store on `fs` and runs `steps` until one fails; returns the model, the
/// failing step's position and error, and the store (`None` if the open failed).
fn run(
    fs: &FaultFs,
    steps: &[Step],
) -> (Model, Option<(usize, StoreError)>, Option<Store<FaultFs>>) {
    let mut model = Model::default();
    let Ok((mut store, rec)) = Store::open(fs.clone(), OPTS) else {
        return (model, None, None);
    };
    assert_eq!(rec, Recovered::default());
    for (i, step) in steps.iter().enumerate() {
        let result = match step {
            Step::Hard(h) => store.persist_hard_state(*h),
            Step::Entries(e) => store.persist_entries(e),
        };
        if let Err(err) = result {
            model.in_flight = Some(step.clone());
            return (model, Some((i, err)), Some(store));
        }
        match step {
            Step::Hard(h) => model.hard = *h,
            Step::Entries(e) => apply(&mut model.entries, e),
        }
    }
    (model, None, Some(store))
}

fn clean_ops() -> u64 {
    let fs = FaultFs::new();
    let (model, err, _) = run(&fs, &script());
    assert!(err.is_none() && model.in_flight.is_none());
    let mut expect = Vec::new();
    for s in script() {
        if let Step::Entries(e) = s {
            apply(&mut expect, &e);
        }
    }
    assert_eq!(model.entries, expect);
    assert_eq!(model.hard, hard(3, None));
    fs.ops()
}

/// A copy of `fs` (whose bytes are all durable) to crash separately.
fn copy(fs: &FaultFs) -> FaultFs {
    fs.crash(usize::MAX)
}

/// Opens what a crash left, checks it against the model, appends one entry after it and
/// checks that a reopen reads the log back with that entry.
fn recover_and_extend(disk: &FaultFs, model: &Model, why: &str) -> Recovered {
    let live = copy(disk);
    let (mut store, rec) =
        Store::open(live.clone(), OPTS).unwrap_or_else(|e| panic!("{why}: open failed: {e}"));
    assert!(
        model.admits(&rec),
        "{why}: recovered {rec:?}\nmodel {model:?}"
    );
    let next = entry(rec.entries.len() as u64 + 1, 9);
    store.persist_entries(std::slice::from_ref(&next)).unwrap();
    let (_, extended) = Store::open(live.crash(0), OPTS)
        .unwrap_or_else(|e| panic!("{why}: reopen after append failed: {e}"));
    let mut want = rec.entries.clone();
    want.push(next);
    assert_eq!(extended.entries, want, "{why}: append after recovery");
    assert_eq!(extended.hard, rec.hard);
    rec
}

/// Catches (each a mutant that turns this red): a persist that returns before its
/// fsync, or before the directory fsync of a new segment or the hard state's rename
/// (an acknowledged write is gone after the crash); an open that accepts a torn record
/// or truncates past the torn tail (an entry that was never written, or an
/// acknowledged one lost); an open that leaves the torn bytes in place (the append
/// after recovery lands behind garbage and the reopen fails); an open that returns
/// unsynced bytes or names as the log without syncing them (a power cut after the
/// open loses what it returned).
///
/// The script is crashed at every operation. Each crash is materialized as a power cut
/// with every count of torn bytes kept, and each recovered store is then crashed at
/// every operation of its own open; and as a process death, opened, then followed by a
/// power cut.
#[test]
fn crash_at_every_operation_and_tear_at_every_byte() {
    let total = clean_ops();
    let mut cases = 0;
    for n in 0..total {
        let fs = FaultFs::new();
        fs.fail_at(n, Fault::Crash);
        let (model, _, _) = run(&fs, &script());
        let restarted = fs.restart();
        let (_, rec) = Store::open(restarted.clone(), OPTS).unwrap();
        assert!(model.admits(&rec), "restart after op {n}: {rec:?}");
        let (_, after) = Store::open(restarted.crash(0), OPTS).unwrap();
        assert_eq!(after, rec, "power cut after a restart at op {n}");
        for keep in 0..=fs.unsynced() {
            let disk = fs.crash(keep);
            let why = format!("crash at op {n}, {keep} torn bytes kept");
            let rec = recover_and_extend(&disk, &model, &why);
            let open_ops = {
                let probe = copy(&disk);
                Store::open(probe.clone(), OPTS).unwrap();
                probe.ops()
            };
            for m in 0..open_ops {
                let again = copy(&disk);
                again.fail_at(m, Fault::Crash);
                assert!(Store::open(again.clone(), OPTS).is_err());
                for keep2 in 0..=again.unsynced() {
                    let (_, rec2) = Store::open(again.crash(keep2), OPTS)
                        .unwrap_or_else(|e| panic!("{why}, open crashed at {m}: {e}"));
                    assert_eq!(rec2, rec, "{why}, open crashed at {m}, {keep2} kept");
                    cases += 1;
                }
            }
        }
    }
    assert!(cases > 1000, "only {cases} cases");
}

/// Catches: a persist that ignores a write or fsync error (it returns `Ok`), and a store
/// that keeps writing after one (a retried fsync can succeed on bytes the kernel
/// dropped). Every operation of the script fails once in turn: the call that hit it
/// must fail, every later call must return `Failed` though the disk works again, and
/// the disk after a crash must hold what the model admits.
#[test]
fn an_error_at_any_operation_stops_the_store() {
    let total = clean_ops();
    let steps = script();
    let mut stopped = 0;
    for n in 0..total {
        let fs = FaultFs::new();
        fs.fail_at(n, Fault::Error);
        let (model, err, store) = run(&fs, &steps);
        let Some(mut store) = store else {
            continue; // the error hit the first open
        };
        let (failed, err) = err.unwrap_or_else(|| panic!("an error at op {n} was not reported"));
        assert!(matches!(err, StoreError::Io(_)), "op {n}: {err}");
        stopped += 1;
        for step in &steps[failed + 1..] {
            let r = match step {
                Step::Hard(h) => store.persist_hard_state(*h),
                Step::Entries(e) => store.persist_entries(e),
            };
            assert!(matches!(r, Err(StoreError::Failed)), "op {n}: {r:?}");
        }
        for keep in 0..=fs.unsynced() {
            let (_, rec) = Store::open(fs.crash(keep), OPTS).unwrap();
            assert!(model.admits(&rec), "op {n}, keep {keep}: {rec:?}");
        }
    }
    assert!(stopped > 30, "only {stopped} errors reached a persist");
}

/// A clean run of the script on a fresh disk.
fn written() -> FaultFs {
    let fs = FaultFs::new();
    let _ = run(&fs, &script());
    fs.crash(0)
}

fn segments(fs: &FaultFs) -> Vec<String> {
    let mut names: Vec<String> = fs
        .list()
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".log"))
        .collect();
    names.sort();
    names
}

/// Catches: an open that accepts a bad CRC in the middle of the log, by cutting the log
/// there as if it were a torn tail (acknowledged entries after it would vanish, on one
/// server only), and a damaged hard state taken as none. Every byte of every file is
/// flipped in turn: only a flip inside the very last record may open, and then the log
/// is the full log minus that record.
#[test]
fn damage_anywhere_but_the_last_record_refuses_open() {
    let base = written();
    let (_, full) = Store::open(copy(&base), OPTS).unwrap();
    let segs = segments(&base);
    assert!(
        segs.len() >= 2,
        "the script must leave several segments: {segs:?}"
    );
    let last_seg = segs.last().unwrap();
    let last_len =
        crate::record::HEADER + crate::record::encode_entry(full.entries.last().unwrap()).len();
    let mut names = segs.clone();
    names.push(HARD_STATE.to_owned());
    for name in &names {
        let len = base.contents(name).unwrap().len();
        for offset in 0..len {
            let fs = copy(&base);
            fs.flip(name, offset);
            let opened = Store::open(fs, OPTS);
            if name == last_seg && offset >= len - last_len {
                let (_, rec) = opened.unwrap();
                assert_eq!(rec.entries, full.entries[..full.entries.len() - 1]);
            } else {
                assert!(
                    matches!(opened, Err(StoreError::Corrupt { .. })),
                    "{name} byte {offset} opened"
                );
            }
        }
    }
}

fn record(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    crate::record::put_record(&mut out, payload);
    out
}

fn segment_bytes(entries: &[Entry]) -> Vec<u8> {
    entries
        .iter()
        .flat_map(|e| record(&crate::record::encode_entry(e)))
        .collect()
}

/// Catches: an open that accepts files that do not form one log: a first segment not
/// at index 1, a gap between segments, an entry out of order or undecodable behind a
/// good CRC, a segment name that does not parse, a hard state with trailing bytes.
#[test]
fn open_refuses_files_that_do_not_form_one_log() {
    type Files = Vec<(String, Vec<u8>)>;
    let cases: Vec<(&str, Files)> = vec![
        (
            "starts at 2",
            vec![(name(1, 2), segment_bytes(&entries(2..=3, 1)))],
        ),
        (
            "gap",
            vec![
                (name(1, 1), segment_bytes(&entries(1..=2, 1))),
                (name(2, 4), segment_bytes(&entries(4..=4, 1))),
            ],
        ),
        (
            "out of order",
            vec![(name(1, 1), segment_bytes(&[entry(1, 1), entry(3, 1)]))],
        ),
        ("index 0", vec![(name(1, 0), Vec::new())]),
        (
            "unknown kind",
            vec![(name(1, 1), {
                let mut p = crate::record::encode_entry(&entry(1, 1));
                p[16] = 7;
                [record(&p), segment_bytes(&entries(2..=2, 1))].concat()
            })],
        ),
        ("bad name", vec![("1-1.log".to_owned(), Vec::new())]),
        ("no dash", vec![("1.log".to_owned(), Vec::new())]),
        (
            "short first index",
            vec![(format!("{:020}-1.log", 1), Vec::new())],
        ),
        (
            "hard state trailing bytes",
            vec![(HARD_STATE.to_owned(), {
                let mut b = record(&crate::record::encode_hard(hard(1, None)));
                b.push(0);
                b
            })],
        ),
    ];
    for (why, files) in cases {
        let fs = FaultFs::new();
        for (n, b) in &files {
            fs.put(n, b);
        }
        assert!(
            matches!(Store::open(fs, OPTS), Err(StoreError::Corrupt { .. })),
            "{why} opened"
        );
    }
}

fn name(seq: u64, first: u64) -> String {
    format!("{seq:020}-{first:020}.log")
}

/// Catches: a caller's malformed batch written (or stopping the store), a truncation
/// that leaves replaced segments behind or deletes one still in use, an open that keeps
/// a segment a later one replaced whole or a stale temporary hard state, and a
/// rotation that never starts a new segment, and a default segment size other than
/// the documented 64 MiB.
#[test]
fn invalid_batches_rotation_and_cleanup() {
    assert_eq!(Options::default().segment_bytes, 64 << 20);
    let fs = FaultFs::new();
    let (mut store, _) = Store::open(fs.clone(), OPTS).unwrap();
    for bad in [
        vec![],
        entries(2..=2, 1),
        vec![entry(0, 1)],
        vec![entry(1, 1), entry(3, 1)],
    ] {
        assert!(matches!(
            store.persist_entries(&bad),
            Err(StoreError::Invalid(_))
        ));
    }
    let huge = Entry {
        id: LogId::new(Term(1), LogIndex(1)),
        payload: Payload::Command(vec![0; crate::record::MAX_PAYLOAD]),
    };
    assert!(matches!(
        store.persist_entries(&[huge]),
        Err(StoreError::Invalid(_))
    ));
    store.persist_entries(&entries(1..=6, 1)).unwrap();
    store.persist_entries(&entries(7..=8, 1)).unwrap();
    store.persist_entries(&entries(9..=9, 1)).unwrap();
    assert_eq!(
        segments(&fs),
        [name(1, 1), name(2, 7)],
        "rotation after 80 bytes"
    );
    store.persist_entries(&entries(7..=7, 2)).unwrap();
    assert_eq!(
        segments(&fs),
        [name(1, 1), name(3, 7)],
        "segment 2 replaced whole"
    );
    store.persist_entries(&entries(4..=4, 3)).unwrap();
    assert_eq!(segments(&fs), [name(1, 1), name(4, 4)]);
    assert_eq!(store.last_index(), 4);
    drop(store);

    fs.put(&name(9, 2), &segment_bytes(&entries(2..=2, 4)));
    fs.put(&name(10, 1), &segment_bytes(&entries(1..=1, 5)));
    fs.put(HARD_STATE_TMP, b"left over");
    fs.put("unrelated", b"kept");
    let (_, rec) = Store::open(fs.clone(), OPTS).unwrap();
    assert_eq!(rec.entries, entries(1..=1, 5));
    assert_eq!(segments(&fs), [name(10, 1)]);
    assert_eq!(fs.contents(HARD_STATE_TMP), None);
    assert!(fs.contents("unrelated").is_some());
}

/// Catches: an error during open that is swallowed (a store opened over a log it could
/// not read).
#[test]
fn an_error_during_open_is_returned() {
    let base = written();
    base.put(HARD_STATE_TMP, b"x");
    let ops = {
        let probe = copy(&base);
        Store::open(probe.clone(), OPTS).unwrap();
        probe.ops()
    };
    for m in 0..ops {
        let fs = copy(&base);
        fs.fail_at(m, Fault::Error);
        assert!(
            matches!(Store::open(fs, OPTS), Err(StoreError::Io(_))),
            "op {m}"
        );
    }
}
