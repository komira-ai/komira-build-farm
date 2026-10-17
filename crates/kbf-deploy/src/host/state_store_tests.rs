use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::fs::symlink;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use kbf_host::testkit::{FakeExec, reply, scratch};
use kbf_host::user::ID;
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{Clock, Cond, Store, SystemClock, Txn};
use rusqlite::Connection;

use super::*;

const STAMP: &str = "20280229T123456Z";

/// Opens a store on `db`. A lock this process held a moment ago can still be held by a
/// copy of its descriptor in a child another test is starting (between its fork and
/// its exec, which closes it), so a `Locked` open is tried again for a while; a lock a
/// store or a restore really holds lasts longer than that.
fn open(db: &Path) -> SqliteStore {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
        match SqliteStore::open(db, clock, Options::default()) {
            Err(SqliteError::Locked { .. }) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            opened => return opened.expect("open the state file"),
        }
    }
}

fn put(store: &SqliteStore, key: &str, value: &str) {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

/// The value of `key` in the state file `db`, read by a store opened on it.
fn read(db: &Path, key: &str) -> Option<String> {
    let store = open(db);
    let found = store.get(key).expect("get");
    found.map(|v| String::from_utf8(v.value).expect("utf-8"))
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

fn me() -> Owner {
    Owner {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
    }
}

/// Everything in `dir`: each name with its bytes, mode and modification time.
fn snapshot(dir: &Path) -> BTreeMap<OsString, (Vec<u8>, u32, i64, i64)> {
    fs::read_dir(dir)
        .expect("list")
        .map(|entry| {
            let path = entry.expect("entry").path();
            let meta = fs::symlink_metadata(&path).expect("stat");
            let bytes = if meta.is_file() {
                fs::read(&path).expect("read")
            } else {
                Vec::new()
            };
            let name = path.file_name().expect("a name").to_owned();
            (name, (bytes, meta.mode(), meta.mtime(), meta.mtime_nsec()))
        })
        .collect()
}

/// `kbf-deploy host <args>`: exit, stdout, stderr.
fn cli(args: &[&OsStr]) -> (ExitCode, String, String) {
    let argv: Vec<OsString> = [OsStr::new("kbf-deploy"), OsStr::new("host")]
        .iter()
        .chain(args)
        .map(|a| a.to_os_string())
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::cli::run(argv, &mut FakeExec::new(0), &|_| None, &mut out, &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (code, text(out), text(err))
}

fn backup_cli(db: &Path, dest: &Path, extra: &[&str]) -> (ExitCode, String, String) {
    let mut args = vec![
        OsStr::new("state-backup"),
        OsStr::new("--db"),
        db.as_os_str(),
        OsStr::new("--dest"),
        dest.as_os_str(),
    ];
    args.extend(extra.iter().map(OsStr::new));
    cli(&args)
}

/// `restore` with the test stamp: exit, stdout, stderr as the verb reports them.
fn restore_at(src: &Path, db: &Path, apply: bool) -> (ExitCode, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = report(restore(src, db, apply, STAMP, &mut out), &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (code, text(out), text(err))
}

/// A backup at `<dir>/backup.db` of a state file holding `key` = `value`, made through
/// the backup verb's own function.
fn backup_holding(dir: &Path, key: &str, value: &str) -> PathBuf {
    let db = dir.join("source.db");
    let store = open(&db);
    put(&store, key, value);
    drop(store);
    let file = dir.join("backup.db");
    backup(&db, &file, None, Perms::new(0o644), &mut Vec::new()).expect("backup");
    file
}

/// A state file at `<dir>/state.db` holding `old` = `1`, closed: a copy of one a store
/// wrote under another name, so no descriptor of its lock was ever open (a restore takes
/// that lock first).
fn live_holding_old(dir: &Path) -> PathBuf {
    let seed = dir.join("seed.db");
    let store = open(&seed);
    put(&store, "old", "1");
    drop(store);
    assert!(
        !beside(&seed, "-wal").exists(),
        "a closed store folds its log in"
    );
    let db = dir.join("state.db");
    fs::copy(&seed, &db).expect("copy");
    db
}

fn kept(db: &Path) -> PathBuf {
    beside(db, &format!(".pre-restore-{STAMP}"))
}

// Catches a backup that is not one consistent snapshot while the frontend commits (a
// transaction's two keys split across the copy), that misses commits still in the log,
// that blocks or is blocked by the writer, or that kbf-state cannot open again.
#[test]
fn a_backup_while_a_writer_commits_is_a_consistent_checked_copy() {
    let dir = scratch("state-store-backup-live");
    let db = dir.join("state.db");
    let store = Arc::new(open(&db));
    let (stop, done) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU64::new(0)),
    );
    let writer = {
        let (store, stop, done) = (Arc::clone(&store), Arc::clone(&stop), Arc::clone(&done));
        thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let txn = Txn::new().put(format!("a/{n:06}"), "x", Cond::Any).put(
                    format!("b/{n:06}"),
                    "x",
                    Cond::Any,
                );
                store.txn(txn).expect("reachable").expect("commits");
                n += 1;
                done.store(n, Ordering::SeqCst);
            }
        })
    };
    let wait_for = |at_least: u64| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while done.load(Ordering::SeqCst) < at_least {
            assert!(Instant::now() < deadline, "the writer stalled");
            thread::sleep(Duration::from_millis(1));
        }
    };

    let mut taken = Vec::new();
    for (i, name) in ["first.db", "second.db"].into_iter().enumerate() {
        let at_least = done.load(Ordering::SeqCst) + 20;
        wait_for(at_least);
        let dest = dir.join(name);
        let (code, out, err) = backup_cli(&db, &dest, &[]);
        assert_eq!(code, ExitCode::SUCCESS, "backup {i}: {err}");
        assert!(out.contains("integrity ok; mode 0640"), "{out}");
        assert_eq!(mode(&dest), 0o640, "the default mode");
        taken.push(dest);
    }
    stop.store(true, Ordering::SeqCst);
    writer.join().expect("the writer ran");
    let total = done.load(Ordering::SeqCst);
    let last = dir.join("last.db");
    assert_eq!(backup_cli(&db, &last, &[]).0, ExitCode::SUCCESS);
    taken.push(last);
    drop(store);

    let mut counts = Vec::new();
    for file in &taken {
        assert!(
            !beside(file, ".kbf-backup-tmp").exists(),
            "no temporary left"
        );
        let copy = open(file);
        let keys = |prefix: &str| -> Vec<String> {
            let mut names = Vec::new();
            for (key, _) in copy.scan_prefix(prefix).expect("scan") {
                names.push(key[prefix.len()..].to_owned());
            }
            names
        };
        let (a, b) = (keys("a/"), keys("b/"));
        assert_eq!(
            a,
            b,
            "{}: every transaction whole or absent",
            file.display()
        );
        counts.push(a.len() as u64);
    }
    assert!(
        counts[0] >= 20 && counts[0] < counts[1] && counts[1] <= total,
        "the copies were taken while the writer committed: {counts:?} of {total}"
    );
    assert_eq!(counts[2], total, "the last copy has every commit");
}

