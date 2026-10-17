//! `RaftMetaLog`, the metadata log `kbf-server` runs, over an in-memory raft
//! directory (`FaultFs`) that a test can restart, crash at any operation, or fill by
//! hand. Each test names the defect it catches.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_front::{MetaLog, MetaLogError};
use kbf_log::{Bootstrap, DecodeError, FarmLogId, Format, LogCommand, MachineVersion, Takeover};
use kbf_meta::{
    ActionAnswer, ActionRecord, Applied, BlobAnswer, BlobWrite, Closure, Command, Epoch, Evicted,
    Location, MetaState, NodeWriter, ObjectId, Retention, Role, ServerId, StagedObject, StagedPut,
    StoreId, TierId, UnreachableReason, Writer,
};
use kbf_raft::{Entry, LogId, LogIndex, Payload, Snapshot, Storage};
use kbf_server::raft_log::{RECORD, RECORD_TMP, TICK};
use kbf_server::{RaftLogError, RaftMetaLog};
use kbf_store::{Fault, FaultFile, FaultFs, Fs, Options, Store};
use kbf_types::{Digest, DigestFunction, FarmTime};

type Log = RaftMetaLog<FaultFs>;

fn id(n: u64) -> FarmLogId {
    FarmLogId::new(NonZeroU64::new(n).expect("non-zero"))
}

async fn open_as(fs: &FaultFs, root: &str, new: u64) -> Result<Log, RaftLogError> {
    RaftMetaLog::open(fs.clone(), Retention::default(), root, move || Ok(id(new))).await
}

async fn open(fs: &FaultFs, new: u64) -> Log {
    open_as(fs, "kbf/", new).await.expect("open the log")
}

/// The error of a start over `fs` that must be refused.
async fn refusal(fs: &FaultFs, new: u64) -> RaftLogError {
    match open_as(fs, "kbf/", new).await {
        Ok(log) => panic!("opened {log:?}"),
        Err(e) => e,
    }
}

async fn epoch(log: &Log) -> u64 {
    match log.commit(Command::AllocEpoch).await {
        Ok(Applied::Epoch(e)) => e.get(),
        other => panic!("AllocEpoch answered {other:?}"),
    }
}

fn put_blob(epoch: u64) -> Command {
    Command::PutBlob {
        digest: Digest::new(DigestFunction::Sha256, [7; 32], 7),
        location: Location {
            store: StoreId::new(0),
            object: ObjectId::new(Epoch::new(epoch), 1),
            offset: 0,
        },
    }
}

/// Catches: a new log whose id is not the one drawn, or is not recorded before the
/// store writes; a record in another form than four exact lines; a log that commits no
/// `Bootstrap`, or a prefix other than `<root><log id>/`; a log that counts other
/// voters than its one (the start-up pass raises the machine version itself only in a
/// single-voter cell).
#[tokio::test]
async fn a_new_log_records_its_id_and_commits_one_bootstrap() {
    let fs = FaultFs::new();
    let log = open(&fs, 0x2a).await;
    assert_eq!(log.log_id(), id(0x2a));
    assert_eq!(log.voters(), 1);
    assert_eq!(log.store_prefix().as_str(), "kbf/000000000000002a/");
    assert_eq!(log.bootstraps(), 1);
    let record = fs.contents(RECORD).expect("a record");
    assert_eq!(
        String::from_utf8(record).expect("text"),
        "kbf raft log record 1\nlog_id 000000000000002a\nserver 1\nvoters 1\n"
    );
    assert_eq!(fs.contents(RECORD_TMP), None);
    assert!(format!("{log:?}").contains("RaftMetaLog"));
}

/// Catches: a start that draws a new log id although the directory records one (the
/// mutant "draw a new log id on every start": the restart then names another log, and
/// either commits a second `Bootstrap` or refuses its own log), and a restart that
/// applies an entry twice or loses one (the second `AllocEpoch` after the restart
/// must be exactly one past the first).
#[tokio::test]
async fn a_restart_keeps_the_log_id_and_applies_each_entry_once() {
    let fs = FaultFs::new();
    let log = open(&fs, 5).await;
    let first = epoch(&log).await;
    assert_eq!(
        log.commit(put_blob(first)).await,
        Ok(Applied::Blob(Ok(BlobWrite::Stored)))
    );
    drop(log);

    let log = open(&fs.restart(), 6).await;
    assert_eq!(log.log_id(), id(5), "the restart drew a new log id");
    assert_eq!(log.bootstraps(), 1, "a second Bootstrap was committed");
    assert_eq!(log.query(|m| m.blob_count()).await, Ok(1));
    assert_eq!(
        epoch(&log).await,
        first + 1,
        "an entry applied twice, or lost"
    );
}

