//! One segment upload: what `Begin` declared, the ranges received, and the check that
//! turns received records into segments ready to store (`docs/design/write-path.md`,
//! section 4.1).

use std::collections::BTreeMap;

use bytes::Bytes;
use kbf_segments::layout::encode_footer;
use kbf_segments::{Footer, IndexEntry, RecordHasher};
use kbf_types::Digest;
use tonic::Status;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::cache::{BuiltSegment, TierRange};

/// One segment as `Begin` declared it, with its footer parsed and checked.
#[derive(Debug)]
pub(super) struct Declared {
    /// The whole segment's length: records, then footer.
    pub length: u64,
    /// The length of the record area (the footer's `index_offset`).
    pub records_len: u64,
    /// The index, in digest order.
    pub entries: Vec<IndexEntry>,
}

/// Parses and checks `Begin`'s segments: each footer must parse for its segment's
/// length, and its records must not overlap. Returns them with the upload's record
/// count.
///
/// # Errors
/// INVALID_ARGUMENT naming the segment and why.
pub(super) fn declare(segments: &[(u64, &[u8])]) -> Result<(Vec<Declared>, usize), Status> {
    if segments.is_empty() {
        return Err(Status::invalid_argument(
            "an upload needs at least one segment",
        ));
    }
    let mut declared = Vec::with_capacity(segments.len());
    let mut records = 0usize;
    for (i, &(length, footer)) in segments.iter().enumerate() {
        let parsed = Footer::parse(footer, length)
            .map_err(|e| Status::invalid_argument(format!("segment {i}: {e}")))?;
        // `parse` read the footer from the end of `footer` and checked that records
        // plus footer are `length`; anything in front of it is refused, not ignored.
        let footer_len = length - parsed.records_len();
        if footer.len() as u64 != footer_len {
            return Err(Status::invalid_argument(format!(
                "segment {i}: {} footer bytes sent for a {footer_len}-byte footer",
                footer.len()
            )));
        }
        let mut by_offset: Vec<&IndexEntry> = parsed.entries().iter().collect();
        by_offset.sort_by_key(|e| e.offset);
        for pair in by_offset.windows(2) {
            if pair[0].range().end > pair[1].offset {
                return Err(Status::invalid_argument(format!(
                    "segment {i}: records {} and {} overlap",
                    pair[0].digest, pair[1].digest
                )));
            }
        }
        records += parsed.entries().len();
        declared.push(Declared {
            length,
            records_len: parsed.records_len(),
            entries: parsed.entries().to_vec(),
        });
    }
    Ok((declared, records))
}

/// The writes of one segment's ranges to the write tier: how many are still running,
/// and the offsets of those that failed. Shared by the segment and the `PutRange` calls
/// that write, which answer here without the upload's lock.
#[derive(Debug, Default)]
pub(super) struct TierWrites {
    pending: AtomicUsize,
    failed: Mutex<Vec<u64>>,
}