// Catches a backup that replaces what is at --dest (another backup, or whatever a
// dangling link points at).
#[test]
fn a_backup_never_replaces_a_file() {
    let dir = scratch("state-store-backup-exists");
    let db = live_holding_old(&dir);
    let dest = dir.join("taken.db");
    fs::write(&dest, b"keep me").expect("seed");
    let (code, _, err) = backup_cli(&db, &dest, &[]);
    assert_eq!(code, ExitCode::from(2), "{err}");
    assert!(err.starts_with("REFUSED: "), "{err}");
    assert!(err.contains("something is there already"), "{err}");
    assert_eq!(fs::read(&dest).expect("read"), b"keep me");

    let link = dir.join("link.db");
    symlink(dir.join("nowhere.db"), &link).expect("link");
    assert_eq!(backup_cli(&db, &link, &[]).0, ExitCode::from(2));
    assert!(!dir.join("nowhere.db").exists());
    assert!(!beside(&link, ".kbf-backup-tmp").exists());
}

// Catches a backup that ignores --owner or --mode, keeps a stale temporary from a run
// that died (VACUUM INTO refuses a file that is not empty), or leaves its temporary.
#[test]
fn a_backup_gets_its_owner_and_mode_and_leaves_no_temporary() {
    let dir = scratch("state-store-backup-owner");
    let db = live_holding_old(&dir);
    let dest = dir.join("owned.db");
    fs::write(beside(&dest, ".kbf-backup-tmp"), b"stale").expect("seed");
    let mut out = Vec::new();
    backup(&db, &dest, Some(me()), Perms::new(0o600), &mut out).expect("backup");
    let out = String::from_utf8(out).expect("utf-8");
    let me = me();
    assert!(
        out.contains(&format!("mode 0600, owner {}:{}", me.uid, me.gid)),
        "{out}"
    );
    assert!(out.contains("1 keys"), "{out}");
    let meta = fs::metadata(&dest).expect("stat");
    assert_eq!(
        (meta.uid(), meta.gid(), mode(&dest)),
        (me.uid, me.gid, 0o600)
    );
    assert!(!beside(&dest, ".kbf-backup-tmp").exists());
    assert_eq!(read(&dest, "old").as_deref(), Some("1"));
}