/// Catches: a log that answers a commit before it is durable (the store's entry
/// sync skipped, or the answer given before the persist): a crash right after the
/// answer, at any point the disk could be in, loses an epoch the caller was given, and
/// the next start hands it out again. And one that applies a committed entry twice
/// after a crash. The process is crashed at every filesystem operation of one commit;
/// each crash is then a process kill (everything written survives) and a power cut
/// (only what was synced does).
#[tokio::test]
async fn a_commit_answered_survives_a_crash_and_is_applied_once() {
    let mut crashed_mid_commit = 0;
    for at in 0.. {
        let fs = FaultFs::new();
        let log = open(&fs, 1).await;
        let before = epoch(&log).await;
        let start = fs.ops();
        fs.fail_at(start + at, Fault::Crash);
        let answered = log.commit(Command::AllocEpoch).await;
        let reached = fs.ops() > start + at;
        drop(log);
        for (keep, names) in [(usize::MAX, u64::MAX), (0, 0)] {
            let reopened = open(&fs.crash_reordered(keep, names), 9).await;
            let next = epoch(&reopened).await;
            match &answered {
                Ok(Applied::Epoch(e)) => {
                    assert_eq!(
                        next,
                        e.get() + 1,
                        "crash at op {at}: a committed epoch lost or doubled"
                    );
                }
                Ok(other) => panic!("AllocEpoch answered {other:?}"),
                Err(_) => assert!(
                    next == before + 1 || next == before + 2,
                    "crash at op {at}: next epoch {next} after {before}"
                ),
            }
        }
        if !reached {
            break; // the commit finished before the planned crash
        }
        crashed_mid_commit += 1;
    }
    assert!(crashed_mid_commit >= 2, "a commit wrote and synced");
}

/// What a start over a crashed first open found.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// The recorded log, as the crashed open drew it.
    Recorded,
    /// No log: the start bootstrapped a new one.
    Fresh,
}

/// The record bytes [`a_new_log_records_its_id_and_commits_one_bootstrap`] pins, for `n`.
fn record_of(n: u64) -> String {
    format!("kbf raft log record 1\nlog_id {n:016x}\nserver 1\nvoters 1\n")
}

/// Starts over `dir`, which a first open of log 1 left; a new log would be log 2.
/// Anything but the recorded log or a fresh one, with its record written whole, panics.
async fn start_after(dir: &FaultFs, at: u64, cut: &str) -> Found {
    let log = match open_as(dir, "kbf/", 2).await {
        Ok(log) => log,
        Err(e) => panic!("crash at op {at}, {cut}: the next start is refused: {e}"),
    };
    let n = log.log_id().get();
    let found = match n {
        1 => Found::Recorded,
        2 => Found::Fresh,
        other => panic!("crash at op {at}, {cut}: log id {other:x}"),
    };
    assert_eq!(log.bootstraps(), 1, "crash at op {at}, {cut}");
    assert_eq!(
        log.store_prefix().as_str(),
        format!("kbf/{n:016x}/"),
        "crash at op {at}, {cut}"
    );
    assert_eq!(
        dir.contents(RECORD).map(String::from_utf8),
        Some(Ok(record_of(n))),
        "crash at op {at}, {cut}: the record is not whole"
    );
    found
}

/// Catches: a log record written where it is read (created as `log-record` and then
/// filled, in one append or several), not to a temporary file renamed over it, or
/// renamed before its bytes are synced: a crash during the first start can leave an
/// empty or torn record, which every later start refuses, so the directory never opens
/// again. And a record whose name is not durable before the store's first file is: a
/// cut that keeps the store's files but not the record leaves a directory "holding
/// files but no log-record" (at this version `Store::open` syncs the directory before
/// it creates a file, so the record's own directory sync is not what keeps this).
/// The first open is crashed at every filesystem operation; each crash is then a
/// process kill and every power cut (each torn tail of each unsynced file, with each
/// subset of the name changes since the last directory sync). Every start after it
/// must find exactly the recorded log or no log at all (and then bootstrap log 2),
/// with its record whole and one `Bootstrap` naming it.
#[tokio::test]
async fn a_crash_during_the_first_open_leaves_the_recorded_log_or_none() {
    let mut crashed = 0;
    let mut recorded_mid_open = 0;
    for at in 0.. {
        let fs = FaultFs::new();
        fs.fail_at(at, Fault::Crash);
        let first = open_as(&fs, "kbf/", 1).await;
        let reached = fs.ops() > at;
        assert_eq!(first.is_ok(), !reached, "crash at op {at}: {first:?}");
        drop(first);

        start_after(&fs.restart(), at, "process kill").await;
        let pending = fs.pending_names();
        assert!(pending < 16, "crash at op {at}: {pending} name changes");
        for keep in 0..=fs.unsynced() {
            for names in 0..(1u64 << pending) {
                let cut = format!("keep {keep}, names {names:b}");
                let found = start_after(&fs.crash_reordered(keep, names), at, &cut).await;
                if reached && keep == 0 && names == 0 && found == Found::Recorded {
                    recorded_mid_open += 1;
                }
            }
        }
        if !reached {
            let after = start_after(&fs.crash(0), at, "a power cut after the open").await;
            assert_eq!(
                after,
                Found::Recorded,
                "an open that finished lost its record"
            );
            break; // the open finished before the planned crash
        }
        crashed += 1;
    }
    // The loop crossed the record's write: some crashes come before it and some after.
    assert!(
        crashed > recorded_mid_open,
        "no crash before the record is durable"
    );
    assert!(
        recorded_mid_open > 0,
        "no crash after the record is durable"
    );
}

