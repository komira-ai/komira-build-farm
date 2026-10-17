//! Eviction (`docs/design/worker-cache.md#65-sizing-eviction-and-the-link-limit`): LRU
//! down to the byte budget, then down to the free-space floor counting only entries no
//! lease links.
//!
//! One key at a time, the evictor takes the key's slot in the same hold of the lock
//! that finds it indexed and unpinned and removes it from the index; then it unlinks
//! the name and releases the slot. A key whose slot someone holds is skipped. It never
//! waits on a slot, so it runs anywhere: after each insert, and on the background
//! thread at start (the drain to a budget lowered between runs). Unlinking removes the
//! cache's name only: a lease's links keep the inode, and its space is freed with the
//! lease.
//!
//! The budget counts every entry, whoever links it, and, on a node with one cache
//! (`CacheConfig::local`), local storage's bytes on the same disk, which are not the
//! cache's to evict: entries make room for them. The free-space floor does not: an
//! entry with more than one link (a running lease's) frees nothing when evicted, so the
//! floor skips it, and when nothing it may evict is left it stops and counts
//! `floor_unreachable` rather than emptying the cache for no space. Each candidate's
//! link count is read once per round, under the lock.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use rustix::fs::{AtFlags, CWD};

use super::layout::{Key, remove_if_present};
use super::{Inner, Point, State, bump};

impl Inner {
    /// One eviction round: the budget, then the floor.
    pub(super) fn evict(&self) {
        // Read once a round, outside the state's lock.
        let storage = self.storage_bytes();
        self.note_storage_over(storage);
        let max = self.budget_of(storage);
        let over = |state: &State| state.index.total() > max;
        while self.evict_one(&mut |state| over(state).then(|| oldest(state, |_| true)).flatten()) {}
        if self.config.min_free_bytes > 0 {
            self.evict_to_floor();
        }
    }

    /// Warns when local storage alone holds the whole budget (a hot store budgeted at
    /// or above the cache): every entry no lease pins is then evicted. Once, until it
    /// no longer does.
    fn note_storage_over(&self, storage: u64) {
        let over = storage > 0 && storage >= self.config.max_bytes;
        if !over {
            self.storage_over.store(false, Ordering::Relaxed);
        } else if !self.storage_over.swap(true, Ordering::Relaxed) {
            // Only pinned entries are kept while it does.
            let max = self.config.max_bytes;
            tracing::warn!("cache: local storage holds {storage} B, all the {max} B budget");
        }
    }

    /// What the entries may hold while local storage holds `storage` bytes.
    pub(super) const fn budget_of(&self, storage: u64) -> u64 {
        self.config.max_bytes.saturating_sub(storage)
    }

    /// Evicts entries no lease links until the filesystem has the floor's free space.
    fn evict_to_floor(&self) {
        let mut linked = HashSet::new();
        let mut unlinked = |key: &Key| {
            let links = rustix::fs::statat(CWD, self.path_of(key), AtFlags::SYMLINK_NOFOLLOW);
            let shared = !linked.contains(key) && links.is_ok_and(|stat| stat.st_nlink > 1);
            if shared {
                linked.insert(*key);
            }
            !linked.contains(key)
        };
        while self.free_bytes() < self.config.min_free_bytes {
            if !self.evict_one(&mut |state| oldest(state, &mut unlinked)) {
                bump(&self.counters.floor_unreachable);
                self.state().pressure = true;
                tracing::warn!("cache: free space is under the floor and no entry can be evicted");
                return;
            }
        }
        self.state().pressure = false;
    }

    /// The free bytes of the cache's filesystem, as an unprivileged user may use them;
    /// all of them when they cannot be read.
    fn free_bytes(&self) -> u64 {
        rustix::fs::statvfs(&self.blobs).map_or(u64::MAX, |s| s.f_bavail.saturating_mul(s.f_frsize))
    }

    /// Evicts the key `pick` finds, if it finds one: whether it did.
    fn evict_one(&self, pick: &mut dyn FnMut(&State) -> Option<Key>) -> bool {
        let mut state = self.state();
        let Some(key) = pick(&state) else {
            return false;
        };
        state.index.remove(&key);
        let slot = self.take(&mut state, key);
        drop(state);
        self.config.hooks.at(Point::Evicting(&key));
        if let Err(e) = remove_if_present(&self.path_of(&key)) {
            tracing::warn!("cache: evicting {}: {e}", key.name());
        }
        bump(&self.counters.evictions);
        drop(slot);
        true
    }
}

/// The least recently used key that is neither pinned nor held and that `ok` accepts.
fn oldest(state: &State, mut ok: impl FnMut(&Key) -> bool) -> Option<Key> {
    let free = |key: &Key| !state.pins.contains_key(key) && !state.slots.contains_key(key);
    state
        .index
        .oldest()
        .find(|key| free(key) && ok(key))
        .copied()
}
