//! What the SQLite store promises beyond the conformance clauses: one process per file,
//! the epoch, the offline restore, and a failed group leaving nothing behind.
//!
//! Every test names the defect it catches.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{open_manual, scratch};
use kbf_state::sqlite::{Options, SqliteError, SqliteStore, restore};
use kbf_state::{Clock, Cond, Event, Revision, Store, StoreTime, SystemClock, Txn, Version};
use rusqlite::Connection;

fn put(store: &SqliteStore, key: &str, value: &str) {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

fn has(store: &SqliteStore, key: &str) -> bool {
    store.get(key).expect("get").is_some()
}

// Catches a second frontend that opens a file another one holds: two writers would each
// hand out the same revisions.
#[test]
fn a_second_store_on_the_same_file_is_refused() {
    let dir = scratch("second-open");
    let (first, clock) = open_manual(&dir, Options::default());
    put(&first, "k", "v");

    let clock: Arc<dyn Clock> = clock;
    let refused = SqliteStore::open(&dir.join("state.db"), clock, Options::default())
        .expect_err("the file is held");
    assert!(
        matches!(refused, SqliteError::Locked { .. }),
        "a second open must be refused with Locked, got {refused}"
    );
    assert!(
        refused.to_string().contains("state.db.lock"),
        "the message names the lock file: {refused}"
    );
    assert!(
        has(&first, "k"),
        "the refused open left the first store alone"
    );
    assert!(
        dir.join("state.db.lock").exists(),
        "the lock is <file>.lock"
    );
}

// Catches a lock that outlives its store, which would refuse every restart.
#[test]
fn the_lock_goes_with_the_store() {
    let dir = scratch("lock-released");
    let (first, _) = open_manual(&dir, Options::default());
    drop(first);
    let (second, _) = open_manual(&dir, Options::default());
    assert_eq!(
        second.epoch(),
        2,
        "the file is opened again after the first store closed"
    );
}

// Catches an epoch that restarts at the same number: a daemon would keep the leases of
// the incarnation before the restart.
#[test]
fn every_open_moves_the_epoch_forward() {
    let dir = scratch("epoch");
    let mut epochs = Vec::new();
    for _ in 0..3 {
        let (store, _) = open_manual(&dir, Options::default());
        epochs.push(store.epoch());
    }
    assert_eq!(epochs, [1, 2, 3]);
}

// Catches a revision counter that restarts at zero after a reopen: versions would be
// reissued and an old compare-and-set could match a new value.
#[test]
fn revisions_continue_after_a_restart() {
    let dir = scratch("revisions");
    let (first, _) = open_manual(&dir, Options::default());
    put(&first, "k", "v1");
    let old = first.get("k").expect("get").expect("k").version;
    drop(first);
    let (second, _) = open_manual(&dir, Options::default());
    put(&second, "k", "v2");
    let new = second.get("k").expect("get").expect("k").version;
    assert!(new > old, "version {new:?} must be past {old:?}");
}

// Catches a path that is not a state store being given a layout (and its data lost) or
// being read as one.
#[test]
fn a_file_that_is_not_a_state_store_is_refused() {
    let dir = scratch("foreign");
    let foreign = dir.join("state.db");
    let conn = Connection::open(&foreign).expect("create");
    conn.execute_batch("CREATE TABLE notes (text TEXT); INSERT INTO notes VALUES ('keep');")
        .expect("fill");
    drop(conn);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    let refused = SqliteStore::open(&foreign, clock, Options::default()).expect_err("foreign");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );
    let conn = Connection::open(&foreign).expect("reopen");
    let kept: String = conn
        .query_row("SELECT text FROM notes", [], |row| row.get(0))
        .expect("the foreign table is untouched");
    assert_eq!(kept, "keep");

    let later = scratch("later-schema");
    let file = later.join("state.db");
    Connection::open(&file)
        .expect("create")
        .execute_batch("PRAGMA user_version = 7; CREATE TABLE t (x);")
        .expect("fill");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    let refused = SqliteStore::open(&file, clock, Options::default()).expect_err("later schema");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );
}

