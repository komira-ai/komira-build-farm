//! The invocations restored from history that are still held: a bounded cache, least
//! recently used out first, so a build page read again is not read from the object store
//! again, and a page token made for one record keeps naming it while it is held.

use std::collections::BTreeMap;
use std::sync::Arc;

use kbf_history::records::InvocationHeader;
use kbf_invocation::{Invocation, RecordKey};

/// What a restored record was made from: the counters of the header it was read under
/// (chunks, last chunk time, records dropped, state, runs and hits). A header with other
/// counters means history holds more (or less) than the cached record shows, so the
/// record is made again.
pub(super) type Stamp = [u64; 6];

/// The stamp of `header`.
pub(super) fn stamp_of(header: &InvocationHeader) -> Stamp {
    [
        u64::from(header.chunks),
        header.last_chunk_ms,
        header.dropped,
        // The state's number, never negative.
        header.state.unsigned_abs().into(),
        header.runs,
        header.hits,
    ]
}

struct Held {
    stamp: Stamp,
    inv: Arc<Invocation>,
    bytes: usize,
    used: u64,
}

/// Restored invocations within `budget` bytes ([`Invocation::bytes`]).
pub(super) struct Cache {
    budget: usize,
    held: BTreeMap<RecordKey, Held>,
    bytes: usize,
    /// Counts uses: the entry with the lowest `used` is the least recently used.
    tick: u64,
}

impl Cache {
    pub(super) fn new(budget: usize) -> Self {
        Self {
            budget,
            held: BTreeMap::new(),
            bytes: 0,
            tick: 0,
        }
    }

    /// The record held under `key` if it was made under `stamp`; one made under another
    /// stamp is dropped.
    pub(super) fn get(&mut self, key: &RecordKey, stamp: Stamp) -> Option<Arc<Invocation>> {
        if self.held.get(key).is_some_and(|h| h.stamp != stamp) {
            self.remove(key);
        }
        self.tick += 1;
        let held = self.held.get_mut(key)?;
        held.used = self.tick;
        Some(Arc::clone(&held.inv))
    }

    /// Holds `inv` under `key`, made under `stamp`, in place of any record held under it,
    /// and drops least recently used records while there are more bytes than the budget.
    /// A record larger than the whole budget is not held.
    pub(super) fn put(&mut self, key: RecordKey, stamp: Stamp, inv: Arc<Invocation>) {
        self.remove(&key);
        let bytes = inv.bytes();
        if bytes > self.budget {
            return;
        }
        self.tick += 1;
        self.bytes += bytes;
        let used = self.tick;
        self.held.insert(
            key,
            Held {
                stamp,
                inv,
                bytes,
                used,
            },
        );
        if self.bytes > self.budget {
            // Least recently used first, until what is held fits.
            let mut by_use: Vec<(u64, RecordKey)> =
                self.held.iter().map(|(k, h)| (h.used, k.clone())).collect();
            by_use.sort_unstable();
            for (_, oldest) in by_use {
                if self.bytes <= self.budget {
                    break;
                }
                self.remove(&oldest);
            }
        }
    }

    fn remove(&mut self, key: &RecordKey) {
        if let Some(held) = self.held.remove(key) {
            self.bytes -= held.bytes;
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.held.len()
    }
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
