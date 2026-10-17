//! The tier commands of [`MetaState`] (docs/design/write-path.md, sections 6.1 and
//! 6.4), and the queries a tier reads at start and the flusher reads at a seal.

use std::collections::{BTreeMap, BTreeSet};

use kbf_types::Digest;

use super::{BlobWrite, MetaState};
use crate::model::{Location, ObjectId, StoreId};
use crate::tier::{ServerId, StagedObject, StagedPut, StagedState, TierError, TierId};

impl MetaState {
    /// `server`'s current tier, if it registered one.
    #[must_use]
    pub fn tier(&self, server: ServerId) -> Option<TierId> {
        self.tiers.get(&server).copied()
    }

    /// `object`, if it is staged.
    #[must_use]
    pub fn staged(&self, object: ObjectId) -> Option<&StagedObject> {
        self.staged.get(&object)
    }

    /// Every staged object, in id order.
    pub fn staged_objects(&self) -> impl Iterator<Item = (ObjectId, &StagedObject)> {
        self.staged.iter().map(|(o, s)| (*o, s))
    }

    /// The objects `object`'s flush waits for (`docs/design/architecture-v2.md`,
    /// section 8, AC: closure blobs flush before the entry): every object on its tier
    /// holding a closure blob of an action-cache entry whose result blob `object` holds,
    /// that is still staged open or sealed. One in conflict is never flushed, so it is
    /// not waited for. Empty for an object not staged.
    #[must_use]
    pub fn flush_waits(&self, object: ObjectId) -> BTreeSet<ObjectId> {
        let Some(staged) = self.staged.get(&object) else {
            return BTreeSet::new();
        };
        staged
            .after
            .iter()
            .copied()
            .filter(|o| {
                self.staged
                    .get(o)
                    .is_some_and(|s| s.state != StagedState::Conflict)
            })
            .collect()
    }

