//! The values the state store trait speaks: versions, revisions, transactions, and the
//! rows of the change feed.

use std::num::NonZeroUsize;
use std::ops::Bound;
use std::time::Duration;

use crate::clock::StoreTime;

/// The position of a change in the store's total order. Revision 0 is before every
/// change. Each row written or removed gets its own revision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(pub u64);

impl Revision {
    /// Before every change: the feed position of a poller that has seen nothing.
    pub const ZERO: Revision = Revision(0);
}

/// The identity of one value of one key, for compare-and-set. It is the revision of the
/// write that stored the value, so a version is never reissued: a key deleted and
/// created again has a new version, and a stale holder of the old one cannot match it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u64);

/// A stored value with its version and expiry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versioned {
    /// The bytes stored.
    pub value: Vec<u8>,
    /// The version of this value.
    pub version: Version,
    /// When the store's clock expires the key, if it has a time to live. The key is live
    /// until the clock reads this and expired from that reading on.
    pub expires_at: Option<StoreTime>,
}

/// What a key must look like for an operation to proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    /// No requirement.
    Any,
    /// The key must not exist (an expired key does not exist).
    Absent,
    /// The key must exist with exactly this version.
    Version(Version),
}

/// One operation of a [`Txn`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Store `value` under `key`, with an optional time to live counted from the commit.
    Put {
        /// The key.
        key: String,
        /// The bytes to store.
        value: Vec<u8>,
        /// How long the store's clock keeps the key.
        ttl: Option<Duration>,
        /// The precondition on the key.
        cond: Cond,
    },
    /// Remove `key` (removing an absent key under [`Cond::Any`] changes nothing).
    Delete {
        /// The key.
        key: String,
        /// The precondition on the key.
        cond: Cond,
    },
    /// Change nothing; fail the transaction unless `key` meets `cond`.
    Check {
        /// The key.
        key: String,
        /// The precondition on the key.
        cond: Cond,
    },
    /// Increment the named counter and return its new value in [`Commit::counters`].
    Next {
        /// The counter.
        counter: String,
    },
}

impl Op {
    /// The key and precondition of an operation that has one.
    #[must_use]
    pub fn key_cond(&self) -> Option<(&str, Cond)> {
        match self {
            Op::Put { key, cond, .. } | Op::Delete { key, cond } | Op::Check { key, cond } => {
                Some((key.as_str(), *cond))
            }
            Op::Next { .. } => None,
        }
    }
}

/// Operations that commit together or not at all. Every precondition is judged against
/// the store as it was before the transaction; the operations then apply in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Txn {
    ops: Vec<Op>,
}

impl Txn {
    /// An empty transaction.
    #[must_use]
    pub fn new() -> Txn {
        Txn::default()
    }

    /// Adds a put with no time to live.
    #[must_use]
    pub fn put(mut self, key: impl Into<String>, value: impl Into<Vec<u8>>, cond: Cond) -> Txn {
        self.ops.push(Op::Put {
            key: key.into(),
            value: value.into(),
            ttl: None,
            cond,
        });
        self
    }

    /// Adds a put whose key the store's clock expires `ttl` after the commit.
    #[must_use]
    pub fn put_ttl(
        mut self,
        key: impl Into<String>,
        value: impl Into<Vec<u8>>,
        ttl: Duration,
        cond: Cond,
    ) -> Txn {
        self.ops.push(Op::Put {
            key: key.into(),
            value: value.into(),
            ttl: Some(ttl),
            cond,
        });
        self
    }

    /// Adds a delete.
    #[must_use]
    pub fn delete(mut self, key: impl Into<String>, cond: Cond) -> Txn {
        self.ops.push(Op::Delete {
            key: key.into(),
            cond,
        });
        self
    }

    /// Adds a precondition on a key the transaction does not write.
    #[must_use]
    pub fn check(mut self, key: impl Into<String>, cond: Cond) -> Txn {
        self.ops.push(Op::Check {
            key: key.into(),
            cond,
        });
        self
    }

    /// Adds an increment of a counter.
    #[must_use]
    pub fn next(mut self, counter: impl Into<String>) -> Txn {
        self.ops.push(Op::Next {
            counter: counter.into(),
        });
        self
    }

    /// The operations, in order.
    #[must_use]
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// How many operations can write a feed row (puts and deletes).
    #[must_use]
    pub fn change_count(&self) -> u64 {
        let n = self
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Put { .. } | Op::Delete { .. }))
            .count();
        n as u64
    }
}

/// A committed transaction, durable when this is returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// The highest revision the transaction wrote, or the store's head when it wrote no row.
    pub revision: Revision,
    /// The new value of each [`Op::Next`], in operation order.
    pub counters: Vec<u64>,
}

