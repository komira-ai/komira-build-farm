//! The metadata log (`--meta-dir`) as a single-voter Raft log on the local disk
//! (docs/design/ha.md, section 16, PR 5).
//!
//! [`RaftMetaLog`] implements the front's [`MetaLog`] over a [`kbf_raft::Host`] whose
//! storage is a [`kbf_store::Store`] in the raft directory, whose transport sends
//! nothing (a single voter has no peer), and whose state machine applies each
//! committed `kbf-log` entry to a [`MetaState`]:
//!
//! - **Opening.** An empty directory gets a new log: its id is drawn, and recorded
//!   with the voter set ([`RECORD`]) before the store writes anything. Every later
//!   start reads the id from the record. The server then elects itself, which commits
//!   its term's blank entry and so applies every entry already in the log, once each.
//!   If no `Bootstrap` has been applied, it commits one naming the log id and the
//!   object key prefix `<prefix root><log id>/`. A start whose record or prefix root
//!   disagrees with the committed `Bootstrap` is refused.
//! - **Commit.** A command is encoded, proposed, made durable (the store syncs before
//!   every persist returns), applied, and only then answered. Commits are grouped: a
//!   command joins the log's queue, and one committer at a time, on tokio's blocking
//!   pool, takes everything queued, proposes it as one batch (one write, one sync),
//!   applies it and answers each caller, then takes what queued meanwhile, until the
//!   queue is empty. A lone command is committed as soon as it arrives; commands that
//!   arrive during a sync share the next one. A storage error answers every command
//!   of its batch `Unavailable`, none with an outcome. A commit polled outside any
//!   tokio runtime that finds no committer running is the committer, on the caller's
//!   thread, so its future is ready in its first poll (kbf-sim-cell relies on that).
//! - **Queries** read this server's applied state. A single voter is always its own
//!   leader, so that is the committed state. The state has its own lock, held by a
//!   commit only while it applies, never across the sync: a query does not wait for a
//!   commit's disk write.
//! - **Leadership** ([`MetaLog::leadership`]): the log id and the term the open
//!   elected this server in. With no peer, nothing can take the leadership away, so
//!   the term is the same for the life of the process, and every start's is greater
//!   than every earlier start's. The farm grants its leases under this term and names
//!   the log id as its lease epoch in `Welcome` ([`crate::farm`]).
//! - **Farm time** continues from the committed one: at open the log takes the
//!   applied farm time as its base, and [`RaftMetaLog::now`] is the base plus the time
//!   since the open, so it never goes back across restarts. A task commits
//!   `Tick(now)` every [`TICK`] while the log is open, the first one [`TICK`] after
//!   the open. A tick is a proposal, which
//!   only a leader may make.
//! - **Machine version.** A new log is at [`MachineVersion::V1`], whose entries are
//!   format 1. [`RaftMetaLog::raise_machine_version`] commits a `MachineVersion`
//!   entry; every entry after it is in the new version's format, so a log raised to
//!   [`MachineVersion::V2`] takes the write tier's commands, and one raised to
//!   [`MachineVersion::V3`] takes `EvictObjects` too, and one raised to
//!   [`MachineVersion::V4`] node writers (`SetWriter`, and objects named by a storage
//!   node's member and boot). A start replays the raise
//!   with the log. A binary that does not run the version (any older one) refuses the
//!   log at that entry. A raise commits alone, under the host's lock; a batch is
//!   encoded under that lock too, in the format committed when it commits, so no
//!   batch holds a `MachineVersion` entry and a command the format does not take is
//!   refused alone while the rest of its batch commits.
//! - **A storage error** stops the log for the life of the process: every later
//!   commit is refused `Unavailable`.
//! - **Dropping the log** stops its tick task, waits for a batch the committer is
//!   writing, and refuses every later one `Unavailable`, so nothing reaches the raft
//!   directory once the drop returns and the directory can be opened again in the
//!   same process (kbf#651).
//!
//! Not yet: snapshots (the metadata state has no snapshot encoding, so the log is
//! never compacted, and a directory holding a snapshot is refused), more than one
//! voter, and the scheduler's state, which stays in memory.

mod machine;
mod recorded;

use std::fmt::{self, Display};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::num::NonZeroU64;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::time::Duration;