// Catches an open that reports a bad file without saying which.
#[test]
fn open_errors_name_the_file() {
    let dir = scratch("garbage");
    let file = dir.join("state.db");
    std::fs::write(&file, vec![b'x'; 4096]).expect("write garbage");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    let refused = SqliteStore::open(&file, clock, Options::default()).expect_err("garbage");
    assert!(matches!(refused, SqliteError::Sqlite(_)), "got {refused}");
    assert!(refused.to_string().contains("state.db"), "got {refused}");

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    let missing = dir.join("no-such-dir").join("state.db");
    let refused = SqliteStore::open(&missing, clock, Options::default()).expect_err("no dir");
    assert!(matches!(refused, SqliteError::Io(_)), "got {refused}");
    assert!(refused.to_string().contains("no-such-dir"), "got {refused}");
}

// Catches expiry times kept on a clock whose origin is lost at restart: a ttl key would
// expire at once, or never, after the process restarts.
#[test]
fn expiry_survives_a_restart_on_the_system_clock() {
    let dir = scratch("system-clock");
    let file = dir.join("state.db");
    let clock = Arc::new(SystemClock::default());
    let as_clock: Arc<dyn Clock> = clock.clone();
    let store = SqliteStore::open(&file, Arc::clone(&as_clock), Options::default()).expect("open");
    store
        .txn(Txn::new().put_ttl("lease", "x", Duration::from_secs(3600), Cond::Any))
        .expect("reachable")
        .expect("commits");
    let before = store.get("lease").expect("get").expect("live");
    drop(store);

    let store = SqliteStore::open(&file, as_clock, Options::default()).expect("reopen");
    let after = store.get("lease").expect("get").expect("still live");
    assert_eq!(
        after.expires_at, before.expires_at,
        "the expiry is the one stored"
    );
    assert!(
        after.expires_at.expect("ttl") > clock.now(),
        "the expiry is in the future"
    );
    let read_by_store = store.now();
    assert!(
        clock.now() >= read_by_store,
        "the store reads the clock it was given"
    );
}

