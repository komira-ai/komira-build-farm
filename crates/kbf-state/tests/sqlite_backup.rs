//! The online backup, the check of a backup, the lock a tool takes on a state file, and
//! the restore under that lock.
//!
//! Every test names the defect it catches.

mod common;

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::{open_manual, scratch};
use kbf_state::sqlite::{
    Options, SqliteError, SqliteStore, StoreLock, Summary, backup, check_backup, restore_locked,
};
use kbf_state::{Clock, Cond, Revision, Store, SystemClock, Txn};
use rusqlite::Connection;

fn put(store: &SqliteStore, key: &str, value: &str) -> Revision {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits")
        .revision
}

fn reopen(path: &Path) -> SqliteStore {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    SqliteStore::open(path, clock, Options::default()).expect("open")
}

/// Bytes 18 and 19 of a file's header: 1 and 1 in a rollback journal mode, 2 and 2 in WAL.
fn journal_bytes(path: &Path) -> [u8; 2] {
    let bytes = fs::read(path).expect("read");
    [bytes[18], bytes[19]]
}

/// The names in `dir`.
fn names(dir: &Path) -> BTreeSet<OsString> {
    fs::read_dir(dir)
        .expect("list")
        .map(|entry| entry.expect("entry").file_name())
        .collect()
}

/// A state store file that is not in WAL mode, made by the backup of a closed store.
fn clean_backup(dir: &Path, name: &str) -> PathBuf {
    let (store, _) = open_manual(dir, Options::default());
    put(&store, "k", "v");
    drop(store);
    let file = dir.join(name);
    backup(&dir.join("state.db"), &file).expect("backup");
    file
}

// Catches a backup that misses the groups still in the live file's write-ahead log, one
// left in WAL mode (its next reader would need a log beside it, and could create one),
// and a summary that does not read the copy.
#[test]
fn a_backup_of_an_open_store_is_one_checked_file_with_every_commit() {
    let dir = scratch("backup-live");
    let (store, _) = open_manual(&dir, Options::default());
    put(&store, "a", "1");
    let head = put(&store, "b", "2");
    assert!(
        dir.join("state.db-wal").exists(),
        "the commits are in the log, so the backup must read through it"
    );

    let copy = dir.join("copy.db");
    let summary = backup(&dir.join("state.db"), &copy).expect("backup while open");
    assert_eq!(
        summary,
        Summary {
            epoch: store.epoch(),
            head: head.0,
            keys: 2
        }
    );
    assert_eq!(journal_bytes(&copy), [1, 1], "a single file");
    assert!(!dir.join("copy.db-wal").exists());
    assert!(!dir.join("copy.db-journal").exists());
    assert_eq!(check_backup(&copy).expect("check"), summary);
    drop(store);

    let restored = reopen(&copy);
    assert_eq!(
        restored.get("a").expect("get").map(|v| v.value),
        Some(b"1".to_vec())
    );
    assert_eq!(
        restored.get("b").expect("get").map(|v| v.value),
        Some(b"2".to_vec())
    );
}

// Catches a backup that creates the state file it was pointed at, copies a file that is
// not a state store, or names no path when the copy cannot be written.
#[test]
fn a_backup_refuses_what_it_cannot_copy() {
    let dir = scratch("backup-refused");
    let missing = dir.join("missing.db");
    let refused = backup(&missing, &dir.join("out.db")).expect_err("no file");
    assert!(matches!(refused, SqliteError::Sqlite(_)), "got {refused}");
    assert!(refused.to_string().contains("missing.db"), "got {refused}");
    assert!(!missing.exists(), "the backup never creates its source");
    assert!(!dir.join("out.db").exists());

    let foreign = dir.join("foreign.db");
    Connection::open(&foreign)
        .expect("create")
        .execute_batch("CREATE TABLE t (x)")
        .expect("fill");
    let refused = backup(&foreign, &dir.join("out.db")).expect_err("foreign");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );
    assert!(!dir.join("out.db").exists(), "nothing is written for it");

    let (store, _) = open_manual(&dir, Options::default());
    drop(store);
    let bad_name = dir.join(OsStr::from_bytes(b"out-\xff.db"));
    let refused = backup(&dir.join("state.db"), &bad_name).expect_err("not UTF-8");
    assert!(refused.to_string().contains("not UTF-8"), "got {refused}");
}