// Catches a backup that reports success, or leaves a file, when there was nothing to
// copy or its temporary name is taken by something it cannot remove.
#[test]
fn a_backup_that_fails_says_so_and_leaves_nothing() {
    let dir = scratch("state-store-backup-fails");
    let dest = dir.join("out.db");
    let (code, _, err) = backup_cli(&dir.join("missing.db"), &dest, &[]);
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert!(
        err.starts_with("FAIL: ") && err.contains("missing.db"),
        "{err}"
    );
    assert!(!dest.exists() && !dir.join("missing.db").exists());

    let db = live_holding_old(&dir);
    fs::create_dir(beside(&dest, ".kbf-backup-tmp")).expect("seed");
    let (code, _, err) = backup_cli(&db, &dest, &[]);
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert!(err.contains("remove the stale"), "{err}");
    assert!(!dest.exists());
}

// Catches --mode taking what is not octal permission bits, and --owner not refusing a
// user the host does not have.
#[test]
fn the_backup_flags_are_checked() {
    assert_eq!(octal("0640").map(Perms::bits), Ok(0o640));
    assert_eq!(octal("600").map(Perms::bits), Ok(0o600));
    for bad in ["8", "17777", "rw", ""] {
        assert!(octal(bad).is_err(), "{bad}");
    }
    let dir = scratch("state-store-backup-flags");
    let db = live_holding_old(&dir);
    let dest = dir.join("out.db");
    let (code, _, err) = backup_cli(&db, &dest, &["--mode", "999"]);
    assert_eq!(code, ExitCode::from(2), "{err}");
    let (code, _, err) = backup_cli(&db, &dest, &["--owner", "kbf-no-such-user-for-a-test"]);
    assert_eq!(code, ExitCode::from(2), "{err}");
    assert!(
        err.starts_with("REFUSED: --owner kbf-no-such-user-for-a-test"),
        "{err}"
    );
    assert!(!dest.exists());
}