// Catches a scan that reads past the prefix, or hands out a key the clock has expired.
#[test]
fn scans_stop_at_the_prefix_and_hide_expired_keys() {
    let dir = scratch("scan");
    let (store, clock) = open_manual(&dir, Options::default());
    for key in ["a/1", "b/1", "b/2", "b/3", "c/1"] {
        put(&store, key, key);
    }
    store
        .txn(Txn::new().put_ttl("b/2", "short", Duration::from_secs(1), Cond::Any))
        .expect("reachable")
        .expect("commits");
    clock.advance(Duration::from_secs(2));
    let keys: Vec<String> = store
        .scan_prefix("b/")
        .expect("scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(keys, ["b/1", "b/3"]);
    assert!(store.scan_prefix("zzz").expect("scan").is_empty());
}

// Catches a limit of zero or an end-of-feed page that moves the cursor backwards.
#[test]
fn the_feed_cursor_never_moves_back() {
    let dir = scratch("cursor");
    let (store, _) = open_manual(&dir, Options::default());
    put(&store, "a", "1");
    put(&store, "b", "2");
    let all = store.changes(Revision::ZERO, 10).expect("changes");
    assert_eq!(all.rows.len(), 2);
    let ahead = store.changes(Revision(100), 10).expect("changes");
    assert!(ahead.rows.is_empty());
    assert_eq!(
        ahead.cursor,
        Revision(100),
        "a poller ahead of the head stays where it is"
    );
    let none = store.changes(Revision::ZERO, 0).expect("changes");
    assert!(none.rows.is_empty());
    assert_eq!(
        none.cursor,
        Revision::ZERO,
        "a page of zero rows does not move the cursor"
    );
    assert!(matches!(all.rows[0].event, Event::Put(_)));
}

// Catches a failed group that leaves part of itself behind, one that is acknowledged, and a
// head that moves although the group did not commit.
#[test]
fn a_failed_group_is_rolled_back_and_reported() {
    let dir = scratch("failed-group");
    let (store, _) = open_manual(&dir, Options::default());
    put(&store, "before", "1");
    let operator = Connection::open(dir.join("state.db")).expect("operator connection");
    operator
        .execute_batch(
            "CREATE TRIGGER refuse BEFORE INSERT ON feed \
             BEGIN SELECT RAISE(ABORT, 'refused by the operator'); END;",
        )
        .expect("break the file");

    // The key is written before its feed row is refused: the write must go with it.
    let failed = store.txn(Txn::new().put("during", "2", Cond::Any));
    let error = failed.expect_err("the group cannot commit");
    assert!(
        error.to_string().contains("refused by the operator"),
        "the error says why: {error}"
    );
    assert!(has(&store, "before"), "an earlier commit is still there");
    assert!(
        !has(&store, "during"),
        "nothing of the failed group is visible"
    );
    assert_eq!(
        store.commit_stats().txns,
        1,
        "a failed group counts no transaction"
    );

    operator
        .execute_batch("DROP TRIGGER refuse")
        .expect("repair");
    put(&store, "after", "3");
    let version = store.get("after").expect("get").expect("after").version;
    assert_eq!(
        version,
        Version(2),
        "the failed group's revision was not used up"
    );
}

// Catches the commit metric not being kept: the operator's latency alarm would never fire.
#[test]
fn commit_latency_is_measured() {
    let dir = scratch("metrics");
    let options = Options {
        latency_limit: Duration::ZERO,
        ..Options::default()
    };
    let (store, _) = open_manual(&dir, options);
    assert_eq!(
        store.commit_metrics().groups,
        0,
        "nothing is counted before the first write"
    );
    put(&store, "a", "1");
    put(&store, "b", "2");
    let metrics = store.commit_metrics();
    assert_eq!(metrics.groups, 2, "two lone writes are two groups");
    assert_eq!(metrics.largest_group, 1);
    assert_eq!(metrics.over_limit, 2, "every group passes a limit of zero");
    assert!(metrics.max_latency > Duration::ZERO && metrics.total_latency >= metrics.max_latency);
}

// Catches a writer that holds a lone write for a group that never forms: the write is
// acknowledged on its own, one commit each.
#[test]
fn a_lone_write_is_its_own_group() {
    let dir = scratch("lone");
    let (store, _) = open_manual(&dir, Options::default());
    let before = store.commit_stats();
    for n in 0..5 {
        put(&store, &format!("k{n}"), "v");
    }
    let stats = store.commit_stats();
    assert_eq!(stats.txns - before.txns, 5);
    assert_eq!(
        stats.syncs - before.syncs,
        5,
        "a sequential caller gets a commit per write"
    );
}

// Catches a group of conflicts being committed: nothing was written, so nothing is synced.
#[test]
fn a_group_that_writes_nothing_syncs_nothing() {
    let dir = scratch("no-writes");
    let (store, _) = open_manual(&dir, Options::default());
    put(&store, "k", "v");
    let before = store.commit_stats();
    let refused = store
        .txn(Txn::new().put("k", "other", Cond::Absent))
        .expect("reachable")
        .expect_err("k exists");
    assert_eq!((refused.index, refused.key.as_str()), (0, "k"));
    assert!(
        store
            .apply_batch(Vec::new())
            .expect("empty batch")
            .is_empty()
    );
    let after = store.commit_stats();
    assert_eq!(
        after.syncs, before.syncs,
        "a rolled-back group is not a sync"
    );
    assert_eq!(after.txns, before.txns);
}

fn backup_of(dir: &std::path::Path, live: &str, name: &str) -> std::path::PathBuf {
    let backup = dir.join(name);
    Connection::open(dir.join(live))
        .expect("open the live file")
        .execute("VACUUM INTO ?1", [backup.to_str().expect("utf-8 path")])
        .expect("back up");
    backup
}

// Catches a restore that lets the restored file reuse an epoch the replaced file issued:
// a daemon holding leases of that epoch would keep them.
#[test]
fn restore_replaces_the_file_and_moves_the_epoch_past_both() {
    let dir = scratch("restore");
    let (first, _) = open_manual(&dir, Options::default());
    put(&first, "kept", "1");
    drop(first);
    let backup = backup_of(&dir, "state.db", "backup.db");

    for _ in 0..2 {
        let (store, _) = open_manual(&dir, Options::default());
        put(&store, "later", "2");
    }
    let live = dir.join("state.db");
    let epoch = restore(&backup, &live).expect("restore");
    assert_eq!(epoch, 4, "past the backup's 1 and the replaced file's 3");
    assert!(
        !dir.join("state.db.restoring").exists(),
        "no staging copy is left"
    );

    let (store, _) = open_manual(&dir, Options::default());
    assert_eq!(
        store.epoch(),
        5,
        "the next start bumps the restored epoch again"
    );
    assert!(has(&store, "kept"), "the backup's data is there");
    assert!(!has(&store, "later"), "the replaced file's data is gone");
    put(&store, "after", "3");
}

// Catches a restore into a place with no file yet.
#[test]
fn restore_creates_a_missing_file() {
    let dir = scratch("restore-new");
    let (first, _) = open_manual(&dir, Options::default());
    put(&first, "kept", "1");
    drop(first);
    let backup = backup_of(&dir, "state.db", "backup.db");
    let fresh = scratch("restore-new-dest");
    let epoch = restore(&backup, &fresh.join("state.db")).expect("restore");
    assert_eq!(epoch, 2);
    let (store, _) = open_manual(&fresh, Options::default());
    assert!(has(&store, "kept"));
}

// Catches a restore that runs while a frontend has the file: it would swap the file under
// a live writer.
#[test]
fn restore_is_refused_while_the_store_is_open() {
    let dir = scratch("restore-busy");
    let (store, _) = open_manual(&dir, Options::default());
    put(&store, "k", "v");
    let other = scratch("restore-busy-source");
    let (source, _) = open_manual(&other, Options::default());
    drop(source);
    let backup = backup_of(&other, "state.db", "backup.db");

    let refused = restore(&backup, &dir.join("state.db")).expect_err("the file is held");
    assert!(
        matches!(refused, SqliteError::Locked { .. }),
        "got {refused}"
    );
    assert!(has(&store, "k"), "the live file is untouched");
}

// Catches a restore that destroys the live file for a backup that is not one.
#[test]
fn restore_refuses_a_backup_that_is_not_a_state_store() {
    let dir = scratch("restore-bad");
    let (first, _) = open_manual(&dir, Options::default());
    put(&first, "k", "v");
    drop(first);
    let live = dir.join("state.db");

    let foreign = dir.join("foreign.db");
    Connection::open(&foreign)
        .expect("create")
        .execute_batch("CREATE TABLE t (x)")
        .expect("fill");
    let refused = restore(&foreign, &live).expect_err("foreign");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );
    assert!(refused.to_string().contains("foreign.db"), "got {refused}");

    let garbage = dir.join("garbage.db");
    std::fs::write(&garbage, vec![b'x'; 4096]).expect("write");
    restore(&garbage, &live).expect_err("garbage");

    let missing = dir.join("missing.db");
    let refused = restore(&missing, &live).expect_err("missing");
    assert!(matches!(refused, SqliteError::Io(_)), "got {refused}");
    assert!(refused.to_string().contains("missing.db"), "got {refused}");

    assert!(
        !dir.join("state.db.restoring").exists(),
        "failed restores leave no staging copy"
    );
    let (store, _) = open_manual(&dir, Options::default());
    assert!(
        has(&store, "k"),
        "the live file survived every refused restore"
    );
}

