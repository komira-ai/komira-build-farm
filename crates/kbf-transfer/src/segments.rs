//! Segment upload and segment-range reads over `kbf.worker.v1.Segments`
//! (`docs/design/write-path.md`, sections 4.1 to 4.3).
//!
//! **Upload.** The blobs of a [`Spool`] that the server lacks (asked through the
//! engine's FindMissing queue) are packed, in spool order, into segments of at most
//! [`MAX_SEGMENT_BYTES`] (a larger blob is a segment of its own), and the segments
//! into uploads of at most [`SEGMENT_UPLOAD_BYTES`] and [`MAX_UPLOAD_RECORDS`], both
//! under the smallest cap a server may run with, so every server admits every upload.
//! An upload commits or fails whole, and up to `--segment-uploads-in-flight` of a
//! spool's uploads are in flight together (the server checks and stages one while the
//! next ones send), so a failed one costs its own blobs, not the ones committed.
//! Each upload is one `Begin` (the footers), `PutRange` calls of at most
//! [`MAX_RANGE_BYTES`], `--segment-upload-streams` at once, cut from the spool file,
//! and one `Commit`. A failed upload is tried once more; then its blobs go one
//! ByteStream write each. A server that answers UNIMPLEMENTED (one without the
//! service) turns segment upload off for the life of the process, logged once.
//!
//! **Reads.** [`Scope::read_located`] asks where blobs are (`Locate`), coalesces blobs
//! of one object whose gaps are under [`COALESCE_GAP_BYTES`] into ranges of at most
//! [`MAX_RANGE_BYTES`], reads them (`ReadRange`), and checks every blob it cuts from
//! them against its digest. Blobs it could not locate or read are returned as missing,
//! for the caller to read one by one.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::{StreamExt as _, TryStreamExt as _, future, stream};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::segments_client::SegmentsClient;
use kbf_proto::worker::{
    BeginRequest, CommitRequest, LocateRequest, ObjectRef, PutRangeRequest, ReadRangeRequest,
    UploadSegment,
};
use kbf_segments::layout::{encode_footer, footer_len};
use kbf_segments::{IndexEntry, MAX_SEGMENT_BYTES};
use tonic::transport::Channel;
use tonic::{Code, Status};

use crate::cas::{Cas, CasError, Chunks, WRITE_CHUNK_BYTES, label};
use crate::engine::Scope;
use crate::spool::{Spool, Spooled, to_proto};
use crate::stats::add;
use crate::wire::MAX_MESSAGE_BYTES;

/// The most bytes one upload declares, a blob larger than this being an upload of its
/// own. Small, so that one failed upload (a PUT cut on a slow store, a lost commit) is
/// retried and falls back for at most this much, and the uploads committed before it
/// stay committed: a cold upload of 14,244 outputs (300 MB) on a store that was busy
/// lost every blob it had sent to one failed 1 GiB upload. Under kbf-server's smallest
/// `--segment-upload-max-bytes` (one 1 GiB blob and its footer), so every server admits
/// it. The server's PUT of a segment of this size is also one a slow store finishes
/// within `--s3-min-put-rate`.
pub const SEGMENT_UPLOAD_BYTES: u64 = 32 << 20;

/// The most records one upload holds: the server's limit.
pub const MAX_UPLOAD_RECORDS: usize = 16_384;

/// The most bytes one `PutRange` or `ReadRange` carries: the server's limit.
pub const MAX_RANGE_BYTES: u64 = 4 << 20;

/// Blobs of one object closer than this are read with the bytes between them.
pub const COALESCE_GAP_BYTES: u64 = 1 << 20;

/// One segment of an upload: spooled blobs at consecutive offsets of its record area.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Planned {
    /// Each blob and its offset in the segment, in spool order.
    pub records: Vec<(Spooled, u64)>,
    /// The record area's length.
    pub records_len: u64,
}

impl Planned {
    /// The segment's length if a `size`-byte blob were added.
    fn length_with(&self, size: u64) -> u64 {
        self.records_len + size + footer_len(self.records.len() as u64 + 1)
    }