/// A host whose user database has `kbf` (uid 990, group 991, also in group 4) and the
/// groups `adm` (4), and a few answers that are not what `getent group` prints.
fn users() -> FakeExec {
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| {
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        match (cmd.program.as_str(), &args[..]) {
            (GETENT, ["passwd", "--", "kbf"]) => Ok(reply(
                0,
                "kbf:x:990:991::/var/lib/kbf:/usr/sbin/nologin\n",
                "",
            )),
            (GETENT, ["passwd", "--", "broken"]) => Err(io::Error::other("no getent")),
            (ID, ["-G", "--", "kbf"]) => Ok(reply(0, "991 4\n", "")),
            (GETENT, ["group", "--", "adm"]) => Ok(reply(0, "adm:x:4:kbf\n", "")),
            (GETENT, ["group", "--", "other"]) => Ok(reply(0, "another:x:5:\n", "")),
            (GETENT, ["group", "--", "bad"]) => Ok(reply(0, "bad:x:four:\n", "")),
            (GETENT, ["group", "--", "failing"]) => Ok(reply(1, "failing:x:6:\n", "")),
            (GETENT, ["group", "--", "unreachable"]) => Err(io::Error::other("no getent")),
            _ => Ok(reply(2, "", "")),
        }
    });
    exec
}

// Catches an owner lookup that takes a missing or misread user or group for a real one
// (the backup would be chowned to the wrong ids), or drops the user's own group.
#[test]
fn an_owner_is_looked_up_by_name() {
    let exec = users();
    assert_eq!(
        owner(&exec, "kbf").expect("kbf"),
        Owner { uid: 990, gid: 991 }
    );
    assert_eq!(
        owner(&exec, "kbf:adm").expect("kbf:adm"),
        Owner { uid: 990, gid: 4 }
    );
    let refused = [
        ("nobody-here", "no user \"nobody-here\""),
        ("broken", "no getent"),
        ("kbf:missing", "no group \"missing\""),
        ("kbf:other", "no group \"other\""),
        ("kbf:bad", "no group \"bad\""),
        ("kbf:failing", "no group \"failing\""),
        ("kbf:unreachable", "no getent"),
    ];
    for (spec, why) in refused {
        let e = owner(&exec, spec).expect_err(spec);
        assert!(matches!(e, StateFileError::Owner { .. }), "{spec}: {e}");
        let text = e.to_string();
        assert!(text.starts_with(&format!("--owner {spec}: ")), "{text}");
        assert!(text.contains(why), "{spec}: {text}");
    }
}

// Catches a restore that loses the current file (no kept copy, or one without its owner
// and mode), that does not put the backup's rows in place, or gives the restored file
// the backup's mode instead of the current file's.
#[test]
fn a_restore_keeps_the_current_file_and_puts_the_backups_rows_in_place() {
    let dir = scratch("state-store-restore");
    let src = backup_holding(&dir, "new", "2");
    let db = live_holding_old(&dir);
    fs::set_permissions(&db, Permissions::from_mode(0o600)).expect("chmod");

    let (code, out, err) = restore_at(&src, &db, true);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.contains("integrity ok (epoch "), "{out}");
    assert!(
        out.contains(&format!(
            "keep {} as {}\n",
            db.display(),
            kept(&db).display()
        )),
        "{out}"
    );
    assert!(out.contains("restored "), "{out}");

    let keep = kept(&db);
    assert_eq!(mode(&keep), 0o600, "the kept copy has the file's mode");
    assert!(
        !beside(&keep, "-wal").exists(),
        "the file had no log to keep"
    );
    assert_eq!(
        mode(&db),
        0o600,
        "the restored file has the replaced file's mode"
    );
    assert!(!beside(&db, ".restoring").exists());
    assert_eq!(
        read(&keep, "old").as_deref(),
        Some("1"),
        "the kept copy is the old file"
    );
    assert_eq!(read(&keep, "new"), None);
    assert_eq!(
        read(&db, "new").as_deref(),
        Some("2"),
        "the backup's rows are in place"
    );
    assert_eq!(read(&db, "old"), None);
}

