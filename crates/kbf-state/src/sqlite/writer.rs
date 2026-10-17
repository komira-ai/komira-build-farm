//! The writer thread: the only connection that writes. It gathers the requests that
//! arrive inside the commit window into one group, applies the group in one SQLite
//! transaction, syncs once, and only then answers every request in it.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, Row, Statement, TransactionBehavior, params};

use super::schema::{KIND_DELETE, KIND_EXPIRE, KIND_PUT};
use super::{CommitMetrics, Options};
use crate::clock::{Clock, StoreTime};
use crate::types::{Commit, Cond, Conflict, Op, Revision, StoreError, Txn, Version};

/// What the writer answers.
pub(super) struct Reply {
    /// One result per transaction of a batch.
    pub(super) commits: Vec<Result<Commit, Conflict>>,
    /// How many keys a reap request removed.
    pub(super) reaped: usize,
}

/// One call on the store, waiting for its answer.
pub(super) struct Request {
    /// The transactions to apply, in order (empty for a reap).
    pub(super) txns: Vec<Txn>,
    /// Whether this is a reap request.
    pub(super) reap: bool,
    /// When the caller queued it: commit latency counts from here.
    pub(super) queued: Instant,
    /// Where the answer goes, once the group it is in is on disk.
    pub(super) reply: SyncSender<Result<Reply, StoreError>>,
}

/// The counters the store reads and the writer updates.
#[derive(Default)]
pub(super) struct Shared {
    /// Transactions committed.
    pub(super) txns: AtomicU64,
    /// SQLite commits the writer made: one per group that wrote anything.
    pub(super) commits: AtomicU64,
    /// The commit latency figures.
    pub(super) metrics: Mutex<CommitMetrics>,
}

pub(super) struct Writer {
    pub(super) conn: Connection,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) options: Options,
    pub(super) shared: Arc<Shared>,
    /// The highest revision handed out and committed.
    pub(super) head: u64,
}