/// Catches: farm time taken from the process clock (it starts again from zero at each
/// open, so it goes back across a restart), a tick that commits nothing, and a log
/// with no tick task (farm time never committed unless a caller ticks).
#[tokio::test(start_paused = true)]
async fn farm_time_continues_from_the_committed_one_across_a_restart() {
    let fs = FaultFs::new();
    let log = open(&fs, 1).await;
    assert_eq!(log.query(|m| m.now()).await, Ok(FarmTime::default()));
    tokio::time::sleep(Duration::from_secs(30)).await;
    let committed = log.tick().await.expect("a tick");
    assert!(committed >= FarmTime::from_millis(30_000), "{committed:?}");
    assert_eq!(log.query(|m| m.now()).await, Ok(committed));
    drop(log);

    let log = open(&fs.restart(), 1).await;
    assert!(
        log.now() >= committed,
        "farm time went back across the restart"
    );
    assert!(log.query(|m| m.now()).await.expect("a query") >= committed);
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let ticked = log.query(|m| m.now()).await.expect("a query");
    assert!(
        ticked >= committed.saturating_add(Duration::from_secs(3)),
        "no tick task: {ticked:?} after {committed:?}"
    );
}

/// Catches: a tick committed as the log opens (an `interval` whose first tick is
/// immediate). Farm time then is the base the open just applied, so the entry carries
/// nothing, and it lands while the opener's first commit runs, moving the applied
/// index under a caller that reads it (it read 4 for 3 in the test above).
/// And a tick task that never ticks: the first tick lands a [`TICK`] after the open.
#[tokio::test(start_paused = true)]
async fn the_first_tick_is_one_tick_after_the_open() {
    let fs = FaultFs::new();
    let log = open(&fs, 7).await;
    let opened = log.applied_index();
    tokio::time::sleep(TICK - Duration::from_millis(1)).await;
    assert_eq!(log.applied_index(), opened, "a tick committed at the open");
    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(
        log.applied_index(),
        opened + 1,
        "no tick a TICK after the open"
    );
}

/// Catches: a start whose prefix root changed being given the new prefix silently (its
/// objects would land outside the log's prefix), and a record that names another log
/// than the one in the store being accepted.
#[tokio::test]
async fn a_start_that_disagrees_with_the_committed_bootstrap_is_refused() {
    let fs = FaultFs::new();
    drop(open(&fs, 3).await);
    let refused = open_as(&fs.restart(), "other/", 4).await;
    assert!(
        matches!(&refused, Err(RaftLogError::Mismatch { committed: Some(b), .. })
            if b.store_prefix == "kbf/0000000000000003/"),
        "{refused:?}"
    );
    assert!(refused.unwrap_err().to_string().contains("prefix changed"));

    let moved = fs.restart();
    moved.put(
        RECORD,
        b"kbf raft log record 1\nlog_id 0000000000000009\nserver 1\nvoters 1\n",
    );
    let refused = refusal(&moved, 4).await;
    assert!(
        matches!(&refused, RaftLogError::Mismatch { expected, .. } if expected.log_id == id(9)),
        "{refused:?}"
    );
}

/// Catches: a record parsed loosely (a log id in another form, a voter set this
/// version does not run, a damaged header) instead of refused; a directory holding
/// someone else's files taken as a new log; and a leftover temporary record (a create
/// that died before its rename) refusing the start instead of being removed.
#[tokio::test]
async fn a_record_is_read_exactly_or_refused() {
    for bad in [
        "kbf raft log record 1\nlog_id 000000000000002A\nserver 1\nvoters 1\n",
        "kbf raft log record 1\nlog_id 2a\nserver 1\nvoters 1\n",
        "kbf raft log record 1\nlog_id 0000000000000000\nserver 1\nvoters 1\n",
        "kbf raft log record 1\nlog_id 000000000000002a\nserver 1\nvoters 1 2\n",
        "kbf raft log record 1\nlog_id zzzzzzzzzzzzzzzz\nserver 1\nvoters 1\n",
        "kbf raft log record 1\nlog_id 000000000000002a",
        "kbf raft log record 2\nlog_id 000000000000002a\nserver 1\nvoters 1\n",
        "\u{fffd}",
    ] {
        let fs = FaultFs::new();
        fs.put(RECORD, bad.as_bytes());
        let refused = refusal(&fs, 1).await;
        assert!(
            matches!(refused, RaftLogError::Record),
            "{bad:?}: {refused:?}"
        );
        assert!(refused.to_string().contains(RECORD));
    }
    let mut bytes = b"kbf raft log record 1\nlog_id ".to_vec();
    bytes.push(0xff);
    let fs = FaultFs::new();
    fs.put(RECORD, &bytes);
    assert!(matches!(refusal(&fs, 1).await, RaftLogError::Record));

    let fs = FaultFs::new();
    fs.put("stray", b"someone else's");
    let refused = refusal(&fs, 1).await;
    assert!(matches!(refused, RaftLogError::NotEmpty), "{refused:?}");
    assert!(refused.to_string().contains("no log-record"));

    let fs = FaultFs::new();
    fs.put(RECORD_TMP, b"half a rec");
    let log = open(&fs, 8).await;
    assert_eq!(log.log_id(), id(8));
    assert_eq!(fs.contents(RECORD_TMP), None);
}

