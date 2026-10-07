//! An in-memory [`ObjectStore`]: the executable spec of the contract, and the store the
//! rest of kbf tests against.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use bytes::Bytes;

use crate::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectInfo, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

struct Entry {
    body: Bytes,
    retain_until: Option<SystemTime>,
}

impl Entry {
    fn locked(&self) -> bool {
        self.retain_until.is_some_and(|t| SystemTime::now() < t)
    }
}

/// A bucket held in memory. It behaves as the capabilities it is built with say:
/// conditional writes refuse an existing key, Object Lock refuses deleting a retained
/// object until its date (by the system clock).
pub struct MemoryStore {
    capabilities: Capabilities,
    objects: Mutex<BTreeMap<ObjectKey, Entry>>,
}

impl MemoryStore {
    /// An empty bucket with these capabilities.
    #[must_use]
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            objects: Mutex::new(BTreeMap::new()),
        }
    }

    fn objects(&self) -> MutexGuard<'_, BTreeMap<ObjectKey, Entry>> {
        // No method panics while holding the lock, so a poisoned map is still whole.
        self.objects.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ObjectStore for MemoryStore {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        if retain_until.is_some() && !self.capabilities.object_lock {
            return Err(ObjectStoreError::Unsupported("Object Lock retention"));
        }
        let mut objects = self.objects();
        if let Some(old) = objects.get(key) {
            if self.capabilities.conditional_put {
                return Err(ObjectStoreError::AlreadyExists(key.clone()));
            }
            if old.locked() {
                return Err(ObjectStoreError::Locked(key.clone()));
            }
        }
        objects.insert(key.clone(), Entry { body, retain_until });
        Ok(())
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let objects = self.objects();
        let entry = objects
            .get(key)
            .ok_or_else(|| ObjectStoreError::NotFound(key.clone()))?;
        let size = entry.body.len() as u64;
        if range.offset() >= size {
            return Err(ObjectStoreError::InvalidRange {
                key: key.clone(),
                range,
            });
        }
        let end = size.min(range.offset() + range.size());
        // Both bounds are at most `size`, which came from a `usize`.
        Ok(entry.body.slice(range.offset() as usize..end as usize))
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let mut objects = self.objects();
        if objects.get(key).is_some_and(Entry::locked) {
            return Err(ObjectStoreError::Locked(key.clone()));
        }
        objects.remove(key);
        Ok(())
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let objects = self.objects();
        // The token is the last key of the previous page; the next page starts after it.
        let start = match after {
            Some(t) => Bound::Excluded(
                ObjectKey::new(t.0.clone())
                    .map_err(|e| ObjectStoreError::Protocol(format!("bad list token: {e}")))?,
            ),
            None => Bound::Unbounded,
        };
        let mut matching = objects
            .range((start, Bound::Unbounded))
            .filter(|(k, _)| prefix.matches(k));
        let page: Vec<ObjectInfo> = matching
            .by_ref()
            .take(usize::from(max_keys.get()))
            .map(|(k, e)| ObjectInfo {
                key: k.clone(),
                size: e.body.len() as u64,
            })
            .collect();
        let next = match (matching.next(), page.last()) {
            (Some(_), Some(last)) => Some(ListToken(last.key.as_str().to_owned())),
            _ => None,
        };
        Ok(ListPage {
            objects: page,
            next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance;

    fn run(caps: Capabilities) -> conformance::Report {
        let store = MemoryStore::new(caps);
        let prefix = KeyPrefix::new("conformance/").unwrap();
        futures::executor::block_on(conformance::run(&store, &prefix))
    }

    /// Catches: any break in the fake's contract (ranges, delete, list paging,
    /// conditional writes, Object Lock). The rest of kbf tests against this fake, so a
    /// fake that drifts from the contract makes every test above it lie.
    #[test]
    fn fake_with_every_capability_passes_conformance() {
        let report = run(Capabilities {
            conditional_put: true,
            object_lock: true,
        });
        assert!(report.passed(), "{report}");
        assert_eq!(report.skipped().count(), 0, "{report}");
    }

    /// Catches: a fake that claims nothing but still enforces (or fails) something, and
    /// a suite that fails a store for lacking an optional capability.
    #[test]
    fn fake_without_capabilities_passes_conformance() {
        let report = run(Capabilities::default());
        assert!(report.passed(), "{report}");
        assert_eq!(report.skipped().count(), 1, "{report}");
    }
}