pub(super) fn to_sql(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The key of a row `reap` removed.
fn key_of(row: &Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}

/// The version and expiry of a `kv` row.
fn version_of(row: &Row<'_>) -> rusqlite::Result<(i64, Option<i64>)> {
    Ok((row.get(0)?, row.get(1)?))
}

pub(super) fn uint(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

pub(super) fn to_nanos(time: StoreTime) -> i64 {
    i64::try_from(time.as_duration().as_nanos()).unwrap_or(i64::MAX)
}

pub(super) fn from_nanos(nanos: i64) -> StoreTime {
    StoreTime::from_duration(Duration::from_nanos(uint(nanos)))
}

impl Writer {
    /// Starts the writer thread, which serves `requests` until every sender is gone.
    pub(super) fn spawn(self, requests: Receiver<Request>) -> io::Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("kbf-state-writer".to_owned())
            .spawn(move || self.run(&requests))
    }

    /// Serves requests until every sender is gone.
    pub(super) fn run(mut self, requests: &Receiver<Request>) {
        while let Ok(first) = requests.recv() {
            let started = first.queued;
            let mut group = vec![first];
            let mut size = group[0].txns.len().max(1);
            let deadline = Instant::now() + self.options.window;
            while size < self.options.max_group {
                let left = deadline.saturating_duration_since(Instant::now());
                match requests.recv_timeout(left) {
                    Ok(next) => {
                        size += next.txns.len().max(1);
                        group.push(next);
                    }
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            }
            self.commit_group(group, started, size);
        }
    }

    fn commit_group(&mut self, group: Vec<Request>, started: Instant, size: usize) {
        let result = self.apply_group(&group);
        let latency = started.elapsed();
        match result {
            Ok(replies) => {
                self.record(latency, size, &replies);
                for (request, reply) in group.into_iter().zip(replies) {
                    // A caller that gave up has dropped its receiver: nothing to tell it.
                    let _ = request.reply.send(Ok(reply));
                }
            }
            Err(error) => {
                let error = StoreError::from(error);
                for request in group {
                    let _ = request.reply.send(Err(error.clone()));
                }
            }
        }
    }

    fn record(&self, latency: Duration, size: usize, replies: &[Reply]) {
        let committed = replies
            .iter()
            .flat_map(|reply| &reply.commits)
            .filter(|result| result.is_ok())
            .count();
        self.shared
            .txns
            .fetch_add(committed as u64, Ordering::AcqRel);
        let mut metrics = self
            .shared
            .metrics
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        metrics.groups += 1;
        metrics.largest_group = metrics.largest_group.max(size as u64);
        metrics.total_latency += latency;
        metrics.max_latency = metrics.max_latency.max(latency);
        metrics.over_limit += u64::from(latency > self.options.latency_limit);
    }

    /// Applies the group in one transaction and commits it, if anything was written. The
    /// head moves only once the commit has succeeded.
    fn apply_group(&mut self, group: &[Request]) -> rusqlite::Result<Vec<Reply>> {
        let now = self.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut applier = Applier::new(&tx, self.head, now)?;
        let mut replies = Vec::with_capacity(group.len());
        for request in group {
            let reaped = applier.reap()?;
            let mut commits = Vec::with_capacity(request.txns.len());
            for txn in &request.txns {
                commits.push(applier.apply(txn)?);
            }
            replies.push(Reply {
                commits,
                reaped: if request.reap { reaped } else { 0 },
            });
        }
        let head = applier.finish()?;
        drop(applier);
        if let Some(head) = head {
            tx.commit()?;
            self.shared.commits.fetch_add(1, Ordering::AcqRel);
            self.head = head;
        }
        Ok(replies)
    }
}

/// The statements of one group, and the revisions it hands out.
struct Applier<'c> {
    get: Statement<'c>,
    put: Statement<'c>,
    delete: Statement<'c>,
    feed: Statement<'c>,
    counter: Statement<'c>,
    expire: Statement<'c>,
    set_head: Statement<'c>,
    now: StoreTime,
    head: u64,
    wrote: bool,
}

impl<'c> Applier<'c> {
    fn new(conn: &'c Connection, head: u64, now: StoreTime) -> rusqlite::Result<Applier<'c>> {
        Ok(Applier {
            get: conn.prepare("SELECT version, expires_at FROM kv WHERE key = ?1")?,
            put: conn.prepare(
                "INSERT OR REPLACE INTO kv (key, value, version, expires_at) VALUES (?1, ?2, ?3, ?4)",
            )?,
            delete: conn.prepare("DELETE FROM kv WHERE key = ?1")?,
            feed: conn.prepare(
                "INSERT INTO feed (revision, key, kind, value) VALUES (?1, ?2, ?3, ?4)",
            )?,
            counter: conn.prepare(
                "INSERT INTO counter (name, value) VALUES (?1, 1) \
                 ON CONFLICT (name) DO UPDATE SET value = value + 1 RETURNING value",
            )?,
            expire: conn.prepare(
                "DELETE FROM kv WHERE expires_at IS NOT NULL AND expires_at <= ?1 RETURNING key",
            )?,
            set_head: conn.prepare("UPDATE meta SET value = ?1 WHERE name = 'head'")?,
            now,
            head,
            wrote: false,
        })
    }

    /// Takes the next revision.
    fn revision(&mut self) -> u64 {
        self.head += 1;
        self.wrote = true;
        self.head
    }

    fn append(
        &mut self,
        revision: u64,
        key: &str,
        kind: i64,
        value: Option<&[u8]>,
    ) -> rusqlite::Result<()> {
        self.feed
            .execute(params![to_sql(revision), key, kind, value])?;
        Ok(())
    }

    /// Removes the keys the clock has expired, with an `Expire` row each; how many.
    fn reap(&mut self) -> rusqlite::Result<usize> {
        let due = self.expire.query_map([to_nanos(self.now)], key_of)?;
        let mut keys = due.collect::<rusqlite::Result<Vec<String>>>()?;
        keys.sort();
        for key in &keys {
            let revision = self.revision();
            self.append(revision, key, KIND_EXPIRE, None)?;
        }
        Ok(keys.len())
    }

    /// The version of `key` if it is live.
    fn live_version(&mut self, key: &str) -> rusqlite::Result<Option<Version>> {
        let now = to_nanos(self.now);
        let row = self.get.query_row([key], version_of).optional()?;
        Ok(row
            .filter(|(_, expires)| expires.is_none_or(|at| at > now))
            .map(|(version, _)| Version(uint(version))))
    }

    fn holds(&mut self, key: &str, cond: Cond) -> rusqlite::Result<bool> {
        let live = self.live_version(key)?;
        Ok(match cond {
            Cond::Any => true,
            Cond::Absent => live.is_none(),
            Cond::Version(version) => live == Some(version),
        })
    }

    /// Applies one transaction: every precondition against the store as it was, then the
    /// operations in order.
    fn apply(&mut self, txn: &Txn) -> rusqlite::Result<Result<Commit, Conflict>> {
        for (index, op) in txn.ops().iter().enumerate() {
            if let Some((key, cond)) = op.key_cond()
                && !self.holds(key, cond)?
            {
                return Ok(Err(Conflict {
                    index,
                    key: key.to_owned(),
                }));
            }
        }
        let mut wrote = None;
        let mut counters = Vec::new();
        for op in txn.ops() {
            match op {
                Op::Put {
                    key, value, ttl, ..
                } => {
                    let revision = self.revision();
                    let expires = ttl.map(|ttl| to_nanos(self.now.saturating_add(ttl)));
                    self.put
                        .execute(params![key, value, to_sql(revision), expires])?;
                    self.append(revision, key, KIND_PUT, Some(value.as_slice()))?;
                    wrote = Some(revision);
                }
                Op::Delete { key, .. } => {
                    if self.delete.execute([key])? > 0 {
                        let revision = self.revision();
                        self.append(revision, key, KIND_DELETE, None)?;
                        wrote = Some(revision);
                    }
                }
                Op::Check { .. } => {}
                Op::Next { counter } => {
                    let value: i64 = self.counter.query_row([counter], |row| row.get(0))?;
                    self.wrote = true;
                    counters.push(uint(value));
                }
            }
        }
        Ok(Ok(Commit {
            revision: Revision(wrote.unwrap_or(self.head)),
            counters,
        }))
    }

    /// Records the head, if the group wrote anything, and returns it.
    fn finish(&mut self) -> rusqlite::Result<Option<u64>> {
        if !self.wrote {
            return Ok(None);
        }
        self.set_head.execute([to_sql(self.head)])?;
        Ok(Some(self.head))
    }
}
