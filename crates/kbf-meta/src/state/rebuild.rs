//! Rebuilding the index from the store: [`Command::AdoptSegment`](super::Command::AdoptSegment),
//! [`Command::RebuildProgress`](super::Command::RebuildProgress) and
//! [`Command::RebuildDone`](super::Command::RebuildDone).
//!
//! A node whose metadata directory was wiped (or that is new) holds objects in the
//! store its log has never heard of. Its rebuild lists the store in key order, reads
//! each segment's footer, and adopts the records; the log keeps how far it got, so a
//! restart resumes, and that it finished, so a later start does not run it again.

use kbf_types::Digest;

use super::{BlobEntry, MetaState};
use crate::model::{Location, ObjectId, StoreId, Writer};

/// How far a log's rebuild of its index is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rebuild {
    /// Not finished: every object key up to and including `after` is handled (`None`:
    /// none is). A log that never started a rebuild is here.
    Pending {
        /// The last object key handled.
        after: Option<String>,
    },
    /// Finished: [`Command::RebuildDone`](super::Command::RebuildDone) applied.
    Done,
}

impl Rebuild {
    /// Whether the rebuild is finished.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }
}

impl Default for Rebuild {
    fn default() -> Self {
        Self::Pending { after: None }
    }
}

/// What a [`Command::AdoptSegment`](super::Command::AdoptSegment) did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adopted {
    /// Records indexed by it: their digests were not held.
    pub inserted: u64,
    /// Records left alone: their digests were held already.
    pub kept: u64,
}

/// Why a segment was not adopted. The command changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdoptRefused {
    /// An object of the current node writer: its own puts index it, and a rebuild that
    /// raced them must not.
    #[error("object {0} is of this log's current node writer, which indexes its own")]
    CurrentWriter(ObjectId),
    /// An object of an epoch this log never allocated: adopting it would hand a later
    /// [`Command::AllocEpoch`](super::Command::AllocEpoch) a writer whose objects exist.
    #[error("object {0} is of an epoch this log never allocated")]
    UnallocatedEpoch(ObjectId),
}

impl MetaState {
    /// How far the rebuild of the index is. A rebuild that is not [`Rebuild::Done`]
    /// resumes after its [`Rebuild::Pending::after`] key.
    #[must_use]
    pub fn rebuild(&self) -> Rebuild {
        self.rebuild.clone()
    }

    pub(super) fn adopt_segment(
        &mut self,
        object: ObjectId,
        entries: Vec<(Digest, u64)>,
    ) -> Result<Adopted, AdoptRefused> {
        match object.writer() {
            Writer::Node(node) if self.current_writer == Some(node) => {
                return Err(AdoptRefused::CurrentWriter(object));
            }
            Writer::Epoch(epoch) if epoch.get() == 0 || epoch.get() >= self.next_epoch => {
                return Err(AdoptRefused::UnallocatedEpoch(object));
            }
            Writer::Epoch(_) | Writer::Node(_) | Writer::Legacy(_) => {}
        }
        // An epoch this log allocated is known already; any other writer is from now on.
        if !matches!(object.writer(), Writer::Epoch(_)) {
            self.adopted_writers.insert(object.writer());
        }
        let now = self.now;
        let mut adopted = Adopted {
            inserted: 0,
            kept: 0,
        };
        for (digest, offset) in entries {
            if self.blobs.contains_key(&digest) {
                adopted.kept += 1;
                continue;
            }
            let location = Location {
                store: StoreId::CONFIGURED,
                object,
                offset,
            };
            self.blobs.insert(
                digest,
                BlobEntry {
                    location,
                    last_touch: now,
                },
            );
            adopted.inserted += 1;
        }
        Ok(adopted)
    }

    pub(super) fn rebuild_progress(&mut self, after: String) -> Rebuild {
        if let Rebuild::Pending { after: held } = &mut self.rebuild
            && !after.is_empty()
            && held.as_ref().is_none_or(|held| after > *held)
        {
            *held = Some(after);
        }
        self.rebuild.clone()
    }

    pub(super) fn rebuild_done(&mut self) -> Rebuild {
        self.rebuild = Rebuild::Done;
        self.rebuild.clone()
    }
}
