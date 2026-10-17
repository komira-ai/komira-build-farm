//! The rebuild of the index from the store ([`Cache::rebuild_index`]): what a storage
//! node whose metadata log is new or was wiped does before it can say a blob it does not
//! index is not held (`docs/design/architecture-v2.md`, section 8).
//!
//! The store is listed in key order under the root both layouts of key share, each
//! segment's footer is read ([`kbf_objstore::footers`]), and the records are adopted into
//! the index ([`Command::AdoptSegment`]): only those whose digest is not indexed, so a
//! blob written while the rebuild ran is never replaced. The log keeps the last key
//! handled ([`Command::RebuildProgress`], once per batch of objects) and that the rebuild
//! finished ([`Command::RebuildDone`]), so a restart resumes after the last key, and a
//! log that finished does not list again.

use std::num::NonZeroUsize;
use std::pin::pin;
use std::time::Duration;

use futures::TryStreamExt as _;
use kbf_meta::{AdoptRefused, Applied, Command, Rebuild};
use kbf_objstore::footers::{FooterRead, FooterReads, SegmentFooter, SkipReason};
use kbf_objstore::{ListToken, ObjectKey, ObjectStore};
use tokio::time::Instant;

use super::{Cache, CacheError, unexpected};
use crate::meta_log::MetaLog;

/// The most records one [`Command::AdoptSegment`] names: a segment of more is adopted by
/// several commands, so no log entry is larger than about 200 KiB.
pub const ADOPT_RECORDS: usize = 4096;

/// How [`Cache::rebuild_index`] reads and commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebuildConfig {
    /// How the footers are read: the listing's page, the reads in flight, the tail.
    pub reads: FooterReads,
    /// How many objects are handled between two [`Command::RebuildProgress`] commits.
    pub batch: NonZeroUsize,
}

impl Default for RebuildConfig {
    fn default() -> Self {
        Self {
            reads: FooterReads::default(),
            batch: NonZeroUsize::new(256).expect("256 is not zero"),
        }
    }
}

/// What a [`Cache::rebuild_index`] found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// The log had finished a rebuild already: nothing was listed.
    pub already_done: bool,
    /// The key the listing resumed after, for a rebuild an earlier start left part-way.
    pub resumed_after: Option<String>,
    /// Objects listed.
    pub objects: u64,
    /// Segments whose records were adopted (those that are this cache's to adopt).
    pub segments: u64,
    /// Records newly indexed.
    pub inserted: u64,
    /// Records left alone: their digest was indexed already.
    pub kept: u64,
    /// Objects of this cache's current writer, left to its own puts.
    pub own: u64,
    /// Objects not read as a segment (too short, torn, or foreign bytes), logged.
    pub skipped: u64,
    /// Objects whose key is not one of this cache's layouts, or of an epoch this log
    /// never allocated; logged.
    pub foreign: u64,
    /// Objects deleted between the listing and the read.
    pub gone: u64,
    /// How long the rebuild took.
    pub duration: Duration,
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Indexes the objects of the store that the index does not know, resuming where an
    /// earlier call stopped, and marks the rebuild done ([`Command::RebuildDone`]): a
    /// log that has that returns at once.
    ///
    /// Every object under the root of both layouts of key (a node writer's
    /// `<root>cas/<member>/<boot>/<seq>`, and an epoch of any log's
    /// `<root><log id>/cas/<epoch>/<seq>`) is read for its footer and adopted by
    /// [`Command::AdoptSegment`], except an object of this cache's own writer (its puts
    /// index those). An object that is not a segment (too short, a torn or foreign tail),
    /// or whose key names no object of this cache, is counted and skipped, not adopted.
    ///
    /// # Errors
    /// The metadata log or the listing fails, or a read of an object's footer fails (not
    /// the object's absence or a damaged footer: those are skipped). What was done stays
    /// done and the log has the key of the last full batch, so a later call resumes
    /// after it; the rebuild is not marked done.
    pub async fn rebuild_index(&self, config: RebuildConfig) -> Result<RebuildReport, CacheError> {
        let started = Instant::now();
        let mut report = RebuildReport::default();
        let after = match self.writer.query(|s| s.rebuild()).await? {
            Rebuild::Done => {
                report.already_done = true;
                return Ok(report);
            }
            Rebuild::Pending { after } => after,
        };
        let from = match &after {
            Some(key) => Some(ListToken::after(&ObjectKey::new(key.clone())?)),
            None => None,
        };
        report.resumed_after = after;
        let mut footers = pin!(kbf_objstore::footers::read_footers(
            &self.writer.objects,
            &self.writer.keys.root,
            from,
            config.reads,
        ));
        let mut last: Option<ObjectKey> = None;
        let mut pending = 0_usize;
        while let Some(read) = footers.try_next().await? {
            report.objects += 1;
            pending += 1;
            match read {
                FooterRead::Segment(segment) => {
                    self.adopt(&segment, &mut report).await?;
                    last = Some(segment.key);
                }
                FooterRead::Skipped(skipped) => {
                    match skipped.why {
                        // Not a verdict on the object: stop, and read it again next time.
                        SkipReason::Read(error) => return Err(error.into()),
                        SkipReason::Gone => report.gone += 1,
                        why @ (SkipReason::Short | SkipReason::NotASegment(_)) => {
                            tracing::warn!(key = %skipped.key, %why, "rebuild: not a segment, skipped");
                            report.skipped += 1;
                        }
                    }
                    last = Some(skipped.key);
                }
            }
            if pending >= config.batch.get() {
                self.progress(&mut last).await?;
                pending = 0;
            }
        }
        self.progress(&mut last).await?;
        match self.writer.commit(Command::RebuildDone).await? {
            Applied::Rebuild(Rebuild::Done) => {}
            other => return Err(unexpected(&other)),
        }
        report.duration = started.elapsed();
        Ok(report)
    }

    /// Commits the last key handled, if any since the last commit.
    async fn progress(&self, last: &mut Option<ObjectKey>) -> Result<(), CacheError> {
        let Some(key) = last.take() else {
            return Ok(());
        };
        let after = key.as_str().to_owned();
        match self
            .writer
            .commit(Command::RebuildProgress { after })
            .await?
        {
            Applied::Rebuild(_) => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    /// Adopts the records of `segment`, in commands of at most [`ADOPT_RECORDS`].
    async fn adopt(
        &self,
        segment: &SegmentFooter,
        report: &mut RebuildReport,
    ) -> Result<(), CacheError> {
        let Some(object) = self.writer.keys.object(&segment.key) else {
            tracing::warn!(key = %segment.key, "rebuild: not an object of this cache, skipped");
            report.foreign += 1;
            return Ok(());
        };
        let entries: Vec<_> = segment
            .footer
            .entries()
            .iter()
            .map(|e| (e.digest, e.offset))
            .collect();
        for chunk in entries.chunks(ADOPT_RECORDS) {
            let command = Command::AdoptSegment {
                object,
                entries: chunk.to_vec(),
            };
            match self.writer.commit(command).await? {
                Applied::Adopted(Ok(adopted)) => {
                    report.inserted += adopted.inserted;
                    report.kept += adopted.kept;
                }
                Applied::Adopted(Err(AdoptRefused::CurrentWriter(_))) => {
                    report.own += 1;
                    return Ok(());
                }
                Applied::Adopted(Err(refused @ AdoptRefused::UnallocatedEpoch(_))) => {
                    tracing::warn!(key = %segment.key, %refused, "rebuild: skipped");
                    report.foreign += 1;
                    return Ok(());
                }
                other => return Err(unexpected(&other)),
            }
        }
        report.segments += 1;
        Ok(())
    }
}
