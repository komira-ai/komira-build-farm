//! What the gate keeps across restarts, and the file it keeps it in.
//!
//! A scheduled erase must run even if the gate restarts before its time, a spent nonce
//! must stay spent, and the caps must count what happened before a restart, so every
//! verb that changes any of this writes the whole state before it answers (and before
//! it sends an erase). The file is replaced atomically: written beside, synced, renamed.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A `privileged-lease` request the gate holds for `grant-admin` (M4.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub serial: String,
    pub lease: String,
    pub signer: String,
    pub nonce: String,
    pub accepted_at: i64,
    /// The request's `not-after`: it bounded acceptance only, and is kept for the record.
    pub not_after: i64,
}

/// An erase the gate scheduled with a grant; it runs at `due`, whatever happens to the
/// lease, unless brought forward.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scheduled {
    pub serial: String,
    pub lease: String,
    pub signer: String,
    pub granted_at: i64,
    pub due: i64,
    /// The signed request's `not-after`, for the record; the erase runs whatever it is.
    pub not_after: i64,
}

/// The erase the gate sent last, outstanding until the Mac reports again after it or
/// 24 hours pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outstanding {
    pub serial: String,
    pub started_at: i64,
}

/// An enforcement the gate posted and has not withdrawn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enforced {
    pub pool: String,
    pub version: String,
    pub build: String,
    pub deadline: String,
    pub set_serial: u64,
    pub started_at: i64,
}

/// The newest set the gate verified for a pool: its serial and the profile digests it
/// names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSet {
    pub serial: u64,
    pub profiles: Vec<String>,
}

/// Everything that survives a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Spent nonces and the `not-after` of the request that carried each.
    pub nonces: BTreeMap<String, i64>,
    pub held: Vec<Held>,
    pub scheduled: Vec<Scheduled>,
    pub outstanding_erase: Option<Outstanding>,
    /// Serials erased and not seen since, with when the erase was sent: they do not
    /// count toward the Mac floor.
    pub erased: BTreeMap<String, i64>,
    /// When each erase of the last 24 hours was sent or reserved (the daily cap).
    pub erase_times: Vec<i64>,
    /// Outstanding enforcements by serial.
    pub enforced: BTreeMap<String, Enforced>,
    /// Per pool: the highest `min_serial` of any set the gate verified.
    pub pool_floors: BTreeMap<String, u64>,
    /// Per pool: the newest set the gate verified.
    pub pool_sets: BTreeMap<String, PoolSet>,
    /// The serial of the newest key statement the gate accepted.
    pub statement_serial: u64,
}

/// Why the state file could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("state file {path}: {reason}")]
pub struct StateError {
    pub path: PathBuf,
    pub reason: String,
}

/// The state file.
#[derive(Clone, Debug)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
        }
    }

    fn error(&self, reason: impl std::fmt::Display) -> StateError {
        StateError {
            path: self.path.clone(),
            reason: reason.to_string(),
        }
    }

    /// Reads the state; a missing file is the empty state (a first start).
    ///
    /// # Errors
    /// The file exists but cannot be read or parsed.
    pub fn load(&self) -> Result<State, StateError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| self.error(e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(self.error(e)),
        }
    }

    /// Replaces the file with `state`, durably.
    ///
    /// # Errors
    /// The file cannot be written, synced or renamed.
    pub fn save(&self, state: &State) -> Result<(), StateError> {
        let tmp = self.path.with_extension("tmp");
        // Plain data with string map keys: serialising it cannot fail.
        let bytes = serde_json::to_vec_pretty(state).expect("the state serialises");
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            let dir = self.path.parent().unwrap_or(Path::new("."));
            std::fs::File::open(dir)?.sync_all()
        };
        write().map_err(|e| self.error(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_a_missing_file_is_empty() {
        let dir = crate::testkit::scratch("state");
        let file = StateFile::new(&dir.join("state.json"));
        assert_eq!(file.load().unwrap(), State::default());
        let mut state = State::default();
        state.nonces.insert("ab".into(), 5);
        state.outstanding_erase = Some(Outstanding {
            serial: "S".into(),
            started_at: 1,
        });
        file.save(&state).unwrap();
        assert_eq!(file.load().unwrap(), state);
    }

    #[test]
    fn unreadable_and_unwritable_files_are_errors() {
        let dir = crate::testkit::scratch("state-bad");
        std::fs::write(dir.join("bad.json"), b"{").unwrap();
        let e = StateFile::new(&dir.join("bad.json")).load().unwrap_err();
        assert!(e.to_string().starts_with("state file "), "{e}");
        // A directory where the file should be.
        assert!(StateFile::new(&dir).load().is_err());
        let nowhere = StateFile::new(&dir.join("no/such/dir/state.json"));
        assert!(nowhere.save(&State::default()).is_err());
    }
}
