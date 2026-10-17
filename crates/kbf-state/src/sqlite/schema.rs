//! The file's layout, the settings every connection takes, and the epoch.

use std::time::Duration;

use rusqlite::{Connection, Row, Transaction, TransactionBehavior};

use super::SqliteError;

/// The `user_version` of a file this binary creates and reads (`kbf --version --json`
/// reports it as `state_schema`).
pub const SCHEMA_VERSION: i64 = 1;

/// How long a connection waits for a lock another connection holds (a checkpoint).
pub(super) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The feed row kinds, stored as integers.
pub(super) const KIND_PUT: i64 = 0;
pub(super) const KIND_DELETE: i64 = 1;
pub(super) const KIND_EXPIRE: i64 = 2;

/// `kv` holds the live keys (a version is the revision that wrote the value, an expiry is
/// nanoseconds on the store's clock), `feed` the change rows by revision, `counter` the
/// counters, and `meta` the head revision and the epoch.
const SCHEMA: &str = "
CREATE TABLE kv (
    key TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL,
    version INTEGER NOT NULL,
    expires_at INTEGER
) WITHOUT ROWID;
CREATE INDEX kv_expiry ON kv (expires_at) WHERE expires_at IS NOT NULL;
CREATE TABLE counter (
    name TEXT PRIMARY KEY NOT NULL,
    value INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE feed (
    revision INTEGER PRIMARY KEY NOT NULL,
    key TEXT NOT NULL,
    kind INTEGER NOT NULL,
    value BLOB
);
CREATE TABLE meta (
    name TEXT PRIMARY KEY NOT NULL,
    value INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO meta (name, value) VALUES ('head', 0), ('epoch', 0);
";

/// The first column of a row, as an integer. The row readers are named functions rather
/// than closures so that each statement that uses one is a single covered line.
pub(super) fn first_i64(row: &Row<'_>) -> rusqlite::Result<i64> {
    row.get(0)
}

/// The first column of a row, as text.
pub(super) fn first_string(row: &Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}

/// The head revision the file records.
pub(super) fn head(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT value FROM meta WHERE name = 'head'", [], first_i64)
}

/// Puts a connection in WAL mode with a sync on every commit (a commit is acknowledged
/// only once it is on disk).
pub(super) fn configure(conn: &Connection) -> Result<(), SqliteError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], first_string)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(SqliteError::NotAStateStore(format!(
            "the filesystem cannot keep a WAL-mode database (journal mode {mode})"
        )));
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

/// Creates the layout in an empty file, and refuses a file that is not this binary's.
pub(super) fn init(conn: &Connection) -> Result<(), SqliteError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", first_i64)?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], first_i64)?;
    if version != 0 || tables != 0 {
        return Err(SqliteError::NotAStateStore(format!(
            "not a kbf state store this binary reads (schema version {version}, {tables} objects)"
        )));
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// Refuses a file that is not a state store this binary reads.
pub(super) fn require_state_store(conn: &Connection) -> Result<(), SqliteError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", first_i64)?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    Err(SqliteError::NotAStateStore(format!(
        "not a kbf state store this binary reads (schema version {version})"
    )))
}

/// The epoch the file records.
pub(super) fn epoch(conn: &Connection) -> Result<u64, SqliteError> {
    let epoch: i64 =
        conn.query_row("SELECT value FROM meta WHERE name = 'epoch'", [], first_i64)?;
    Ok(u64::try_from(epoch).unwrap_or_default())
}

/// Moves the epoch past both the one the file records and `floor`, durably, and returns
/// it. A restart passes zero; a restore passes the epoch of the file it replaces, so an
/// epoch is never issued twice.
pub(super) fn bump_epoch(conn: &Connection, floor: u64) -> Result<u64, SqliteError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let next = epoch(&tx)?.max(floor).saturating_add(1);
    let stored = i64::try_from(next).unwrap_or(i64::MAX);
    tx.execute("UPDATE meta SET value = ?1 WHERE name = 'epoch'", [stored])?;
    tx.commit()?;
    Ok(next)
}