/// Catches: a committed entry that does not decode being skipped (this replica would
/// then differ from every other) instead of refusing the open; a machine that goes on
/// applying after one (the refusal would name a later entry than the first bad one);
/// and a directory holding a snapshot (which this version never writes) opened as an
/// empty state.
#[tokio::test]
async fn an_entry_or_snapshot_this_version_cannot_read_refuses_the_open() {
    let fs = FaultFs::new();
    drop(open(&fs, 2).await);
    let dir = fs.restart();
    let (mut store, held) = Store::open(dir.clone(), Options::default()).expect("the store");
    let last = held.entries.last().expect("entries").id;
    let bad = |index: LogIndex| Entry {
        id: LogId::new(last.term, index),
        payload: Payload::Command(vec![0xff]),
    };
    let first_bad = last.index.next();
    store
        .write_entries(&[bad(first_bad), bad(first_bad.next())])
        .expect("append");
    drop(store);
    let refused = refusal(&dir.restart(), 2).await;
    assert!(
        matches!(&refused, RaftLogError::Decode { index, .. } if *index == last.index.0 + 1),
        "{refused:?}"
    );
    assert!(refused.to_string().contains("entry"));

    let dir = fs.restart();
    let (mut store, held) = Store::open(dir.clone(), Options::default()).expect("the store");
    let snapshot = Snapshot {
        base: held.entries.last().expect("entries").id,
        state: Vec::new(),
    };
    store.write_snapshot(&snapshot).expect("a snapshot");
    drop(store);
    let refused = refusal(&dir.restart(), 2).await;
    assert!(matches!(refused, RaftLogError::Open(_)), "{refused:?}");
    assert!(refused.to_string().contains("snapshot"));
}

/// Catches: an applied index read from the Raft core instead of the metadata state.
/// The core counts an entry applied when it hands the host the effect, so after a
/// storage error part-way through a commit the core's index names an entry the state
/// never applied (kbf-sim-cell's durability check would then see a phantom entry).
/// The clock is paused, so the tick task commits nothing between the reads: its first
/// tick is a [`TICK`] after the open, and the clock does not move while a commit runs.
#[tokio::test(start_paused = true)]
async fn the_applied_index_is_the_state_s_and_stops_at_a_failed_commit() {
    let fs = FaultFs::new();
    let log = open(&fs, 6).await;
    let opened = log.applied_index();
    // The blank entry of the term and the Bootstrap.
    assert_eq!(opened, 2);
    epoch(&log).await;
    assert_eq!(log.applied_index(), opened + 1);
    fs.fail_at(fs.ops(), Fault::Error);
    let refused = log.commit(Command::AllocEpoch).await;
    assert!(
        matches!(refused, Err(MetaLogError::Unavailable(_))),
        "{refused:?}"
    );
    assert_eq!(log.applied_index(), opened + 1);
}

/// Catches: a control command in the metadata log applied as a no-op (or skipped)
/// instead of refusing the open. This binary writes none; a log holding one was written
/// by a later binary whose farm state this one would silently drop.
#[tokio::test]
async fn a_control_command_in_the_metadata_log_refuses_the_open() {
    let fs = FaultFs::new();
    drop(open(&fs, 2).await);
    let dir = fs.restart();
    let (mut store, held) = Store::open(dir.clone(), Options::default()).expect("the store");
    let last = held.entries.last().expect("entries").id;
    let takeover = LogCommand::Takeover(Takeover {
        term: NonZeroU64::MIN,
        now: FarmTime::from_millis(1),
    });
    let bytes = kbf_log::encode(Format::V1, &takeover).expect("encode");
    let control = Entry {
        id: LogId::new(last.term, last.index.next()),
        payload: Payload::Command(bytes),
    };
    store.write_entries(&[control]).expect("append");
    drop(store);
    let refused = refusal(&dir.restart(), 2).await;
    assert!(
        matches!(&refused, RaftLogError::Decode { index, .. } if *index == last.index.0 + 1),
        "{refused:?}"
    );
}

/// Catches: a write-tier command in the metadata log applied, or skipped, by this
/// binary, whose log is format 1 (docs/design/write-path.md section 9, "a log holding
/// each new variant, opened by a reader without it, is refused"). Each of the five
/// commands, alone at the end of the log, refuses the open with the entry's index.
/// Mutant: the machine decodes under format 2.
#[tokio::test]
async fn a_write_tier_command_in_a_format_1_log_refuses_the_open() {
    let object = ObjectId::new(Epoch::new(1), 1);
    let tier = kbf_meta::TierId::new(1);
    let commands = [
        Command::AddTier {
            server: kbf_meta::ServerId::new(1),
            tier,
        },
        Command::PutStaged {
            tier,
            objects: Vec::new(),
        },
        Command::SealStaged { object },
        Command::ObjectFlushed { object },
        Command::FlushConflict { object },
    ];
    for command in commands {
        let fs = FaultFs::new();
        drop(open(&fs, 2).await);
        let dir = fs.restart();
        let (mut store, held) = Store::open(dir.clone(), Options::default()).expect("the store");
        let last = held.entries.last().expect("entries").id;
        let bytes =
            kbf_log::encode(Format::V2, &LogCommand::Meta(command.clone())).expect("encode");
        let entry = Entry {
            id: LogId::new(last.term, last.index.next()),
            payload: Payload::Command(bytes),
        };
        store.write_entries(&[entry]).expect("append");
        drop(store);
        let refused = refusal(&dir.restart(), 2).await;
        assert!(
            matches!(&refused, RaftLogError::Decode { index, .. } if *index == last.index.0 + 1),
            "{command:?}: {refused:?}"
        );
    }
}