use futures::future::OptionFuture;
use kbf_front::{Leadership, MetaLog, MetaLogError};
use kbf_log::{Bootstrap, DecodeError, EncodeError, FarmLogId, LogCommand, MachineVersion};
use kbf_meta::{Applied, Command, MetaState, Retention};
use kbf_objstore::{KeyError, KeyPrefix};
use kbf_raft::{
    Config, ConfigError, Host, HostError, LogIndex, Membership, Message, OpenError, Role, ServerId,
    Transport,
};
use kbf_store::{Fs, Options, StdFs, Store, StoreError};
use kbf_types::FarmTime;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use self::machine::MetaMachine;
pub use self::machine::NoSnapshots;
pub use self::recorded::{RECORD, RECORD_TMP};

/// The file in the raft directory that [`RaftMetaLog::open_dir`] holds an exclusive
/// lock on (`flock`) while the log is open, so a second server started on the same
/// directory is refused.
pub const LOCK: &str = "log.lock";

/// The highest machine version this binary runs: [`MachineVersion::V4`], the write
/// tier's commands, `EvictObjects` and node writers (`SetWriter`). A log stays at the version it was raised to; a
/// new one starts at [`MachineVersion::V1`].
pub const RUNS: MachineVersion = MachineVersion::V4;

/// How often the log commits farm time.
pub const TICK: Duration = Duration::from_secs(1);

/// The only server of a single-voter log.
const ME: ServerId = ServerId(1);

/// The transport of a single voter: it has no peer, so the core never sends.
#[derive(Debug)]
struct NoTransport;

impl Transport for NoTransport {
    fn send(&mut self, _: ServerId, _: Message) {}
}

type RaftHost<F> = Host<Store<F>, NoTransport, MetaMachine>;

/// Why the log could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum RaftLogError {
    /// A filesystem call on the raft directory failed.
    #[error("raft dir: {0}")]
    Io(#[from] io::Error),
    /// The log record is not one this binary writes.
    #[error("raft dir: {RECORD} is not a log record this kbf-server reads")]
    Record,
    /// The directory holds files but no log record: not a log this server created.
    #[error("raft dir: holds files but no {RECORD}")]
    NotEmpty,
    /// The object key prefix is not a valid prefix.
    #[error("the object key prefix: {0}")]
    Prefix(#[from] KeyError),
    /// The store refused to open.
    #[error("raft dir: {0}")]
    Store(#[from] StoreError),
    /// The host refused what the store holds.
    #[error("raft dir: {0}")]
    Open(#[from] OpenError<StoreError, NoSnapshots>),
    /// The single voter's membership was refused.
    #[error("raft membership: {0}")]
    Membership(#[from] ConfigError),
    /// Another open log holds the raft directory's [`LOCK`] (a second server started
    /// on the same `--meta-dir`), or the lock could not be taken.
    #[error(
        "raft dir {}: could not lock {LOCK}; is another kbf-server using it? ({source})",
        .dir.display()
    )]
    Locked {
        /// The raft directory.
        dir: PathBuf,
        /// Why the lock was not taken.
        source: io::Error,
    },
    /// The host stopped while the log opened.
    #[error("raft log: {0}")]
    Host(#[from] HostError<StoreError>),
    /// An entry could not be encoded.
    #[error("raft log: {0}")]
    Encode(#[from] EncodeError),
    /// A committed entry does not decode; the log applies nothing past it.
    #[error("raft log: entry {index}: {source}")]
    Decode {
        /// The entry's index.
        index: u64,
        /// Why it does not decode.
        source: DecodeError,
    },
    /// The machine version this binary was asked to run is not one it knows.
    #[error("raft log: this kbf-server does not run machine version {}", .0.get())]
    Runs(MachineVersion),
    /// The committed `Bootstrap` is not the one this start expects: the record names
    /// another log, or the prefix root changed since the log was created.
    #[error(
        "raft log: committed bootstrap {committed:?}, but this start expects {expected:?}; \
         the log record or the object key prefix changed"
    )]
    Mismatch {
        /// The `Bootstrap` the log applied.
        committed: Option<Bootstrap>,
        /// The one this start would have committed.
        expected: Bootstrap,
    },
}

/// The metadata log as a single-voter Raft log in one directory (see the module
/// docs). Dropping it stops its tick task and waits for a commit being written (one
/// sync at most); a commit queued after that is refused.
pub struct RaftMetaLog<F: Fs = StdFs> {
    shared: Arc<Shared<F>>,
    ticker: AbortHandle,
    /// The raft directory's [`LOCK`], held while the log is open (logs opened with
    /// [`RaftMetaLog::open_dir`]); released when the log is dropped.
    lock: Option<File>,
}

struct Shared<F: Fs> {
    host: Mutex<RaftHost<F>>,
    /// The commands waiting for the committer.
    queue: Mutex<Queue>,
    /// The machine's applied state ([`MetaMachine::state`]), read without `host`.
    state: Arc<RwLock<MetaState>>,
    log_id: FarmLogId,
    /// The term the open elected this server in.
    term: u64,
    prefix: KeyPrefix,
    /// The farm time applied when the log opened.
    base: FarmTime,
    opened: Instant,
    /// The `Bootstrap` entries applied by the time the log opened.
    bootstraps: usize,
    /// Set when the log is dropped: a batch that takes `host` after it is refused.
    closed: AtomicBool,
}

impl RaftMetaLog<StdFs> {
    /// Opens the log in `dir` (created if missing), creating a new one with a random
    /// id if the directory is empty. Objects go under `<prefix_root><log id>/`. The
    /// directory's [`LOCK`] is taken before anything in it is read, and held until the
    /// log is dropped.
    ///
    /// # Errors
    /// [`RaftLogError::Locked`] if another open log holds the directory; otherwise as
    /// [`RaftMetaLog::open`].
    pub async fn open_dir(
        dir: &Path,
        retention: Retention,
        prefix_root: &str,
    ) -> Result<Self, RaftLogError> {
        let fs = StdFs::new(dir)?;
        let lock = lock_dir(dir)?;
        let mut log = Self::open(fs, retention, prefix_root, random_log_id).await?;
        log.lock = Some(lock);
        Ok(log)
    }
}

/// `dir`'s [`LOCK`], created if missing, with an exclusive `flock` on it. The lock
/// goes with the file's descriptor, so it is released when the file is closed, by
/// the log's drop or by the process's exit.
fn lock_dir(dir: &Path) -> Result<File, RaftLogError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(e) => Err(RaftLogError::Locked {
            dir: dir.to_owned(),
            source: io::Error::from(e),
        }),
    }
}

