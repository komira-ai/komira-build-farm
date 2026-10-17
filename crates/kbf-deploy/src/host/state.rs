//! The deploy state, `/var/lib/kbf-deploy/state.json`, and the lock it is changed under.
//!
//! The file is one JSON object: a `schema` number ([`SCHEMA`]) and the fields of
//! [`State`]. It is changed only through a [`Locked`], which holds an exclusive `flock(2)`
//! on `/run/kbf-deploy.lock` for as long as it lives, so two kbf-deploy runs cannot both
//! read the state, change it and write it back (the second would undo the first). The
//! lock file and the state file are made 0600, root's alone. A write makes a new file
//! beside the old one, syncs it, renames it over the old one and syncs the directory: a
//! reader sees the old state or the new one whole, never part of either, and a power cut
//! after the write returned keeps the new one. That is also why [`status`] and a dry run
//! ([`read_unlocked`]), which only read, need no lock.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use kbf_host::path::Root;
use serde::{Deserialize, Serialize};

use super::CommitId;
use super::layout;

/// The `schema` number this build writes and the only one it reads.
pub const SCHEMA: u64 = 1;

/// What a host has deployed. Each commit names a directory under
/// `/usr/local/kbf/releases/`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// The release `current` should point to.
    pub current: Option<CommitId>,
    /// The release `previous` should point to: what a rollback goes back to.
    pub previous: Option<CommitId>,
    /// A release verified and ready to swap in.
    pub staged: Option<CommitId>,
    /// A release swapped in and not yet confirmed.
    pub pending_confirm: Option<PendingConfirm>,
    /// The order number below which this host takes no release. Only stored here.
    pub order_floor: u64,
}

/// A swap that rolls back unless it is confirmed by `deadline`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingConfirm {
    /// The release swapped in.
    pub commit: CommitId,
    /// The release that ran before it, which a rollback restores; `None` on a host's
    /// first deploy.
    pub previous: Option<CommitId>,
    /// Seconds since the Unix epoch.
    pub deadline: u64,
}

/// Why the state could not be read, written or locked.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("cannot lock {path}: {source}")]
    Lock { path: PathBuf, source: io::Error },
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} is not a deploy state: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path} has schema {found}; this kbf-deploy reads only schema {SCHEMA}")]
    Schema { path: PathBuf, found: String },
    #[error("cannot write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

/// The deploy lock, held: the only way to read the state for a change and to write it.
/// Dropping it releases the lock.
#[derive(Debug)]
pub struct Locked {
    _lock: File,
    file: PathBuf,
}

impl Locked {
    /// Take the exclusive lock on `/run/kbf-deploy.lock` under `root`, waiting while
    /// another run holds it. The lock file is made (mode 0600) when missing; its
    /// directory must exist.
    pub fn acquire(root: &Root) -> Result<Locked, StateError> {
        let path = root.join(&layout::path(layout::LOCK));
        let fail = |source| StateError::Lock {
            path: path.clone(),
            source,
        };
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(fail)?;
        lock.lock().map_err(fail)?;
        Ok(Locked {
            _lock: lock,
            file: state_file(root),
        })
    }

    /// The state; `None` when no state file exists yet.
    pub fn read(&self) -> Result<Option<State>, StateError> {
        read_file(&self.file)
    }

    /// Replace the state file with `state`, whole (see the module docs). The state
    /// directory must exist.
    pub fn write(&self, state: &State) -> Result<(), StateError> {
        let fail = |source| StateError::Write {
            path: self.file.clone(),
            source,
        };
        let mut bytes = serde_json::to_vec_pretty(&OnDisk {
            schema: SCHEMA,
            state,
        })
        .expect("a state serializes");
        bytes.push(b'\n');
        replace(&self.file, &bytes).map_err(fail)
    }
}

/// The file as written: the schema number first, then the state's fields.
#[derive(Serialize)]
struct OnDisk<'a> {
    schema: u64,
    #[serde(flatten)]
    state: &'a State,
}

fn state_file(root: &Root) -> PathBuf {
    root.join(&layout::path(layout::STATE_FILE))
}

/// Write `bytes` to a new file beside `path` (mode 0600: root's alone), sync it, rename it over `path` and sync
/// the directory.
fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_file_name(".state.json.tmp");
    // A copy left by an interrupted run; `create_new` never follows a link there.
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    // The two syncs are for a power cut, which no test makes: none would fail without them.
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    let dir = path.parent().expect("the state file is in a directory");
    File::open(dir)?.sync_all()
}

/// The state under `root`, read without the lock: for a dry run, which changes nothing
/// and so takes no lock (taking it would make the lock file). `None` when there is no
/// state file yet.
pub fn read_unlocked(root: &Root) -> Result<Option<State>, StateError> {
    read_file(&state_file(root))
}

/// The state in `path`; `None` when there is no such file.
fn read_file(path: &Path) -> Result<Option<State>, StateError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StateError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    parse(path, &bytes).map(Some)
}

/// A state file's bytes: refused unless its `schema` is [`SCHEMA`] and every other
/// field is one of [`State`]'s.
fn parse(path: &Path, bytes: &[u8]) -> Result<State, StateError> {
    let fail = |source| StateError::Parse {
        path: path.to_owned(),
        source,
    };
    let mut object: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(bytes).map_err(fail)?;
    let schema = object.remove("schema").unwrap_or(serde_json::Value::Null);
    if schema != SCHEMA {
        return Err(StateError::Schema {
            path: path.to_owned(),
            found: schema.to_string(),
        });
    }
    serde_json::from_value(serde_json::Value::Object(object)).map_err(fail)
}

/// What the `current` link is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CurrentLink {
    Missing,
    /// A symbolic link, with its target as written.
    To(PathBuf),
    /// Something other than a link: a file or a directory.
    NotALink,
}

/// What `host status` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Whether a state file exists; without one, `state` is the empty state.
    pub exists: bool,
    pub state: State,
    pub link: CurrentLink,
}

impl Status {
    /// Whether `current` points at the state's current release: a link to exactly
    /// `releases/<commit>`, or no link at all when the state has no current release.
    pub fn link_matches(&self) -> bool {
        match (&self.state.current, &self.link) {
            (Some(c), CurrentLink::To(target)) => *target == layout::release_link(c),
            (None, CurrentLink::Missing) => true,
            _ => false,
        }
    }
}

/// The state and the `current` link under `root`, read without the lock: a write
/// replaces the state file by a rename, so the file read is one whole state.
pub fn status(root: &Root) -> Result<Status, StateError> {
    let state = read_unlocked(root)?;
    let link_path = root.join(&layout::path(layout::CURRENT));
    let link = current_link(fs::read_link(&link_path)).map_err(|source| StateError::Read {
        path: link_path,
        source,
    })?;
    Ok(Status {
        exists: state.is_some(),
        state: state.unwrap_or_default(),
        link,
    })
}

/// What `read_link` of `current` says: EINVAL is something there that is not a link.
fn current_link(read: io::Result<PathBuf>) -> io::Result<CurrentLink> {
    match read {
        Ok(target) => Ok(CurrentLink::To(target)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(CurrentLink::Missing),
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => Ok(CurrentLink::NotALink),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
pub(crate) mod tests;