fn add_tier() -> Command {
    Command::AddTier {
        server: ServerId::new(1),
        tier: TierId::new(1),
    }
}

/// Appends `command` to the log `fs` holds, as the next entry of its last term; returns
/// the entry's index.
fn append(fs: &FaultFs, format: Format, command: &LogCommand) -> u64 {
    let (mut store, held) = Store::open(fs.clone(), Options::default()).expect("the store");
    let last = held.entries.last().expect("entries").id;
    let entry = Entry {
        id: LogId::new(last.term, last.index.next()),
        payload: Payload::Command(kbf_log::encode(format, command).expect("encode")),
    };
    store.write_entries(&[entry]).expect("append");
    last.index.0 + 1
}

/// Catches (docs/design/metadata-snapshots.md section 4, "PR 4 adds the raise"): a raise
/// the machine does not apply (the write tier's commands stay refused: mutant "the
/// machine keeps decoding format 1"), a raise committed again when the log is already
/// there (mutant "commit the entry on every call"), a version lowered by a later call,
/// and a raise a restart forgets (the format must come from the log, not the process).
/// Before the raise a tier command is refused without stopping the log. The clock is
/// paused, so no tick lands between the reads of the applied index.
#[tokio::test(start_paused = true)]
async fn raising_the_machine_version_to_2_lets_tier_commands_commit_and_lasts() {
    let fs = FaultFs::new();
    let log = open(&fs, 3).await;
    assert_eq!(log.machine_version(), MachineVersion::V1);
    let refused = log.commit(add_tier()).await;
    assert!(
        matches!(refused, Err(MetaLogError::Unavailable(_))),
        "{refused:?}"
    );
    let before = log.applied_index();
    log.raise_machine_version(MachineVersion::V2)
        .expect("a raise");
    assert_eq!(log.applied_index(), before + 1);
    assert_eq!(log.machine_version(), MachineVersion::V2);
    assert_eq!(log.commit(add_tier()).await, Ok(Applied::Tier(Ok(()))));
    let after = log.applied_index();
    for to in [MachineVersion::V2, MachineVersion::V1] {
        log.raise_machine_version(to).expect("nothing to raise");
    }
    assert_eq!(log.applied_index(), after, "a raise committed twice");
    assert_eq!(log.machine_version(), MachineVersion::V2);
    drop(log);
    let log = open(&fs.restart(), 3).await;
    assert_eq!(log.machine_version(), MachineVersion::V2);
    assert_eq!(log.commit(add_tier()).await, Ok(Applied::Tier(Ok(()))));
    // A raise the disk refuses is refused, and raises nothing.
    let fs = FaultFs::new();
    let log = open(&fs, 5).await;
    fs.fail_at(fs.ops(), Fault::Error);
    let refused = log.raise_machine_version(MachineVersion::V2);
    assert!(
        matches!(refused, Err(MetaLogError::Unavailable(_))),
        "{refused:?}"
    );
    assert_eq!(log.machine_version(), MachineVersion::V1);
}

/// Catches (docs/design/write-tier-scenarios.md, scenario 30): a binary that runs only
/// machine version 1, given a log raised to 2 (a rollback after the tier ran), opening
/// it instead of refusing it at the raise (mutant "a version past what this binary
/// runs is skipped"): it would answer every staged blob absent. The refusal names the
/// raise's index and changes nothing a later open needs. And a raise to a version this
/// binary does not run, asked for or found in the log, refused.
#[tokio::test]
async fn a_log_raised_past_what_a_binary_runs_refuses_its_open() {
    let fs = FaultFs::new();
    let log = open(&fs, 4).await;
    log.raise_machine_version(MachineVersion::V2)
        .expect("a raise");
    let raised = log.applied_index();
    assert_eq!(log.commit(add_tier()).await, Ok(Applied::Tier(Ok(()))));
    let v5 = MachineVersion::new(NonZeroU32::new(5).expect("non-zero"));
    assert!(log.raise_machine_version(v5).is_err());
    assert_eq!(log.machine_version(), MachineVersion::V2);
    drop(log);
    let rolled_back = RaftMetaLog::open_running(
        fs.restart(),
        Retention::default(),
        "kbf/",
        || Ok(id(4)),
        MachineVersion::V1,
    )
    .await;
    assert!(
        matches!(&rolled_back, Err(RaftLogError::Decode { index, .. }) if *index == raised),
        "{rolled_back:?}"
    );
    let unknown =
        RaftMetaLog::open_running(fs.restart(), Retention::default(), "kbf/", || Ok(id(4)), v5)
            .await;
    assert!(
        matches!(&unknown, Err(RaftLogError::Runs(v)) if *v == v5),
        "{unknown:?}"
    );
    let log = open(&fs.restart(), 4).await;
    assert_eq!(log.machine_version(), MachineVersion::V2);
    assert_eq!(log.commit(add_tier()).await, Ok(Applied::Tier(Ok(()))));
    drop(log);
    let dir = fs.restart();
    let later = append(&dir, Format::V2, &LogCommand::MachineVersion(v5));
    let refused = refusal(&dir.restart(), 4).await;
    assert!(
        matches!(&refused, RaftLogError::Decode { index, .. } if *index == later),
        "{refused:?}"
    );
}

