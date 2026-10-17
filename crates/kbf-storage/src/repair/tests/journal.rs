use std::collections::BTreeMap;

use kbf_store::{Fault, FaultFs, Fs};
use kbf_types::Digest;

use super::{digest, record};
use crate::repair::{JOURNAL, JOURNAL_TMP, Journal, JournalError, Options, RepairRecord};

/// Small enough that the script below compacts.
const OPTS: Options = Options { compact_frames: 4 };

#[derive(Clone, Copy, Debug)]
enum Step {
    Owed(u8),
    Done(u8),
}

/// Owed and settled in an order that owes a blob again after it was settled and
/// compacts twice (at the sixth and the ninth step).
fn script() -> Vec<Step> {
    use Step::{Done, Owed};
    vec![
        Owed(1),
        Owed(2),
        Done(1),
        Owed(3),
        Owed(1),
        Done(2),
        Done(3),
        Owed(4),
        Done(1),
        Done(4),
        Owed(5),
    ]
}

/// What the script has had acknowledged, and what a crash may add or drop.
#[derive(Clone, Debug, Default)]
struct Model {
    /// The debts after every acknowledged step.
    owed: BTreeMap<Digest, RepairRecord>,
    /// Debts settled since the last sync: a crash may bring them back, because a
    /// settled frame is not synced.
    settled_since_sync: BTreeMap<Digest, RepairRecord>,
    in_flight: Option<Step>,
}

impl Model {
    fn acknowledged(&mut self, step: Step) {
        match step {
            Step::Owed(n) => {
                if self.owed.insert(digest(n), record(n)).is_none() {
                    // The sync that made this frame durable made the earlier ones so.
                    self.settled_since_sync.clear();
                }
            }
            Step::Done(n) => {
                if let Some(r) = self.owed.remove(&digest(n)) {
                    self.settled_since_sync.insert(r.digest, r);
                }
            }
        }
    }

    /// Whether `found` is what a crash could leave: every acknowledged debt, except the
    /// one a settle in flight may have written; no debt that was never owed; and of the
    /// debts settled since the last sync, any.
    fn admits(&self, found: &BTreeMap<Digest, RepairRecord>) -> bool {
        let settling = match self.in_flight {
            Some(Step::Done(n)) => Some(digest(n)),
            _ => None,
        };
        let owing = match self.in_flight {
            Some(Step::Owed(n)) => Some(record(n)),
            _ => None,
        };
        let kept = self
            .owed
            .iter()
            .all(|(d, r)| found.get(d) == Some(r) || Some(*d) == settling);
        let nothing_new = found.iter().all(|(d, r)| {
            self.owed.get(d) == Some(r)
                || self.settled_since_sync.get(d) == Some(r)
                || owing == Some(*r)
        });
        kept && nothing_new
    }
}

fn found(journal: &Journal<FaultFs>) -> BTreeMap<Digest, RepairRecord> {
    journal.records().map(|r| (r.digest, *r)).collect()
}

fn play(journal: &mut Journal<FaultFs>, step: Step) -> Result<(), JournalError> {
    match step {
        Step::Owed(n) => journal.owed(record(n)),
        Step::Done(n) => journal.done(&digest(n)),
    }
}

/// Opens a journal on `fs` and plays `steps` until one fails; returns the model, the
/// failing step and its error, and the journal (`None` if the open failed).
fn run(
    fs: &FaultFs,
    steps: &[Step],
) -> (
    Model,
    Option<(usize, JournalError)>,
    Option<Journal<FaultFs>>,
) {
    let mut model = Model::default();
    let Ok(mut journal) = Journal::open(fs.clone(), OPTS) else {
        return (model, None, None);
    };
    for (i, step) in steps.iter().enumerate() {
        if let Err(err) = play(&mut journal, *step) {
            model.in_flight = Some(*step);
            return (model, Some((i, err)), Some(journal));
        }
        model.acknowledged(*step);
    }
    (model, None, Some(journal))
}

fn clean_ops() -> u64 {
    let fs = FaultFs::new();
    let (model, err, journal) = run(&fs, &script());
    assert!(err.is_none() && model.in_flight.is_none());
    assert_eq!(found(&journal.expect("opened")), model.owed);
    assert_eq!(
        model.owed.keys().copied().collect::<Vec<_>>(),
        [digest(5)],
        "the script ends owing one blob"
    );
    fs.ops()
}

/// A copy of `fs` (whose bytes are all durable) to crash separately.
fn copy(fs: &FaultFs) -> FaultFs {
    fs.crash(usize::MAX)
}

