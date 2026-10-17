//! The helper's word: `DIR/status.json`, which the root helper writes and the client
//! reads. A missing file means there is no helper; a file the client cannot understand
//! is a refusal with the reason, never a guess.

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::types::{Staged, is_hex};

/// The schema this client reads.
pub const SCHEMA: u64 = 1;

/// What the helper says it is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperState {
    Idle,
    Staged,
    ActivatedUnconfirmed,
    Confirmed,
    Reverted,
    Refused,
}

/// A well-formed status file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub installed: Option<String>,
    pub previous: Option<String>,
    pub staged: Option<String>,
    pub state: HelperState,
    pub refusal: String,
    pub acked_order_id: u64,
    pub staged_facts: Option<Staged>,
}

/// What reading the file found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Helper {
    /// No file: no helper runs here.
    Missing,
    /// A file the client cannot use, and why.
    Unreadable(String),
    /// The helper's report.
    Reported(Report),
}

#[derive(Deserialize)]
struct Raw {
    schema: u64,
    installed_commit: Option<String>,
    previous_commit: Option<String>,
    staged_commit: Option<String>,
    state: String,
    refusal: String,
    acked_order_id: u64,
    staged: Option<RawStaged>,
}

#[derive(Deserialize)]
struct RawStaged {
    protocol: u32,
    min_protocol: u32,
    state_schema: u32,
}

/// Reads the status file at `path`.
#[must_use]
pub fn read(path: &Path) -> Helper {
    match fs::read(path) {
        Ok(bytes) => parse(&bytes).map_or_else(Helper::Unreadable, Helper::Reported),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Helper::Missing,
        Err(e) => Helper::Unreadable(format!("cannot read {}: {e}", path.display())),
    }
}

fn parse(bytes: &[u8]) -> Result<Report, String> {
    let raw: Raw = serde_json::from_slice(bytes).map_err(|e| format!("status.json: {e}"))?;
    if raw.schema != SCHEMA {
        return Err(format!(
            "status.json has schema {}; this node reads schema {SCHEMA}",
            raw.schema
        ));
    }
    let state = match raw.state.as_str() {
        "idle" => HelperState::Idle,
        "staged" => HelperState::Staged,
        "activated_unconfirmed" => HelperState::ActivatedUnconfirmed,
        "confirmed" => HelperState::Confirmed,
        "reverted" => HelperState::Reverted,
        "refused" => HelperState::Refused,
        other => return Err(format!("status.json: unknown state {other:?}")),
    };
    for (field, commit) in [
        ("installed_commit", &raw.installed_commit),
        ("previous_commit", &raw.previous_commit),
        ("staged_commit", &raw.staged_commit),
    ] {
        if commit.as_deref().is_some_and(|c| !is_hex(c, 40)) {
            return Err(format!(
                "status.json: {field} is not 40 lowercase hex digits"
            ));
        }
    }
    Ok(Report {
        installed: raw.installed_commit,
        previous: raw.previous_commit,
        staged: raw.staged_commit,
        state,
        refusal: raw.refusal,
        acked_order_id: raw.acked_order_id,
        staged_facts: raw.staged.map(|s| Staged {
            protocol: s.protocol,
            min_protocol: s.min_protocol,
            state_schema: s.state_schema,
        }),
    })
}

impl Report {
    /// Whether `commit` is one this host holds: installed, staged, or the previous one.
    #[must_use]
    pub fn holds(&self, commit: &str) -> bool {
        [&self.installed, &self.previous, &self.staged]
            .into_iter()
            .flatten()
            .any(|held| held == commit)
    }
}
