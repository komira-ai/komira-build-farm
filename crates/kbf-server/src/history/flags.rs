//! `--history-objects` and `--history-index-root`: where build history is kept.

use std::sync::Arc;

use kbf_history::{HistoryRoot, IndexRoot};
use kbf_objstore::{Floor, FsStore, FsStoreError, KeyError, KeyPrefix};

use super::driver::{History, Options, over};
use crate::StateStore;
use crate::config::{ConfigError, s3_store};
use crate::stores::{FILE_PREFIX_ROOT, StoreUri};

/// Display order of the history flags: after every other flag of the server and of the
/// daemon that shares its help (`kbf-all-in-one`), before `--help` (999).
const AFTER_ALL: usize = 900;

// The flags that turn build history on. With neither, nothing is recorded and the server
// behaves as it did before they existed. Not a doc comment: clap would take a doc comment
// on a flattened struct for the program's description.
//
// `--history-objects` needs a state store, which `HistoryFlags::open` checks when the
// server starts rather than clap when it parses: `kbf-all-in-one` always has one, though
// its command line may not name it.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct HistoryFlags {
    /// Where build history is kept: an object store, `s3://bucket/prefix?endpoint=http://host:port` (the key pair is the `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` of the environment) or `file:///absolute/path` (a directory this process owns, not the cold store's). The attempts and cache hits of each invocation a client named are recorded under `<prefix>history/` after their results are sent, never before and never delaying one; the index rows go to `--state-store`, which this flag needs. A full queue, a slow object store or a slow state store drops records and counts them (`kbf_history_dropped_total` on `/metrics`), and never fails or delays a call. The writer reads and writes only keys under `history/`. Left out, nothing is recorded
    #[arg(long, value_name = "URI", display_order = AFTER_ALL)]
    pub history_objects: Option<StoreUri>,
    /// Where history's index rows live in the state store: `ui/`, or `<prefix>ui/` for a prefix that ends in `/`, so that one state store can hold several farms' history apart. Left out, it is `ui/`. Needs `--history-objects`
    #[arg(
        long,
        value_name = "ROOT",
        requires = "history_objects",
        value_parser = parse_index_root,
        display_order = AFTER_ALL
    )]
    pub history_index_root: Option<IndexRoot>,
}

/// Why history could not be started.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// `--history-objects` without a state store: the index rows have nowhere to go.
    #[error("--history-objects needs --state-store: history's index rows are kept there")]
    NeedsStateStore,
    /// The `file://` directory could not be opened.
    #[error("--history-objects: {0}")]
    Open(#[from] FsStoreError),
    /// The `s3://` store could not be configured.
    #[error("--history-objects: {0}")]
    Store(#[from] ConfigError),
    /// The store's key prefix is not one history can live under.
    #[error("--history-objects: {0}")]
    Prefix(#[from] KeyError),
}

/// `--history-index-root`'s value: `ui/` under an optional prefix.
fn parse_index_root(text: &str) -> Result<IndexRoot, String> {
    let prefix = text
        .strip_suffix("ui/")
        .ok_or_else(|| format!("{text:?} must end in `ui/`"))?;
    IndexRoot::under(prefix).map_err(|e| e.to_string())
}

impl HistoryFlags {
    /// The history writer these flags name, over `state`'s store; `None` when
    /// `--history-objects` is not given.
    ///
    /// # Errors
    /// [`HistoryError`]: no state store, or a store that cannot be opened.
    pub fn open(&self, state: Option<&StateStore>) -> Result<Option<History>, HistoryError> {
        self.open_with(state, &|name| std::env::var(name).ok())
    }

    /// [`HistoryFlags::open`], reading an `s3://` store's key pair through `env` (the
    /// process environment, for the server).
    ///
    /// # Errors
    /// As [`HistoryFlags::open`].
    pub fn open_with(
        &self,
        state: Option<&StateStore>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<History>, HistoryError> {
        let Some(uri) = &self.history_objects else {
            return Ok(None);
        };
        let state = state.ok_or(HistoryError::NeedsStateStore)?;
        let index_root = self.history_index_root.clone().unwrap_or_default();
        let store = Arc::clone(&state.store);
        let (place, rows) = (describe(uri), index_root.as_str());
        tracing::info!("history: objects in {place}, index {rows}");
        let options = Options::default();
        match uri {
            StoreUri::File(dir) => {
                let objects = FsStore::open(dir, Floor::KeepFree)?;
                let root = HistoryRoot::under(&KeyPrefix::new(FILE_PREFIX_ROOT)?)?;
                Ok(Some(over(objects, root, store, index_root, options)))
            }
            StoreUri::S3(s3) => {
                let objects = s3_store(s3, env)?;
                let root = HistoryRoot::under(&KeyPrefix::new(s3.prefix.clone())?)?;
                Ok(Some(over(objects, root, store, index_root, options)))
            }
        }
    }
}

/// `uri` as the start log names it: the scheme and place, no query.
fn describe(uri: &StoreUri) -> String {
    match uri {
        StoreUri::File(dir) => format!("file://{}", dir.display()),
        StoreUri::S3(s3) => format!("s3://{}/{}", s3.bucket, s3.prefix),
    }
}

/// `state` with the history the flags name, started when the server's task starts
/// ([`crate::history`]).
///
/// # Errors
/// [`HistoryError`]: see [`HistoryFlags::open`].
pub fn with_history(
    state: Option<StateStore>,
    flags: &HistoryFlags,
) -> Result<Option<StateStore>, HistoryError> {
    let history = flags.open(state.as_ref())?;
    Ok(state.map(|state| StateStore { history, ..state }))
}
