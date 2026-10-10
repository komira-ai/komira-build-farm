//! The host loop: one [`Raft`] core driven over a [`Storage`], a [`Transport`] and a
//! [`Machine`], carrying out the core's effects in the order its contract requires.
//!
//! The core decides; the host does what it decides. For each input (a tick, a message,
//! a proposal) the host hands the core's effects to its parts in order, with one rule
//! added: before an effect that leaves the process or acts on committed state (a
//! [`Effect::Send`] or an [`Effect::Apply`]), every persist before it is made durable
//! with [`Storage::sync`]. Consecutive persists share one sync, and an input whose
//! last effects are persists ends with one, so everything an input persisted is
//! durable when the call returns.
//!
//! A storage error is fail-stop: the call that met it returns
//! [`HostError::Storage`], nothing after the failed operation is sent or applied, and
//! every later call returns [`HostError::Stopped`] without touching the core. A node
//! that could not make an entry durable must never acknowledge it, and its core's
//! memory may already be ahead of its disk.

use crate::{
    CompactError, Config, ConfigError, Effect, Entry, HardState, LogId, LogIndex, Message,
    NotLeader, Raft, ServerId,
};

use thiserror::Error;

/// A snapshot of the state machine: the id of the last entry it folds in, and the
/// machine's state as of that entry, as [`Machine::snapshot`] encoded it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The last entry the snapshot holds.
    pub base: LogId,
    /// The state machine's bytes.
    pub state: Vec<u8>,
}

/// What a [`Storage`] holds for one server: the vote, the newest snapshot, and the
/// entries after that snapshot's base (after index 0 with no snapshot), in index
/// order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stored {
    /// The last hard state written.
    pub hard: HardState,
    /// The newest snapshot, if one was written.
    pub snapshot: Option<Snapshot>,
    /// The entries after the snapshot's base.
    pub entries: Vec<Entry>,
}

impl Stored {
    /// The snapshot base: index 0, term 0 with no snapshot.
    #[must_use]
    pub fn base(&self) -> LogId {
        self.snapshot
            .as_ref()
            .map_or_else(LogId::default, |s| s.base)
    }
}

/// A server's durable state. Writes may be buffered; [`Storage::sync`] makes every
/// write before it durable. The host never reads back what it wrote except through
/// [`Storage::load`] when it opens.
pub trait Storage {
    /// Why an operation failed. The host stops at the first one.
    type Error: std::error::Error;

    /// Everything stored, as the host's earlier writes left it.
    ///
    /// # Errors
    ///
    /// The storage could not be read.
    fn load(&mut self) -> Result<Stored, Self::Error>;

    /// Replaces the hard state.
    ///
    /// # Errors
    ///
    /// The write failed.
    fn write_hard_state(&mut self, hard: HardState) -> Result<(), Self::Error>;

    /// Drops every stored entry at or after the index of the first of `entries`, then
    /// appends them. `entries` is never empty and follows the snapshot base.
    ///
    /// # Errors
    ///
    /// The write failed.
    fn write_entries(&mut self, entries: &[Entry]) -> Result<(), Self::Error>;

    /// Makes every earlier write durable.
    ///
    /// # Errors
    ///
    /// The writes could not be made durable. Some of them may have been.
    fn sync(&mut self) -> Result<(), Self::Error>;

    /// Replaces the snapshot with `snapshot` and drops every entry through its base,
    /// durably, before it returns. The host calls it only when every earlier write is
    /// synced.
    ///
    /// # Errors
    ///
    /// The write failed; the old snapshot and entries may still be the ones stored.
    fn write_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), Self::Error>;
}

/// The network as Raft sees it: messages may be lost, delayed, duplicated or
/// reordered, so sending never fails.
pub trait Transport {
    /// Sends `msg` to `to`.
    fn send(&mut self, to: ServerId, msg: Message);
}

/// The replicated state machine.
pub trait Machine {
    /// Why a snapshot could not be restored.
    type Error: std::error::Error;

    /// Applies a committed entry. Entries arrive in index order, each once per host,
    /// starting after the snapshot the machine was restored from.
    fn apply(&mut self, entry: &Entry);

    /// The machine's state as of the last entry applied.
    fn snapshot(&self) -> Vec<u8>;

    /// Replaces the machine's state with `snapshot`'s.
    ///
    /// # Errors
    ///
    /// The bytes do not decode.
    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), Self::Error>;
}

/// Why [`Host::open`] failed.
#[derive(Debug, Error)]
pub enum OpenError<S, M> {
    /// The storage could not be read.
    #[error("loading the stored state: {0}")]
    Storage(S),
    /// The state machine refused the stored snapshot.
    #[error("restoring the state machine: {0}")]
    Machine(M),
    /// The core refused the configuration or the stored log.
    #[error("restoring the core: {0}")]
    Core(ConfigError),
}

/// Why a host call failed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HostError<E> {
    /// A storage operation failed; the host has stopped.
    #[error("storage failed, the host has stopped: {0}")]
    Storage(E),
    /// An earlier storage error stopped the host.
    #[error("the host stopped after a storage error")]
    Stopped,
    /// A proposal reached a server that does not lead.
    #[error(transparent)]
    NotLeader(NotLeader),
    /// The core refused a compaction.
    #[error(transparent)]
    Compact(CompactError),
}

