//! [`Command::EvictObjects`](super::Command::EvictObjects): forgetting objects the store
//! no longer holds, and the action-cache entries that needed them.

use std::collections::BTreeSet;
use std::iter;

use super::{Evicted, MetaState};
use crate::model::ObjectId;

impl MetaState {
    /// Every object the index names: each object some blob's current location is in,
    /// staged objects included (their only copy is a block on a tier until the flusher
    /// writes it; [`MetaState::staged`] tells them apart). An object named only by
    /// entries [`Command::EvictObjects`] or `Collect` removed, or whose blobs all
    /// healed into other objects, is not in it. One scan of the index.
    ///
    /// [`Command::EvictObjects`]: super::Command::EvictObjects
    #[must_use]
    pub fn indexed_objects(&self) -> BTreeSet<ObjectId> {
        self.blobs
            .values()
            .map(|entry| entry.location.object)
            .collect()
    }

    /// One scan of the index, then (only if a blob left) one of the action cache, as
    /// `collect` does. The index scan fills the removed list in digest order, so the
    /// action scan finds each needed digest in it by binary search.
    pub(super) fn evict_objects(&mut self, objects: &BTreeSet<ObjectId>) -> Evicted {
        let objects: BTreeSet<ObjectId> = objects
            .iter()
            .filter(|o| !self.staged.contains_key(o))
            .copied()
            .collect();
        let mut held = BTreeSet::new();
        let mut blobs = Vec::new();
        self.blobs.retain(|digest, entry| {
            let object = entry.location.object;
            if !objects.contains(&object) {
                return true;
            }
            held.insert(object);
            blobs.push(*digest);
            false
        });
        let mut actions = Vec::new();
        if !blobs.is_empty() {
            self.actions.retain(|action, entry| {
                let record = &entry.record;
                let needs_removed = iter::once(&record.result)
                    .chain(record.closure.iter())
                    .any(|d| blobs.binary_search(d).is_ok());
                if needs_removed {
                    actions.push(*action);
                }
                !needs_removed
            });
        }
        for object in &objects {
            self.unreachable.remove(object);
            self.lost.remove(object);
        }
        self.losses += held.len() as u64;
        Evicted { blobs, actions }
    }
}
