//! What the engine counts: [`TransferStats`] per lease (`docs/design/worker-cache.md#46-metrics-through-the-api`)
//! and [`EngineStats`] for the process.
//!
//! A lease's counters are logged once, when the last handle on them goes (its
//! [`crate::Scope`], and any fetch it started that is still finishing). Carrying them
//! to the API (`auxiliary_metadata`, the invocation metrics) is a later stage.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use kbf_types::LeaseId;

/// One lease's transfers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransferStats {
    /// ByteStream reads this lease started, each attempt counted (a retry is a read).
    pub read_calls: u64,
    /// Blob bytes those reads received.
    pub read_bytes: u64,
    /// Reads served by a fetch another caller (this lease or another) had started.
    pub merged_reads: u64,
    /// Digests this lease asked FindMissing about.
    pub find_missing_digests: u64,
    /// Uploads skipped because FindMissing said the server holds the blob.
    pub outputs_present: u64,
    /// ByteStream writes, each attempt counted.
    pub write_calls: u64,
    /// Blob bytes this lease's successful writes sent. A blob two leases write at once
    /// is sent by both, and counts for both.
    pub written_bytes: u64,
    /// Blobs this lease's successful writes stored (a failed or retried attempt counts
    /// in `write_calls` only).
    pub written_blobs: u64,
    /// Requests retried after an UNAVAILABLE or a broken connection.
    pub retries: u64,
}

impl fmt::Display for TransferStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "read_calls={} read_bytes={} merged_reads={} find_missing_digests={} \
             outputs_present={} write_calls={} written_bytes={} written_blobs={} retries={}",
            self.read_calls,
            self.read_bytes,
            self.merged_reads,
            self.find_missing_digests,
            self.outputs_present,
            self.write_calls,
            self.written_bytes,
            self.written_blobs,
            self.retries
        )
    }
}

/// Where a lease's output upload spent its time, for the driver's `outputs phases:`
/// line. Milliseconds marked "summed" add up the durations of calls that run at once,
/// so they can exceed the wall time; `upload_ms` is wall time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UploadStats {
    /// FindMissing calls this lease's [`crate::Cas::missing`] made, each attempt counted.
    pub find_missing_calls: u64,
    /// Those calls' durations, summed, a wait for a request permit included.
    pub find_missing_call_ms: u64,
    /// Segment uploads attempted (Begin to Commit; a retried upload counts twice).
    pub uploads: u64,
    /// Segments those attempts declared.
    pub segments: u64,
    /// `PutRange` calls made.
    pub put_range_calls: u64,
    /// `Begin` calls' durations, summed.
    pub begin_ms: u64,
    /// `PutRange` calls' durations, summed (the wait for a permit is not in it).
    pub put_range_ms: u64,
    /// `Commit` calls' durations, summed.
    pub commit_ms: u64,
    /// Time `PutRange` calls waited for a share of the engine's request permits, summed.
    pub permit_wait_ms: u64,
    /// Wall time of [`crate::Cas::put_spool`] calls.
    pub upload_ms: u64,
}

/// [`TransferStats`] as they are counted, and the lease they are logged for.
#[derive(Debug, Default)]
pub(crate) struct Counts {
    pub lease: Option<LeaseId>,
    pub read_calls: AtomicU64,
    pub read_bytes: AtomicU64,
    pub merged_reads: AtomicU64,
    pub find_missing_digests: AtomicU64,
    pub outputs_present: AtomicU64,
    pub write_calls: AtomicU64,
    pub written_bytes: AtomicU64,
    pub written_blobs: AtomicU64,
    pub retries: AtomicU64,
    pub find_missing_calls: AtomicU64,
    pub find_missing_us: AtomicU64,
    pub uploads: AtomicU64,
    pub segments: AtomicU64,
    pub put_range_calls: AtomicU64,
    pub begin_us: AtomicU64,
    pub put_range_us: AtomicU64,
    pub commit_us: AtomicU64,
    pub permit_wait_us: AtomicU64,
    pub upload_us: AtomicU64,
}

impl Counts {
    pub fn new(lease: Option<LeaseId>) -> Self {
        let mut counts = Self::default();
        counts.lease = lease;
        counts
    }

    pub fn stats(&self) -> TransferStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        TransferStats {
            read_calls: get(&self.read_calls),
            read_bytes: get(&self.read_bytes),
            merged_reads: get(&self.merged_reads),
            find_missing_digests: get(&self.find_missing_digests),
            outputs_present: get(&self.outputs_present),
            write_calls: get(&self.write_calls),
            written_bytes: get(&self.written_bytes),
            written_blobs: get(&self.written_blobs),
            retries: get(&self.retries),
        }
    }
}

impl Counts {
    pub fn upload_stats(&self) -> UploadStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let ms = |counter: &AtomicU64| get(counter) / 1000;
        UploadStats {
            find_missing_calls: get(&self.find_missing_calls),
            find_missing_call_ms: ms(&self.find_missing_us),
            uploads: get(&self.uploads),
            segments: get(&self.segments),
            put_range_calls: get(&self.put_range_calls),
            begin_ms: ms(&self.begin_us),
            put_range_ms: ms(&self.put_range_us),
            commit_ms: ms(&self.commit_us),
            permit_wait_ms: ms(&self.permit_wait_us),
            upload_ms: ms(&self.upload_us),
        }
    }
}

impl Drop for Counts {
    fn drop(&mut self) {
        if let Some(lease) = &self.lease {
            let stats = self.stats();
            tracing::info!(lease = %lease, "transfers: {stats}");
        }
    }
}

/// Adds `n` to `counter`.
pub(crate) fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// Adds the microseconds since `start` to `counter`.
pub(crate) fn add_since(counter: &AtomicU64, start: Instant) {
    add(
        counter,
        u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX),
    );
}

/// The process's transfers, now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineStats {
    /// Requests holding a permit now.
    pub requests_in_flight: usize,
    /// Bytes of in-memory reads holding the byte budget now.
    pub bytes_in_flight: usize,
    /// Distinct blobs being fetched into memory now (each shared by its callers).
    pub fetches: usize,
    /// FindMissing calls made.
    pub find_missing_calls: u64,
    /// Times the server answered FindMissing with UNIMPLEMENTED; after the first, the
    /// engine stops asking and uploads every blob.
    pub find_missing_unavailable: u64,
}
