//! Unit tests for what a test cannot reach from outside: the filesystem check on a path
//! that is on a network filesystem, and the error plumbing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::guard::{
    canonical_db, filesystem_type, held, lock_failed, lock_path, network_filesystem,
    refuse_network_filesystem,
};
use super::read::changes_after_head;
use super::restore::ignore_not_found;
use super::{Options, SqliteError, SqliteStore};
use crate::{Clock, Cond, Revision, Store, StoreError, SystemClock, Txn};

fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::default())
}

/// An empty directory of its own under the system's temporary directory.
fn unit_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-state-unit-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).expect("create the directory");
    dir
}

// Catches a state file on NFS being opened: its locks and syncs are not reliable there.
#[test]
fn a_network_filesystem_is_refused_before_anything_is_created() {
    let path = Path::new("/kbf-state-test-nonexistent/state.db");
    let refused = SqliteStore::open_checked(path, clock(), Options::default(), &|_| Ok(0x6969))
        .expect_err("NFS");
    assert!(
        matches!(&refused, SqliteError::NetworkFs { fs: "NFS", .. }),
        "got {refused}"
    );
    assert!(
        refused.to_string().contains("local disk"),
        "the message says what to do: {refused}"
    );
    assert!(
        !lock_path(path).exists(),
        "the refusal comes before the lock file is created"
    );
}

// Catches the probe being asked about the wrong directory.
#[test]
fn the_probe_is_given_the_directory_of_the_file() {
    let asked: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let probe = |dir: &Path| -> io::Result<u32> {
        asked.lock().expect("lock").push(dir.to_owned());
        Err(io::Error::other("probe stopped"))
    };
    for file in ["/data/kbf/state.db", "state.db"] {
        let refused =
            SqliteStore::open_checked(Path::new(file), clock(), Options::default(), &probe)
                .expect_err("the probe fails");
        assert!(matches!(refused, SqliteError::Io(_)), "got {refused}");
        assert!(
            refused.to_string().contains("probe stopped"),
            "got {refused}"
        );
    }
    assert_eq!(
        *asked.lock().expect("lock"),
        [PathBuf::from("/data/kbf"), PathBuf::from(".")]
    );
}

// Catches a table that misses a network filesystem, or names a local one: every row of the
// table is checked, by its number and its name.
#[test]
fn network_filesystems_are_told_from_local_ones() {
    for (magic, name) in [
        (0x0000_6969, "NFS"),
        (0x0000_517B, "SMB"),
        (0xFF53_4D42, "CIFS"),
        (0xFE53_4D42, "SMB2"),
        (0x00C3_6400, "CephFS"),
        (0x0102_1997, "9p"),
        (0x5346_414F, "AFS"),
        (0x7375_7245, "Coda"),
        (0x0BD0_0BD0, "Lustre"),
        (0x4750_4653, "GPFS"),
        (0x7461_636F, "OCFS2"),
    ] {
        assert_eq!(network_filesystem(magic), Some(name), "{magic:#x}");
    }
    // ext4, xfs, btrfs, tmpfs, overlayfs, zfs, and "unknown".
    for magic in [
        0xEF53,
        0x5846_5342,
        0x9123_683E,
        0x0102_1994,
        0x794C_7630,
        0x2FC1_2FC1,
        0,
    ] {
        assert_eq!(
            network_filesystem(magic),
            None,
            "{magic:#x} is a local filesystem"
        );
    }
}

// Catches a refusal that also stops a state file on a local filesystem.
#[test]
fn a_local_filesystem_is_not_refused() {
    for file in ["/data/kbf/state.db", "state.db"] {
        refuse_network_filesystem(Path::new(file), &|_| Ok(0xEF53))
            .expect("ext4 is a local filesystem");
    }
}