/// Catches: a binary whose highest machine version stays 2 (mutant `RUNS = V2`), which
/// could never take `EvictObjects`; and an `EvictObjects` committed into a log below
/// version 3, which a version-2 replica would refuse. The raise lasts across a restart.
#[tokio::test(start_paused = true)]
async fn raising_the_machine_version_to_3_lets_evict_objects_commit_and_lasts() {
    let evict = || Command::EvictObjects {
        objects: [ObjectId::new(Epoch::new(1), 1)].into(),
    };
    let fs = FaultFs::new();
    let log = open(&fs, 6).await;
    for to in [MachineVersion::V1, MachineVersion::V2] {
        log.raise_machine_version(to).expect("a raise");
        let refused = log.commit(evict()).await;
        assert!(
            matches!(refused, Err(MetaLogError::Unavailable(_))),
            "at {to:?}: {refused:?}"
        );
    }
    log.raise_machine_version(MachineVersion::V3)
        .expect("a raise");
    assert_eq!(log.machine_version(), MachineVersion::V3);
    assert_eq!(
        log.commit(evict()).await,
        Ok(Applied::Evicted(Evicted::default()))
    );
    drop(log);
    let log = open(&fs.restart(), 6).await;
    assert_eq!(log.machine_version(), MachineVersion::V3);
    assert_eq!(
        log.commit(evict()).await,
        Ok(Applied::Evicted(Evicted::default()))
    );
}

/// Catches: a binary that does not run machine version 4 (mutant `RUNS = V3`), which
/// could never register a storage node's writer; and a `SetWriter`, or a put of a node
/// writer's object, committed into a log below version 4, which a version-3 replica
/// would refuse. Raised, both commit, and the raise lasts across a restart.
#[tokio::test(start_paused = true)]
async fn raising_the_machine_version_to_4_lets_node_writers_commit_and_lasts() {
    let one = NonZeroU64::MIN;
    let writer = NodeWriter::new(one, one);
    let put = Command::PutBlob {
        digest: blob_digest(1),
        location: Location {
            store: StoreId::new(0),
            object: ObjectId::of(Writer::Node(writer), 1),
            offset: 0,
        },
    };
    let fs = FaultFs::new();
    let log = open(&fs, 7).await;
    for to in [MachineVersion::V1, MachineVersion::V2, MachineVersion::V3] {
        log.raise_machine_version(to).expect("a raise");
        for command in [Command::SetWriter(writer), put.clone()] {
            let refused = log.commit(command).await;
            assert!(
                matches!(refused, Err(MetaLogError::Unavailable(_))),
                "at {to:?}: {refused:?}"
            );
        }
    }
    log.raise_machine_version(MachineVersion::V4)
        .expect("a raise");
    assert_eq!(
        log.commit(Command::SetWriter(writer)).await,
        Ok(Applied::Writer(Ok(())))
    );
    assert_eq!(
        log.commit(put.clone()).await,
        Ok(Applied::Blob(Ok(BlobWrite::Stored)))
    );
    drop(log);
    let log = open(&fs.restart(), 7).await;
    assert_eq!(log.machine_version(), MachineVersion::V4);
    let next = NodeWriter::new(one, one.saturating_add(1));
    assert_eq!(
        log.commit(Command::SetWriter(next)).await,
        Ok(Applied::Writer(Ok(()))),
        "the replayed log remembers boot 1, so boot 2 is the next"
    );
}

fn blob_digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], 7)
}

fn at(epoch: u64, seq: u64, offset: u64) -> Location {
    Location {
        store: StoreId::new(0),
        object: ObjectId::new(Epoch::new(epoch), seq),
        offset,
    }
}

type Answers = (
    [BlobAnswer; 4],
    ActionAnswer,
    Option<TierId>,
    Option<StagedObject>,
    Option<UnreachableReason>,
    MetaState,
);

/// What a reader of the state asks after a restart: the blobs, the action, the tier
/// and the staged object by name, and the whole state, which also holds the marks
/// (whose generations are entry indexes) and the farm time.
fn answers(m: &MetaState, object: ObjectId) -> Answers {
    (
        [1, 2, 3, 4].map(|n| m.blob(&blob_digest(n))),
        m.action(&blob_digest(9)),
        m.tier(ServerId::new(1)),
        m.staged(object).cloned(),
        m.unreachable(ObjectId::new(Epoch::new(1), 7)),
        m.clone(),
    )
}