    /// The segment's whole length: records, then footer.
    pub fn length(&self) -> u64 {
        self.records_len + footer_len(self.records.len() as u64)
    }

    fn push(&mut self, spooled: Spooled) {
        self.records.push((spooled, self.records_len));
        self.records_len += spooled.digest.size_bytes;
    }

    /// The footer, built as the server builds its own: sorted by digest.
    pub fn footer(&self) -> Vec<u8> {
        let mut entries: Vec<IndexEntry> = self
            .records
            .iter()
            .map(|(s, offset)| IndexEntry {
                digest: s.digest,
                offset: *offset,
                crc32c: s.crc32c,
            })
            .collect();
        entries.sort_by_key(|e| e.digest);
        encode_footer(self.records_len, &entries)
    }

    /// The record area in ranges of at most `max` bytes.
    pub fn ranges(&self, max: u64) -> Vec<(u64, u64)> {
        let max = max.max(1);
        (0..self.records_len.div_ceil(max))
            .map(|i| (i * max, ((i + 1) * max).min(self.records_len)))
            .collect()
    }

    /// Where the bytes `start..end` of the record area are in the spool, as
    /// (spool offset, length) pieces, adjacent pieces joined.
    pub fn pieces(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut pieces: Vec<(u64, u64)> = Vec::new();
        for (spooled, at) in &self.records {
            let (from, to) = (*at, at + spooled.digest.size_bytes);
            if to <= start || from >= end {
                continue;
            }
            let (lo, hi) = (from.max(start), to.min(end));
            let piece = (spooled.offset + (lo - from), hi - lo);
            match pieces.last_mut() {
                Some(last) if last.0 + last.1 == piece.0 => last.1 += piece.1,
                _ => pieces.push(piece),
            }
        }
        pieces
    }
}

/// Packs `records`, in order, into segments of at most `segment_limit` bytes (a blob
/// that does not fit an empty one gets a segment of its own) and the segments into
/// uploads of at most `upload_bytes` and `upload_records`.
pub(crate) fn plan(
    records: &[Spooled],
    segment_limit: u64,
    upload_bytes: u64,
    upload_records: usize,
) -> Vec<Vec<Planned>> {
    let mut uploads = Vec::new();
    let mut upload: Vec<Planned> = Vec::new();
    let (mut closed_len, mut count) = (0u64, 0usize);
    let mut segment = Planned::default();
    for &spooled in records {
        let size = spooled.digest.size_bytes;
        if !segment.records.is_empty() && segment.length_with(size) > segment_limit {
            closed_len += segment.length();
            upload.push(std::mem::take(&mut segment));
        }
        let over = closed_len + segment.length_with(size) > upload_bytes || count >= upload_records;
        if over && (count > 0) {
            if !segment.records.is_empty() {
                upload.push(std::mem::take(&mut segment));
            }
            uploads.push(std::mem::take(&mut upload));
            (closed_len, count) = (0, 0);
        }
        segment.push(spooled);
        count += 1;
    }
    if !segment.records.is_empty() {
        upload.push(segment);
    }
    if !upload.is_empty() {
        uploads.push(upload);
    }
    uploads
}

/// Why one attempt at an upload stopped.
#[derive(Debug)]
enum Failed {
    /// A call failed.
    Call(Status),
    /// The spool could not be read: nothing else would work either.
    Spool(std::io::Error),
}

/// What [`Scope::read_located`] found: every blob it read and checked, and the
/// digests it could not locate or read.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Located {
    /// The blobs read, each checked against its digest.
    pub blobs: Vec<(Digest, Vec<u8>)>,
    /// The digests to read another way.
    pub missing: Vec<Digest>,
}

/// One coalesced read: a byte range of one object and the blobs in it.
#[derive(Debug)]
struct Run {
    object: ObjectRef,
    start: u64,
    end: u64,
    blobs: Vec<(Digest, u64)>,
}