impl TierWrites {
    /// Records that the write of the range at `offset` ended, and whether it wrote the
    /// bytes (`ok`): a failed one is forgotten by [`Receiving::forget_failed`].
    pub(super) fn done(&self, offset: u64, ok: bool) {
        if !ok {
            lock(&self.failed).push(offset);
        }
        self.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The ranges of one segment received so far, and the bytes they carried: written into
/// a buffer the segment's length (records, then room for the footer the server builds),
/// or, on a write tier that took the segment's records, written there as they arrive
/// (then nothing of them is held here).
///
/// Unless the upload is trusted, each record is checked as its bytes arrive: its
/// SHA-256 and CRC-32C advance over the bytes received contiguously from its start,
/// whichever order the ranges came in, so at the last range only the records it
/// completed have anything left to hash. A trusted upload (one on a write tier) hashes
/// nothing.
#[derive(Debug)]
pub(super) struct Receiving {
    /// The segment's bytes, unless they go to `tier`: then empty.
    buffer: Vec<u8>,
    /// The records' range on the write tier, when they are written there, and the
    /// writes to it.
    tier: Option<(Arc<TierRange>, Arc<TierWrites>)>,
    /// Received ranges, start to end; they never overlap.
    ranges: BTreeMap<u64, u64>,
    /// Indices into the declared entries, by record offset.
    by_offset: Vec<usize>,
    /// One per declared entry.
    records: Vec<Record>,
    /// The first record found not to hash to its digest.
    bad: Option<String>,
    /// The daemon's digests are trusted: nothing is hashed here (the flusher does it
    /// before the PUT, or the cache does if the tier does not take the segment).
    trusted: bool,
}

/// How far one record has been checked.
#[derive(Debug, Default)]
struct Record {
    /// The hash of the first `done` bytes, absent while a task advances it.
    hasher: Option<RecordHasher>,
    done: u64,
    /// A task is advancing this record.
    busy: bool,
    /// The record's CRC-32C once all its bytes hashed to its digest.
    crc: Option<u32>,
}

/// What a task holds while it hashes one step of a record.
pub(super) type Held = (RecordHasher, Vec<u8>);

/// What a `PutRange` did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Put {
    /// The bytes were written.
    Written,
    /// The same range was received before; nothing was written.
    Resent,
}

fn mismatch(entry: &IndexEntry, actual: &Digest) -> String {
    format!(
        "record at {} sent as {} hashes to {actual}",
        entry.offset, entry.digest
    )
}

/// Runs the records `claimed` by a `put` to the end of the bytes received so far.
/// `next` is [`Receiving::next`] under whatever lock holds the segment.
pub(super) fn advance<F>(claimed: &[usize], mut next: F)
where
    F: FnMut(usize, Option<(RecordHasher, usize)>) -> Option<Held>,
{
    for &i in claimed {
        let mut finished = None;
        while let Some((mut hasher, bytes)) = next(i, finished.take()) {
            hasher.update(&bytes);
            finished = Some((hasher, bytes.len()));
        }
    }
}

impl Receiving {
    /// A segment of `declared`'s shape. With `trusted`, nothing is hashed: the records
    /// are held as received (or written to `tier`, a range of the records' length, when
    /// there is one) and built into a segment [`BuiltSegment::verified`] is false for.
    pub(super) fn new(declared: &Declared, trusted: bool, tier: Option<TierRange>) -> Self {
        let mut by_offset: Vec<usize> = (0..declared.entries.len()).collect();
        by_offset.sort_by_key(|&i| declared.entries[i].offset);
        let mut records: Vec<Record> = declared.entries.iter().map(|_| Record::default()).collect();
        let mut bad = None;
        // An empty record has no bytes to wait for.
        for (entry, record) in declared.entries.iter().zip(&mut records) {
            if entry.digest.size_bytes == 0 && !trusted {
                let (actual, crc) = RecordHasher::new().finish();
                if actual == entry.digest {
                    record.crc = Some(crc);
                } else {
                    bad.get_or_insert_with(|| mismatch(entry, &actual));
                }
            }
        }
        let length = usize::try_from(declared.length).unwrap_or(usize::MAX);
        // Zeroed pages are allocated lazily: memory follows the bytes received.
        let buffer = if tier.is_some() {
            Vec::new()
        } else {
            vec![0; length]
        };
        Self {
            buffer,
            tier: tier.map(|range| (Arc::new(range), Arc::default())),
            ranges: BTreeMap::new(),
            by_offset,
            records,
            bad,
            trusted,
        }
    }

