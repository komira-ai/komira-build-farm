//! The SQLite backend: the state store for `all-in-one` and for a single frontend.
//!
//! - **One writer.** A thread owns the only connection that writes. Calls queue to it;
//!   it gathers the requests that arrive inside the commit window ([`Options::window`],
//!   5 ms by default, [`crate::GROUP_COMMIT_WINDOW`]) into one `BEGIN IMMEDIATE`
//!   transaction, commits it with one sync (WAL mode, `synchronous=FULL`), and only then
//!   answers every request in the group. Compare-and-set, transactions and counters run
//!   inside that thread, so they are linearizable by construction.
//! - **Reads** use a connection of their own: in WAL mode they never wait for the writer
//!   and see only committed groups, so the change feed needs no horizon beyond the head.
//! - **One process.** The store holds an exclusive `flock` on `<file>.lock` for as long as
//!   it is open, and refuses a file on a network filesystem. A second store on the same
//!   file fails to open with [`SqliteError::Locked`]. The network-filesystem check reads
//!   the `statfs` type of the file's directory, so it is made on Linux only: on any other
//!   system nothing is refused, and keeping the file on a local disk is the operator's job.
//! - **Handing over.** [`Store::demote`] stops the writer and releases the lock while the
//!   reader stays open, so a successor process can open the file and write while this one
//!   goes on reading the change feed. Every write after it is `StoreError::Fenced`.
//! - **Epoch.** Every open bumps the epoch recorded in the file ([`SqliteStore::epoch`]);
//!   [`restore`] does too, so a restored or swapped file never resumes the old file's
//!   leases.
//! - **Backup and restore.** [`backup`] copies the file while a store writes it, into a
//!   single file that [`check_backup`] checks; [`restore`] (or [`restore_locked`], under
//!   a [`StoreLock`] the caller took) puts one back while no store has the file open.
//! - **Clock.** Expiry times are stored as nanoseconds on the store's clock, so the clock
//!   must mean the same thing after a restart: use [`crate::SystemClock`] in production.
//!
//! Commit latency, from the call queuing a write to its group being on disk, is kept in
//! [`CommitMetrics`] for the operator's metrics to export.

mod backup;
mod error;
mod guard;
mod read;
mod restore;
mod schema;
mod writer;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::Connection;

pub use backup::{Summary, backup, check_backup};
pub use error::SqliteError;
pub use guard::StoreLock;
pub use restore::{restore, restore_locked};
pub use schema::SCHEMA_VERSION;

use crate::GROUP_COMMIT_WINDOW;
use crate::clock::{Clock, StoreTime};
use crate::store::Store;
use crate::types::{
    Changes, Commit, CommitStats, Conflict, Revision, ScanPage, ScanRange, StoreError, Txn,
    Versioned,
};
use writer::{Reply, Request, Shared, Writer};

/// How the store groups writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// How long the writer waits, after the first request of a group, for more to join it.
    pub window: Duration,
    /// The group is committed as soon as it holds this many transactions, window or not.
    pub max_group: usize,
    /// A group whose commit latency passes this is counted in
    /// [`CommitMetrics::over_limit`].
    pub latency_limit: Duration,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            window: GROUP_COMMIT_WINDOW,
            max_group: 4096,
            latency_limit: Duration::from_millis(100),
        }
    }
}

/// Commit latency figures, for the operator's metrics. Latency runs from a call queuing
/// its write to the group it is in being on disk, so it includes the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitMetrics {
    /// Groups committed.
    pub groups: u64,
    /// The most transactions any group held.
    pub largest_group: u64,
    /// The sum of the groups' latencies.
    pub total_latency: Duration,
    /// The longest any group took.
    pub max_latency: Duration,
    /// How many groups passed [`Options::latency_limit`].
    pub over_limit: u64,
}

/// The SQLite state store.
pub struct SqliteStore {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    epoch: u64,
    /// Taken at drop or [`Store::demote`], so the writer sees its queue close; `None`
    /// once the store has given up the writer role.
    requests: Mutex<Option<SyncSender<Request>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
    reader: Mutex<Connection>,
    shared: Arc<Shared>,
    /// Held until the store is dropped or demoted; released after the writer is gone.
    lock: Mutex<Option<std::fs::File>>,
}

impl SqliteStore {
    /// Opens the state file `path`, creating it when it does not exist, with the system's
    /// own filesystem check. Bumps the epoch.
    ///
    /// # Errors
    /// [`SqliteError`]: the file is on a network filesystem, another process holds it,
    /// it is not a state store this binary reads, or a call failed.
    pub fn open(
        path: &Path,
        clock: Arc<dyn Clock>,
        options: Options,
    ) -> Result<SqliteStore, SqliteError> {
        SqliteStore::open_checked(path, clock, options, &guard::filesystem_type)
    }