/// Coalesces located blobs (object, offset) into runs: blobs of one object, sorted by
/// offset, whose gaps are under `gap` and whose run stays within `max` bytes. A blob
/// longer than `max` is a run of its own.
fn coalesce(mut located: Vec<(ObjectRef, u64, Digest)>, gap: u64, max: u64) -> Vec<Run> {
    located.sort_by_key(|(o, offset, _)| (o.epoch, o.seq, *offset));
    let mut runs: Vec<Run> = Vec::new();
    for (object, offset, digest) in located {
        let end = offset + u64::try_from(digest.size_bytes).unwrap_or(0);
        // Sorted by offset within an object, so `offset >= run.start`.
        if let Some(run) = runs.last_mut()
            && run.object == object
            && offset.saturating_sub(run.end) < gap
            && end.max(run.end) - run.start <= max
        {
            run.end = run.end.max(end);
            run.blobs.push((digest, offset));
            continue;
        }
        runs.push(Run {
            object,
            start: offset,
            end,
            blobs: vec![(digest, offset)],
        });
    }
    runs
}

impl Scope {
    fn segments(&self) -> SegmentsClient<Channel> {
        self.inner.wire.segments()
    }

    /// Stores every blob of `spool` the server lacks, as [`Cas::put_spool`] says.
    pub(crate) async fn upload_spool(&self, spool: Arc<Spool>) -> Result<(), CasError> {
        let records = spool.records();
        let present = future::join_all(
            records
                .iter()
                .map(|r| async move { self.present(&to_proto(&r.digest)).await }),
        )
        .await;
        let missing: Vec<Spooled> = records
            .into_iter()
            .zip(present)
            .filter_map(|(r, present)| (!present).then_some(r))
            .collect();
        let uploads = plan(
            &missing,
            MAX_SEGMENT_BYTES,
            SEGMENT_UPLOAD_BYTES,
            MAX_UPLOAD_RECORDS,
        );
        // Owned items, as for the ranges below. The first failure stops the rest.
        stream::iter(uploads)
            .map(|upload| {
                let spool = Arc::clone(&spool);
                async move { self.upload_or_fall_back(&spool, &upload).await }
            })
            .buffer_unordered(self.inner.config.segment_uploads.max(1))
            .try_for_each(|()| future::ready(Ok(())))
            .await
    }

    /// One upload, tried twice, then blob by blob.
    async fn upload_or_fall_back(
        &self,
        spool: &Arc<Spool>,
        upload: &[Planned],
    ) -> Result<(), CasError> {
        let inner = &self.inner;
        for attempt in 0..2 {
            if inner.segments_off.load(Ordering::SeqCst) {
                break;
            }
            match self.send_upload(spool, upload).await {
                Ok(()) => return Ok(()),
                Err(Failed::Spool(e)) => {
                    return Err(CasError::Read("spool".to_owned(), e.to_string()));
                }
                Err(Failed::Call(status)) if status.code() == Code::Unimplemented => {
                    // Later uploads see the flag and do not ask; of uploads already in
                    // flight, only the one that turns it off says so.
                    if !inner.segments_off.swap(true, Ordering::SeqCst) {
                        tracing::warn!("Segments UNIMPLEMENTED; uploading outputs blob by blob");
                    }
                }
                Err(Failed::Call(status)) => {
                    tracing::warn!(attempt, "segment upload failed: {status}");
                }
            }
        }
        let records: Vec<Spooled> = upload
            .iter()
            .flat_map(|s| s.records.iter().map(|(r, _)| *r))
            .collect();
        self.send_each(spool, &records).await
    }

    /// One attempt: Begin, every range, Commit.
    async fn send_upload(&self, spool: &Spool, upload: &[Planned]) -> Result<(), Failed> {
        let mut client = self.segments();
        let begin = BeginRequest {
            segments: upload
                .iter()
                .map(|s| UploadSegment {
                    length: s.length(),
                    footer: s.footer(),
                })
                .collect(),
        };
        let id = client
            .begin(begin)
            .await
            .map_err(Failed::Call)?
            .into_inner()
            .upload;
        // Owned items: a stream of borrowed ones is not `Send` for every lifetime.
        let mut ranges: Vec<(usize, u64, u64)> = Vec::new();
        for (i, segment) in upload.iter().enumerate() {
            let cut = segment.ranges(MAX_RANGE_BYTES);
            ranges.extend(cut.into_iter().map(|(start, end)| (i, start, end)));
        }
        stream::iter(ranges)
            .map(|(i, start, end)| self.put_range(spool, id, i, &upload[i], start, end))
            .buffer_unordered(self.inner.config.segment_streams.max(1))
            .try_for_each(|()| future::ready(Ok(())))
            .await?;
        client
            .commit(CommitRequest { upload: id })
            .await
            .map_err(Failed::Call)?;
        let (blobs, bytes) = upload.iter().fold((0, 0), |(n, b), s| {
            (n + s.records.len() as u64, b + s.records_len)
        });
        add(&self.counts.written_blobs, blobs);
        add(&self.counts.written_bytes, bytes);
        Ok(())
    }