/// One server: a core, its storage, its transport and its state machine.
#[derive(Debug)]
pub struct Host<S, T, M> {
    core: Raft,
    storage: S,
    transport: T,
    machine: M,
    stopped: bool,
}

impl<S: Storage, T: Transport, M: Machine> Host<S, T, M> {
    /// Opens a server from what `storage` holds: `machine` (in its initial state) is
    /// restored from the stored snapshot, if there is one, and the core from the hard
    /// state, the snapshot's base and the entries after it (see [`Raft::restore`]).
    ///
    /// # Errors
    ///
    /// [`OpenError`] naming the part that refused.
    pub fn open(
        config: Config,
        mut storage: S,
        transport: T,
        mut machine: M,
        entropy: u64,
    ) -> Result<Self, OpenError<S::Error, M::Error>> {
        let stored = storage.load().map_err(OpenError::Storage)?;
        let base = stored.base();
        if let Some(snapshot) = &stored.snapshot {
            machine.restore(snapshot).map_err(OpenError::Machine)?;
        }
        let core = Raft::restore(config, stored.hard, base, stored.entries, entropy)
            .map_err(OpenError::Core)?;
        Ok(Self {
            core,
            storage,
            transport,
            machine,
            stopped: false,
        })
    }

    /// The core, for its role, term, log and indexes.
    #[must_use]
    pub fn core(&self) -> &Raft {
        &self.core
    }

    /// The storage.
    #[must_use]
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// The transport.
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// The state machine.
    #[must_use]
    pub fn machine(&self) -> &M {
        &self.machine
    }

    /// Whether a storage error has stopped the host.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// One tick of logical time (see [`Raft::tick`]).
    ///
    /// # Errors
    ///
    /// [`HostError::Storage`] or [`HostError::Stopped`].
    pub fn tick(&mut self, entropy: u64) -> Result<(), HostError<S::Error>> {
        self.running()?;
        let effects = self.core.tick(entropy);
        self.carry_out(effects)
    }

    /// A message from `from` (see [`Raft::receive`]).
    ///
    /// # Errors
    ///
    /// [`HostError::Storage`] or [`HostError::Stopped`].
    pub fn receive(
        &mut self,
        from: ServerId,
        msg: Message,
        entropy: u64,
    ) -> Result<(), HostError<S::Error>> {
        self.running()?;
        let effects = self.core.receive(from, msg, entropy);
        self.carry_out(effects)
    }

    /// Appends `command` to the log if this server leads, and returns its index. It
    /// is committed when the state machine applies an entry at that index.
    ///
    /// # Errors
    ///
    /// [`HostError::NotLeader`], [`HostError::Storage`] or [`HostError::Stopped`].
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex, HostError<S::Error>> {
        self.running()?;
        let proposed = self.core.propose(command).map_err(HostError::NotLeader)?;
        self.carry_out(proposed.effects)?;
        Ok(proposed.index)
    }

    /// Snapshots the state machine as of the last applied entry, writes the snapshot
    /// and compacts the log through that entry. Returns the new snapshot base; with
    /// nothing applied since the last snapshot, returns the current one and writes
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`HostError::Storage`] or [`HostError::Stopped`].
    pub fn snapshot(&mut self) -> Result<LogId, HostError<S::Error>> {
        self.running()?;
        let through = self.core.applied_index();
        let current = self.core.snapshot_base();
        if through == current.index {
            return Ok(current);
        }
        let state = self.machine.snapshot();
        // Every input ends synced, so no write is pending here.
        let base = self.core.compact(through).map_err(HostError::Compact)?;
        let snapshot = Snapshot { base, state };
        self.stop_on_error(|s| s.write_snapshot(&snapshot))?;
        Ok(base)
    }

    fn running(&self) -> Result<(), HostError<S::Error>> {
        if self.stopped {
            Err(HostError::Stopped)
        } else {
            Ok(())
        }
    }

    fn stop_on_error(
        &mut self,
        op: impl FnOnce(&mut S) -> Result<(), S::Error>,
    ) -> Result<(), HostError<S::Error>> {
        op(&mut self.storage).map_err(|e| {
            self.stopped = true;
            HostError::Storage(e)
        })
    }

    /// Carries out `effects` in order. A send or an apply waits for the persists
    /// before it to be synced; the input ends synced.
    fn carry_out(&mut self, effects: Vec<Effect>) -> Result<(), HostError<S::Error>> {
        let mut unsynced = false;
        for effect in effects {
            match effect {
                Effect::PersistHardState(hard) => {
                    self.stop_on_error(|s| s.write_hard_state(hard))?;
                    unsynced = true;
                }
                Effect::PersistEntries(entries) => {
                    self.stop_on_error(|s| s.write_entries(&entries))?;
                    unsynced = true;
                }
                Effect::Send { to, msg } => {
                    if std::mem::take(&mut unsynced) {
                        self.stop_on_error(S::sync)?;
                    }
                    self.transport.send(to, msg);
                }
                Effect::Apply(entry) => {
                    if std::mem::take(&mut unsynced) {
                        self.stop_on_error(S::sync)?;
                    }
                    self.machine.apply(&entry);
                }
            }
        }
        if unsynced {
            self.stop_on_error(S::sync)?;
        }
        Ok(())
    }
}