// Catches a kept copy that leaves out the commits a killed frontend left in the log
// beside the file (they are part of the current state), and a restore that leaves that
// log beside the restored file, where SQLite would replay it into the wrong file.
#[test]
fn a_killed_frontends_log_is_kept_with_the_file() {
    let dir = scratch("state-store-restore-log");
    let src = backup_holding(&dir, "new", "2");
    // A copy of a file and its log taken while the store has them open is what a kill
    // leaves: the commit is in the log alone.
    let live = dir.join("live.db");
    let store = open(&live);
    put(&store, "logged", "3");
    let db = dir.join("state.db");
    fs::copy(&live, &db).expect("copy the file");
    fs::copy(beside(&live, "-wal"), beside(&db, "-wal")).expect("copy the log");
    drop(store);
    assert!(fs::metadata(beside(&db, "-wal")).expect("log").len() > 0);

    let (code, out, err) = restore_at(&src, &db, true);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.contains(", with its log"), "{out}");
    let keep = kept(&db);
    assert!(
        beside(&keep, "-wal").exists(),
        "the log is kept beside the kept file"
    );
    assert_eq!(
        read(&keep, "logged").as_deref(),
        Some("3"),
        "the logged commit is kept"
    );
    assert_eq!(
        read(&db, "logged"),
        None,
        "the old log was not replayed into the new file"
    );
    assert_eq!(read(&db, "new").as_deref(), Some("2"));
}

/// A restore of a backup into the state file of a store that is open (a running
/// frontend), with `apply` or not: refused, and nothing written.
fn refused_while_open(name: &str, apply: bool) {
    let same = |dir: &Path| dir.join("state.db");
    refused_while_open_as(name, &same, &same, apply);
}

/// [`refused_while_open`] with the frontend opening the state file by the name
/// `frontend` gives (in the test's directory) and the restore given the name `operator`
/// gives: two spellings of one file.
fn refused_while_open_as(
    name: &str,
    frontend: &dyn Fn(&Path) -> PathBuf,
    operator: &dyn Fn(&Path) -> PathBuf,
    apply: bool,
) {
    let dir = scratch(name);
    let src = backup_holding(&dir, "new", "2");
    fs::create_dir(dir.join("real")).expect("a directory");
    symlink("real", dir.join("alias")).expect("a linked directory");
    symlink("real/state.db", dir.join("link.db")).expect("a linked file");
    let store = open(&frontend(&dir));
    put(&store, "old", "1");
    let db = operator(&dir);
    let before = snapshot(&dir);
    let (code, out, err) = restore_at(&src, &db, apply);
    assert_eq!(code, ExitCode::from(2), "{out}{err}");
    assert!(err.starts_with("REFUSED: "), "{err}");
    assert!(err.contains("the frontend is running"), "{err}");
    assert_eq!(snapshot(&dir), before, "nothing written");
    assert!(
        store.get("old").expect("get").is_some(),
        "the live store reads on"
    );
}

// Catches a restore that runs while the frontend holds the store's lock: it would
// replace the file under a live writer.
#[test]
fn a_restore_is_refused_while_the_frontend_holds_the_lock() {
    refused_while_open("state-store-restore-busy", true);
}

// Catches a dry run that plans a restore while the frontend holds the store's lock (the
// operator would be told it can go ahead).
#[test]
fn a_dry_run_is_refused_while_the_frontend_holds_the_lock() {
    refused_while_open("state-store-restore-busy-dry", false);
}

// Catches a lock keyed by the name a process was given rather than the file: a frontend
// started on a link to the state file holds `link.db.lock`, so a restore given the
// file's own path would find its lock free and replace the file under the live writer
// (an acknowledged commit lost).
#[test]
fn a_restore_by_the_files_path_is_refused_while_the_frontend_has_it_by_a_link() {
    let link = |dir: &Path| dir.join("link.db");
    let real = |dir: &Path| dir.join("real/state.db");
    refused_while_open_as("state-store-restore-busy-link", &link, &real, true);
    refused_while_open_as("state-store-restore-busy-link-dry", &link, &real, false);
}

// Catches the same through a link to the file's directory, both ways: a frontend on the
// file's own path and a restore given a path through the linked directory, and the
// reverse.
#[test]
fn a_restore_through_a_linked_directory_is_refused_while_the_frontend_has_the_file() {
    let real = |dir: &Path| dir.join("real/state.db");
    let alias = |dir: &Path| dir.join("alias/state.db");
    refused_while_open_as("state-store-restore-busy-alias", &real, &alias, true);
    refused_while_open_as("state-store-restore-busy-alias-dry", &alias, &real, false);
}