    /// Sends the bytes `start..end` of `segment`'s records, read from the spool.
    async fn put_range(
        &self,
        spool: &Spool,
        upload: u64,
        index: usize,
        segment: &Planned,
        start: u64,
        end: u64,
    ) -> Result<(), Failed> {
        // The permit first: a range read into memory waits for one no longer than the
        // permits allow, so the ranges buffered stay within the uploads' share however
        // many uploads are in flight.
        let _permit = self.inner.upload_request().await;
        let mut bytes = Vec::with_capacity(usize::try_from(end - start).unwrap_or(0));
        for (offset, len) in segment.pieces(start, end) {
            bytes.extend(spool.read(offset, len).await.map_err(Failed::Spool)?);
        }
        let request = PutRangeRequest {
            upload,
            segment: u32::try_from(index).unwrap_or(u32::MAX),
            offset: start,
            bytes,
        };
        add(&self.counts.write_calls, 1);
        self.segments()
            .put_range(request)
            .await
            .map(drop)
            .map_err(Failed::Call)
    }

    /// Sends each of `records` with its own ByteStream write, `--transfer-in-flight`
    /// at a time: the fallback. A blob larger than `--transfer-memory-blob-bytes` is
    /// streamed from the spool, not read whole.
    async fn send_each(&self, spool: &Arc<Spool>, records: &[Spooled]) -> Result<(), CasError> {
        let memory = self.inner.config.memory_blob_bytes;
        let chunk = self.inner.config.chunk_bytes;
        stream::iter(records.to_vec())
            .map(|r| async move {
                let digest = to_proto(&r.digest);
                if r.digest.size_bytes > memory {
                    let chunks = spool_chunks(Arc::clone(spool), r, chunk);
                    return self.send_chunks(&digest, chunks).await.map(drop);
                }
                let bytes = spool.read_record(&r).await?;
                self.send(&digest, &bytes).await.map(drop)
            })
            .buffer_unordered(self.inner.config.in_flight.max(1))
            .try_for_each(|()| future::ready(Ok(())))
            .await
    }

    /// Reads `digests` as ranges of the objects that hold them, as the module docs
    /// say. Each blob is checked against its digest.
    ///
    /// # Errors
    /// [`CasError::Corrupt`] for a blob whose bytes do not hash to its digest.
    pub async fn read_located(&self, digests: &[Digest]) -> Result<Located, CasError> {
        let mut found = Vec::new();
        let mut result = Located::default();
        for batch in digests.chunks(self.inner.config.find_missing_chunk.max(1)) {
            let request = LocateRequest {
                digests: batch.to_vec(),
            };
            let answer = {
                let _permit = self.inner.request().await;
                self.segments().locate(request).await
            };
            let Ok(answer) = answer else {
                result.missing.extend_from_slice(batch);
                continue;
            };
            // Each location names its digest: pair by digest, never by position, so an
            // answer short or out of order leaves the digests it does not place missing.
            let mut placed: HashMap<(String, i64), (ObjectRef, u64)> = HashMap::new();
            for location in answer.into_inner().locations {
                if let (Some(digest), Some(object)) = (location.digest, location.object) {
                    placed.insert((digest.hash, digest.size_bytes), (object, location.offset));
                }
            }
            for digest in batch {
                match placed.get(&(digest.hash.clone(), digest.size_bytes)) {
                    Some(&(object, offset)) => found.push((object, offset, digest.clone())),
                    None => result.missing.push(digest.clone()),
                }
            }
        }
        let runs = coalesce(found, COALESCE_GAP_BYTES, MAX_RANGE_BYTES);
        let read: Vec<(Run, Option<Vec<u8>>)> = stream::iter(runs)
            .map(|run| async move {
                let bytes = self.read_run(&run).await;
                (run, bytes)
            })
            .buffer_unordered(self.inner.config.segment_streams.max(1))
            .collect()
            .await;
        for (run, bytes) in read {
            let Some(bytes) = bytes else {
                result.missing.extend(run.blobs.into_iter().map(|(d, _)| d));
                continue;
            };
            for (digest, offset) in run.blobs {
                let at = usize::try_from(offset - run.start).unwrap_or(usize::MAX);
                let len = usize::try_from(digest.size_bytes).unwrap_or(usize::MAX);
                let blob = bytes
                    .get(at..at.saturating_add(len))
                    .unwrap_or_default()
                    .to_vec();
                let actual = crate::cas::digest_of(&blob);
                if actual != digest {
                    return Err(CasError::Corrupt(label(&digest), label(&actual)));
                }
                add(&self.counts.read_bytes, blob.len() as u64);
                result.blobs.push((digest, blob));
            }
        }
        Ok(result)
    }