// Catches a failed lock call being reported as another process holding the file (the
// operator would go looking for a process that does not exist), and a held lock being
// reported as anything else.
#[test]
fn a_lock_call_that_failed_is_not_a_held_lock() {
    let db = Path::new("/x/state.db");
    let held = lock_failed(db, lock_path(db), fs::TryLockError::WouldBlock);
    assert!(matches!(held, SqliteError::Locked { .. }), "got {held}");
    assert!(
        held.to_string().contains("another kbf process"),
        "got {held}"
    );
    let failed = lock_failed(
        db,
        lock_path(db),
        fs::TryLockError::Error(io::Error::from(io::ErrorKind::PermissionDenied)),
    );
    assert!(matches!(failed, SqliteError::Io(_)), "got {failed}");
    assert!(
        failed.to_string().contains("/x/state.db.lock"),
        "the text names the lock file: {failed}"
    );
}
// Catches a lock probe that takes a refused lock for a free one (a restore would plan to
// run under a live store), or a failed lock call for either answer.
#[test]
fn a_probe_tells_a_held_lock_from_a_free_one_and_a_failure() {
    let db = Path::new("/x/state.db");
    assert!(!held(db, lock_path(db), Ok(())).expect("free"));
    assert!(held(db, lock_path(db), Err(fs::TryLockError::WouldBlock)).expect("held"));
    let failed = held(
        db,
        lock_path(db),
        Err(fs::TryLockError::Error(io::Error::from(
            io::ErrorKind::PermissionDenied,
        ))),
    )
    .expect_err("failed");
    assert!(matches!(failed, SqliteError::Io(_)), "got {failed}");
}

// Catches a file's one name that is not its resolved path: a name without a file name
// in it resolved as a file, a bare name not put in the working directory, a plain file
// not left as it is, a link loop followed forever or taken for a file, and a
// path through a file taken for a name nobody holds.
#[test]
fn a_files_one_name_resolves_links_and_refuses_what_it_cannot_resolve() {
    assert_eq!(
        canonical_db(Path::new("/")).expect("the root"),
        Path::new("/")
    );
    let here = std::env::current_dir()
        .expect("cwd")
        .canonicalize()
        .expect("resolve");
    assert_eq!(
        canonical_db(Path::new("kbf-state-unit-no-such.db")).expect("a bare name"),
        here.join("kbf-state-unit-no-such.db"),
        "a bare name is in the working directory"
    );
    let dir = unit_dir("canonical").canonicalize().expect("resolve");
    std::os::unix::fs::symlink("b.db", dir.join("a.db")).expect("link");
    std::os::unix::fs::symlink("a.db", dir.join("b.db")).expect("link");
    let looped = canonical_db(&dir.join("a.db")).expect_err("a loop");
    assert!(
        looped
            .to_string()
            .contains("symbolic links follow one another"),
        "got {looped}"
    );
    let file = dir.join("a-file");
    fs::write(&file, b"").expect("write");
    assert_eq!(
        canonical_db(&file).expect("a file"),
        file,
        "a file is its own name"
    );
    let under = canonical_db(&file.join("state.db")).expect_err("not a directory");
    assert_eq!(under.kind(), io::ErrorKind::NotADirectory, "got {under}");
    assert_eq!(
        canonical_db(&dir.join("./x/../new.db"))
            .expect_err("no x")
            .kind(),
        io::ErrorKind::NotFound
    );
    fs::create_dir_all(dir.join("x")).expect("x");
    assert_eq!(
        canonical_db(&dir.join("./x/../new.db")).expect("resolved"),
        dir.join("new.db")
    );
}

// Catches a probe that fails on a real directory, which would refuse every start.
#[test]
fn the_real_probe_reads_the_filesystem_of_a_directory() {
    let magic = filesystem_type(&std::env::temp_dir()).expect("statfs of the temp directory");
    assert_eq!(
        network_filesystem(magic),
        None,
        "the test host's temp directory is local"
    );
}