// Catches a restore given a link as --db: the rename would replace the link with the
// restored file and leave the file the link named (the frontend's, if it opens by the
// file's own name) as it was.
#[test]
fn a_restore_refuses_a_linked_state_file() {
    let dir = scratch("state-store-restore-db-link");
    let src = backup_holding(&dir, "new", "2");
    let db = live_holding_old(&dir);
    let link = dir.join("link.db");
    symlink("state.db", &link).expect("link");
    let before = snapshot(&dir);
    for apply in [false, true] {
        let (code, out, err) = restore_at(&src, &link, apply);
        assert_eq!(code, ExitCode::from(2), "apply {apply}: {out}{err}");
        assert!(
            err.starts_with("REFUSED: --db ") && err.contains("a symbolic link"),
            "{err}"
        );
        assert_eq!(snapshot(&dir), before, "apply {apply}: nothing written");
    }
    assert_eq!(read(&db, "old").as_deref(), Some("1"));
}

/// A restore of `src` (made in `dir` by `make`) over a state file, with and without
/// `apply`: refused for `why`, and nothing written.
fn refused_source(name: &str, make: &dyn Fn(&Path) -> PathBuf, why: &str) {
    let dir = scratch(name);
    let good = backup_holding(&dir, "new", "2");
    let db = live_holding_old(&dir);
    let src = make(&good);
    let before = snapshot(&dir);
    for apply in [false, true] {
        let (code, _, err) = restore_at(&src, &db, apply);
        assert_eq!(code, ExitCode::from(2), "apply {apply}: {err}");
        assert!(
            err.starts_with("REFUSED: not a backup to restore: "),
            "{err}"
        );
        assert!(err.contains(why), "{why}: {err}");
        assert_eq!(snapshot(&dir), before, "{why}: nothing written");
    }
    assert_eq!(read(&db, "old").as_deref(), Some("1"));
}

/// A file beside `good`, named `name`.
fn sibling(good: &Path, name: &str) -> PathBuf {
    good.with_file_name(name)
}

// Catches a restore that takes another application's database: its rows would replace
// the live file's.
#[test]
fn a_restore_refuses_another_applications_database() {
    let foreign = |good: &Path| {
        let foreign = sibling(good, "foreign.db");
        let conn = Connection::open(&foreign).expect("create");
        conn.execute_batch("CREATE TABLE t (x)").expect("fill");
        foreign
    };
    refused_source("state-store-restore-foreign", &foreign, "schema version 0");
}

// Catches a restore that takes a state store of another schema version: it has the same
// tables, so nothing but the version check stops its rows being read with the wrong
// layout.
#[test]
fn a_restore_refuses_a_state_store_of_another_schema_version() {
    let newer = |good: &Path| {
        let newer = sibling(good, "newer.db");
        fs::copy(good, &newer).expect("copy");
        let conn = Connection::open(&newer).expect("open");
        conn.pragma_update(None, "user_version", 2)
            .expect("version");
        newer
    };
    refused_source("state-store-restore-newer", &newer, "schema version 2");
}

// Catches a restore that takes a damaged backup, one whose damage only
// `integrity_check` sees.
#[test]
fn a_restore_refuses_a_damaged_backup() {
    let make = |good: &Path| {
        let damaged = sibling(good, "damaged.db");
        fs::copy(good, &damaged).expect("copy");
        damage(&damaged);
        damaged
    };
    refused_source("state-store-restore-damaged", &make, "damaged");
}