/// Eight bytes from the system's random source; a zero draw becomes 1.
fn random_log_id() -> io::Result<FarmLogId> {
    let mut bytes = [0; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let n = NonZeroU64::new(u64::from_le_bytes(bytes)).unwrap_or(NonZeroU64::MIN);
    Ok(FarmLogId::new(n))
}

impl<F> RaftMetaLog<F>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    /// Opens the log in `fs`, as the module docs describe. `new_log_id` is called
    /// only when `fs` is empty, to draw the id of the log created there. Reading the
    /// log runs on the calling thread; it is called once, at start.
    ///
    /// # Errors
    /// [`RaftLogError`]: the directory, its record or its store is refused, a
    /// committed entry does not decode, or the committed `Bootstrap` is not this
    /// start's.
    pub async fn open(
        fs: F,
        retention: Retention,
        prefix_root: &str,
        new_log_id: impl FnOnce() -> io::Result<FarmLogId>,
    ) -> Result<Self, RaftLogError> {
        Self::open_running(fs, retention, prefix_root, new_log_id, RUNS).await
    }

    /// [`RaftMetaLog::open`], for a binary that runs machine versions up to `runs`: a
    /// log raised past it is refused at the raise (`RaftLogError::Decode`), as a
    /// rollback to an older binary is. The server runs [`RUNS`]; a lower `runs` stands
    /// for an older binary in tests of that rollback.
    ///
    /// # Errors
    /// As [`RaftMetaLog::open`], and [`RaftLogError::Runs`] for a version this binary
    /// does not know.
    pub async fn open_running(
        fs: F,
        retention: Retention,
        prefix_root: &str,
        new_log_id: impl FnOnce() -> io::Result<FarmLogId>,
        runs: MachineVersion,
    ) -> Result<Self, RaftLogError> {
        let machine = MetaMachine::new(retention, runs).ok_or(RaftLogError::Runs(runs))?;
        let log_id = match recorded::read(&fs)? {
            Some(id) => id,
            None => {
                let id = new_log_id()?;
                recorded::write(&fs, id)?;
                id
            }
        };
        let prefix = KeyPrefix::new(format!("{prefix_root}{log_id}/"))?;
        // One read of the directory for both the store and the host (kbf#689).
        let (store, stored) = Store::open(fs, Options::default())?;
        let config = Config {
            id: ME,
            membership: Membership::new([ME], [])?,
            election_ticks: 2,
            heartbeat_ticks: 1,
            max_entries_per_append: 1,
        };
        let mut host = Host::open_stored(config, store, stored, NoTransport, machine, 0)?;
        // A single voter elects itself within two election timeouts; winning commits
        // the term's blank entry, which applies every entry before it.
        while host.core().role() != Role::Leader {
            host.tick(0)?;
        }
        decoded(host.machine())?;
        let expected = Bootstrap {
            log_id,
            store_prefix: prefix.as_str().to_owned(),
        };
        if host.machine().bootstrap().is_none() {
            let format = host.machine().format();
            let entry = kbf_log::encode(format, &LogCommand::Bootstrap(expected.clone()))?;
            host.propose(entry)?;
        }
        let committed = host.machine().bootstrap().cloned();
        if committed.as_ref() != Some(&expected) {
            return Err(RaftLogError::Mismatch {
                committed,
                expected,
            });
        }
        let base = host.machine().meta().now();
        host.machine_mut().keep_answers();
        let shared = Arc::new(Shared {
            base,
            bootstraps: host.machine().bootstraps(),
            term: host.core().hard_state().term.0,
            state: host.machine().state(),
            host: Mutex::new(host),
            queue: Mutex::default(),
            log_id,
            prefix,
            opened: Instant::now(),
            closed: AtomicBool::new(false),
        });
        let ticker = tokio::spawn(tick_every(Arc::downgrade(&shared))).abort_handle();
        Ok(Self {
            shared,
            ticker,
            lock: None,
        })
    }

    /// Raises the log's machine version to `to` by committing a `MachineVersion`
    /// entry, unless the log is there already (a version is never lowered). Every
    /// entry after it is encoded and decoded in `to`'s format: raising to
    /// [`MachineVersion::V2`] lets the write tier's commands commit
    /// (docs/design/metadata-snapshots.md, section 4). It is one-way: a binary that
    /// runs only an earlier version refuses the log from then on.
    ///
    /// # Errors
    /// [`MetaLogError::Unavailable`] if `to` is past what this binary runs, or the
    /// log has stopped.
    pub fn raise_machine_version(&self, to: MachineVersion) -> Result<(), MetaLogError> {
        let mut host = self.shared.lock();
        let machine = host.machine();
        if to > machine.runs() {
            let runs = machine.runs().get();
            let d = format!(
                "machine version {} is past {runs}, the one this binary runs",
                to.get()
            );
            return Err(MetaLogError::Unavailable(d));
        }
        if machine.version() >= to {
            return Ok(());
        }
        let entry = kbf_log::encode(machine.format(), &LogCommand::MachineVersion(to))
            .map_err(unavailable)?;
        // A single voter applies its own proposal before `propose` returns.
        host.propose(entry).map(|_| ()).map_err(unavailable)
    }

    /// Commits `Tick(now)` and returns the committed farm time after it.
    ///
    /// # Errors
    /// [`MetaLogError::Unavailable`] if the log has stopped.
    pub async fn tick(&self) -> Result<FarmTime, MetaLogError> {
        self.shared.tick().await
    }
}

