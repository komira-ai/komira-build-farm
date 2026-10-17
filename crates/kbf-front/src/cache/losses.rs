//! Loss marks as `/metrics` families (docs/design/architecture-v2.md, section 8): the
//! blobs whose only copy was in a block on a write tier that is gone, read from the
//! metadata state at the scrape.

use std::fmt::Write as _;

use kbf_objstore::ObjectStore;

use super::{Cache, CacheError};
use crate::meta_log::MetaLog;

/// The lost-blob gauge and counter at one moment
/// ([`kbf_meta::MetaState::lost_blobs`], [`kbf_meta::MetaState::blob_losses`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Losses {
    /// Blobs marked lost now: held in the index at an object marked unreachable while
    /// it was staged. Each is `Unavailable`, listed by FindMissing, and makes an
    /// action-cache entry naming it a miss, until it is uploaded again.
    pub lost_blobs: u64,
    /// Blobs ever marked lost; it never falls.
    pub blob_losses: u64,
}

impl Losses {
    /// Appends the two families to `out`, in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        let families = [
            (
                "kbf_cache_lost_blobs",
                "gauge",
                "Blobs whose only copy was on a write tier abandoned, or whose block was \
                 lost or damaged, before it was flushed: unavailable until uploaded again",
                self.lost_blobs,
            ),
            (
                "kbf_cache_blob_losses_total",
                "counter",
                "Blobs marked lost: each blob of a staged object marked unreachable, \
                 counted when the mark is made",
                self.blob_losses,
            ),
        ];
        for (name, kind, help, value) in families {
            let _ = writeln!(
                out,
                "# HELP {name} {help}.\n# TYPE {name} {kind}\n{name} {value}"
            );
        }
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// The lost-blob gauge and counter, from the metadata state now.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn losses(&self) -> Result<Losses, CacheError> {
        Ok(self
            .writer
            .query(|s| Losses {
                lost_blobs: s.lost_blobs(),
                blob_losses: s.blob_losses(),
            })
            .await?)
    }
}