// Catches a restore that takes a missing file, one that is not a database, or a
// WAL-mode file (part of it may be in a log beside it, which a restore does not copy).
#[test]
fn a_restore_refuses_what_is_not_a_single_file_database() {
    let missing = |good: &Path| sibling(good, "missing.db");
    refused_source("state-store-restore-missing", &missing, "missing.db");
    let garbage = |good: &Path| {
        let garbage = sibling(good, "garbage.db");
        fs::write(&garbage, vec![b'x'; 4096]).expect("write");
        garbage
    };
    refused_source(
        "state-store-restore-garbage",
        &garbage,
        "not a SQLite database",
    );
    // The backup's source, a store's own file, closed: in WAL mode.
    let wal_mode = |good: &Path| sibling(good, "source.db");
    refused_source("state-store-restore-wal", &wal_mode, "WAL-mode");
}

/// Swaps the root pages of two indexes, so each holds the other table's rows: the file
/// opens and reads, and only `integrity_check` sees it.
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

// Catches a dry run that changes anything: the file, its log, its lock, or a kept copy.
#[test]
fn a_dry_run_changes_nothing() {
    let dir = scratch("state-store-restore-dry");
    let src = backup_holding(&dir, "new", "2");
    let live = dir.join("live.db");
    let store = open(&live);
    put(&store, "logged", "3");
    let db = dir.join("state.db");
    fs::copy(&live, &db).expect("copy the file");
    fs::copy(beside(&live, "-wal"), beside(&db, "-wal")).expect("copy the log");
    drop(store);
    fs::write(beside(&db, ".lock"), b"").expect("a lock file nobody holds");

    let before = snapshot(&dir);
    let (code, out, err) = restore_at(&src, &db, false);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.contains(", with its log"), "{out}");
    assert!(
        out.ends_with("dry run: nothing changed; --apply restores\n"),
        "{out}"
    );
    assert_eq!(snapshot(&dir), before, "the dry run wrote nothing");

    let fresh = scratch("state-store-restore-dry-fresh");
    let (code, out, err) = restore_at(&src, &fresh.join("state.db"), false);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.contains("state.db: nothing to keep"), "{out}");
    assert_eq!(snapshot(&fresh), BTreeMap::new(), "not even a lock file");
}

// Catches a restore into a place with no state file yet that leaves the file (or the
// lock file it made) with the running user's defaults rather than the directory's
// owner and 0600, and the CLI not reaching the restore.
#[test]
fn a_restore_into_a_new_place_is_the_directorys_and_0600() {
    let dir = scratch("state-store-restore-new");
    let src = backup_holding(&dir, "new", "2");
    let fresh = scratch("state-store-restore-new-dest");
    let db = fresh.join("state.db");
    let (code, out, err) = cli(&[
        OsStr::new("state-restore"),
        OsStr::new("--src"),
        src.as_os_str(),
        OsStr::new("--db"),
        db.as_os_str(),
        OsStr::new("--apply"),
    ]);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.contains("nothing to keep"), "{out}");
    let dir_meta = fs::metadata(&fresh).expect("stat");
    for file in [db.clone(), beside(&db, ".lock")] {
        let meta = fs::metadata(&file).expect("stat");
        assert_eq!((meta.uid(), meta.gid()), (dir_meta.uid(), dir_meta.gid()));
    }
    assert_eq!(mode(&db), 0o600);
    assert_eq!(read(&db, "new").as_deref(), Some("2"));
}

// Catches a restore that overwrites a kept copy already at its name, and failures to
// read the state file's place reported as success or as a refusal.
#[test]
fn restore_failures_are_failures() {
    let dir = scratch("state-store-restore-fails");
    let src = backup_holding(&dir, "new", "2");
    let db = live_holding_old(&dir);
    fs::write(kept(&db), b"an earlier copy").expect("seed");
    let (code, _, err) = restore_at(&src, &db, true);
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert!(err.starts_with("FAIL: create "), "{err}");
    assert_eq!(fs::read(kept(&db)).expect("read"), b"an earlier copy");
    assert_eq!(read(&db, "old").as_deref(), Some("1"), "nothing replaced");

    let file = dir.join("a-file");
    fs::write(&file, b"").expect("write");
    for apply in [false, true] {
        let (code, _, err) = restore_at(&src, &file.join("state.db"), apply);
        assert_eq!(code, ExitCode::FAILURE, "apply {apply}: {err}");
    }
    // A name that cannot be read: refused as a link by the verb, so the plan is asked
    // directly.
    let looped = dir.join("looped.db");
    symlink("looped.db", &looped).expect("link");
    let (code, _, err) = restore_at(&src, &looped, false);
    assert_eq!(code, ExitCode::from(2), "{err}");
    let failed = plan(&looped, STAMP).err().expect("a link loop");
    assert!(matches!(failed, StateFileError::Io { .. }), "{failed}");
    assert!(failed.to_string().starts_with("read "), "{failed}");
    let (code, _, err) = restore_at(&src, &dir.join("no-dir/state.db"), false);
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert!(err.contains("no-dir"), "{err}");
}