impl<F: Fs> RaftMetaLog<F> {
    /// The log's id, from its record.
    #[must_use]
    pub fn log_id(&self) -> FarmLogId {
        self.shared.log_id
    }

    /// The key prefix of every object, as the log's `Bootstrap` committed it.
    #[must_use]
    pub fn store_prefix(&self) -> KeyPrefix {
        self.shared.prefix.clone()
    }

    /// Farm time now: the farm time applied when the log opened, plus the time since.
    #[must_use]
    pub fn now(&self) -> FarmTime {
        self.shared.now()
    }

    /// The index of the last entry the metadata state has applied (blank entries
    /// count). Every entry through it was durable in the raft directory before it was
    /// applied.
    #[must_use]
    pub fn applied_index(&self) -> u64 {
        self.shared.lock().machine().applied().0
    }

    /// The machine version the log has committed: [`MachineVersion::V1`] until
    /// [`RaftMetaLog::raise_machine_version`] raises it.
    #[must_use]
    pub fn machine_version(&self) -> MachineVersion {
        self.shared.lock().machine().version()
    }

    /// How many servers vote in the log's cell: its Raft membership's voters. One
    /// for every log this version opens.
    #[must_use]
    pub fn voters(&self) -> usize {
        self.shared.lock().core().membership().voters().count()
    }