/// A transaction that did not apply because a precondition failed. Nothing was written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("operation {index} on {key:?} failed its precondition")]
pub struct Conflict {
    /// Which operation of the transaction.
    pub index: usize,
    /// The key it named.
    pub key: String,
}

/// The store itself failed (as opposed to a [`Conflict`], which is an answer).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The backend could not complete the call; the text says why.
    #[error("state store failure: {0}")]
    Failed(String),
    /// This handle is no longer the store's writer: a newer one took the generation
    /// ([`crate::Fenced`]), or the backend gave up the writer role
    /// ([`crate::Store::demote`]). Nothing was written; no retry on this handle can succeed.
    #[error("state store fenced: this handle is no longer the writer")]
    Fenced,
}

/// What happened to a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The key was written with this value.
    Put(Vec<u8>),
    /// The key was deleted.
    Delete,
    /// The store's clock expired the key.
    Expire,
}

/// One row of the change feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The row's position; the feed is ordered by it.
    pub revision: Revision,
    /// The key changed.
    pub key: String,
    /// What happened.
    pub event: Event,
}

/// A page of the change feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changes {
    /// Rows after the requested revision, in revision order.
    pub rows: Vec<Change>,
    /// Where to resume: pass it as `after` to the next call. It never passes a revision
    /// that can still commit, so a poller that resumes from it skips no row.
    pub cursor: Revision,
}

/// Counts a backend keeps so group commit can be observed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitStats {
    /// Transactions committed.
    pub txns: u64,
    /// Durable syncs (fsyncs) performed.
    pub syncs: u64,
}

/// Which way a C8 scan walks the key order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Ascending key order, from the lower bound up.
    #[default]
    Forward,
    /// Descending key order, from the upper bound down: newest first over keys that
    /// embed an ascending time, or the last key at or before a bound with a limit of 1.
    Reverse,
}

/// One page request of a C8 range scan: the live keys under `prefix` that lie within
/// `lower` and `upper`, walked in `direction`, at most `limit` of them.
///
/// The bounds are positions in the key order whichever way the scan walks; a bound
/// outside the prefix is clipped to it. [`ScanRange::resume`] turns a page's
/// [`ResumeToken`] into the request for the next page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanRange<'a> {
    /// Only keys that start with this are returned.
    pub prefix: &'a str,
    /// The smallest key the scan may return.
    pub lower: Bound<&'a str>,
    /// The largest key the scan may return.
    pub upper: Bound<&'a str>,
    /// The order the rows are walked and returned in.
    pub direction: Direction,
    /// The most rows one page holds.
    pub limit: NonZeroUsize,
}

impl<'a> ScanRange<'a> {
    /// A forward scan of every live key under `prefix`, `limit` rows a page.
    #[must_use]
    pub fn new(prefix: &'a str, limit: NonZeroUsize) -> ScanRange<'a> {
        ScanRange {
            prefix,
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
            direction: Direction::Forward,
            limit,
        }
    }

    /// The same scan in descending key order.
    #[must_use]
    pub fn reverse(self) -> ScanRange<'a> {
        ScanRange {
            direction: Direction::Reverse,
            ..self
        }
    }

    /// The request for the page after the one that returned `token`: the bound on the
    /// side the scan walked from moves to just past the token's key (the lower bound of
    /// a forward scan, the upper bound of a reverse one); the other bound stays.
    #[must_use]
    pub fn resume(self, token: &'a ResumeToken) -> ScanRange<'a> {
        let past = Bound::Excluded(token.key());
        match self.direction {
            Direction::Forward => ScanRange {
                lower: past,
                ..self
            },
            Direction::Reverse => ScanRange {
                upper: past,
                ..self
            },
        }
    }
}

/// Where a paged range scan resumes (C8): past the last key of the page that returned
/// it, in the scan's direction ([`ScanRange::resume`]). It is a key, not a position in a
/// snapshot, so it stays valid across commits and restarts; a caller may carry it in a
/// URL ([`ResumeToken::key`], [`ResumeToken::after`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResumeToken(String);

impl ResumeToken {
    /// The token that resumes a scan past `key`.
    #[must_use]
    pub fn after(key: impl Into<String>) -> ResumeToken {
        ResumeToken(key.into())
    }

    /// The last key the page returned.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.0
    }
}

/// A page of a bounded range scan (C8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanPage {
    /// Live keys of the request's range, in its direction, at most its limit.
    pub rows: Vec<(String, Versioned)>,
    /// Set when a live key of the range followed the last row, in the scan's direction,
    /// when the page was read; `None` when the page reached the end of the range.
    pub resume: Option<ResumeToken>,
}
