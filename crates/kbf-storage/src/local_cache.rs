//! The one cache of a node that runs local storage and a daemon on one disk
//! ([`LocalCache`]; `docs/design/architecture-v2.md`, sections 10 and 15 item 8).
//!
//! On such a node (`all-in-one`, and `storage-and-daemon` once it is built) the daemon's
//! input cache and the node's local storage are one cache, never two copies of a blob:
//!
//! - **One directory and one index of whole blobs**: the daemon's input cache
//!   (`<data-dir>/cache`), whose entries leases link (and shared read-only trees link,
//!   at their own path, `<data-dir>/trees`). Local storage keeps no read copies of its
//!   own beside it: its write tier deletes a block once it is flushed instead of keeping
//!   it as a flushed copy. What local storage still holds on the disk is the write tier's
//!   unflushed blocks: acknowledged writes waiting for the cold store, which no cache may
//!   drop.
//! - **One budget**: the input cache evicts its entries (least recently used, never one
//!   a running lease pins) until its entries and local storage's bytes together fit its
//!   budget. Local storage's bytes are not evictable, so they are what the entries make
//!   room for.
//! - **One fill**: [`LocalCache::fill`], what both hold on the disk, each byte counted
//!   once.
//!
//! This type is where the two halves meet: local storage reports its bytes
//! ([`LocalCache::attach_storage`]), the input cache its own
//! ([`LocalCache::attach_inputs`]), and each reads the other's. Either may attach first;
//! until one has, its bytes read as 0.

use std::fmt;
use std::sync::OnceLock;

/// What one half of the node holds on the disk now, in bytes.
pub type HeldBytes = Box<dyn Fn() -> u64 + Send + Sync>;

/// The one cache of a node that runs local storage and a daemon on one disk: see the
/// module docs. Shared by the two halves behind an `Arc`.
#[derive(Default)]
pub struct LocalCache {
    storage: OnceLock<HeldBytes>,
    inputs: OnceLock<HeldBytes>,
}

/// What the one cache holds on the disk ([`LocalCache::fill`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fill {
    /// The input cache's entries.
    pub inputs: u64,
    /// Local storage's: the write tier's blocks, flushed copies included if any are
    /// kept.
    pub storage: u64,
}

impl Fill {
    /// Both, each byte once.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.inputs.saturating_add(self.storage)
    }
}

impl LocalCache {
    /// A cache neither half has attached to yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Local storage's side: `bytes` answers what it holds on the disk. Whether this
    /// was the first attach (a second one is ignored).
    pub fn attach_storage(&self, bytes: HeldBytes) -> bool {
        self.storage.set(bytes).is_ok()
    }

    /// The input cache's side: `bytes` answers what its entries hold. Whether this was
    /// the first attach (a second one is ignored).
    pub fn attach_inputs(&self, bytes: HeldBytes) -> bool {
        self.inputs.set(bytes).is_ok()
    }

    /// What local storage holds on the disk now; 0 before it attached.
    #[must_use]
    pub fn storage_bytes(&self) -> u64 {
        self.storage.get().map_or(0, |bytes| bytes())
    }

    /// What the input cache's entries hold now; 0 before it attached.
    #[must_use]
    pub fn input_bytes(&self) -> u64 {
        self.inputs.get().map_or(0, |bytes| bytes())
    }

    /// What both hold now.
    #[must_use]
    pub fn fill(&self) -> Fill {
        Fill {
            inputs: self.input_bytes(),
            storage: self.storage_bytes(),
        }
    }
}

impl fmt::Debug for LocalCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalCache")
            .field("fill", &self.fill())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{Fill, LocalCache};

    /// Catches a fill that counts one half twice or leaves one out, a half read before
    /// it attached as anything but 0, and a second attach replacing the first (a second
    /// input cache on the same node would then take over the budget): the first
    /// attach of each side wins and the fill is their sum, read live.
    #[test]
    fn the_fill_is_each_half_once_and_the_first_attach_wins() {
        let cache = LocalCache::new();
        assert_eq!(cache.fill(), Fill::default());
        let storage = Arc::new(AtomicU64::new(7));
        let read = Arc::clone(&storage);
        assert!(cache.attach_storage(Box::new(move || read.load(Ordering::Relaxed))));
        assert!(!cache.attach_storage(Box::new(|| 1000)));
        assert!(cache.attach_inputs(Box::new(|| 30)));
        assert!(!cache.attach_inputs(Box::new(|| 1000)));
        storage.store(12, Ordering::Relaxed);
        let fill = cache.fill();
        assert_eq!(
            fill,
            Fill {
                inputs: 30,
                storage: 12
            }
        );
        assert_eq!(fill.total(), 42);
        assert_eq!(
            Fill {
                inputs: u64::MAX,
                storage: 1
            }
            .total(),
            u64::MAX
        );
        assert_eq!(
            format!("{cache:?}"),
            "LocalCache { fill: Fill { inputs: 30, storage: 12 } }"
        );
    }
}
