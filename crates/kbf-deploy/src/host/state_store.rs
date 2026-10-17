//! `host state-backup` and `host state-restore`: the SQLite state store's file (the
//! path of the frontend's `--state-store=sqlite:///<path>`) copied while the frontend
//! runs, and put back while it is stopped. Both are Linux root verbs an operator's
//! wrapper runs; both use `kbf-state`'s own backup, check, lock and restore.
//!
//! ```text
//! kbf-deploy host state-backup --db FILE --dest FILE [--owner USER[:GROUP]] [--mode 0640]
//! kbf-deploy host state-restore --src FILE --db FILE [--apply]
//! ```
//!
//! `state-backup` runs without `--apply`: it changes no live state, and writes only
//! `--dest` (through `<dest>.kbf-backup-tmp` beside it). It is refused when anything is
//! at `--dest`. The copy is SQLite's `VACUUM INTO` on a connection of its own, one read
//! transaction, so it is consistent while the frontend commits, the commits still in the
//! write-ahead log included, and it never blocks the frontend's writer. The copy is
//! turned into a single file (the rollback journal, not WAL), passes `integrity_check`
//! and the state store's schema check, gets its owner and mode, is synced, and is linked
//! to `--dest` (which fails, rather than replaces, if a file appeared there meanwhile);
//! then the directory is synced. It prints the copy's epoch, head revision and key count.
//!
//! `state-restore` plans by default and changes things only with `--apply`. Both are
//! refused when `--db` is a symbolic link (the rename would replace the link, not the
//! file it names), when `--db` has a second hard link (the lock is beside one name, so
//! a frontend that opened another would not be seen, and the rename would leave it
//! writing the replaced file; the store itself refuses to open such a file, and a
//! backup of one only warns, its copy being consistent), when the source is not a single-file state store backup whose
//! `integrity_check` is `ok` (what `state-backup` writes), and while a process holds the
//! store's lock (the `flock` every open store holds: the frontend is running). The lock
//! file is `<db>.lock` beside the file's resolved name (links followed), so the
//! frontend's spelling of the path and the operator's need not be the same. The dry run
//! probes the lock without creating the lock file; the apply takes it and holds it to
//! the end, so no frontend can start in between. Under the lock it keeps the current
//! file as `<db>.pre-restore-<UTC stamp>`, and its write-ahead log, if there is one, as
//! that name with `-wal` after it. With the lock free, no connection has the file open,
//! so a log with frames in it holds commits a stopped frontend did not fold back into
//! the file (it was killed): they are part of the current state, so the kept copy takes
//! them (SQLite replays the log when the kept file is opened) rather than the restore
//! refusing; the `-shm` index is rebuilt from the log, so it is not kept. Then
//! `kbf-state`'s restore removes any log a restore that died left beside
//! `<db>.restoring`, copies the source to `<db>.restoring`, checks it again,
//! moves its epoch past both files', gives it the current file's owner and mode (or,
//! with no current file, the directory's owner and mode 0600), syncs it, removes the old
//! log beside `<db>` and renames the copy over `<db>`, and syncs the directory. The lock
//! file goes to the same owner, so the frontend can open it after a restore made it.
//!
//! Each exits 0 when done (or planned), 1 when a step failed (`FAIL: ...`), 2 refused
//! (`REFUSED: ...`), on stderr.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown, fchown};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

use clap::Args;
use kbf_host::exec::{Exec, HostExec};
use kbf_host::path::Root;
use kbf_host::step::{Cmd, Owner, Perms};
use kbf_host::user::{self, GETENT};
use kbf_state::sqlite::{self, SqliteError, StoreLock, Summary};
use time::OffsetDateTime;

use crate::linux::host::tool;

#[derive(Args)]
pub struct BackupArgs {
    /// The state file the frontend writes (the path in its `--state-store=sqlite:///`).
    #[arg(long)]
    db: PathBuf,
    /// The backup to write; refused when anything is there.
    #[arg(long)]
    dest: PathBuf,
    /// The backup's owner, `USER[:GROUP]` by name (without a group, the user's own);
    /// without it, the running user's.
    #[arg(long)]
    owner: Option<String>,
    /// The backup's permission bits, in octal.
    #[arg(long, default_value = "0640", value_parser = octal)]
    mode: Perms,
}

