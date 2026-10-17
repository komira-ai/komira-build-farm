//! One database transaction of the store: loads and locks the keys a [`Txn`] names,
//! judges its preconditions against what it loaded, and writes rows, feed rows and
//! counters with revisions from the commit head.
//!
//! The commit head is one row. A writer locks it first and holds the lock to its commit,
//! so revisions commit in the order they were taken and the feed has no gap a poller
//! could step over (C5). Every writer takes the locks in the same order, head then keys,
//! so none deadlocks on another.

use std::collections::BTreeMap;
use std::time::Duration;

use postgres::Error;

use super::session::{Pooled, Session};
use super::{Fail, Flaw};
use crate::{Commit, Cond, Conflict, Op, Revision, Txn};

const KIND_PUT: i16 = 0;
const KIND_DELETE: i16 = 1;
const KIND_EXPIRE: i16 = 2;

/// How a transaction sees the database.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Level {
    /// Writes: a conflict the head lock did not prevent surfaces as a serialization
    /// failure, and the caller runs the transaction again.
    Serializable,
    /// Claims: after the head lock, every statement reads the latest committed rows.
    ReadCommitted,
}

/// Where revisions come from.
enum Alloc {
    /// The head row, locked: `head` as the transaction found it, `last` the last
    /// revision it has taken.
    Locked { head: i64, last: i64 },
    /// A database sequence, which is not part of the transaction (the C5 flaw).
    Sequence,
}

/// A stored row, as loaded.
#[derive(Clone, Debug)]
pub(super) struct Row {
    pub value: Vec<u8>,
    pub version: i64,
    pub expires_us: Option<i64>,
}

impl Row {
    pub(super) fn from_row(row: &postgres::Row, first: usize) -> Row {
        Row {
            value: row.get(first),
            version: row.get(first + 1),
            expires_us: row.get(first + 2),
        }
    }

    fn live(&self, now: i64) -> bool {
        self.expires_us.is_none_or(|at| at > now)
    }
}

/// A duration in whole microseconds, saturating.
pub(super) fn micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

pub(super) struct Tx<'p> {
    session: Pooled<'p>,
    /// The store's time for this transaction, read once after the head lock.
    now: i64,
    alloc: Alloc,
    /// Every key this transaction has loaded or written, as it stands now. The head lock
    /// keeps other writers out, so a loaded row stays what was loaded.
    cache: BTreeMap<String, Option<Row>>,
    /// Something was written that a commit must make durable.
    dirty: bool,
    open: bool,
}

