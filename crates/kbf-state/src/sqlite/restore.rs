//! The offline restore: replaces a state file with a backup of one, under the lock, and
//! moves the epoch past both.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::SqliteError;
use super::guard::StoreLock;
use super::schema::{bump_epoch, configure, epoch, first_i64, first_string, require_state_store};

/// `dest`'s name with `suffix` after it.
fn beside(dest: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = dest.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Replaces the state file `dest` with a copy of `backup`, which must be a kbf state
/// store (a file `sqlite3 .backup` wrote from one, or a copy of the file taken while no
/// process held it). The file in `dest` is dropped, with any journal beside it. The
/// epoch of the restored file becomes one past the larger of the backup's and the one
/// `dest` held, so a restored file cannot resume leases the replaced one issued; that
/// epoch is returned, and the store's next start bumps it again.
///
/// # Errors
/// [`SqliteError::Locked`] while a process has `dest` open (the restore is offline);
/// [`SqliteError::NetworkFs`] on a network filesystem; [`SqliteError::NotAStateStore`]
/// when `backup` is not a state store; otherwise the failing call. `dest` is untouched
/// when the restore fails before it replaces the file.
pub fn restore(backup: &Path, dest: &Path) -> Result<u64, SqliteError> {
    restore_into(backup, dest).map_err(|error| error.at(dest))
}

fn restore_into(backup: &Path, dest: &Path) -> Result<u64, SqliteError> {
    let _lock = StoreLock::take(dest)?;
    restore_with(backup, dest, &leave_as_copied)
}

/// The staged copy as `fs::copy` left it.
fn leave_as_copied(_staging: &Path) -> io::Result<()> {
    Ok(())
}

/// [`restore`] under a lock the caller already holds, so that it can work on the file
/// before the restore under the same lock (keep a copy of it). `prepare` is given the
/// staged copy (`<file>.restoring`, checked, its epoch stamped, closed) just before it
/// is renamed over the file, to give it its owner and mode; when it fails, nothing is
/// replaced.
///
/// # Errors
/// As [`restore`], less the lock's; and `prepare`'s.
pub fn restore_locked(
    lock: &StoreLock,
    backup: &Path,
    prepare: &dyn Fn(&Path) -> io::Result<()>,
) -> Result<u64, SqliteError> {
    restore_with(backup, lock.db(), prepare).map_err(|error| error.at(lock.db()))
}

fn restore_with(
    backup: &Path,
    dest: &Path,
    prepare: &dyn Fn(&Path) -> io::Result<()>,
) -> Result<u64, SqliteError> {
    let staging = beside(dest, ".restoring");
    let restored = stage(backup, dest, &staging).and_then(|epoch| {
        prepare(&staging)?;
        install(&staging, dest)?;
        Ok(epoch)
    });
    if restored.is_err() {
        // Best effort: a failed restore leaves no staging copy behind.
        let _ = fs::remove_file(&staging);
    }
    restored
}

/// An absent file is as good as a removed one.
pub(super) fn ignore_not_found(error: io::Error) -> io::Result<()> {
    if error.kind() == io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error)
    }
}

/// Moves the staged file over `dest`, after the journal of the file it replaces is gone
/// (a journal left beside a different file would be replayed into it).
fn install(staging: &Path, dest: &Path) -> Result<(), SqliteError> {
    for suffix in ["-wal", "-shm"] {
        fs::remove_file(beside(dest, suffix)).or_else(ignore_not_found)?;
    }
    fs::rename(staging, dest)?;
    let dir = dest.parent().filter(|dir| !dir.as_os_str().is_empty());
    fs::File::open(dir.unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(())
}

/// Copies `backup` to `staging`, checks it, stamps its epoch and folds its journal in,
/// so that one file is all there is to move. A log a restore that died left beside the
/// staged name goes first: opening the copy would replay it into the copy.
fn stage(backup: &Path, dest: &Path, staging: &Path) -> Result<u64, SqliteError> {
    for suffix in ["-wal", "-shm"] {
        fs::remove_file(beside(staging, suffix)).or_else(ignore_not_found)?;
    }
    fs::copy(backup, staging).map_err(|error| SqliteError::from(error).at(backup))?;
    let replaced = if dest.exists() {
        epoch(&Connection::open(dest)?)?
    } else {
        0
    };
    let conn = Connection::open(staging)?;
    configure(&conn)?;
    require_state_store(&conn).map_err(|error| error.at(backup))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], first_string)?;
    if integrity != "ok" {
        return Err(SqliteError::NotAStateStore(format!(
            "{}: the backup is damaged ({integrity})",
            backup.display()
        )));
    }
    let restored = bump_epoch(&conn, replaced)?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], first_i64)?;
    Ok(restored)
}
