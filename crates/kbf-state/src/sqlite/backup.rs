//! The online backup of a state file, and the check of a backup before it is restored.
//!
//! The backup is `VACUUM INTO`: one read transaction on its own connection, so in WAL
//! mode it sees one committed state (the groups in the write-ahead log included) while
//! the store goes on writing, and it never blocks the writer. The online backup API was
//! the other way; copying page by page, it starts over whenever another process writes,
//! so under a steady write rate it may never finish.
//!
//! A backup is a single file: it is turned to the rollback journal before it is checked,
//! so no write-ahead log beside it holds part of it, and reading it creates no file.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use super::SqliteError;
use super::schema::{BUSY_TIMEOUT, epoch, first_i64, first_string, head, require_state_store};
use super::writer::uint;

/// The first 16 bytes of every SQLite database file.
const MAGIC: &[u8] = b"SQLite format 3\0";

/// Bytes 18 and 19 of the header (write and read version) of a file in a rollback
/// journal mode; a WAL-mode file has 2 and 2.
const ROLLBACK: &[u8] = &[1, 1];

/// What a state file holds, as its backup's check reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The epoch the file records (the store's next open moves past it).
    pub epoch: u64,
    /// The head revision: the last committed change.
    pub head: u64,
    /// Rows in the key table, expired keys that were not reaped yet included.
    pub keys: u64,
}

/// Writes a consistent copy of the state file `db` to `into`, which must not exist (or
/// be empty), while a store may be writing `db`; syncs it and checks it as
/// [`check_backup`] does. `db` is never created or changed: a missing `db` fails.
///
/// # Errors
/// [`SqliteError::NotAStateStore`] when `db` is not a state store or the copy fails its
/// check; otherwise the failing call.
pub fn backup(db: &Path, into: &Path) -> Result<Summary, SqliteError> {
    copy_into(db, into).map_err(|error| error.at(db))?;
    check_backup(into)
}

fn copy_into(db: &Path, into: &Path) -> Result<(), SqliteError> {
    let Some(name) = into.to_str() else {
        return Err(SqliteError::Io(format!(
            "{}: the backup's path is not UTF-8",
            into.display()
        )));
    };
    let source = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    source.busy_timeout(BUSY_TIMEOUT)?;
    require_state_store(&source)?;
    source.execute("VACUUM INTO ?1", [name])?;
    drop(source);
    let copy = Connection::open_with_flags(into, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    copy.query_row("PRAGMA journal_mode = DELETE", [], first_string)?;
    drop(copy);
    File::open(into)?.sync_all()?;
    Ok(())
}

/// Checks that `file` is a backup a restore can take whole: a single-file SQLite
/// database (not in WAL mode, so no log beside it holds part of it) whose schema is the
/// state store's and whose `integrity_check` is `ok`; and reads what it holds. The file
/// is opened read-only and nothing is created beside it.
///
/// # Errors
/// [`SqliteError::NotAStateStore`] when it is not one; otherwise the failing call.
pub fn check_backup(file: &Path) -> Result<Summary, SqliteError> {
    check(file).map_err(|error| error.at(file))
}

fn check(file: &Path) -> Result<Summary, SqliteError> {
    let mut header = Vec::with_capacity(100);
    File::open(file)?.take(100).read_to_end(&mut header)?;
    single_file(&header)?;
    let conn = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    require_state_store(&conn)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], first_string)?;
    if integrity != "ok" {
        return Err(SqliteError::NotAStateStore(format!(
            "the file is damaged ({integrity})"
        )));
    }
    let keys = conn.query_row("SELECT count(*) FROM kv", [], first_i64)?;
    Ok(Summary {
        epoch: epoch(&conn)?,
        head: uint(head(&conn)?),
        keys: uint(keys),
    })
}

/// Refuses a header that is not a SQLite database's, or one of a WAL-mode file.
fn single_file(header: &[u8]) -> Result<(), SqliteError> {
    if header.get(..MAGIC.len()) != Some(MAGIC) {
        return Err(SqliteError::NotAStateStore(
            "not a SQLite database".to_owned(),
        ));
    }
    if header.get(18..20) != Some(ROLLBACK) {
        return Err(SqliteError::NotAStateStore(
            "a WAL-mode file, which a log beside it may complete; a backup is a single \
             file (take it with the online backup)"
                .to_owned(),
        ));
    }
    Ok(())
}
