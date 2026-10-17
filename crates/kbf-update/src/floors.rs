//! What the client remembers across restarts so that it never goes backwards: the newest
//! offer generation it has completed, and the newest order id it has accepted. One file,
//! `DIR/client.json`, replaced whole by a rename.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The two numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Floors {
    /// The newest offer generation whose directory is complete in the inbox; 0 before
    /// any.
    pub generation: u64,
    /// The newest order id accepted; 0 before any.
    pub order_id: u64,
}

/// Why the file cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum FloorsError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} is not the client's state file: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// The file, and how to replace it.
#[derive(Debug)]
pub struct FloorsFile {
    dir: PathBuf,
    path: PathBuf,
}

impl FloorsFile {
    /// The file under `dir` and what it holds (zeros when there is none yet).
    ///
    /// # Errors
    /// The file exists and cannot be read or understood. The client does not start with
    /// a state it cannot trust: a guessed zero would let an old offer or order in again.
    pub fn open(dir: &Path) -> Result<(Self, Floors), FloorsError> {
        let path = dir.join("client.json");
        let floors = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| FloorsError::Parse {
                path: path.clone(),
                source,
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Floors::default(),
            Err(source) => {
                return Err(FloorsError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };
        Ok((
            Self {
                dir: dir.to_owned(),
                path,
            },
            floors,
        ))
    }

    /// Replaces the file with `floors`: written beside it, synced, renamed over it.
    ///
    /// # Errors
    /// The temporary file cannot be written or renamed.
    pub fn save(&self, floors: &Floors) -> io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o640)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(floors).map_err(io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        File::open(&self.dir)?.sync_all()
    }
}