// Catches a backup with damaged pages being restored over a good file.
#[test]
fn restore_refuses_a_damaged_backup() {
    let dir = scratch("restore-damaged");
    let (first, _) = open_manual(&dir, Options::default());
    for n in 0..200 {
        put(&first, &format!("key-{n:04}"), &"x".repeat(200));
    }
    drop(first);
    let backup = backup_of(&dir, "state.db", "backup.db");
    let mut bytes = std::fs::read(&backup).expect("read");
    let from = bytes.len() / 2;
    for byte in &mut bytes[from..from + 2048] {
        *byte = 0xFF;
    }
    std::fs::write(&backup, bytes).expect("damage");
    restore(&backup, &dir.join("state.db")).expect_err("a damaged backup");
}

// Catches a writer that dies and leaves callers waiting for an answer that never comes, or a
// store that cannot be dropped after its writer has died.
#[test]
fn a_stopped_writer_fails_calls_instead_of_hanging() {
    /// A clock that panics, in the writer thread, once armed.
    struct Trap(AtomicBool);
    impl Clock for Trap {
        fn now(&self) -> StoreTime {
            assert!(
                !self.0.load(Ordering::SeqCst),
                "the clock was armed to fail"
            );
            StoreTime::ZERO
        }
    }

    let dir = scratch("writer-stops");
    let trap = Arc::new(Trap(AtomicBool::new(false)));
    let clock: Arc<dyn Clock> = trap.clone();
    let store = SqliteStore::open(&dir.join("state.db"), clock, Options::default()).expect("open");
    put(&store, "k", "v");
    trap.0.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let error = store
            .txn(Txn::new().put("a", "1", Cond::Any))
            .expect_err("the writer is gone");
        assert!(
            error.to_string().contains("writer has stopped"),
            "got {error}"
        );
    }
    drop(store);
}