/// Format 3's first condition, a version-2 log still opens: a log
/// at machine version 2, written by this binary with format-1 and format-2 commands,
/// reopens under this binary (which runs version 3) at version 2, replays to the same
/// answers, and still refuses `EvictObjects` until it is raised explicitly. Catches:
/// an open that raises the log to what the binary runs (mutant: `MetaMachine::new`
/// starts at `runs`; the version reads 3), and a replay that applies an old entry
/// otherwise than it applied live (the whole state is compared; mutant: replayed
/// entries executed at index 0, which moves the mark's generation). After a second
/// restart an explicit raise to 3 changes no answer and lets the evict commit.
#[tokio::test(start_paused = true)]
async fn a_version_2_log_reopens_at_version_2_with_the_same_answers() {
    let evict = || Command::EvictObjects {
        objects: [ObjectId::new(Epoch::new(1), 1)].into(),
    };
    let staged = ObjectId::new(Epoch::new(1), 5);
    let fs = FaultFs::new();
    let log = open(&fs, 7).await;
    let e = epoch(&log).await;
    assert_eq!(e, 1);
    // Format 1: PutBlob, PutBlobs, PutAction, ObjectUnreachable.
    let format_1 = [
        Command::PutBlob {
            digest: blob_digest(1),
            location: at(e, 1, 0),
        },
        Command::PutBlobs(vec![
            (blob_digest(2), at(e, 1, 7)),
            (blob_digest(3), at(e, 2, 0)),
        ]),
        Command::PutAction {
            role: Role::Daemon,
            action: blob_digest(9),
            record: ActionRecord {
                result: blob_digest(1),
                closure: [blob_digest(2)].into_iter().collect::<Closure>(),
            },
        },
        Command::ObjectUnreachable {
            object: ObjectId::new(Epoch::new(e), 7),
            reason: UnreachableReason::Missing,
        },
    ];
    for command in format_1 {
        let applied = log.commit(command.clone()).await;
        assert!(
            matches!(
                applied,
                Ok(Applied::Blob(Ok(_))
                    | Applied::Blobs(Ok(_))
                    | Applied::Action(Ok(()))
                    | Applied::Marked(Ok(())))
            ),
            "{command:?}: {applied:?}"
        );
    }
    log.raise_machine_version(MachineVersion::V2)
        .expect("a raise");
    // Format 2: AddTier, PutStaged, SealStaged.
    assert_eq!(log.commit(add_tier()).await, Ok(Applied::Tier(Ok(()))));
    let put_staged = Command::PutStaged {
        tier: TierId::new(1),
        objects: vec![StagedPut {
            object: staged,
            end: 7,
            seal: false,
            blobs: vec![(blob_digest(4), 0)],
        }],
    };
    assert!(
        matches!(log.commit(put_staged).await, Ok(Applied::Staged(Ok(_)))),
        "PutStaged"
    );
    assert_eq!(
        log.commit(Command::SealStaged { object: staged }).await,
        Ok(Applied::Tier(Ok(())))
    );
    let before = log.query(|m| answers(m, staged)).await.expect("a query");
    drop(log);

    let log = open(&fs.restart(), 7).await;
    assert_eq!(
        log.machine_version(),
        MachineVersion::V2,
        "the open raised it"
    );
    assert_eq!(log.query(|m| answers(m, staged)).await, Ok(before));
    let refused = log.commit(evict()).await;
    assert!(
        matches!(refused, Err(MetaLogError::Unavailable(_))),
        "{refused:?}"
    );
    let before = log.query(|m| answers(m, staged)).await.expect("a query");
    drop(log);

    // The second open replays the refused evict as nothing (it was never committed),
    // and an explicit raise then takes it.
    let log = open(&fs.restart(), 7).await;
    assert_eq!(log.machine_version(), MachineVersion::V2);
    assert_eq!(log.query(|m| answers(m, staged)).await, Ok(before.clone()));
    log.raise_machine_version(MachineVersion::V3)
        .expect("a raise");
    assert_eq!(log.query(|m| answers(m, staged)).await, Ok(before));
    assert!(
        matches!(log.commit(evict()).await, Ok(Applied::Evicted(e)) if e.blobs.len() == 2),
        "the raise did not let the evict commit"
    );
}

/// Format 3's second condition: a node whose log is at version 2 refuses an
/// `EvictObjects` entry loudly, never skips it. The entry is appended by hand (this
/// binary never encodes one into a version-2 log), in format-3 bytes, after a raise to
/// 2 and a format-2 command; both this binary (which runs 3) and one that runs only 2
/// refuse the open at the entry's index with `UnknownCommand("MetaCommand")`, and keep
/// refusing it. Catches: a machine that decodes every entry under format 3 (mutant:
/// `decode(Format::V3, ..)` in `MetaMachine::apply`; the evict then applies), and one
/// that skips an entry it cannot decode (mutant: `Err(_) => {}`; the log opens without
/// the entry).
#[tokio::test]
async fn an_evict_entry_in_a_version_2_log_refuses_the_open() {
    let fs = FaultFs::new();
    let log = open(&fs, 8).await;
    let e = epoch(&log).await;
    assert_eq!(
        log.commit(put_blob(e)).await,
        Ok(Applied::Blob(Ok(BlobWrite::Stored)))
    );
    log.raise_machine_version(MachineVersion::V2)
        .expect("a raise");
    assert_eq!(log.commit(add_tier()).await, Ok(Applied::Tier(Ok(()))));
    drop(log);
    let dir = fs.restart();
    let evict = LogCommand::Meta(Command::EvictObjects {
        objects: [ObjectId::new(Epoch::new(e), 1)].into(),
    });
    let index = append(&dir, Format::V3, &evict);
    for runs in [MachineVersion::V3, MachineVersion::V2, MachineVersion::V3] {
        let refused = RaftMetaLog::open_running(
            dir.restart(),
            Retention::default(),
            "kbf/",
            || Ok(id(8)),
            runs,
        )
        .await;
        assert!(
            matches!(
                &refused,
                Err(RaftLogError::Decode { index: i, source: DecodeError::UnknownCommand("MetaCommand") })
                    if *i == index
            ),
            "running {runs:?}: {refused:?}"
        );
    }
}