    /// How many `Bootstrap` entries the log held when it opened, the one it may have
    /// just committed included. One for every log this version writes.
    #[must_use]
    pub fn bootstraps(&self) -> usize {
        self.shared.bootstraps
    }
}

impl<F: Fs> fmt::Debug for RaftMetaLog<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RaftMetaLog")
            .field("log_id", &self.shared.log_id)
            .field("prefix", &self.shared.prefix)
            .finish_non_exhaustive()
    }
}

impl<F: Fs> Drop for RaftMetaLog<F> {
    /// A committer runs on the blocking pool and outlives this handle: the batch it
    /// is writing holds `host`, so taking `host` here waits for that batch, and every
    /// batch that takes `host` after `closed` is set is refused.
    fn drop(&mut self) {
        self.ticker.abort();
        self.shared.closed.store(true, Ordering::SeqCst);
        drop(self.shared.lock());
    }
}

impl<F> MetaLog for RaftMetaLog<F>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.shared.execute(command).await
    }

    async fn query<R, Q>(&self, f: Q) -> Result<R, MetaLogError>
    where
        Q: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        Ok(f(&machine::read(&self.shared.state)))
    }

    fn leadership(&self) -> Option<Leadership> {
        Some(Leadership {
            log_id: self.shared.log_id.non_zero(),
            term: self.shared.term,
        })
    }
}

/// Commands waiting to be committed, each with the caller to answer.
#[derive(Default)]
struct Queue {
    waiting: Vec<(Command, oneshot::Sender<Answer>)>,
    /// A committer is running; it takes everything queued before it stops.
    committing: bool,
}

type Answer = Result<Applied, MetaLogError>;

impl<F: Fs> Shared<F> {
    fn lock(&self) -> MutexGuard<'_, RaftHost<F>> {
        // The host stops itself on a storage error rather than panicking midway, so a
        // poisoned lock still guards a whole host.
        self.host.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> FarmTime {
        self.base.saturating_add(self.opened.elapsed())
    }

