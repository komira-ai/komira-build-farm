//! The updater's state file (S4.1): the newest key statement, the serial floor, the
//! installed set, the staged set and an apply in progress. It is written whole to a
//! temporary file, synced and renamed over the old one, so a crash leaves the old state
//! or the new, never a mix. An apply records itself as in progress before it installs
//! anything, so a crash mid-apply is reported and resumed. Once its set can no longer
//! pass the checks it may be replaced by a newer platform-signed set, which is staged
//! and installed in full: the installed record is not trusted to say what is on the
//! node while an apply is unfinished.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Refusal;
use crate::set::SoftwareSet;
use crate::signed::Envelope;

/// A set the state names, with its digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Held {
    /// Lowercase hex SHA-256 of the set's signed payload.
    pub digest: String,
    /// The key that signed it, hex: whether an apply in progress still holds off other
    /// sets depends on that key still being named.
    pub signer: String,
    /// The set.
    pub set: SoftwareSet,
}

/// Everything the updater remembers between requests and restarts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// The newest root-signed key statement seen.
    pub statement: Option<Envelope>,
    /// The highest `min_serial` of any staged set: nothing below it installs.
    pub floor: u64,
    /// The installed set.
    pub installed: Option<Held>,
    /// The staged set, whose changed artifacts are in the staging directory.
    pub staged: Option<Held>,
    /// A set whose apply started and has not finished. It is kept apart from `staged`,
    /// which a failed restage clears, so its hold on the node does not depend on it.
    pub in_progress: Option<Held>,
}

impl State {
    /// Reads the state file; a missing file is the empty state of a fresh node.
    ///
    /// # Errors
    /// The file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<State, Refusal> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| Refusal::State(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(Refusal::State(format!("{}: {e}", path.display()))),
        }
    }

    /// Replaces the state file: a temporary file beside it, synced, renamed over it, and
    /// the directory synced so the rename itself survives a crash.
    ///
    /// # Errors
    /// Any write, sync or rename fails.
    pub fn save(&self, path: &Path) -> Result<(), Refusal> {
        let err = |e: std::io::Error| Refusal::State(format!("{}: {e}", path.display()));
        let dir = path
            .parent()
            .ok_or_else(|| err(std::io::ErrorKind::InvalidInput.into()))?;
        let tmp = path.with_extension("tmp");
        let mut file = fs::File::create(&tmp).map_err(err)?;
        file.write_all(&serde_json::to_vec_pretty(self).expect("the state serializes"))
            .map_err(err)?;
        file.sync_all().map_err(err)?;
        fs::rename(&tmp, path).map_err(err)?;
        fs::File::open(dir).and_then(|d| d.sync_all()).map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// Catches: a missing state file treated as an error (a fresh node could never
    /// start), a save that loses a field, or a corrupt file read as empty (which would
    /// drop the floor and allow a replay).
    #[test]
    fn state_round_trips_and_a_corrupt_file_is_an_error() {
        let dir = testkit::scratch("state");
        let path = dir.join("state.json");
        assert_eq!(State::load(&path), Ok(State::default()));
        let state = State {
            statement: Some(testkit::seal_statement(&testkit::statement(1, &[]))),
            floor: 3,
            installed: Some(Held {
                digest: "a".into(),
                signer: "c".into(),
                set: testkit::set(4),
            }),
            staged: Some(Held {
                digest: "b".into(),
                signer: "d".into(),
                set: testkit::set(5),
            }),
            in_progress: Some(Held {
                digest: "e".into(),
                signer: "f".into(),
                set: testkit::set(6),
            }),
        };
        state.save(&path).unwrap();
        assert_eq!(State::load(&path), Ok(state));
        assert!(!path.with_extension("tmp").exists());
        fs::write(&path, b"{").unwrap();
        assert!(matches!(State::load(&path), Err(Refusal::State(_))));
        // A directory where the file should be cannot be read.
        assert!(matches!(State::load(&dir), Err(Refusal::State(_))));
    }

    /// Catches: a save that reports success when it could not write.
    #[test]
    fn a_failed_save_is_an_error() {
        let dir = testkit::scratch("state-fail");
        let missing = dir.join("no-such-dir").join("state.json");
        assert!(matches!(
            State::default().save(&missing),
            Err(Refusal::State(_))
        ));
        assert!(matches!(
            State::default().save(Path::new("/")),
            Err(Refusal::State(_))
        ));
    }
}