    /// The bytes of `run`, in reads of at most [`MAX_RANGE_BYTES`]; `None` if any read
    /// failed (the server no longer serves the range).
    async fn read_run(&self, run: &Run) -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(usize::try_from(run.end - run.start).unwrap_or(0));
        let mut at = run.start;
        while at < run.end {
            let length = (run.end - at).min(MAX_RANGE_BYTES);
            let request = ReadRangeRequest {
                object: Some(run.object),
                offset: at,
                length,
            };
            let _permit = self.inner.request().await;
            add(&self.counts.read_calls, 1);
            let answer = self.segments().read_range(request).await.ok()?;
            bytes.extend(answer.into_inner().bytes);
            at += length;
        }
        Some(bytes)
    }
}

/// The `Segments` client over the engine's connection, decoding up to the front's
/// message limit.
pub(crate) fn client(channel: Channel) -> SegmentsClient<Channel> {
    SegmentsClient::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES)
}

/// The bytes of the spooled blob `r`, `chunk` bytes at a time, read when asked for.
pub(crate) fn spool_chunks(spool: Arc<Spool>, r: Spooled, chunk: usize) -> Chunks {
    let chunk = chunk.max(1) as u64;
    let end = r.offset + r.digest.size_bytes;
    Box::pin(stream::unfold(r.offset, move |at| {
        let spool = Arc::clone(&spool);
        async move {
            if at >= end {
                return None;
            }
            let len = chunk.min(end - at);
            Some((spool.read(at, len).await, at + len))
        }
    }))
}

/// Stores every blob of `spool` through `cas`'s own [`Cas::put`] (or
/// [`Cas::put_chunks`] for one over a megabyte, streamed from the spool),
/// `in_flight` at a time: what [`Cas::put_spool`] does for a store without segment
/// upload.
pub(crate) async fn put_each<C: Cas + ?Sized>(cas: &C, spool: &Arc<Spool>) -> Result<(), CasError> {
    stream::iter(spool.records())
        .map(|r| async move {
            let digest = to_proto(&r.digest);
            let stored = if r.digest.size_bytes > SPOOL_READ_WHOLE_BYTES {
                cas.put_chunks(
                    digest.clone(),
                    spool_chunks(Arc::clone(spool), r, WRITE_CHUNK_BYTES),
                )
                .await?
            } else {
                cas.put(spool.read_record(&r).await?).await?
            };
            if stored != digest {
                return Err(CasError::Corrupt(label(&digest), label(&stored)));
            }
            Ok(())
        })
        .buffer_unordered(cas.in_flight().max(1))
        .try_for_each(|()| future::ready(Ok(())))
        .await
}

/// The largest spooled blob [`put_each`] reads whole; a larger one is streamed.
const SPOOL_READ_WHOLE_BYTES: u64 = 1 << 20;

#[cfg(test)]
#[path = "segments_tests.rs"]
mod tests;
