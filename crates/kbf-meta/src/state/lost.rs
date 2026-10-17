//! Loss marks (docs/design/architecture-v2.md, section 8): the blobs of an object
//! marked unreachable while it was staged.
//!
//! A staged object's only copy is its block on a write tier until the flusher puts it
//! in the store. The cache marks a staged object unreachable only when that block is
//! gone or wrong: the tier abandoned (`--hot-store-abandon`), the block missing or
//! short at a start, or a record failing its digest before the PUT. So each blob
//! indexed in such an object is lost, not merely unreachable: no store can produce it.
//! A mark on an object that was flushed is not a loss (a store that answered 404 may
//! answer again).
//!
//! The marks are derived from [`Command::ObjectUnreachable`](super::Command) as it
//! applies, so every replica derives the same ones from the same log and nothing new
//! is written to it. A lost blob answers as any unreachable one does
//! ([`BlobAnswer::Unavailable`](crate::BlobAnswer::Unavailable)): FindMissing lists
//! it, an action-cache entry naming it is a miss, and a re-upload heals it.

use super::MetaState;
use crate::model::ObjectId;

impl MetaState {
    /// How many blobs are lost now: index entries at an object marked unreachable while
    /// it was staged. A blob leaves the count when it is uploaded again, when its entry
    /// is collected or evicted, or when its object is marked reachable again.
    #[must_use]
    pub fn lost_blobs(&self) -> u64 {
        self.lost.values().sum()
    }

    /// How many blobs were ever marked lost, each counted when its object's mark is
    /// made: the entries the object held then (a blob listed in it but indexed in
    /// another object is not lost). It never falls.
    #[must_use]
    pub const fn blob_losses(&self) -> u64 {
        self.blob_losses
    }

    /// Records the blobs of `object` as lost if it is staged: called as a mark applies,
    /// before the mark takes it out of the staged table. A staged object never carries
    /// a mark (a mark unstages it, and a marked object is never staged again), so its
    /// blobs are counted once.
    pub(super) fn mark_lost(&mut self, object: ObjectId) {
        let Some(staged) = self.staged.get(&object) else {
            return;
        };
        let held = staged
            .blobs
            .keys()
            .filter(|d| {
                self.blobs
                    .get(d)
                    .is_some_and(|e| e.location.object == object)
            })
            .count() as u64;
        if held > 0 {
            self.lost.insert(object, held);
            self.blob_losses += held;
        }
    }

    /// One index entry at `object` left it: moved to a new copy, or removed.
    pub(super) fn entry_left(&mut self, object: ObjectId) {
        if let Some(held) = self.lost.get_mut(&object) {
            *held -= 1;
            if *held == 0 {
                self.lost.remove(&object);
            }
        }
    }
}