/// Catches: a replica that takes a later `Bootstrap` over the first (a leader that died
/// with its `Bootstrap` uncommitted may leave one in the log that a later leader
/// commits): its objects would move to the later prefix. A later one is applied as a
/// no-op, and counted.
#[tokio::test]
async fn a_later_bootstrap_changes_nothing() {
    let fs = FaultFs::new();
    drop(open(&fs, 4).await);
    let dir = fs.restart();
    let (mut store, held) = Store::open(dir.clone(), Options::default()).expect("the store");
    let last = held.entries.last().expect("entries").id;
    let later = LogCommand::Bootstrap(Bootstrap {
        log_id: id(5),
        store_prefix: "elsewhere/".to_owned(),
    });
    let entry = Entry {
        id: LogId::new(last.term, last.index.next()),
        payload: Payload::Command(kbf_log::encode(Format::V1, &later).expect("encode")),
    };
    store.write_entries(&[entry]).expect("append");
    drop(store);
    let log = open(&dir.restart(), 6).await;
    assert_eq!(log.log_id(), id(4));
    assert_eq!(log.store_prefix().as_str(), "kbf/0000000000000004/");
    assert_eq!(log.bootstraps(), 2);
}

/// Catches: a log that keeps acknowledging commits after a storage error (it can no
/// longer make them durable), or that turns the error into an answer other than
/// UNAVAILABLE; queries keep reading the applied state.
#[tokio::test]
async fn a_storage_error_stops_every_later_commit() {
    let fs = FaultFs::new();
    let log = open(&fs, 1).await;
    let first = epoch(&log).await;
    fs.fail_at(fs.ops(), Fault::Error);
    for _ in 0..2 {
        let refused = log.commit(Command::AllocEpoch).await;
        assert!(
            matches!(refused, Err(MetaLogError::Unavailable(_))),
            "{refused:?}"
        );
    }
    assert!(log.tick().await.is_err());
    assert_eq!(log.query(|m| m.blob_count()).await, Ok(0));
    drop(log);
    let log = open(&fs.restart(), 1).await;
    assert_eq!(epoch(&log).await, first + 1);
}

/// Catches: a directory that cannot be listed opened as an empty one (a new log drawn
/// over an existing one), instead of refused.
#[tokio::test]
async fn an_unreadable_directory_is_refused() {
    let fs = FaultFs::new();
    fs.fail_at(0, Fault::Error);
    let refused = refusal(&fs, 1).await;
    assert!(matches!(refused, RaftLogError::Io(_)), "{refused:?}");
    assert!(refused.to_string().starts_with("raft dir"));
}

/// Catches (kbf#689): an open that reads the directory twice, once for the store and
/// once for the host, so an entry appended between the two reads leaves the host one
/// entry past the store, and the open's first write is refused ("entries do not
/// continue the log"). Here an entry is appended to the log as the open lists the
/// directory for the `n`th time, for each `n` up to one past the reads one open needs:
/// every open must succeed and keep committing, with the entry in its log if the append
/// came before the read.
#[tokio::test]
async fn an_entry_appended_while_the_log_opens_does_not_split_its_views() {
    for n in 1..=3 {
        let fs = FaultFs::new();
        let log = open(&fs, 1).await;
        let first = epoch(&log).await;
        drop(log);
        let fs = fs.restart();
        let listing = AppendAtListing {
            inner: fs.clone(),
            at: n,
            listed: Arc::new(AtomicU64::new(0)),
        };
        let reopened = RaftMetaLog::open(listing, Retention::default(), "kbf/", || Ok(id(2))).await;
        let log = reopened.unwrap_or_else(|e| panic!("listing {n}: {e}"));
        let next = match log.commit(Command::AllocEpoch).await {
            Ok(Applied::Epoch(e)) => e.get(),
            other => panic!("listing {n}: AllocEpoch answered {other:?}"),
        };
        assert!(next > first, "listing {n}: epoch {next} after {first}");
    }
}

/// A [`FaultFs`] that appends an `AllocEpoch` entry to its log just before its `at`th
/// listing, as another writer of the directory would.
#[derive(Clone, Debug)]
struct AppendAtListing {
    inner: FaultFs,
    at: u64,
    listed: Arc<AtomicU64>,
}

impl Fs for AppendAtListing {
    type File = FaultFile;

    fn list(&self) -> io::Result<Vec<String>> {
        if self.listed.fetch_add(1, Ordering::SeqCst) + 1 == self.at {
            let alloc = LogCommand::Meta(Command::AllocEpoch);
            append(&self.inner, Format::V1, &alloc);
        }
        self.inner.list()
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        self.inner.read(name)
    }

    fn file_len(&self, name: &str) -> io::Result<u64> {
        self.inner.file_len(name)
    }

    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read_at(name, offset, len)
    }

    fn create(&self, name: &str) -> io::Result<FaultFile> {
        self.inner.create(name)
    }

    fn create_positioned(&self, name: &str) -> io::Result<FaultFile> {
        self.inner.create_positioned(name)
    }

    fn open_append(&self, name: &str) -> io::Result<FaultFile> {
        self.inner.open_append(name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.inner.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.inner.sync_dir()
    }
}