// Catches a check that passes a file a restore must not take whole: not a database, a
// WAL-mode file (part of it may be in a log beside it), another application's database,
// or a damaged one; and a check that creates a file beside the one it reads.
#[test]
fn the_check_takes_only_a_whole_intact_state_store() {
    let dir = scratch("check");
    let good = clean_backup(&dir, "good.db");
    let before = names(&dir);
    assert_eq!(check_backup(&good).expect("good").keys, 1);
    assert_eq!(names(&dir), before, "the check creates nothing");

    let missing = check_backup(&dir.join("missing.db")).expect_err("missing");
    assert!(matches!(missing, SqliteError::Io(_)), "got {missing}");
    assert!(missing.to_string().contains("missing.db"), "got {missing}");

    let garbage = dir.join("garbage.db");
    fs::write(&garbage, vec![b'x'; 4096]).expect("write");
    let refused = check_backup(&garbage).expect_err("garbage");
    assert!(
        refused.to_string().contains("not a SQLite database"),
        "got {refused}"
    );
    fs::write(&garbage, b"SQLite format 3\0").expect("write");
    let refused = check_backup(&garbage).expect_err("a header cut short");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );

    let live = dir.join("state.db");
    assert_eq!(journal_bytes(&live), [2, 2]);
    let refused = check_backup(&live).expect_err("WAL mode");
    assert!(refused.to_string().contains("WAL-mode"), "got {refused}");

    let foreign = dir.join("foreign.db");
    Connection::open(&foreign)
        .expect("create")
        .execute_batch("CREATE TABLE t (x)")
        .expect("fill");
    let refused = check_backup(&foreign).expect_err("foreign");
    assert!(
        refused.to_string().contains("schema version 0"),
        "got {refused}"
    );

    let damaged = dir.join("damaged.db");
    fs::copy(&good, &damaged).expect("copy");
    damage(&damaged);
    let refused = check_backup(&damaged).expect_err("damaged");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );
    assert!(refused.to_string().contains("damaged"), "got {refused}");
    assert!(
        refused.to_string().contains("damaged.db"),
        "names the file: {refused}"
    );
}