#[derive(Args)]
pub struct RestoreArgs {
    /// The backup to restore, as `state-backup` writes it.
    #[arg(long)]
    src: PathBuf,
    /// The state file to replace (the path in the frontend's `--state-store=sqlite:///`).
    #[arg(long)]
    db: PathBuf,
    /// Restore; without it, only check and print the plan.
    #[arg(long)]
    apply: bool,
}

fn octal(text: &str) -> Result<Perms, String> {
    u32::from_str_radix(text, 8)
        .ok()
        .filter(|bits| *bits <= 0o7777)
        .map(Perms::new)
        .ok_or_else(|| format!("{text:?} is not octal permission bits (at most 7777)"))
}

/// Why a state file verb did not finish.
#[derive(Debug, thiserror::Error)]
pub enum StateFileError {
    #[error("{}: something is there already; a backup never replaces a file", .0.display())]
    DestExists(PathBuf),
    #[error("--owner {spec}: {why}")]
    Owner { spec: String, why: String },
    #[error("not a backup to restore: {0}")]
    Source(SqliteError),
    #[error(
        "{}: a process holds its lock (the frontend is running); stop it and try again",
        .0.display()
    )]
    InUse(PathBuf),
    #[error(
        "--db {}: a symbolic link; give the state file's own path (a restore renames \
         over the name it is given, so it would replace the link, not the file)",
        .0.display()
    )]
    DbIsLink(PathBuf),
    #[error(
        "--db {}: {links} names (hard links); a frontend that opens another name locks \
         another file, so the restore would replace the file under it. Keep one name \
         (remove the other links) and try again",
        path.display()
    )]
    DbHardLinked { path: PathBuf, links: u64 },
    #[error("{0}")]
    Store(SqliteError),
    #[error("{what} {}: {source}", path.display())]
    Io {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

/// `host state-backup`, with `--owner` looked up on this host.
pub fn run_backup(a: BackupArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    report(backup_verb(&a, out), err)
}

fn backup_verb(a: &BackupArgs, out: &mut dyn Write) -> Result<(), StateFileError> {
    let owner = match &a.owner {
        Some(spec) => Some(owner(&HostExec::new(Root::host()), spec)?),
        None => None,
    };
    backup(&a.db, &a.dest, owner, a.mode, out)
}

/// `host state-restore`, its kept copy stamped with the time now.
pub fn run_restore(a: RestoreArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let stamp = utc_stamp(SystemTime::now());
    report(restore(&a.src, &a.db, a.apply, &stamp, out), err)
}

fn report(result: Result<(), StateFileError>, err: &mut dyn Write) -> ExitCode {
    let Err(e) = result else {
        return ExitCode::SUCCESS;
    };
    let failed = matches!(e, StateFileError::Store(_) | StateFileError::Io { .. });
    let (word, code) = if failed { ("FAIL", 1) } else { ("REFUSED", 2) };
    let _ = writeln!(err, "{word}: {e}");
    ExitCode::from(code)
}

/// `USER[:GROUP]` as ids, from the host's user and group databases (`getent`): the
/// user's uid, and the group's gid or, without a group, the user's own.
pub fn owner<E: Exec>(exec: &E, spec: &str) -> Result<Owner, StateFileError> {
    let refused = |why: String| StateFileError::Owner {
        spec: spec.to_owned(),
        why,
    };
    let (name, group) = match spec.split_once(':') {
        Some((name, group)) => (name, Some(group)),
        None => (spec, None),
    };
    let found = user::lookup(exec, name)
        .map_err(|e| refused(e.to_string()))?
        .ok_or_else(|| refused(format!("no user {name:?}")))?;
    let gid = match group {
        Some(group) => group_id(exec, group).map_err(refused)?,
        None => found.gid,
    };
    Ok(Owner {
        uid: found.uid,
        gid,
    })
}

/// The gid of the group `name`: `getent group`, `name:password:gid:members`.
fn group_id<E: Exec>(exec: &E, name: &str) -> Result<u32, String> {
    let cmd = Cmd::new(tool(GETENT)).arg("group").arg("--").arg(name);
    let out = exec.query(&cmd).map_err(|e| format!("{cmd}: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = text.trim_end().split(':').collect();
    let gid = match fields[..] {
        [group, _, gid, _] if group == name => gid.parse().ok(),
        _ => None,
    };
    gid.filter(|_| out.status.success())
        .ok_or_else(|| format!("no group {name:?} ({cmd}: {}, {text:?})", out.status))
}

/// `path` with `suffix` after its name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The directory `path` is in.
fn dir_of(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

fn io_at(what: &'static str, path: &Path) -> impl FnOnce(io::Error) -> StateFileError {
    let path = path.to_owned();
    move |source| StateFileError::Io { what, path, source }
}

/// An absent file is as good as a removed one.
fn gone(error: io::Error) -> io::Result<()> {
    if error.kind() == io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error)
    }
}

/// Gives an open file its owner (when one is given) and mode, and syncs it.
fn seal(file: &File, owner: Option<Owner>, perms: Perms) -> io::Result<()> {
    if let Some(o) = owner {
        fchown(file, Some(o.uid), Some(o.gid))?;
    }
    file.set_permissions(Permissions::from_mode(perms.bits()))?;
    file.sync_all()
}

/// Syncs the directory `path` is in, so a name made or replaced there lasts.
fn sync_dir(path: &Path) -> Result<(), StateFileError> {
    let dir = dir_of(path);
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_at("sync the directory", dir))
}

/// The link count of the file `db`, when it has more than one name.
fn extra_names(db: &Path) -> Option<u64> {
    fs::metadata(db)
        .ok()
        .filter(|m| m.is_file() && m.nlink() > 1)
        .map(|m| m.nlink())
}

/// Writes a consistent, checked copy of `db` to `dest`, which must not exist.
pub fn backup(
    db: &Path,
    dest: &Path,
    owner: Option<Owner>,
    perms: Perms,
    out: &mut dyn Write,
) -> Result<(), StateFileError> {
    if dest.symlink_metadata().is_ok() {
        return Err(StateFileError::DestExists(dest.to_owned()));
    }
    let tmp = beside(dest, ".kbf-backup-tmp");
    fs::remove_file(&tmp)
        .or_else(gone)
        .map_err(io_at("remove the stale", &tmp))?;
    let written = write_backup(db, dest, &tmp, owner, perms);
    // The link at `dest` is the backup; the temporary name goes, written or not.
    let _ = fs::remove_file(&tmp);
    let s = written?;
    let owned = match owner {
        Some(o) => format!(", owner {}:{}", o.uid, o.gid),
        None => String::new(),
    };
    if let Some(links) = extra_names(db) {
        let _ = writeln!(
            out,
            "warning: {} has {links} names (hard links); the copy is consistent, but a \
             frontend and a restore must agree on one name (state-restore refuses this file)",
            db.display()
        );
    }
    let _ = writeln!(
        out,
        "backed up {} to {}: epoch {}, head revision {}, {} keys, integrity ok; \
         mode {perms}{owned}",
        db.display(),
        dest.display(),
        s.epoch,
        s.head,
        s.keys
    );
    Ok(())
}

fn write_backup(
    db: &Path,
    dest: &Path,
    tmp: &Path,
    owner: Option<Owner>,
    perms: Perms,
) -> Result<Summary, StateFileError> {
    let summary = sqlite::backup(db, tmp).map_err(StateFileError::Store)?;
    let file = File::open(tmp).map_err(io_at("open", tmp))?;
    seal(&file, owner, perms).map_err(io_at("set the owner and mode of", tmp))?;
    fs::hard_link(tmp, dest).map_err(io_at("link the backup to", dest))?;
    sync_dir(dest)?;
    Ok(summary)
}

/// What a restore does to the current file and gives the restored one.
struct Plan {
    /// Where the current file is kept, when there is one.
    keep: Option<PathBuf>,
    /// Whether a write-ahead log is beside the current file, to keep with it.
    wal: bool,
    owner: Owner,
    perms: Perms,
}

/// Checks `src`, plans the restore of `db` and, with `apply`, restores it.
pub fn restore(
    src: &Path,
    db: &Path,
    apply: bool,
    stamp: &str,
    out: &mut dyn Write,
) -> Result<(), StateFileError> {
    if db
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(StateFileError::DbIsLink(db.to_owned()));
    }
    if let Some(links) = extra_names(db) {
        return Err(StateFileError::DbHardLinked {
            path: db.to_owned(),
            links,
        });
    }
    let s = sqlite::check_backup(src).map_err(StateFileError::Source)?;
    let lock = hold(db, apply)?;
    let plan = plan(db, stamp)?;
    let _ = writeln!(
        out,
        "source {}: a state store backup, integrity ok (epoch {}, head revision {}, {} keys)",
        src.display(),
        s.epoch,
        s.head,
        s.keys
    );
    let with_log = if plan.wal { ", with its log" } else { "" };
    let _ = match &plan.keep {
        Some(keep) => writeln!(
            out,
            "keep {} as {}{}",
            db.display(),
            keep.display(),
            with_log
        ),
        None => writeln!(out, "no state file at {}: nothing to keep", db.display()),
    };
    let _ = writeln!(
        out,
        "replace {} with a copy of the source: checked, its epoch moved past both files', \
         owner {}:{}, mode {}, synced, renamed over it",
        db.display(),
        plan.owner.uid,
        plan.owner.gid,
        plan.perms
    );
    let Some(lock) = lock else {
        let _ = writeln!(out, "dry run: nothing changed; --apply restores");
        return Ok(());
    };
    let epoch = execute(&lock, src, &plan)?;
    let _ = writeln!(
        out,
        "restored {} (epoch {epoch}; the frontend's next start moves past it)",
        db.display()
    );
    Ok(())
}

