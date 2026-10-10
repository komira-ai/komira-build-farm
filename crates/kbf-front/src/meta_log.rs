//! The seam between the front and the metadata state machine.
//!
//! The front never holds [`MetaState`] itself: it commits [`Command`]s and asks
//! queries through a [`MetaLog`]. Today that is [`MemoryMetaLog`], one state behind a
//! lock in this process (`--store=memory`). The replicated log implements the same
//! trait: `commit` proposes the command and resolves once it has applied, and `query`
//! runs on the leader. The front's code does not change when it slots in.

use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};

use kbf_meta::{Applied, Command, Epoch, MetaState, Retention};

/// Why the metadata log could not answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MetaLogError {
    /// No leader could be reached within the hold. Becomes UNAVAILABLE: the front
    /// never turns "I could not ask" into "absent".
    #[error("the metadata leader is unavailable: {0}")]
    Unavailable(String),
}

/// The metadata state machine as the front reaches it.
pub trait MetaLog: Send + Sync + 'static {
    /// Commits `command` and returns what applying it did. Resolves only after the
    /// command has applied, so a caller may act on the outcome.
    fn commit(
        &self,
        command: Command,
    ) -> impl Future<Output = Result<Applied, MetaLogError>> + Send;

    /// Runs a read-only query against the committed state, on the leader.
    fn query<R, F>(&self, f: F) -> impl Future<Output = Result<R, MetaLogError>> + Send
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send;
}

/// The single-process metadata log: one [`MetaState`] behind a lock, every command
/// applied in the order it arrives, at the next index of a log that exists only in
/// memory. For `--store=memory` and tests; it never fails.
#[derive(Debug)]
pub struct MemoryMetaLog {
    state: Mutex<Applier>,
}

/// The state and the index of the last command applied to it.
#[derive(Debug)]
struct Applier {
    meta: MetaState,
    last_index: u64,
}

impl Applier {
    /// Applies `command` as the next entry; the first is at index 1.
    fn execute(&mut self, command: Command) -> Applied {
        self.last_index += 1;
        self.meta.execute(self.last_index, command)
    }
}

impl MemoryMetaLog {
    /// An empty state with `retention`, at farm time zero.
    #[must_use]
    pub fn new(retention: Retention) -> Self {
        Self {
            state: Mutex::new(Applier {
                meta: MetaState::new(retention),
                last_index: 0,
            }),
        }
    }

    /// Allocates a writer epoch now, for a cache built without an async context
    /// ([`Cache::memory`](crate::Cache::memory)). The same as committing
    /// [`Command::AllocEpoch`].
    pub fn alloc_epoch(&self) -> Epoch {
        match self.state().execute(Command::AllocEpoch) {
            Applied::Epoch(epoch) => epoch,
            other => unreachable!("AllocEpoch applied as {other:?}"),
        }
    }

    fn state(&self) -> MutexGuard<'_, Applier> {
        // `MetaState::execute` and the queries do not panic midway through a change, so
        // a poisoned state is still whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl MetaLog for MemoryMetaLog {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        Ok(self.state().execute(command))
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        Ok(f(&self.state().meta))
    }
}