impl<'p> Tx<'p> {
    /// Opens a transaction and takes the head lock. `over` is an injected clock reading,
    /// or `None` for the database's own clock.
    pub(super) fn begin(
        mut session: Pooled<'p>,
        level: Level,
        over: Option<i64>,
        flaw: Flaw,
    ) -> Result<Tx<'p>, Fail> {
        session.client.batch_execute(match level {
            Level::Serializable => "BEGIN ISOLATION LEVEL SERIALIZABLE",
            Level::ReadCommitted => "BEGIN ISOLATION LEVEL READ COMMITTED",
        })?;
        let mut tx = Tx {
            session,
            now: 0,
            alloc: Alloc::Sequence,
            cache: BTreeMap::new(),
            dirty: false,
            open: true,
        };
        if flaw != Flaw::SequenceRevisions {
            let s: &mut Session = &mut tx.session;
            let head: i64 = s.client.query_one(&s.st.head_lock, &[])?.get(0);
            tx.alloc = Alloc::Locked { head, last: head };
        }
        let s: &mut Session = &mut tx.session;
        tx.now = s.client.query_one(&s.st.now, &[&over])?.get(0);
        Ok(tx)
    }

    pub(super) fn dirty(&self) -> bool {
        self.dirty
    }

    /// Makes the transaction durable.
    pub(super) fn commit(mut self) -> Result<(), Fail> {
        if let Alloc::Locked { head, last } = self.alloc
            && last != head
        {
            let s: &mut Session = &mut self.session;
            s.client.execute(&s.st.head_set, &[&last])?;
        }
        self.open = false;
        if let Err(err) = self.session.client.batch_execute("COMMIT") {
            // A refusal from the server (a serialization failure) leaves the session
            // usable. A lost connection leaves it unknown whether the commit happened,
            // and the session is not reused.
            self.session.broken = err.as_db_error().is_none();
            return Err(err.into());
        }
        Ok(())
    }

    fn next_rev(&mut self) -> Result<i64, Error> {
        match &mut self.alloc {
            Alloc::Locked { last, .. } => {
                *last += 1;
                Ok(*last)
            }
            Alloc::Sequence => {
                let s: &mut Session = &mut self.session;
                Ok(s.client.query_one(&s.st.seq, &[])?.get(0))
            }
        }
    }

    fn head_revision(&self) -> u64 {
        match self.alloc {
            Alloc::Locked { last, .. } => u64::try_from(last).unwrap_or(0),
            Alloc::Sequence => 0,
        }
    }

    /// Adds the rows of `rows` to what the transaction knows (the claim loaded them).
    pub(super) fn remember(&mut self, key: String, row: Row) {
        self.cache.insert(key, Some(row));
    }

    /// Loads the rows of `keys` that are not known yet, locked, and expires those the
    /// clock has passed.
    fn load(&mut self, keys: &[&str]) -> Result<(), Error> {
        let wanted: Vec<String> = keys
            .iter()
            .filter(|key| !self.cache.contains_key(**key))
            .map(|key| (*key).to_owned())
            .collect();
        if !wanted.is_empty() {
            let s: &mut Session = &mut self.session;
            let rows = s.client.query(&s.st.load, &[&wanted])?;
            for key in wanted {
                self.cache.insert(key, None);
            }
            for row in rows {
                let key: String = row.get(0);
                self.cache.insert(key, Some(Row::from_row(&row, 1)));
            }
        }
        for key in keys {
            let dead = self
                .cache
                .get(*key)
                .and_then(Option::as_ref)
                .is_some_and(|row| !row.live(self.now));
            if dead {
                self.expire(key)?;
            }
        }
        Ok(())
    }

    fn expire(&mut self, key: &str) -> Result<(), Error> {
        let rev = self.next_rev()?;
        let s: &mut Session = &mut self.session;
        s.client.execute(&s.st.delete, &[&key])?;
        s.client
            .execute(&s.st.feed, &[&rev, &key, &KIND_EXPIRE, &None::<&[u8]>])?;
        self.cache.insert(key.to_owned(), None);
        self.dirty = true;
        Ok(())
    }

    /// Removes every key the clock has expired, one `Expire` row each, in key order.
    pub(super) fn reap(&mut self) -> Result<usize, Error> {
        let now = self.now;
        let s: &mut Session = &mut self.session;
        let keys: Vec<String> = s
            .client
            .query(&s.st.due, &[&now])?
            .iter()
            .map(|row| row.get(0))
            .collect();
        for key in &keys {
            self.expire(key)?;
        }
        Ok(keys.len())
    }

    /// Locks up to `limit` live rows under `prefix`, in key order. With `skip_locked`
    /// a row another transaction holds is passed over; without it the call waits.
    pub(super) fn claim_rows(
        &mut self,
        prefix: &str,
        limit: i64,
        skip_locked: bool,
    ) -> Result<Vec<(String, Row)>, Error> {
        let now = self.now;
        let s: &mut Session = &mut self.session;
        let statement = if skip_locked {
            &s.st.claim
        } else {
            &s.st.claim_waiting
        };
        let rows = s.client.query(statement, &[&prefix, &now, &limit])?;
        Ok(rows
            .iter()
            .map(|row| (row.get(0), Row::from_row(row, 1)))
            .collect())
    }

    fn live(&self, key: &str) -> Option<&Row> {
        self.cache.get(key).and_then(Option::as_ref)
    }

    /// Applies one transaction on top of what this database transaction has done so far.
    /// Every precondition is judged before the first write, so a conflict writes nothing.
    pub(super) fn apply(&mut self, txn: &Txn) -> Result<Result<Commit, Conflict>, Error> {
        let keys: Vec<&str> = txn
            .ops()
            .iter()
            .filter_map(|op| op.key_cond().map(|(key, _)| key))
            .collect();
        self.load(&keys)?;
        for (index, op) in txn.ops().iter().enumerate() {
            let Some((key, cond)) = op.key_cond() else {
                continue;
            };
            let row = self.live(key);
            let met = match cond {
                Cond::Any => true,
                Cond::Absent => row.is_none(),
                Cond::Version(v) => {
                    row.is_some_and(|row| i64::try_from(v.0).is_ok_and(|v| v == row.version))
                }
            };
            if !met {
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
                    let rev = self.next_rev()?;
                    let expires = ttl.map(|ttl| self.now.saturating_add(micros(ttl)));
                    let s: &mut Session = &mut self.session;
                    s.client
                        .execute(&s.st.upsert, &[&key.as_str(), value, &rev, &expires])?;
                    s.client.execute(
                        &s.st.feed,
                        &[&rev, &key.as_str(), &KIND_PUT, &Some(value.as_slice())],
                    )?;
                    self.cache.insert(
                        key.clone(),
                        Some(Row {
                            value: value.clone(),
                            version: rev,
                            expires_us: expires,
                        }),
                    );
                    wrote = Some(rev);
                }
                Op::Delete { key, .. } => {
                    if self.live(key).is_some() {
                        let rev = self.next_rev()?;
                        let s: &mut Session = &mut self.session;
                        s.client.execute(&s.st.delete, &[&key.as_str()])?;
                        s.client.execute(
                            &s.st.feed,
                            &[&rev, &key.as_str(), &KIND_DELETE, &None::<&[u8]>],
                        )?;
                        self.cache.insert(key.clone(), None);
                        wrote = Some(rev);
                    }
                }
                Op::Check { .. } => {}
                Op::Next { counter } => {
                    let s: &mut Session = &mut self.session;
                    let value: i64 = s
                        .client
                        .query_one(&s.st.counter, &[&counter.as_str()])?
                        .get(0);
                    counters.push(u64::try_from(value).unwrap_or(0));
                    self.dirty = true;
                }
            }
        }
        if wrote.is_some() {
            self.dirty = true;
        }
        let revision =
            wrote.map_or_else(|| self.head_revision(), |r| u64::try_from(r).unwrap_or(0));
        Ok(Ok(Commit {
            revision: Revision(revision),
            counters,
        }))
    }
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if self.open && self.session.client.batch_execute("ROLLBACK").is_err() {
            self.session.broken = true;
        }
    }
}
