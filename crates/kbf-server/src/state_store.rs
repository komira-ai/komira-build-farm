//! `--state-store`: where the scheduler keeps its state (`docs/design/architecture-v2.md`,
//! section 4.3).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use kbf_state::{Fenced, Store};

use crate::farm::{EpochPolicy, StateError};
use crate::members::MembershipConfig;

/// A `--state-store` value. Only SQLite is wired; a Postgres store plugs in behind the
/// [`Store`] trait of `kbf-state` when it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateStoreUri {
    /// `sqlite:///absolute/path`: a database file on a local disk, owned by one process.
    Sqlite(PathBuf),
}

/// Why a `--state-store` value is refused. No message carries any part of the value: a
/// mistyped `postgres://` URL holds a password, and the command line is world-readable
/// and logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateStoreUriError {
    /// Any value that is not `sqlite://` followed by a path.
    #[error("only sqlite:///<absolute path> is supported (postgres is not wired yet)")]
    Unsupported,
    /// A `sqlite://` value that does not name a file by an absolute path.
    #[error("sqlite:// needs an absolute file path, as in sqlite:///var/lib/kbf/state.db")]
    SqlitePath,
}

impl FromStr for StateStoreUri {
    type Err = StateStoreUriError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let path = s
            .strip_prefix("sqlite://")
            .ok_or(StateStoreUriError::Unsupported)?;
        if path.starts_with('/') && path.len() > 1 {
            Ok(Self::Sqlite(PathBuf::from(path)))
        } else {
            Err(StateStoreUriError::SqlitePath)
        }
    }
}

/// The parser of `--state-store`. clap quotes a value back in the error of any parser it
/// wraps (`invalid value '<value>'`), and a value may hold a password, so this one builds
/// its own error, which is the fixed text of [`StateStoreUriError`].
#[derive(Clone, Copy)]
pub struct StateStoreParser;

impl clap::builder::TypedValueParser for StateStoreParser {
    type Value = StateStoreUri;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<StateStoreUri, clap::Error> {
        let refuse = |why: StateStoreUriError| {
            cmd.clone().error(
                clap::error::ErrorKind::ValueValidation,
                format!("--state-store: {why}"),
            )
        };
        value
            .to_str()
            .ok_or(StateStoreUriError::Unsupported)
            .and_then(|text| text.parse::<StateStoreUri>())
            .map_err(refuse)
    }
}

impl StateStoreUri {
    /// What a start does with the store's epoch. A SQLite file is the process's own, so
    /// every start is a new incarnation: a restored or swapped file can never resume
    /// leases an earlier one granted.
    #[must_use]
    pub const fn epoch_policy(&self) -> EpochPolicy {
        match self {
            Self::Sqlite(_) => EpochPolicy::Bump,
        }
    }

    /// The file the store is.
    #[must_use]
    pub fn path(&self) -> &Path {
        let Self::Sqlite(path) = self;
        path
    }

    /// Opens the store. The file is created, with its directory, if missing (directories
    /// it makes are 0700 and the file 0600: the scheduler's state is for this user alone),
    /// and held by this process alone: a second open of it (a second frontend) is
    /// refused.
    ///
    /// This blocks, as the store's calls do, and may be called from an async task.
    ///
    /// # Errors
    /// [`StateError::Open`]: the file is held by another process, is on a network
    /// filesystem or is not a state store.
    pub fn open(&self) -> Result<StateStore, StateError> {
        self.open_at(None, self.epoch_policy())
    }

    /// Opens the store as the successor of the process that held generation `generation`
    /// and has let go of it, and keeps its epoch: the daemons that reconnect keep the runs
    /// of the leases they hold. Nothing is written unless the store is at exactly that
    /// generation, so a file that was restored or swapped since is refused, and so is one
    /// another writer has taken.
    ///
    /// # Errors
    /// [`StateError::Open`]: as [`StateStoreUri::open`], or the store is not at
    /// `generation`.
    pub fn open_after_handover(&self, generation: u64) -> Result<StateStore, StateError> {
        self.open_at(Some(generation), EpochPolicy::Keep)
    }

    fn open_at(&self, expect: Option<u64>, epoch: EpochPolicy) -> Result<StateStore, StateError> {
        let Self::Sqlite(path) = self;
        // This process is the store's writer from here: it takes the next generation, and
        // every write through the handle is checked against it ([`Fenced`]).
        let fenced = Fenced::take(open_sqlite(path)?, expect).map_err(open_error)?;
        Ok(StateStore {
            store: Arc::new(fenced),
            epoch,
            // The defaults; a start sets the --member-* flags (`start::with_members`).
            members: MembershipConfig::default(),
            history: None,
        })
    }
}

impl crate::Args {
    /// The state store `--state-store` names, opened, or `None` when the flag is not
    /// given (the scheduler's state is then in memory).
    ///
    /// # Errors
    /// [`StateError`]: see [`StateStoreUri::open`].
    pub fn open_state_store(&self) -> Result<Option<StateStore>, StateError> {
        self.state_store
            .as_ref()
            .map(StateStoreUri::open)
            .transpose()
    }
}

fn open_error(why: impl fmt::Display) -> StateError {
    StateError::Open(why.to_string())
}

fn open_sqlite(path: &Path) -> Result<Arc<dyn Store>, StateError> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    use kbf_state::SystemClock;
    use kbf_state::sqlite::{Options, SqliteStore};
    // Only the root has no parent, and it exists: the open below then says what is wrong.
    let dir = path.parent().unwrap_or(path);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| open_error(format!("{}: {e}", dir.display())))?;
    // SQLite makes its file with the process's umask; the file is made here, 0600, and
    // the journal and WAL files SQLite makes beside it take its mode.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| open_error(format!("{}: {e}", path.display())))?;
    let store = SqliteStore::open(path, Arc::new(SystemClock::default()), Options::default())
        .map_err(open_error)?;
    Ok(Arc::new(store))
}

/// An open state store, and what a start does with its epoch.
pub struct StateStore {
    /// The store.
    pub store: Arc<dyn Store>,
    /// The epoch the start takes.
    pub epoch: EpochPolicy,
    /// How long storage members live and dwell in the store ([`crate::members`]).
    pub members: MembershipConfig,
    /// Build history to write, if `--history-objects` names where: it keeps its index
    /// rows in this store ([`crate::history`]).
    pub history: Option<crate::history::History>,
}

#[cfg(test)]
mod tests;