/// Swaps the root pages of two indexes, so each index holds the other table's rows: the
/// file still opens and reads, and only `integrity_check` sees it.
fn damage(file: &Path) {
    let conn = Connection::open(file).expect("open");
    conn.execute_batch(
        "CREATE TABLE t1 (a); CREATE TABLE t2 (a);
         CREATE INDEX i1 ON t1 (a); CREATE INDEX i2 ON t2 (a);
         INSERT INTO t1 VALUES (1), (2), (3); INSERT INTO t2 VALUES (10), (20), (30), (40);",
    )
    .expect("fill");
    let root = |name: &str| -> i64 {
        conn.query_row(
            "SELECT rootpage FROM sqlite_master WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .expect("root page")
    };
    let (one, two) = (root("i1"), root("i2"));
    conn.execute_batch("PRAGMA writable_schema = ON")
        .expect("writable schema");
    conn.execute(
        "UPDATE sqlite_master SET rootpage = CASE name WHEN 'i1' THEN ?2 ELSE ?1 END
         WHERE name IN ('i1', 'i2')",
        [one, two],
    )
    .expect("swap");
}

// Catches a lock that is not the store's own (a restore could then run under a live
// frontend, or a frontend start under a restore), and a probe that is wrong or creates
// the lock file.
#[test]
fn the_tool_lock_is_the_stores_lock() {
    let dir = scratch("tool-lock");
    let db = dir.join("state.db");
    assert!(
        !StoreLock::is_held(&db).expect("probe"),
        "no lock file: nobody holds it"
    );
    assert!(
        !dir.join("state.db.lock").exists(),
        "the probe creates nothing"
    );

    let (store, _) = open_manual(&dir, Options::default());
    assert!(
        StoreLock::is_held(&db).expect("probe"),
        "an open store holds it"
    );
    let refused = StoreLock::take(&db).expect_err("the store holds it");
    assert!(
        matches!(refused, SqliteError::Locked { .. }),
        "got {refused}"
    );
    drop(store);
    assert!(
        !StoreLock::is_held(&db).expect("probe"),
        "released with the store"
    );

    let lock = StoreLock::take(&db).expect("take");
    let resolved = dir.canonicalize().expect("resolve").join("state.db.lock");
    assert_eq!(
        (lock.db(), lock.lock_file()),
        (db.as_path(), resolved.as_path())
    );
    assert!(StoreLock::is_held(&db).expect("probe"));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    let refused = SqliteStore::open(&db, clock, Options::default()).expect_err("held by the tool");
    assert!(
        matches!(refused, SqliteError::Locked { .. }),
        "got {refused}"
    );
    drop(lock);
    drop(reopen(&db));

    let file = dir.join("a-file");
    fs::write(&file, b"").expect("write");
    let refused = StoreLock::is_held(&file.join("state.db")).expect_err("not a directory");
    assert!(matches!(refused, SqliteError::Io(_)), "got {refused}");
    assert!(
        refused.to_string().contains("the lock of ")
            && refused.to_string().contains("a-file/state.db"),
        "got {refused}"
    );
    assert!(
        !StoreLock::is_held(&dir.join("no-dir/state.db")).expect("probe"),
        "a directory that does not exist has no lock in it"
    );
}

/// A directory with the state file's directory `real/`, a link `alias` to it, and a link
/// `link.db` to `real/state.db`.
fn linked(name: &str) -> PathBuf {
    let dir = scratch(name);
    fs::create_dir(dir.join("real")).expect("a directory");
    std::os::unix::fs::symlink("real", dir.join("alias")).expect("a linked directory");
    std::os::unix::fs::symlink("real/state.db", dir.join("link.db")).expect("a linked file");
    dir
}

/// Whether a store opened on `by` holds the lock of every other name of the file, and
/// a tool's lock on any of them keeps the store from opening by `by`.
fn one_lock_for_every_name(dir: &Path, by: &str) {
    let names = [
        "real/state.db",
        "link.db",
        "alias/state.db",
        "real/../real/state.db",
    ];
    let store = reopen(&dir.join(by));
    for name in names {
        let db = dir.join(name);
        assert!(StoreLock::is_held(&db).expect("probe"), "{by} holds {name}");
        let refused = StoreLock::take(&db).expect_err(name);
        assert!(
            matches!(refused, SqliteError::Locked { .. }),
            "{by}, {name}: got {refused}"
        );
    }
    drop(store);
    for name in names {
        let lock = StoreLock::take(&dir.join(name)).expect(name);
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
        let refused = SqliteStore::open(&dir.join(by), clock, Options::default())
            .expect_err("held by the tool");
        assert!(
            matches!(refused, SqliteError::Locked { .. }),
            "{name} keeps {by} shut: got {refused}"
        );
        assert_eq!(
            lock.lock_file(),
            dir.canonicalize()
                .expect("resolve")
                .join("real/state.db.lock"),
            "{name}"
        );
    }
}

// Catches a lock keyed by the name a process was given rather than the file it names: a
// frontend on a link to the state file and a restore given the file's own path (or a
// path through a linked directory) would each take a lock of its own, and the restore
// would replace the file under the live writer.
#[test]
fn every_name_of_a_state_file_has_one_lock() {
    for by in ["link.db", "real/state.db", "alias/state.db"] {
        let dir = linked(&format!("one-lock-{}", by.replace('/', "-")));
        one_lock_for_every_name(&dir, by);
    }
}

// Catches a first open through a link to a file not made yet keyed by the link's name:
// the file the store then creates is the link's target, whose own name must be locked.
#[test]
fn a_link_to_a_file_not_made_yet_locks_the_file_it_names() {
    let dir = linked("one-lock-new");
    let lock = StoreLock::take(&dir.join("link.db")).expect("take");
    assert!(
        !dir.join("real/state.db").exists(),
        "the lock makes no state file"
    );
    assert!(StoreLock::is_held(&dir.join("real/state.db")).expect("probe"));
    drop(lock);
    let store = reopen(&dir.join("link.db"));
    put(&store, "k", "v");
    assert!(
        dir.join("real/state.db").exists(),
        "the store made the file the link names"
    );
    assert!(StoreLock::is_held(&dir.join("alias/state.db")).expect("probe"));
}

// Catches a restore that opens its staged copy with the log of a restore that died left
// beside it: SQLite would replay that log, another file's pages, into the copy.
#[test]
fn a_restore_drops_a_log_a_dead_restore_left() {
    let dir = scratch("restore-stale-log");
    let source = clean_backup(&dir, "backup.db");
    // A log with a commit in it, as a store has while it is open.
    let other = dir.join("other");
    fs::create_dir(&other).expect("a directory");
    let (store, _) = open_manual(&other, Options::default());
    put(&store, "stale", "x");
    let live = dir.join("live.db");
    fs::copy(
        other.join("state.db-wal"),
        dir.join("live.db.restoring-wal"),
    )
    .expect("plant");
    fs::write(dir.join("live.db.restoring-shm"), vec![0xA5; 32768]).expect("plant");
    drop(store);

    let lock = StoreLock::take(&live).expect("take");
    restore_locked(&lock, &source, &|_| Ok(())).expect("restore");
    drop(lock);
    for left in [
        "live.db.restoring-wal",
        "live.db.restoring-shm",
        "live.db.restoring",
    ] {
        assert!(!dir.join(left).exists(), "{left} is gone");
    }
    let restored = reopen(&live);
    assert!(
        restored.get("k").expect("get").is_some(),
        "the backup's rows"
    );
    assert_eq!(
        restored.get("stale").expect("get"),
        None,
        "no replayed rows"
    );
}

// Catches a stale log that cannot be removed being ignored: the restore must stop, not
// open the copy with it.
#[test]
fn a_restore_stops_when_a_dead_restores_log_cannot_go() {
    let dir = scratch("restore-stale-log-dir");
    let source = clean_backup(&dir, "backup.db");
    let live = dir.join("live.db");
    fs::create_dir(dir.join("live.db.restoring-wal")).expect("plant");
    let lock = StoreLock::take(&live).expect("take");
    restore_locked(&lock, &source, &|_| Ok(())).expect_err("cannot remove a directory");
    assert!(!live.exists(), "nothing restored");
    assert!(!dir.join("live.db.restoring").exists(), "nothing staged");
}

// Catches a restore under the caller's lock that skips `prepare`, runs it on another
// file than the staged copy, or replaces the file when `prepare` fails.
#[test]
fn a_restore_under_the_lock_prepares_the_staged_copy_first() {
    let dir = scratch("restore-locked");
    let source = clean_backup(&dir, "backup.db");
    let live = dir.join("live.db");
    let lock = StoreLock::take(&live).expect("take");

    let failed = restore_locked(&lock, &source, &|_| Err(std::io::Error::other("planted")))
        .expect_err("prepare failed");
    assert!(failed.to_string().contains("planted"), "got {failed}");
    assert!(
        failed.to_string().contains("live.db"),
        "names the file: {failed}"
    );
    assert!(!live.exists(), "nothing replaced");
    assert!(
        !dir.join("live.db.restoring").exists(),
        "no staging copy left"
    );

    let seen = Mutex::new(Vec::new());
    let prepare = |staging: &Path| {
        seen.lock().expect("lock").push(staging.to_owned());
        fs::set_permissions(staging, fs::Permissions::from_mode(0o600))
    };
    let epoch = restore_locked(&lock, &source, &prepare).expect("restore");
    assert_eq!(*seen.lock().expect("lock"), [dir.join("live.db.restoring")]);
    let mode = fs::metadata(&live).expect("restored").permissions().mode() & 0o7777;
    assert_eq!(mode, 0o600, "what prepare did is what was renamed in");
    drop(lock);
    let store = reopen(&live);
    assert_eq!(store.epoch(), epoch + 1);
    assert!(store.get("k").expect("get").is_some());
}

// Catches a store opened (or a tool lock taken) on a file with a second hard link: the
// lock is beside one name, so a process holding another name's lock would write the
// file a restore renames over, and acknowledged commits would be lost.
#[test]
fn a_state_file_with_a_second_name_is_refused() {
    let dir = scratch("hard-link");
    let (store, _) = open_manual(&dir, Options::default());
    drop(store);
    let db = dir.join("state.db");
    let other = dir.join("hl.db");
    fs::hard_link(&db, &other).expect("a second name");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    for name in [&db, &other] {
        let refused =
            SqliteStore::open(name, Arc::clone(&clock), Options::default()).expect_err("opened");
        assert!(
            matches!(refused, SqliteError::HardLinked { links: 2, .. }),
            "got {refused}"
        );
        assert!(refused.to_string().contains("hard links"), "{refused}");
        let refused = StoreLock::take(name).expect_err("locked");
        assert!(
            matches!(refused, SqliteError::HardLinked { .. }),
            "got {refused}"
        );
    }
    assert!(!dir.join("hl.db.lock").exists(), "refused before the lock");
    // A directory has several links of its own; it is not a second name of a state file.
    let refused = SqliteStore::open(&dir, Arc::clone(&clock), Options::default())
        .expect_err("a directory is no state file");
    assert!(
        !matches!(refused, SqliteError::HardLinked { .. }),
        "got {refused}"
    );
    fs::remove_file(&other).expect("one name again");
    drop(SqliteStore::open(&db, clock, Options::default()).expect("one name opens"));
    StoreLock::take(&db).expect("one name locks");
}