/// With `apply`, the store's lock, held to the end of the restore; without it, a probe
/// of the lock that changes nothing. Refused either way while a process holds it.
fn hold(db: &Path, apply: bool) -> Result<Option<StoreLock>, StateFileError> {
    let held = if apply {
        match StoreLock::take(db) {
            Ok(lock) => return Ok(Some(lock)),
            Err(SqliteError::Locked { .. }) => true,
            Err(error) => return Err(StateFileError::Store(error)),
        }
    } else {
        StoreLock::is_held(db).map_err(StateFileError::Store)?
    };
    if held {
        return Err(StateFileError::InUse(db.to_owned()));
    }
    Ok(None)
}

fn plan(db: &Path, stamp: &str) -> Result<Plan, StateFileError> {
    let meta = match fs::metadata(db) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let dir = dir_of(db);
            let meta = fs::metadata(dir).map_err(io_at("read", dir))?;
            return Ok(Plan {
                keep: None,
                wal: false,
                owner: Owner {
                    uid: meta.uid(),
                    gid: meta.gid(),
                },
                perms: Perms::new(0o600),
            });
        }
        Err(e) => return Err(io_at("read", db)(e)),
    };
    Ok(Plan {
        keep: Some(beside(db, &format!(".pre-restore-{stamp}"))),
        wal: beside(db, "-wal").exists(),
        owner: Owner {
            uid: meta.uid(),
            gid: meta.gid(),
        },
        perms: Perms::new(meta.mode() & 0o7777),
    })
}