    fn queue(&self) -> MutexGuard<'_, Queue> {
        // Nothing panics while holding the queue's lock.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<F> Shared<F>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    /// Queues `command`, starts a committer if none is running, and waits for the
    /// answer, which comes only after the command is durable and applied.
    async fn execute(self: &Arc<Self>, command: Command) -> Result<Applied, MetaLogError> {
        let (reply, answer) = oneshot::channel();
        let start = {
            let mut queue = self.queue();
            queue.waiting.push((command, reply));
            !std::mem::replace(&mut queue.committing, true)
        };
        if start {
            let shared = Arc::clone(self);
            let committer = move || shared.commit_queued();
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => drop(runtime.spawn_blocking(committer)),
                // A caller that polls by hand, outside any runtime (kbf-sim-cell),
                // commits on its own thread: its answer is ready when this poll ends.
                Err(_) => committer(),
            }
        }
        // The committer dropped the reply only if it panicked.
        answer
            .await
            .unwrap_or(Err(MetaLogError::Unavailable(NOT_ANSWERED.to_owned())))
    }

    /// The committer: commits what is queued as one batch, answers it, and repeats
    /// until the queue is empty. Seeing it empty and stopping happen under the queue's
    /// lock, so a command queued after that starts the next committer.
    fn commit_queued(&self) {
        loop {
            let batch = {
                let mut queue = self.queue();
                queue.committing = !queue.waiting.is_empty();
                std::mem::take(&mut queue.waiting)
            };
            if batch.is_empty() {
                return;
            }
            let (entries, replies): (Vec<_>, Vec<_>) = batch.into_iter().unzip();
            // A panic answers nothing: its callers are told the log did not answer,
            // and the committer goes on with the next batch.
            let answers =
                catch_unwind(AssertUnwindSafe(|| self.commit_batch(entries))).unwrap_or_default();
            for (reply, answer) in replies.into_iter().zip(answers) {
                // A caller that stopped waiting is not answered.
                let _ = reply.send(answer);
            }
        }
    }

    /// Encodes `commands` in the log's committed format, proposes the ones that
    /// encode as one batch, and returns each command's answer, in order: a command the
    /// format does not take (a write tier command before the raise to
    /// [`MachineVersion::V2`]) is refused and the rest commit. A single voter commits
    /// and applies its own proposals before `propose_batch` returns, so every answer is
    /// there unless the batch failed.
    ///
    /// The format is read under the host's lock, which a raise also commits under, so
    /// no raise falls between encoding a batch and committing it, and a batch holds no
    /// `MachineVersion` entry: every entry in it is decoded in the format it was
    /// encoded in.
    fn commit_batch(&self, commands: Vec<Command>) -> Vec<Answer> {
        let mut host = self.lock();
        let format = host.machine().format();
        let mut entries = Vec::with_capacity(commands.len());
        let encoded: Vec<Result<(), MetaLogError>> = commands
            .into_iter()
            .map(|command| {
                let entry = kbf_log::encode(format, &LogCommand::Meta(command));
                entries.push(entry.map_err(unavailable)?);
                Ok(())
            })
            .collect();
        let count = entries.len();
        // Checked under `host`, which the drop takes after setting `closed`.
        let batch = still_open(&self.closed)
            .and_then(|()| host.propose_batch(entries).map_err(unavailable));
        let proposed: Vec<Answer> = match batch {
            Ok(first) => (first.0..)
                .take(count)
                .map(|index| host.machine_mut().take_answer(LogIndex(index)))
                .map(|answer| answer.ok_or(MetaLogError::Unavailable(NOT_APPLIED.to_owned())))
                .collect(),
            Err(e) => vec![Err(e); count],
        };
        let mut proposed = proposed.into_iter();
        encoded
            .into_iter()
            .map(|encoded| {
                encoded.and_then(|()| {
                    let missing = MetaLogError::Unavailable(NOT_APPLIED.to_owned());
                    proposed.next().unwrap_or(Err(missing))
                })
            })
            .collect()
    }

    async fn tick(self: &Arc<Self>) -> Result<FarmTime, MetaLogError> {
        self.execute(Command::Tick(self.now())).await?;
        Ok(machine::read(&self.state).now())
    }
}

const NOT_APPLIED: &str = "the metadata log did not apply its own proposal";

const CLOSED: &str = "the metadata log was closed";

const NOT_ANSWERED: &str = "the metadata log's committer stopped without answering";

/// A log error as the front sees it: the leader could not answer.
fn unavailable(e: impl Display) -> MetaLogError {
    MetaLogError::Unavailable(format!("the metadata log: {e}"))
}

/// `Unavailable` once the log has been dropped.
fn still_open(closed: &AtomicBool) -> Result<(), MetaLogError> {
    if closed.load(Ordering::SeqCst) {
        Err(MetaLogError::Unavailable(CLOSED.to_owned()))
    } else {
        Ok(())
    }
}

/// The machine's decode failure, if an entry stopped it.
fn decoded(machine: &MetaMachine) -> Result<(), RaftLogError> {
    machine.failed().map_or(Ok(()), |(index, e)| {
        Err(RaftLogError::Decode {
            index: index.0,
            source: e.clone(),
        })
    })
}

/// Commits farm time every [`TICK`] for as long as the log is open, the first one
/// [`TICK`] after the open: at the open, farm time is the base the log just applied,
/// so a tick then would commit nothing new. An error is dropped: it has stopped the
/// log, and the next commit reports it.
async fn tick_every<F>(shared: Weak<Shared<F>>)
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    let mut every = tokio::time::interval_at(Instant::now() + TICK, TICK);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let tick = shared.upgrade().map(|log| async move { log.tick().await });
        let _ = OptionFuture::from(tick).await;
    }
}

#[cfg(test)]
mod tests;