    /// [`SqliteStore::open`] with the filesystem check's probe (the type number of a
    /// directory's filesystem) given, so a test can present a network filesystem.
    fn open_checked(
        path: &Path,
        clock: Arc<dyn Clock>,
        options: Options,
        probe: &dyn Fn(&Path) -> io::Result<u32>,
    ) -> Result<SqliteStore, SqliteError> {
        match SqliteStore::start(path, clock, options, probe) {
            Ok(store) => Ok(store),
            Err(error) => Err(error.at(path)),
        }
    }

    fn start(
        path: &Path,
        clock: Arc<dyn Clock>,
        options: Options,
        probe: &dyn Fn(&Path) -> io::Result<u32>,
    ) -> Result<SqliteStore, SqliteError> {
        guard::refuse_network_filesystem(path, probe)?;
        guard::refuse_hard_link(path)?;
        let (lock, _) = guard::take_lock(path)?;
        let conn = Connection::open(path)?;
        schema::init(&conn)?;
        schema::configure(&conn)?;
        let epoch = schema::bump_epoch(&conn, 0)?;
        let head = schema::head(&conn)?;
        let shared = Arc::new(Shared::default());
        let reader = Connection::open(path)?;
        schema::configure(&reader)?;
        reader.pragma_update(None, "query_only", true)?;

        let (requests, queue) = mpsc::sync_channel(options.max_group.max(1));
        let writer = Writer {
            conn,
            clock: Arc::clone(&clock),
            options,
            shared: Arc::clone(&shared),
            head: writer::uint(head),
        };
        let handle = writer.spawn(queue)?;
        Ok(SqliteStore {
            path: path.to_owned(),
            clock,
            epoch,
            requests: Mutex::new(Some(requests)),
            writer: Mutex::new(Some(handle)),
            reader: Mutex::new(reader),
            shared,
            lock: Mutex::new(Some(lock)),
        })
    }

    /// The state file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The clock the store was opened with.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// The store's incarnation: one past the epoch of the previous open (or restore) of
    /// the file. A daemon that sees a new epoch kills the leases of the old one.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The commit latency figures so far.
    #[must_use]
    pub fn commit_metrics(&self) -> CommitMetrics {
        *self
            .shared
            .metrics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a request for the writer and waits for its group to be on disk.
    fn call(&self, txns: Vec<Txn>, reap: bool) -> Result<Reply, StoreError> {
        let (reply, answer) = mpsc::sync_channel(1);
        let request = Request {
            txns,
            reap,
            queued: Instant::now(),
            reply,
        };
        let sender = held(&self.requests).clone().ok_or(StoreError::Fenced)?;
        // A writer that has stopped would not take the request, which is then dropped here
        // with its reply sender, or drops it unanswered: either way the receive fails and
        // the caller is told the writer has stopped.
        let _ = sender.send(request);
        // The clone must be gone before the wait: a demote joins the writer, which ends
        // only when every sender is.
        drop(sender);
        answer.recv().ok().ok_or_else(gone)?
    }

    fn reader(&self) -> std::sync::MutexGuard<'_, Connection> {
        held(&self.reader)
    }

    /// Stops the writer after the calls already queued, then releases the file lock.
    fn stop_writer(&self) {
        // Closing the queue ends the writer after the requests already in it.
        drop(held(&self.requests).take());
        // A writer that panicked has nothing more to say to us.
        let _ = held(&self.writer).take().map(JoinHandle::join);
        drop(held(&self.lock).take());
    }
}

fn held<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The answer to a call the writer cannot take or will never answer.
fn gone() -> StoreError {
    StoreError::Failed("the state store's writer has stopped".to_owned())
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.path)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl Drop for SqliteStore {
    fn drop(&mut self) {
        self.stop_writer();
    }
}

impl Store for SqliteStore {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Ok(self.call(batch, false)?.commits)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        read::get(&self.reader(), key, self.clock.now())
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        read::scan_prefix(&self.reader(), prefix, self.clock.now())
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        read::scan_range(&self.reader(), scan, self.clock.now())
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        read::changes(&mut self.reader(), after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        Ok(self.call(Vec::new(), true)?.reaped)
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats {
            txns: self.shared.txns.load(Ordering::Acquire),
            syncs: self.shared.commits.load(Ordering::Acquire),
        }
    }

    fn now(&self) -> StoreTime {
        self.clock.now()
    }

    fn demote(&self) -> Result<(), StoreError> {
        self.stop_writer();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