fn execute(lock: &StoreLock, src: &Path, plan: &Plan) -> Result<u64, StateFileError> {
    let (db, lock_file) = (lock.db(), lock.lock_file());
    let (uid, gid) = (plan.owner.uid, plan.owner.gid);
    chown(lock_file, Some(uid), Some(gid)).map_err(io_at("chown", lock_file))?;
    if let Some(keep) = &plan.keep {
        copy_new(db, keep, plan)?;
        if plan.wal {
            copy_new(&beside(db, "-wal"), &beside(keep, "-wal"), plan)?;
        }
        sync_dir(keep)?;
    }
    let prepare = |staging: &Path| seal(&File::open(staging)?, Some(plan.owner), plan.perms);
    sqlite::restore_locked(lock, src, &prepare).map_err(StateFileError::Store)
}

/// Copies `from` to a new file `to` with the plan's owner and mode, synced.
fn copy_new(from: &Path, to: &Path, plan: &Plan) -> Result<(), StateFileError> {
    let mut source = File::open(from).map_err(io_at("open", from))?;
    let mut copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .map_err(io_at("create", to))?;
    io::copy(&mut source, &mut copy).map_err(io_at("copy into", to))?;
    seal(&copy, Some(plan.owner), plan.perms).map_err(io_at("set the owner and mode of", to))
}

/// `at` in UTC as `YYYYMMDDTHHMMSSZ`, the stamp of a kept file's name.
pub fn utc_stamp(at: SystemTime) -> String {
    let t = OffsetDateTime::from(at);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

#[cfg(test)]
#[path = "state_store_tests.rs"]
mod tests;