// Catches an error that loses its text on the way to the caller.
#[test]
fn errors_carry_what_failed() {
    let broken = rusqlite::Error::InvalidQuery;
    let message = broken.to_string();
    assert_eq!(
        SqliteError::from(rusqlite::Error::InvalidQuery).to_string(),
        message
    );
    assert_eq!(
        StoreError::from(rusqlite::Error::InvalidQuery),
        StoreError::Failed(message)
    );
    let at = |error: SqliteError| error.at(Path::new("/x/state.db")).to_string();
    assert_eq!(
        at(SqliteError::Io("denied".to_owned())),
        "/x/state.db: denied"
    );
    assert_eq!(
        at(SqliteError::Sqlite("busy".to_owned())),
        "/x/state.db: busy"
    );
    assert_eq!(
        at(SqliteError::NotAStateStore("no".to_owned())),
        "/x/state.db: no"
    );
    let unchanged = SqliteError::NetworkFs {
        path: PathBuf::from("/x/state.db"),
        fs: "NFS",
    };
    let text = unchanged.to_string();
    assert_eq!(
        at(unchanged),
        text,
        "the variants that name their path are left alone"
    );
}

// Catches a journal mode that is not WAL being accepted: a commit would not be one sync.
#[test]
fn a_connection_that_cannot_keep_a_wal_is_refused() {
    let memory = rusqlite::Connection::open_in_memory().expect("open");
    let refused = super::schema::configure(&memory).expect_err("memory databases have no WAL");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );
}

// Catches a restore that treats a failed removal of the old journal as "already gone": the
// journal would be replayed into the restored file.
#[test]
fn only_a_missing_file_counts_as_removed() {
    assert!(ignore_not_found(io::Error::from(io::ErrorKind::NotFound)).is_ok());
    let refused = ignore_not_found(io::Error::from(io::ErrorKind::PermissionDenied));
    assert_eq!(
        refused.expect_err("denied").kind(),
        io::ErrorKind::PermissionDenied
    );
}

// Catches a connection that is not in WAL mode with a sync on every commit (the setting
// that makes an acknowledgement a durable write), on a real file: the failing case above
// is the in-memory one.
#[test]
fn a_file_connection_is_wal_with_full_sync() {
    let dir = unit_dir("configure");
    let conn = rusqlite::Connection::open(dir.join("state.db")).expect("open");
    super::schema::configure(&conn).expect("a file keeps a WAL");
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(mode, "wal");
    let sync: i64 = conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("synchronous");
    assert_eq!(sync, 2, "2 is FULL; 1 (NORMAL) would not sync each commit");
    drop(conn);
    fs::remove_dir_all(&dir).expect("remove the directory");
}

// Catches a feed page read without one snapshot for the head and the rows: a group
// committed between the two reads would put rows past the head in the page, and the
// cursor would fall behind the last row it returned. The commit is made at exactly that
// moment, so the test does not depend on a race.
#[test]
fn a_page_is_read_from_one_snapshot() {
    let dir = unit_dir("snapshot");
    let store =
        SqliteStore::open(&dir.join("state.db"), clock(), Options::default()).expect("open");
    let put = |key: &str| {
        store
            .txn(Txn::new().put(key, "v", Cond::Any))
            .expect("reachable")
            .expect("commits");
    };
    put("a");
    put("b");

    let page = changes_after_head(&mut store.reader(), Revision::ZERO, 10, &mut || put("c"))
        .expect("changes");
    let revisions: Vec<u64> = page.rows.iter().map(|row| row.revision.0).collect();
    assert_eq!(
        revisions,
        [1, 2],
        "the page holds the commits the head covers, and not the one made after it was read"
    );
    assert_eq!(page.cursor, Revision(2), "the cursor is the head it read");

    let next = store.changes(page.cursor, 10).expect("changes");
    let revisions: Vec<u64> = next.rows.iter().map(|row| row.revision.0).collect();
    assert_eq!(
        revisions,
        [3],
        "the next page starts at the commit it missed"
    );
    assert_eq!(next.cursor, Revision(3));
    drop(store);
    fs::remove_dir_all(&dir).expect("remove the directory");
}