// Catches a stamp that is not UTC or not sortable by name.
#[test]
fn the_stamp_is_utc_and_sorts_by_time() {
    let at = |secs: u64| utc_stamp(UNIX_EPOCH + Duration::from_secs(secs));
    assert_eq!(at(1_835_440_496), STAMP);
    assert_eq!(at(1_798_761_599), "20261231T235959Z");
}

// Catches a directory taken wrongly for a relative name or the root.
#[test]
fn a_files_directory_is_its_parent_or_here() {
    assert_eq!(
        dir_of(Path::new("/var/lib/kbf/state.db")),
        Path::new("/var/lib/kbf")
    );
    assert_eq!(dir_of(Path::new("state.db")), Path::new("."));
    assert_eq!(dir_of(Path::new("/")), Path::new("."));
}

// Catches a restore of a state file that has a second hard link: a frontend that opened
// the other name holds that name's lock, so the restore would find its own free and
// rename over a file the frontend is still writing.
#[test]
fn a_restore_refuses_a_hard_linked_state_file() {
    let dir = scratch("state-store-restore-db-hardlink");
    let src = backup_holding(&dir, "new", "2");
    let db = live_holding_old(&dir);
    fs::hard_link(&db, dir.join("hl.db")).expect("a second name");
    let before = snapshot(&dir);
    for target in [db.clone(), dir.join("hl.db")] {
        for apply in [false, true] {
            let (code, out, err) = restore_at(&src, &target, apply);
            assert_eq!(code, ExitCode::from(2), "apply {apply}: {out}{err}");
            assert!(
                err.starts_with("REFUSED: --db ") && err.contains("2 names (hard links)"),
                "{err}"
            );
            assert_eq!(snapshot(&dir), before, "apply {apply}: nothing written");
        }
    }
    fs::remove_file(dir.join("hl.db")).expect("one name again");
    let (code, out, err) = restore_at(&src, &db, true);
    assert_eq!(code, ExitCode::SUCCESS, "{out}{err}");
}

// Catches a backup of a hard-linked state file that stays silent: its copy is
// consistent, so it is written, and the operator is told the file has two names.
#[test]
fn a_backup_of_a_hard_linked_state_file_warns() {
    let dir = scratch("state-store-backup-hardlink");
    let db = live_holding_old(&dir);
    let (code, out, err) = backup_cli(&db, &dir.join("one.db"), &[]);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(!out.contains("warning"), "{out}");
    fs::hard_link(&db, dir.join("hl.db")).expect("a second name");
    let (code, out, err) = backup_cli(&db, &dir.join("two.db"), &[]);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(
        out.starts_with("warning: ") && out.contains("2 names"),
        "{out}"
    );
    assert!(dir.join("two.db").exists());

    // A directory has several links of its own; it is not a second name of a file.
    fs::create_dir(dir.join("sub")).expect("a directory");
    fs::create_dir(dir.join("sub").join("inner")).expect("a subdirectory");
    let src = backup_holding(&dir, "new", "2");
    let (_, out, err) = restore_at(&src, &dir.join("sub"), false);
    assert!(!format!("{out}{err}").contains("hard links"), "{out}{err}");
}
