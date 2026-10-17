//! Writer fencing: the generation row, and the store handle that checks it on every write.

use std::sync::Arc;

use crate::clock::StoreTime;
use crate::store::Store;
use crate::types::{
    Changes, Commit, CommitStats, Cond, Conflict, Revision, ScanPage, ScanRange, StoreError, Txn,
    Version, Versioned,
};

/// The row that holds the generation: eight bytes, big-endian. It is written only when a
/// writer's role begins ([`Fenced::take`]).
pub const GENERATION: &str = "meta/generation";

/// Why a generation could not be taken.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TakeError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Another writer took the generation between this one reading it and writing it.
    #[error("another writer took the generation first")]
    Lost,
    /// The store is not at the generation the taker expected (a restored or swapped
    /// file, or a writer that began since).
    #[error("the store is at generation {found}, not the expected {expected}")]
    Mismatch {
        /// What the taker was told to expect.
        expected: u64,
        /// What the store holds (0 when it has none).
        found: u64,
    },
    /// The generation row is not eight bytes.
    #[error("the generation row is not an 8-byte number")]
    Corrupt,
}

/// A handle on a store that holds one generation. Every transaction it applies carries a
/// check that the generation row is still the version this handle wrote, so once another
/// handle takes the next generation, every write here fails with [`StoreError::Fenced`]
/// and none lands. Reads are not fenced.
pub struct Fenced {
    inner: Arc<dyn Store>,
    generation: u64,
    version: Version,
}

impl std::fmt::Debug for Fenced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fenced")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Fenced {
    /// Takes the next generation of `inner` (the first, 1, for a store with none) and
    /// returns the handle that holds it. With `expect`, the store must be at exactly
    /// that generation now, or nothing is written: a successor that was told which
    /// generation it takes over never takes over a file that is not that one.
    ///
    /// # Errors
    /// [`TakeError`]: the store failed, the generation is not what was expected, the row
    /// is not a number, or another writer took it first.
    pub fn take(inner: Arc<dyn Store>, expect: Option<u64>) -> Result<Fenced, TakeError> {
        let (found, cond) = match inner.get(GENERATION)? {
            None => (0, Cond::Absent),
            Some(row) => {
                let bytes =
                    <[u8; 8]>::try_from(row.value.as_slice()).map_err(|_| TakeError::Corrupt)?;
                (u64::from_be_bytes(bytes), Cond::Version(row.version))
            }
        };
        if let Some(expected) = expect.filter(|expected| *expected != found) {
            return Err(TakeError::Mismatch { expected, found });
        }
        let generation = found + 1;
        let txn = Txn::new().put(GENERATION, generation.to_be_bytes(), cond);
        let commit = inner.txn(txn)?.map_err(|_| TakeError::Lost)?;
        // The one put of the transaction wrote the highest revision, which is its version.
        let version = Version(commit.revision.0);
        Ok(Fenced {
            inner,
            generation,
            version,
        })
    }

    /// The generation this handle holds.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn fenced(&self) -> Result<(), StoreError> {
        match self.inner.get(GENERATION)? {
            Some(row) if row.version == self.version => Ok(()),
            _ => Err(StoreError::Fenced),
        }
    }
}

impl Store for Fenced {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        // The check is the last operation of each transaction, so a conflict on it has
        // the index one past the transaction's own operations.
        let own: Vec<usize> = batch.iter().map(|txn| txn.ops().len()).collect();
        let checked = batch
            .into_iter()
            .map(|txn| txn.check(GENERATION, Cond::Version(self.version)))
            .collect();
        let results = self.inner.apply_batch(checked)?;
        let stale = |(result, len): (&Result<Commit, Conflict>, &usize)| matches!(result, Err(c) if c.index == *len);
        if results.iter().zip(&own).any(stale) {
            return Err(StoreError::Fenced);
        }
        Ok(results)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.fenced()?;
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }

    fn demote(&self) -> Result<(), StoreError> {
        self.inner.demote()
    }
}
