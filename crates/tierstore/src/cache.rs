//! A RAM read cache in front of a [`Backend`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;

use crate::{Backend, ByteRange, Error, Listed, Meta};

/// What a cached read was asked for: an object, and a range of it or the whole of it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CacheKey {
    key: String,
    range: Option<ByteRange>,
}

struct Slot {
    bytes: Bytes,
    /// When the slot was last filled or read, as a position in the order of uses.
    tick: u64,
}

#[derive(Default)]
struct Cache {
    entries: BTreeMap<CacheKey, Slot>,
    /// Ticks to keys, least recently used first. Every slot has exactly one tick here.
    order: BTreeMap<u64, CacheKey>,
    clock: u64,
    bytes: u64,
    /// Counts the deletes. A read that started before a delete does not fill the cache.
    epoch: u64,
}

/// What a cached read counts against the budget: its length, and one byte at least, so that
/// empty reads (an empty range, a range at the end of the object) do not pile up unbounded.
fn charge(bytes: &Bytes) -> u64 {
    (bytes.len() as u64).max(1)
}

impl Cache {
    /// The cached bytes, which become the most recently used.
    fn touch(&mut self, key: &CacheKey) -> Option<Bytes> {
        self.clock += 1;
        let clock = self.clock;
        let slot = self.entries.get_mut(key)?;
        self.order.remove(&slot.tick);
        slot.tick = clock;
        self.order.insert(clock, key.clone());
        Some(slot.bytes.clone())
    }

    fn remove(&mut self, key: &CacheKey) {
        if let Some(slot) = self.entries.remove(key) {
            self.order.remove(&slot.tick);
            self.bytes -= charge(&slot.bytes);
        }
    }

    /// The least recently used key. Only asked for while there is a slot.
    fn oldest(&self) -> CacheKey {
        let first = self.order.values().next();
        first.cloned().expect("the cache holds a slot")
    }

    /// Caches `bytes` as the most recently used, then drops the least recently used until
    /// the cache is within `budget`. Bytes that alone are over the budget are not cached.
    fn insert(&mut self, key: CacheKey, bytes: Bytes, budget: u64) {
        let size = charge(&bytes);
        if size > budget {
            return;
        }
        self.remove(&key);
        self.clock += 1;
        let tick = self.clock;
        self.order.insert(tick, key.clone());
        self.entries.insert(key, Slot { bytes, tick });
        self.bytes += size;
        while self.bytes > budget {
            let oldest = self.oldest();
            self.remove(&oldest);
        }
    }

    /// Drops every cached range of an object, and counts the delete.
    fn drop_key(&mut self, key: &str) {
        self.epoch += 1;
        let start = CacheKey {
            key: key.to_owned(),
            range: None,
        };
        let doomed: Vec<CacheKey> = self
            .entries
            .range(start..)
            .map(|(cached, _)| cached)
            .take_while(|cached| cached.key == key)
            .cloned()
            .collect();
        for cached in doomed {
            self.remove(&cached);
        }
    }
}

/// A [`Backend`] that answers repeated reads from RAM.
///
/// A read is cached under the object's key and the range asked for, so a read of another
/// range of the same object is a read of the backend. The cache holds at most `budget`
/// bytes, least recently used out first (an empty read counts as one byte); a read larger
/// than the budget is not cached.
/// Errors are never cached. A delete drops the object's cached ranges, and a read that was
/// in flight during the delete does not fill the cache afterwards. Everything but `get` and
/// `delete` goes to the backend as it is.
pub struct CachedStore {
    inner: Arc<dyn Backend>,
    budget: u64,
    cache: Mutex<Cache>,
}

impl CachedStore {
    /// A cache of at most `budget` bytes in front of `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn Backend>, budget: u64) -> Self {
        Self {
            inner,
            budget,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// The bytes the cache holds.
    #[must_use]
    pub fn cached_bytes(&self) -> u64 {
        self.cache().bytes
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        // No method panics while holding the lock, so a poisoned cache is still whole.
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached bytes, if any, and the epoch they were looked up in.
    fn lookup(&self, key: &CacheKey) -> (Option<Bytes>, u64) {
        let mut cache = self.cache();
        let hit = cache.touch(key);
        (hit, cache.epoch)
    }

    /// Caches bytes read in `epoch`, unless an object was deleted since.
    fn fill(&self, epoch: u64, key: CacheKey, bytes: Bytes) {
        let mut cache = self.cache();
        if cache.epoch == epoch {
            cache.insert(key, bytes, self.budget);
        }
    }
}

#[async_trait]
impl Backend for CachedStore {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let cache_key = CacheKey {
            key: key.to_owned(),
            range,
        };
        let (hit, epoch) = self.lookup(&cache_key);
        if let Some(bytes) = hit {
            return Ok(bytes);
        }
        let bytes = self.inner.get(key, range).await?;
        self.fill(epoch, cache_key, bytes.clone());
        Ok(bytes)
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await?;
        self.cache().drop_key(key);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}