    /// Writes `bytes` at `offset` of the record area, unless they lie outside it or
    /// overlap a received range with other bounds. A range received before, with the
    /// same bounds, is acknowledged and nothing is written: its bytes may already have
    /// been read by the check.
    ///
    /// With a write, returns the records whose check can go on, claimed for the caller,
    /// who must run them with [`advance`].
    ///
    /// # Errors
    /// INVALID_ARGUMENT for an empty range, one outside the records, or an overlap.
    pub(super) fn put(
        &mut self,
        declared: &Declared,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(Put, Vec<usize>), Status> {
        self.forget_failed();
        let records_len = declared.records_len;
        let end = offset
            .checked_add(bytes.len() as u64)
            .filter(|&end| end <= records_len && end > offset)
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "range of {} bytes at {offset} is empty or outside the {records_len}-byte \
                     record area",
                    bytes.len()
                ))
            })?;
        if self.ranges.get(&offset) == Some(&end) {
            return Ok((Put::Resent, Vec::new()));
        }
        let before = self.ranges.range(..end).next_back();
        if let Some((&start, &stop)) = before
            && stop > offset
        {
            return Err(Status::invalid_argument(format!(
                "range {offset}..{end} overlaps the received range {start}..{stop}"
            )));
        }
        if let Some((_, writes)) = &self.tier {
            // The caller writes the bytes to the tier, then calls `TierWrites::done`.
            writes.pending.fetch_add(1, Ordering::SeqCst);
        } else {
            // `end <= records_len <= buffer.len()`, both fit in usize.
            let (from, to) = (offset as usize, end as usize);
            self.buffer[from..to].copy_from_slice(bytes);
        }
        self.ranges.insert(offset, end);
        let claimed = if self.trusted {
            Vec::new()
        } else {
            self.claim(declared, offset, end)
        };
        Ok((Put::Written, claimed))
    }

    /// The range on the write tier a written range's bytes go to, and its writes, when
    /// the records are written there: the caller of [`Receiving::put`] writes them, then
    /// calls [`TierWrites::done`].
    pub(super) fn tier(&self) -> Option<(Arc<TierRange>, Arc<TierWrites>)> {
        self.tier.clone()
    }

    /// Forgets the ranges whose write to the tier failed, so they are missing at the
    /// commit and can be sent again. Called before a range is recorded and before a
    /// commit looks at what was received.
    pub(super) fn forget_failed(&mut self) {
        if let Some((_, writes)) = &self.tier {
            for offset in std::mem::take(&mut *lock(&writes.failed)) {
                self.ranges.remove(&offset);
            }
        }
    }

    /// Claims the records overlapping `start..end` that no task is advancing and that
    /// have bytes to hash.
    fn claim(&mut self, declared: &Declared, start: u64, end: u64) -> Vec<usize> {
        let first = self
            .by_offset
            .partition_point(|&i| declared.entries[i].range().end <= start);
        let mut claimed = Vec::new();
        for k in first..self.by_offset.len() {
            let i = self.by_offset[k];
            let entry = &declared.entries[i];
            if entry.offset >= end {
                break;
            }
            let record = &self.records[i];
            // A finished or empty record has nothing past `done` to cover.
            if record.busy {
                continue;
            }
            let at = entry.offset + record.done;
            if self.covered_until(at, entry.range().end) > at {
                self.records[i].busy = true;
                claimed.push(i);
            }
        }
        claimed
    }

    /// How far the received bytes run unbroken from `from`, up to `cap`.
    fn covered_until(&self, from: u64, cap: u64) -> u64 {
        let mut at = from;
        while at < cap {
            match self.ranges.range(..=at).next_back() {
                Some((_, &stop)) if stop > at => at = stop.min(cap),
                _ => break,
            }
        }
        at
    }

    /// One step of the check of record `i`, which the caller claimed: takes back the
    /// hasher and length `finished` of the last step, and returns the hasher with the
    /// next bytes received unbroken, or `None` when there are none (the record is then
    /// no longer claimed) or the record is complete (its CRC-32C is kept if it hashed to
    /// its digest; if not, [`Receiving::refusal`] names it).
    pub(super) fn next(
        &mut self,
        declared: &Declared,
        i: usize,
        finished: Option<(RecordHasher, usize)>,
    ) -> Option<Held> {
        let entry = &declared.entries[i];
        let record = &mut self.records[i];
        if let Some((hasher, len)) = finished {
            record.done += len as u64;
            record.hasher = Some(hasher);
        }
        if record.done == entry.digest.size_bytes {
            record.busy = false;
            let (actual, crc) = record.hasher.take().unwrap_or_default().finish();
            if actual == entry.digest {
                record.crc = Some(crc);
            } else {
                self.bad.get_or_insert_with(|| mismatch(entry, &actual));
            }
            return None;
        }
        let from = entry.offset + record.done;
        let upto = self.covered_until(from, entry.range().end);
        let record = &mut self.records[i];
        if upto == from {
            record.busy = false;
            return None;
        }
        let hasher = record.hasher.take().unwrap_or_default();
        // Inside the record area, which `declare` checked lies inside the buffer.
        Some((hasher, self.buffer[from as usize..upto as usize].to_vec()))
    }

    /// The refusal for the first record found not to hash to its digest, if any.
    pub(super) fn refusal(&self) -> Option<Status> {
        self.bad.as_deref().map(Status::invalid_argument)
    }

    /// Why a commit cannot go on: a refused record, or a record some task is still
    /// checking or a range still being written to the tier (a client that commits
    /// before its ranges are answered).
    pub(super) fn not_ready(&self) -> Option<Status> {
        let writing = self
            .tier
            .as_ref()
            .is_some_and(|(_, w)| w.pending.load(Ordering::SeqCst) > 0);
        let busy = self.checking() | writing;
        self.refusal()
            .or_else(|| busy.then(|| Status::failed_precondition("ranges are still being checked")))
    }

    /// Whether a task is still checking some record.
    pub(super) fn checking(&self) -> bool {
        self.records.iter().any(|r| r.busy)
    }

    /// The first record of `declared` not wholly covered by received ranges.
    pub(super) fn first_missing<'a>(&self, declared: &'a Declared) -> Option<&'a Digest> {
        declared
            .entries
            .iter()
            .find(|e| self.covered_until(e.offset, e.range().end) < e.range().end)
            .map(|e| &e.digest)
    }

    /// Builds the segment to store from the received records, then a footer built here
    /// from the declared (digest, offset) entries and the CRC-32C the check computed.
    /// For a trusted upload nothing was computed: the footer carries the daemon's
    /// CRC-32C and the segment is not [`BuiltSegment::verified`], so the cache hashes
    /// it before any PUT.
    ///
    /// # Errors
    /// INVALID_ARGUMENT for a record that did not hash to its digest; FAILED_PRECONDITION
    /// for one not checked (a commit that raced the ranges it follows).
    pub(super) fn check(self, declared: &Declared) -> Result<BuiltSegment, Status> {
        if let Some(refused) = self.refusal() {
            return Err(refused);
        }
        let mut buffer = self.buffer;
        let mut entries = Vec::with_capacity(declared.entries.len());
        for (i, entry) in declared.entries.iter().enumerate() {
            let crc32c = if self.trusted {
                entry.crc32c
            } else {
                self.records[i].crc.ok_or_else(|| {
                    Status::failed_precondition(format!("record {} was not checked", entry.digest))
                })?
            };
            entries.push(IndexEntry {
                digest: entry.digest,
                offset: entry.offset,
                crc32c,
            });
        }
        if let Some((range, _)) = self.tier {
            // No range is being written (`not_ready`), so this is the last reference.
            let range = Arc::into_inner(range);
            return Ok(BuiltSegment {
                body: Bytes::new(),
                entries,
                verified: false,
                staged: range,
            });
        }
        let footer = encode_footer(declared.records_len, &entries);
        // The same entry count as the declared footer, so the same length: the buffer's
        // tail after the records.
        buffer[declared.records_len as usize..].copy_from_slice(&footer);
        Ok(BuiltSegment {
            body: Bytes::from(buffer),
            entries,
            verified: !self.trusted,
            staged: None,
        })
    }
}

#[cfg(test)]
#[path = "upload_tests.rs"]
mod tests;