    /// Records that the object holding `result`, if it is staged (open or sealed), is
    /// flushed after every other object on its tier staged with a blob of `closure`.
    pub(super) fn order_flush<'a>(
        &mut self,
        result: &Digest,
        closure: impl Iterator<Item = &'a Digest>,
    ) {
        let Some((object, tier)) = self.staged_at(result) else {
            return;
        };
        let after: BTreeSet<ObjectId> = closure
            .filter_map(|d| self.staged_at(d))
            .filter(|(o, t)| *o != object && *t == tier)
            .map(|(o, _)| o)
            .collect();
        // `staged_at` found `object` staged.
        self.staged
            .entry(object)
            .and_modify(|staged| staged.after.extend(after));
    }

    /// The object and tier `digest` is staged on, if its object is staged and not in
    /// conflict.
    fn staged_at(&self, digest: &Digest) -> Option<(ObjectId, TierId)> {
        let location = self.blobs.get(digest)?.location;
        let staged = self.staged.get(&location.object)?;
        (staged.state != StagedState::Conflict).then_some((location.object, staged.tier))
    }

    pub(super) fn add_tier(&mut self, server: ServerId, tier: TierId) -> Result<(), TierError> {
        if self.retired_tiers.contains(&tier) {
            return Err(TierError::TierRetired(tier));
        }
        if let Some((owner, _)) = self.tiers.iter().find(|(_, t)| **t == tier) {
            return if *owner == server {
                Ok(())
            } else {
                Err(TierError::TierTaken {
                    tier,
                    server: *owner,
                })
            };
        }
        if let Some(old) = self.tiers.get(&server).copied() {
            if self.staged.values().any(|s| s.tier == old) {
                return Err(TierError::StillStaged { server, tier: old });
            }
            self.retired_tiers.insert(old);
        }
        self.tiers.insert(server, tier);
        Ok(())
    }

    pub(super) fn put_staged(
        &mut self,
        tier: TierId,
        objects: Vec<StagedPut>,
    ) -> Result<Vec<BlobWrite>, TierError> {
        if !self.tiers.values().any(|t| *t == tier) {
            return Err(TierError::UnknownTier(tier));
        }
        let mut named = BTreeSet::new();
        for put in &objects {
            if !named.insert(put.object) {
                return Err(TierError::Repeated(put.object));
            }
            self.stageable(tier, put)?;
        }
        let mut writes = Vec::new();
        for put in objects {
            let object = put.object;
            let entry = self.staged.entry(object).or_insert(StagedObject {
                tier,
                len: 0,
                state: StagedState::Open,
                blobs: BTreeMap::new(),
                after: BTreeSet::new(),
            });
            entry.len = entry.len.max(put.end);
            if put.seal && entry.state == StagedState::Open {
                entry.state = StagedState::Sealed;
            }
            // The offset each digest is listed at: the first committed, so the index and
            // the footer the flusher builds from the list agree.
            let listed: Vec<(Digest, u64)> = put
                .blobs
                .into_iter()
                .map(|(digest, offset)| (digest, *entry.blobs.entry(digest).or_insert(offset)))
                .collect();
            for (digest, offset) in listed {
                let location = Location {
                    store: StoreId::CONFIGURED,
                    object,
                    offset,
                };
                writes.push(self.put_blob(digest, location));
            }
        }
        Ok(writes)
    }

    /// Whether `put` may be applied on `tier`; changes nothing.
    fn stageable(&self, tier: TierId, put: &StagedPut) -> Result<(), TierError> {
        let object = put.object;
        self.writable(object)?;
        if self.unreachable.contains_key(&object) {
            return Err(TierError::Unreachable(object));
        }
        if self.left.contains_key(&object) {
            return Err(TierError::Left(object));
        }
        for (digest, offset) in &put.blobs {
            if offset
                .checked_add(digest.size_bytes)
                .is_none_or(|end| end > put.end)
            {
                return Err(TierError::PastEnd {
                    object,
                    digest: *digest,
                });
            }
        }
        let Some(staged) = self.staged.get(&object) else {
            return Ok(());
        };
        if staged.tier != tier {
            return Err(TierError::OtherTier {
                object,
                tier: staged.tier,
            });
        }
        // A sealed object takes only a repeat of what it already holds: a forward retried
        // after the commit that sealed it.
        let repeat =
            put.end <= staged.len && put.blobs.iter().all(|(d, _)| staged.blobs.contains_key(d));
        if staged.state != StagedState::Open && !repeat {
            return Err(TierError::Sealed(object));
        }
        Ok(())
    }

    pub(super) fn seal_staged(&mut self, object: ObjectId) -> Result<(), TierError> {
        self.known(object)?;
        match self.staged.get_mut(&object) {
            Some(staged) => {
                if staged.state == StagedState::Open {
                    staged.state = StagedState::Sealed;
                }
                Ok(())
            }
            None => self.left_already(object, Ok(())),
        }
    }

    pub(super) fn object_flushed(&mut self, object: ObjectId) -> Result<(), TierError> {
        self.known(object)?;
        match self.staged.get(&object).map(|s| s.state) {
            Some(StagedState::Open) => Err(TierError::NotSealed(object)),
            Some(StagedState::Sealed | StagedState::Conflict) => {
                self.unstage(object);
                Ok(())
            }
            None => self.left_already(object, Ok(())),
        }
    }

    pub(super) fn flush_conflict(&mut self, object: ObjectId) -> Result<(), TierError> {
        self.known(object)?;
        match self.staged.get_mut(&object) {
            Some(staged) if staged.state == StagedState::Open => Err(TierError::NotSealed(object)),
            Some(staged) => {
                staged.state = StagedState::Conflict;
                Ok(())
            }
            None => self.left_already(object, Err(TierError::Left(object))),
        }
    }

    /// The answer for a command naming an object that is not staged: `left` if it left
    /// the table, else [`TierError::NotStaged`].
    fn left_already(&self, object: ObjectId, left: Result<(), TierError>) -> Result<(), TierError> {
        if self.left.contains_key(&object) {
            left
        } else {
            Err(TierError::NotStaged(object))
        }
    }

    /// Takes `object` out of the staged table, if it is there, and records when it left.
    pub(super) fn unstage(&mut self, object: ObjectId) {
        if self.staged.remove(&object).is_some() {
            self.left.insert(object, self.now);
        }
    }
}