/// Opens what a crash left, checks it against the model, owes one more blob and checks
/// that a power cut after that keeps it.
fn recover_and_extend(disk: &FaultFs, model: &Model, why: &str) -> BTreeMap<Digest, RepairRecord> {
    let live = copy(disk);
    let mut journal =
        Journal::open(live.clone(), OPTS).unwrap_or_else(|e| panic!("{why}: open failed: {e}"));
    let recovered = found(&journal);
    assert!(
        model.admits(&recovered),
        "{why}: recovered {recovered:?}\nmodel {model:?}"
    );
    journal.owed(record(9)).expect("owe after recovery");
    let after = Journal::open(live.crash(0), OPTS)
        .unwrap_or_else(|e| panic!("{why}: reopen after an append failed: {e}"));
    let mut want = recovered.clone();
    want.insert(digest(9), record(9));
    assert_eq!(found(&after), want, "{why}: owed after recovery");
    recovered
}

/// Catches (each a mutant that turns this red): `owed` that returns before its sync (an
/// acknowledged debt is gone after the power cut); a compaction that renames before it
/// syncs the new file, or skips the directory sync (a debt is lost, or an old file
/// comes back); an open that accepts a torn frame or cuts past the torn tail (a debt
/// that was never owed, or an acknowledged one lost); an open that leaves the torn bytes
/// in place (the debt owed after recovery lands behind garbage and the reopen fails); an
/// open that returns unsynced bytes without syncing them.
///
/// The script is crashed at every operation. Each crash is materialized as a power cut
/// with every count of torn bytes kept, with and without the name changes since the last
/// directory sync; each recovered journal is then crashed at every operation of its own
/// open; and the disk is also read as a process death followed by a power cut.
#[test]
fn crash_at_every_operation_and_tear_at_every_byte() {
    let total = clean_ops();
    let mut cases = 0;
    for n in 0..total {
        let fs = FaultFs::new();
        fs.fail_at(n, Fault::Crash);
        let (model, _, _) = run(&fs, &script());
        let restarted = fs.restart();
        let journal = Journal::open(restarted.clone(), OPTS).expect("open after a restart");
        assert!(model.admits(&found(&journal)), "restart after op {n}");
        let again = Journal::open(restarted.crash(0), OPTS).expect("open after a power cut");
        assert_eq!(found(&again), found(&journal), "power cut after op {n}");
        for keep in 0..=fs.unsynced() {
            for names in [0, u64::MAX] {
                let disk = fs.crash_reordered(keep, names);
                let why = format!("crash at op {n}, {keep} torn bytes kept, names {names:x}");
                let recovered = recover_and_extend(&disk, &model, &why);
                let open_ops = {
                    let probe = copy(&disk);
                    Journal::open(probe.clone(), OPTS).expect("probe open");
                    probe.ops()
                };
                for m in 0..open_ops {
                    let second = copy(&disk);
                    second.fail_at(m, Fault::Crash);
                    assert!(Journal::open(second.clone(), OPTS).is_err());
                    for keep2 in 0..=second.unsynced() {
                        let reopened = Journal::open(second.crash(keep2), OPTS)
                            .unwrap_or_else(|e| panic!("{why}, open crashed at {m}: {e}"));
                        assert_eq!(
                            found(&reopened),
                            recovered,
                            "{why}, open crashed at {m}, {keep2} kept"
                        );
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(cases > 100, "only {cases} cases");
}

/// Catches: a journal that ignores a write or sync error (it returns `Ok`), hides which
/// error it was (the I/O error is the source), or keeps writing after one (a retried
/// fsync can succeed on bytes the kernel dropped). Every operation of the script fails
/// once in turn: the call that hit it fails, every later call fails with `Failed`
/// though the disk works again, and the disk after a crash holds what the model admits.
#[test]
fn an_error_at_any_operation_stops_the_journal() {
    let total = clean_ops();
    let steps = script();
    let mut stopped = 0;
    for n in 0..total {
        let fs = FaultFs::new();
        fs.fail_at(n, Fault::Error);
        let (model, err, journal) = run(&fs, &steps);
        let Some(mut journal) = journal else {
            continue; // the error hit the open
        };
        let (failed, err) = err.unwrap_or_else(|| panic!("an error at op {n} was not reported"));
        assert!(matches!(err, JournalError::Io(_)), "op {n}: {err}");
        let cause = std::error::Error::source(&err).map(ToString::to_string);
        assert_eq!(cause, Some(format!("injected error at op {n}")));
        stopped += 1;
        for step in &steps[failed + 1..] {
            let r = play(&mut journal, *step);
            assert!(matches!(r, Err(JournalError::Failed)), "op {n}: {r:?}");
        }
        for keep in 0..=fs.unsynced() {
            let disk = Journal::open(fs.crash(keep), OPTS).expect("open after a power cut");
            assert!(model.admits(&found(&disk)), "op {n}, keep {keep}");
        }
    }
    assert!(stopped > 20, "only {stopped} errors reached a call");
}

/// A journal written by the script, whose durable bytes are all it wrote.
fn written(steps: &[Step]) -> FaultFs {
    let fs = FaultFs::new();
    let (_, err, _) = run(&fs, steps);
    assert!(err.is_none());
    copy(&fs)
}

/// Catches: an open that accepts a damaged frame in the middle of the file by cutting
/// the file there as if it were a torn tail (the debts after it would vanish), and a
/// damaged magic taken as a new file. Every byte of the file is flipped in turn: only a
/// flip inside the very last frame may open, and then the file is the whole file minus
/// that frame.
#[test]
fn damage_anywhere_but_the_last_frame_refuses_open() {
    use Step::{Done, Owed};
    let base = written(&[Owed(1), Owed(2), Done(1), Owed(3)]);
    let bytes = base.contents(JOURNAL).expect("the journal");
    let last = 8 + 57;
    for offset in 0..bytes.len() {
        let fs = copy(&base);
        fs.flip(JOURNAL, offset);
        let opened = Journal::open(fs, OPTS);
        if offset >= bytes.len() - last {
            let journal = opened.expect("a torn last frame opens");
            assert_eq!(
                journal.records().map(|r| r.digest).collect::<Vec<_>>(),
                [digest(2)]
            );
        } else {
            assert!(
                matches!(opened, Err(JournalError::Corrupt { .. })),
                "byte {offset} opened"
            );
        }
    }
}

/// A frame as the journal writes it, around any payload.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(payload.len()).expect("small").to_le_bytes());
    out.extend_from_slice(&crc32c::crc32c(payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn with_tail(tail: &[u8]) -> FaultFs {
    let fs = FaultFs::new();
    fs.put(JOURNAL, &[b"KBFRPJ01".as_slice(), tail].concat());
    fs
}

fn open_error(fs: FaultFs) -> JournalError {
    match Journal::open(fs, OPTS) {
        Ok(_) => panic!("opened"),
        Err(e) => e,
    }
}

/// Catches: an open that accepts what is not a journal or does not form one: a magic
/// that differs, a file shorter than the magic that is not the start of it, a whole
/// frame of a record this code does not know (a newer writer's: cutting it as a torn
/// tail would forget debts), and a known record of the wrong length.
#[test]
fn open_refuses_files_that_are_not_this_journal() {
    let fs = FaultFs::new();
    fs.put(JOURNAL, b"KBFRPJ02");
    assert!(matches!(
        open_error(fs),
        JournalError::Corrupt { at: 0, .. }
    ));
    let fs = FaultFs::new();
    fs.put(JOURNAL, b"KBFX");
    assert!(matches!(
        open_error(fs),
        JournalError::Corrupt { at: 0, .. }
    ));
    let fs = FaultFs::new();
    fs.put(JOURNAL, b"KBFRPJ01 and more");
    let _ = Journal::open(fs.clone(), OPTS).expect("a magic then a torn tail opens");
    assert_eq!(fs.contents(JOURNAL).expect("file"), b"KBFRPJ01");
    let mut unknown = vec![9];
    unknown.extend_from_slice(&[0; 40]);
    assert!(matches!(
        open_error(with_tail(&frame(&unknown))),
        JournalError::Corrupt { at: 8, .. }
    ));
    let mut short_owed = vec![1];
    short_owed.extend_from_slice(&[0; 40]);
    assert!(matches!(
        open_error(with_tail(&frame(&short_owed))),
        JournalError::Corrupt { at: 8, .. }
    ));
    let mut long_done = vec![2];
    long_done.extend_from_slice(&[0; 56]);
    assert!(matches!(
        open_error(with_tail(&frame(&long_done))),
        JournalError::Corrupt { at: 8, .. }
    ));
}

/// Catches: a frame length taken at its word. A length of zero or over the largest
/// payload is not a frame, whatever bytes follow, and as the tail of the file it is torn,
/// not a reason to read past the end or allocate by it.
#[test]
fn an_impossible_frame_length_is_a_torn_tail() {
    for len in [0u32, 58, u32::MAX] {
        let mut tail = len.to_le_bytes().to_vec();
        tail.extend_from_slice(&[0; 4]);
        tail.extend_from_slice(&[7; 20]);
        let fs = with_tail(&tail);
        let journal = Journal::open(fs.clone(), OPTS).expect("a torn tail opens");
        assert!(journal.is_empty());
        assert_eq!(
            fs.contents(JOURNAL).expect("file"),
            b"KBFRPJ01",
            "len {len}"
        );
    }
}

/// Catches: an open that cannot start from a file a crash cut short while it was being
/// created: empty, or part of the magic. It is rewritten, and a debt owed afterwards
/// survives a power cut.
#[test]
fn a_file_cut_short_at_creation_is_started_again() {
    for stub in [&b""[..], &b"KBFR"[..]] {
        let fs = FaultFs::new();
        fs.put(JOURNAL, stub);
        let mut journal = Journal::open(fs.clone(), OPTS).expect("open a stub");
        assert!(journal.is_empty());
        journal.owed(record(1)).expect("owed");
        let reopened = Journal::open(fs.crash(0), OPTS).expect("reopen");
        assert_eq!(found(&reopened).len(), 1);
    }
}

/// Catches: a leftover compaction file that stays (it would be read as a journal one
/// day) or is taken for the journal.
#[test]
fn a_leftover_compaction_file_is_removed_at_open() {
    let fs = FaultFs::new();
    fs.put(JOURNAL_TMP, b"half a compaction");
    Journal::open(fs.clone(), OPTS).expect("open");
    assert_eq!(fs.contents(JOURNAL_TMP), None);
    assert!(!fs.list().expect("list").contains(&JOURNAL_TMP.to_owned()));
}

/// Catches: a debt that does not survive the primary restarting (owed returns before
/// the disk has it), one owed twice that is written twice or whose record is replaced
/// (the first replica named is kept), a settle of what was never owed that grows the
/// file, a debt settled and owed again that is not owed, and a settled debt that is
/// still reported.
#[test]
fn owed_survives_a_restart_and_settling_is_exact() {
    let fs = FaultFs::new();
    let mut journal = Journal::open(fs.clone(), Options::default()).expect("open");
    assert!(journal.is_empty());
    journal.owed(record(1)).expect("owe 1");
    let one = fs.contents(JOURNAL).expect("file").len();
    let mut again = record(1);
    again.replica = kbf_map::MemberId(99);
    journal.owed(again).expect("owe 1 again");
    journal
        .done(&digest(7))
        .expect("settle what was never owed");
    assert_eq!(
        fs.contents(JOURNAL).expect("file").len(),
        one,
        "nothing written"
    );
    journal.owed(record(2)).expect("owe 2");
    assert!(journal.is_owed(&digest(1)) && journal.is_owed(&digest(2)));
    assert_eq!(journal.len(), 2);

    // The primary dies and the machine loses power: only what was synced is left.
    let restarted = Journal::open(fs.crash(0), Options::default()).expect("reopen");
    assert_eq!(
        restarted.records().copied().collect::<Vec<_>>(),
        [record(1), record(2)],
        "both debts, the first record of blob 1"
    );
    journal.done(&digest(1)).expect("settle 1");
    assert!(!journal.is_owed(&digest(1)));
    journal.owed(record(1)).expect("owe 1 once more");
    assert!(journal.is_owed(&digest(1)));
}

/// Catches: a journal that grows without bound as debts come and go (no compaction), a
/// compaction that runs when most of the file is still debts, one that drops a debt or
/// keeps a settled one, and one that is not durable once `done` returns (the rename or
/// its directory sync skipped): a power cut after it must read exactly the debts owed.
#[test]
fn compaction_keeps_exactly_the_debts_and_is_durable() {
    let fs = FaultFs::new();
    let mut journal = Journal::open(fs.clone(), OPTS).expect("open");
    for n in 1..=4 {
        journal.owed(record(n)).expect("owe");
    }
    journal.done(&digest(2)).expect("settle");
    assert_eq!(
        fs.contents(JOURNAL).expect("file").len(),
        8 + 4 * 65 + 49,
        "five frames and three debts: not worth rewriting"
    );
    for round in 0..20 {
        let n = 10 + round;
        journal.owed(record(n)).expect("owe");
        journal.done(&digest(n)).expect("settle");
    }
    let len = fs.contents(JOURNAL).expect("file").len();
    assert!(len < 8 + 10 * 65, "the file is {len} bytes");
    assert_eq!(fs.contents(JOURNAL_TMP), None, "no compaction file is left");
    // The settled frames are not synced, so the debt that makes the next sync is the
    // one after which a power cut must read exactly the debts owed. It is appended to
    // the file the compaction renamed into place, which is why that must be durable.
    journal.owed(record(5)).expect("owe after compaction");
    let after_power_cut = Journal::open(fs.crash(0), OPTS).expect("reopen");
    assert_eq!(
        after_power_cut.records().copied().collect::<Vec<_>>(),
        [record(1), record(3), record(4), record(5)]
    );
}
