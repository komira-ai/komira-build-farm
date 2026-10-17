//! How many writes a node has open now: what a rollout asks before it stops the node
//! (`docs/design/architecture-v2.md`, section 11). A write is open from the moment its
//! call is taken in until the call ends, by an answer, an error or the caller going away;
//! a node that is a chain's replica counts the copy it is sent as a write too. A daemon's
//! segment upload is one from its `Begin` until the commit started for it ends, which runs
//! detached ([`crate::Storage::segments_counted`]).
//!
//! The count is a gauge the service moves ([`OpenWrites::begin`]) and the registration
//! reads ([`OpenWrites::count`]), like [`super::CatchUp`]: a handle both hold.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The number of writes open on a node, shared between the service that counts them and
/// whatever reports the count.
#[derive(Clone, Debug, Default)]
pub struct OpenWrites(Arc<AtomicU64>);

impl OpenWrites {
    /// A node with no write open.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The writes open now. A write begun after this was read is not in it.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// A write begins; it ends when the guard is dropped, whatever the way (an early
    /// return, an error, the call's future dropped).
    pub fn begin(&self) -> OpenWrite {
        self.0.fetch_add(1, Ordering::AcqRel);
        OpenWrite(Arc::clone(&self.0))
    }
}

/// One open write; dropping it closes the write.
#[derive(Debug)]
#[must_use = "the write is closed when this is dropped"]
pub struct OpenWrite(Arc<AtomicU64>);

impl Drop for OpenWrite {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