// Catches a delete that spends a revision or writes a feed row for a key that was not there,
// and a check that is not held to its condition.
#[test]
fn deletes_and_checks_follow_their_preconditions() {
    let dir = scratch("delete-check");
    let (store, _) = open_manual(&dir, Options::default());
    put(&store, "k", "v");
    let version = store.get("k").expect("get").expect("k").version;

    let absent = store
        .txn(Txn::new().delete("missing", Cond::Any))
        .expect("reachable")
        .expect("deleting nothing commits");
    assert_eq!(
        absent.revision,
        Revision(1),
        "it wrote no row, so it reports the head"
    );
    assert_eq!(
        store
            .changes(Revision::ZERO, 10)
            .expect("changes")
            .rows
            .len(),
        1
    );

    store
        .txn(
            Txn::new()
                .check("k", Cond::Version(version))
                .put("held", "1", Cond::Absent),
        )
        .expect("reachable")
        .expect("the check holds");
    let refused = store
        .txn(
            Txn::new()
                .check("k", Cond::Absent)
                .put("never", "1", Cond::Any),
        )
        .expect("reachable")
        .expect_err("k exists");
    assert_eq!((refused.index, refused.key.as_str()), (0, "k"));
    assert!(!has(&store, "never"), "a failed check writes nothing");

    store
        .txn(Txn::new().delete("k", Cond::Version(version)))
        .expect("reachable")
        .expect("the version is current");
    assert!(!has(&store, "k"));
    let feed = store.changes(Revision::ZERO, 10).expect("changes").rows;
    assert_eq!(
        feed.last().map(|row| (&row.event, row.key.as_str())),
        Some((&Event::Delete, "k"))
    );
}

// Catches a store whose Debug output hides which file it is.
#[test]
fn a_store_describes_itself() {
    let dir = scratch("debug");
    let (store, _) = open_manual(&dir, Options::default());
    let text = format!("{store:?}");
    assert!(
        text.contains("SqliteStore") && text.contains("state.db"),
        "got {text}"
    );
}

// Catches an acknowledgement that is not yet in the files a crash leaves: the store is
// still open (nothing closed it, nothing checkpointed the journal) when its two files are
// copied, as they would be found after a kill, and the copy is opened. Every acknowledged
// write must be in it. (A copy cannot show a missing fsync, which only a power cut does;
// the sync setting itself is held by the unit test of the connection settings.)
#[test]
fn the_files_a_crash_leaves_hold_every_acknowledged_write() {
    let dir = scratch("crash-live");
    let (store, _) = open_manual(&dir, Options::default());
    for n in 0..20 {
        put(&store, &format!("k{n:02}"), &format!("v{n}"));
    }
    let head = store.changes(Revision::ZERO, 100).expect("changes").cursor;

    let after_crash = scratch("crash-copy");
    for name in ["state.db", "state.db-wal"] {
        std::fs::copy(dir.join(name), after_crash.join(name))
            .expect("the file and its journal are there while the store is open");
    }
    let (copy, _) = open_manual(&after_crash, Options::default());
    for n in 0..20 {
        let found = copy.get(&format!("k{n:02}")).expect("get").expect("kept");
        assert_eq!(found.value, format!("v{n}").into_bytes());
    }
    assert_eq!(
        copy.changes(Revision::ZERO, 100).expect("changes").cursor,
        head,
        "the feed is the one that was acknowledged"
    );
    drop(store);
}

// Catches an expiry that is off by one at the boundary: a key lives while the clock is before
// its expiry and is gone at the instant it is reached, in a read, a scan and the reaper.
#[test]
fn a_key_expires_at_exactly_its_ttl() {
    let dir = scratch("ttl-boundary");
    let (store, clock) = open_manual(&dir, Options::default());
    let ttl = Duration::from_secs(10);
    store
        .txn(Txn::new().put_ttl("lease", "x", ttl, Cond::Any))
        .expect("reachable")
        .expect("commits");

    clock.advance(ttl - Duration::from_nanos(1));
    assert!(
        has(&store, "lease"),
        "one nanosecond before the ttl it lives"
    );
    assert_eq!(store.scan_prefix("lea").expect("scan").len(), 1);
    assert_eq!(store.reap_expired().expect("reap"), 0, "nothing is due yet");

    clock.advance(Duration::from_nanos(1));
    assert!(!has(&store, "lease"), "at the ttl it is gone");
    assert!(store.scan_prefix("lea").expect("scan").is_empty());
    assert_eq!(
        store.reap_expired().expect("reap"),
        1,
        "the reaper removes it"
    );
    let feed = store.changes(Revision::ZERO, 10).expect("changes");
    assert_eq!(
        feed.rows.last().map(|row| row.event.clone()),
        Some(Event::Expire),
        "the expiry is in the feed"
    );
}
