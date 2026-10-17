//! Reads history back: a run by its id, an invocation's header and newest summary by
//! its key, and its records a bounded page of chunks at a time.
//!
//! A run id finds its own chunk with no per-run row: its time bytes give the UTC day,
//! the newest `ui/stream/<day>/<stream>/<first ms>` row at or before its time gives the
//! invocation and writer (so a fallback stream id reused after a restart still resolves
//! to its own life's chunk), the `ichunk` rows of that invocation and writer give the
//! chunks whose sequence range holds it, and a chunk's footer gives the block. That is
//! the index plus at most three range reads (footer, run block, failed-run block) per
//! candidate chunk: one, or two when events arrived out of order across a cut.
//!
//! Every answer holds only records whose instance `allow` accepts, and an invocation
//! is answered only when `allow` accepts every instance its header lists, and, for
//! [`Reader::invocation`], every instance its newest chunk's summary lists (an
//! invocation spanning instances is all-must-allow, ui-data.md section 8). A list
//! that cannot be checked whole fails closed: an empty one, or one the writer marked
//! truncated, answers no caller.
//!
//! Index calls go through the [`HistoryIndex`]'s offload, so the reader's `async fn`s
//! may be awaited on a runtime worker.
//!
//! The two lookups above (a stream's rows, an invocation's chunk rows) still use
//! `scan_prefix`: each prefix holds one stream's or one invocation's rows, a few. The
//! lists across builds (`listing`) read bounded ranges (suite clause C8).

use std::fmt;
use std::str::FromStr;

use bytes::Bytes;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_state::StoreError;
use prost::Message;

use crate::container::{self, BlockEntry, Footer, FormatError};
use crate::index::HistoryIndex;
use crate::keys::{Day, HistoryRoot, IndexRoot, InvKey, WriterId};
use crate::records::{
    ChunkBody, ChunkRecord, ChunkRow, ChunkSummary, FailedRun, HitRecord, InvState,
    InvocationHeader, RunRecord, SECTION_FAILED, SECTION_RUN, StreamRow, chunk_key,
};
use crate::run_id::RunId;
use crate::scoped::Scoped;

/// Why a read failed (as opposed to finding nothing).
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The index failed.
    #[error(transparent)]
    Index(#[from] StoreError),
    /// The object store failed.
    #[error(transparent)]
    Object(#[from] ObjectStoreError),
    /// An object is not a history object this build reads.
    #[error(transparent)]
    Format(#[from] FormatError),
    /// An index row does not decode.
    #[error("index row {key} does not decode: {message}")]
    Row {
        /// The row.
        key: String,
        /// Why.
        message: String,
    },
}

/// One run, as `GET /v1/runs/{id}` needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct RunView {
    /// The invocation it belongs to.
    pub inv: InvKey,
    /// The run record.
    pub run: RunRecord,
    /// Its failed-run section, if it failed and has one.
    pub failed: Option<FailedRun>,
}

/// Chunks a page of records reads at most, all at once: 32 GETs in parallel, at most
/// 32 chunks (about 8 MiB of raw records) resident for one answer.
pub const MAX_PAGE_CHUNKS: usize = 32;

/// An invocation's summary, as `GET /v1/invocations/{id}` needs it: the header row and
/// the newest chunk's summary section, one range read.
#[derive(Clone, Debug, PartialEq)]
pub struct InvocationView {
    /// The header row.
    pub header: InvocationHeader,
    /// Closed with no records dropped. An invocation left open (the frontend crashed or
    /// is still writing) or with a hole is not complete.
    pub complete: bool,
    /// When not complete: the time of the last chunk written, where the gap starts.
    pub gap_from_ms: Option<u64>,
    /// The summary section of the newest chunk (the one with the latest record): its
    /// writer's slowest runs, its digest table and the header as of it. `None` when the
    /// invocation has no chunk.
    pub newest: Option<ChunkSummary>,
}

/// Where a page of an invocation's records starts: after this chunk, named by its
/// writer and sequence (the order of `ichunk` rows). Opaque to callers, who pass back
/// what a page gave them; its text form is `<writer: 16 hex>-<seq: 8 hex>`, which
/// names no part of the index's keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChunkCursor {
    writer: WriterId,
    seq: u32,
}

impl fmt::Display for ChunkCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{:08x}", self.writer, self.seq)
    }
}

impl FromStr for ChunkCursor {
    type Err = CursorTextError;

    fn from_str(text: &str) -> Result<ChunkCursor, CursorTextError> {
        let parsed = text.split_once('-').and_then(|(w, s)| ids(w, s));
        parsed.ok_or_else(|| CursorTextError(text.to_owned()))
    }
}

/// Text that should have been a [`ChunkCursor`] and is not.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a records cursor (16 and 8 lowercase hex digits, joined by '-')")]
pub struct CursorTextError(String);

/// The chunk of writer `writer` and sequence `seq`, from their fixed-width lowercase
/// hex digits.
fn ids(writer: &str, seq: &str) -> Option<ChunkCursor> {
    if !(hex_digits(writer, 16) && hex_digits(seq, 8)) {
        return None;
    }
    // Sixteen and eight hex digits fit a `u64` and a `u32`: neither parse fails.
    Some(ChunkCursor {
        writer: WriterId(u64::from_str_radix(writer, 16).unwrap_or_default()),
        seq: u32::from_str_radix(seq, 16).unwrap_or_default(),
    })
}

/// Whether `text` is exactly `len` lowercase hex digits.
fn hex_digits(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One `ichunk` row: the chunk its key names and the row.
struct Chunk {
    at: ChunkCursor,
    row: ChunkRow,
}

/// One page of an invocation's records, chunk by chunk in `ichunk` row order (writer,
/// then sequence), each chunk's runs in sequence order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordsPage {
    /// Run records.
    pub runs: Vec<RunRecord>,
    /// Failed-run sections.
    pub failed: Vec<FailedRun>,
    /// Hit records.
    pub hits: Vec<HitRecord>,
    /// Where the next page starts; `None` after the last chunk.
    pub next: Option<ChunkCursor>,
}

/// Where a run is: its chunk's key, row and invocation.
struct Located {
    inv: InvKey,
    key: ObjectKey,
    row: ChunkRow,
}

/// Reads history through the index and the object store.
pub struct Reader<O> {
    objects: Scoped<O>,
    root: HistoryRoot,
    pub(crate) index: HistoryIndex,
    /// Index rows one list page scans at most.
    pub(crate) scan_budget: usize,
}

impl<O: ObjectStore> Reader<O> {
    /// A reader of the objects under `root` in `objects`, through the rows in `index`.
    #[must_use]
    pub fn new(objects: O, root: HistoryRoot, index: HistoryIndex) -> Reader<O> {
        Reader {
            objects: Scoped::history(objects, &root),
            root,
            index,
            scan_budget: crate::listing::MAX_SCAN,
        }
    }

    /// The same reader, scanning at most `rows` (at least 1) index rows for one page of
    /// a list, where the default is [`MAX_SCAN`](crate::MAX_SCAN).
    #[must_use]
    pub fn scanning_at_most(mut self, rows: usize) -> Reader<O> {
        self.scan_budget = rows.max(1);
        self
    }

    /// The run `id`, if history holds it and its instance is allowed.
    ///
    /// # Errors
    /// [`ReadError`] when the index or the object store fails, or a row or object does
    /// not decode.
    pub async fn run(
        &self,
        id: RunId,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<RunView>, ReadError> {
        // Events reach the writer out of order, so two chunks' sequence ranges may
        // overlap: try each chunk whose range holds the run, in order (one or two).
        for at in self.locate(id).await? {
            let range = footer_range(&at.row)?;
            let footer = decode_tail(&self.objects.get_range(&at.key, range).await?)?;
            let run_key = chunk_key(SECTION_RUN, id.seq());
            let block = self.block(&at.key, &footer, &run_key).await?;
            if let Some(run) = find_run(&block, id, allow) {
                return self.with_failed(at, &footer, run).await.map(Some);
            }
        }
        Ok(None)
    }

    /// `run`, found in chunk `at` whose footer is `footer`, with its failed-run section.
    async fn with_failed(
        &self,
        at: Located,
        footer: &Footer,
        run: RunRecord,
    ) -> Result<RunView, ReadError> {
        let id = run.run_id();
        let failed_key = chunk_key(SECTION_FAILED, id.seq());
        let failed = if run.failed() {
            let block = self.block(&at.key, footer, &failed_key).await?;
            find_failed(&block, id.seq())
        } else {
            None
        };
        Ok(RunView {
            inv: at.inv,
            run,
            failed,
        })
    }

    /// Invocation `inv`'s header and its newest chunk's summary, if history holds it and
    /// `allow` accepts every instance either lists: the index and one range read.
    ///
    /// # Errors
    /// As [`Reader::run`].
    pub async fn invocation(
        &self,
        inv: &InvKey,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<InvocationView>, ReadError> {
        let Some(header) = self.header(inv, allow).await? else {
            return Ok(None);
        };
        let rows = self
            .chunk_rows(self.index.root().ichunk_prefix(inv))
            .await?;
        // The newest chunk holds the latest record; of equals, the last row.
        let newest = rows.iter().max_by_key(|c| c.row.last_ms);
        let newest = match newest {
            Some(c) => {
                let object = chunk_object_key(&self.root, inv, c);
                let tail = self
                    .objects
                    .get_range(&object, footer_range(&c.row)?)
                    .await?;
                let summary = ChunkSummary::decode(decode_tail(&tail)?.summary.as_slice())
                    .map_err(|e| FormatError::decode("chunk summary", &e))?;
                // The newest chunk's row can land before the header written with it (a
                // failed transaction, a header held for its read), and its summary names
                // that chunk's instances, runs and digests. All must allow those too; a
                // summary with no header to check is refused.
                if !summary.header.as_ref().is_some_and(|h| all_allow(h, allow)) {
                    return Ok(None);
                }
                Some(summary)
            }
            None => None,
        };
        let complete = header.state() == InvState::Closed && header.dropped == 0;
        Ok(Some(InvocationView {
            gap_from_ms: (!complete).then_some(header.last_chunk_ms),
            complete,
            header,
            newest,
        }))
    }

    /// A page of invocation `inv`'s records: up to `chunks` chunks (at least 1, at most
    /// [`MAX_PAGE_CHUNKS`]) after `after`, read all at once, if history holds the
    /// invocation and `allow` accepts every instance it lists. Records of instances
    /// `allow` refuses are left out.
    ///
    /// # Errors
    /// As [`Reader::run`].
    pub async fn records(
        &self,
        inv: &InvKey,
        after: Option<&ChunkCursor>,
        chunks: usize,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<RecordsPage>, ReadError> {
        if self.header(inv, allow).await?.is_none() {
            return Ok(None);
        }
        let rows = self
            .chunk_rows(self.index.root().ichunk_prefix(inv))
            .await?;
        let start = after.map_or(0, |after| rows.partition_point(|c| c.at <= *after));
        let page = &rows[start..];
        let take = chunks.clamp(1, MAX_PAGE_CHUNKS).min(page.len());
        let reads = page[..take].iter().map(|c| async move {
            let object = chunk_object_key(&self.root, inv, c);
            let range = ByteRange::new(0, c.row.bytes);
            let range = range.ok_or(FormatError::Truncated("chunk row"))?;
            Ok::<_, ReadError>(self.objects.get_range(&object, range).await?)
        });
        let mut out = RecordsPage::default();
        for bytes in futures::future::try_join_all(reads).await? {
            add_records(&mut out, &bytes, allow)?;
        }
        out.next = (take < page.len()).then(|| page[take - 1].at);
        Ok(Some(out))
    }

    /// Invocation `inv`'s header row, if it exists and `allow` accepts every instance
    /// it lists (and it lists them all: see [`all_allow`]).
    async fn header(
        &self,
        inv: &InvKey,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<InvocationHeader>, ReadError> {
        let key = self.index.root().invid(inv);
        let Some(row) = self.index.get(key.clone()).await? else {
            return Ok(None);
        };
        let header = decode_row::<InvocationHeader>(&key, &row.value)?;
        Ok(all_allow(&header, allow).then_some(header))
    }

    /// The chunks that may hold run `id`, through the index: every chunk of its stream's
    /// invocation and writer whose sequence range holds it, in order. None when no row
    /// names it.
    async fn locate(&self, id: RunId) -> Result<Vec<Located>, ReadError> {
        let rows = self.index.root();
        let streams = self
            .index
            .scan_prefix(rows.stream_prefix(id.day(), id.stream()))
            .await?;
        // Rows sort by open time: the run's stream is the newest opened at or before it.
        let found = streams
            .iter()
            .rev()
            .find(|(k, _)| IndexRoot::stream_first_ms(k).is_some_and(|t| t <= id.time_ms()));
        let Some((key, value)) = found else {
            return Ok(Vec::new());
        };
        let stream = decode_row::<StreamRow>(key, &value.value)?;
        let inv = InvKey::parse(&stream.inv_key).map_err(|e| ReadError::Row {
            key: key.clone(),
            message: e.to_string(),
        })?;
        let writer = WriterId(stream.writer);
        let prefix = format!("{}{writer}/", rows.ichunk_prefix(&inv));
        let chunks = self.chunk_rows(prefix).await?.into_iter();
        Ok(chunks
            .filter(|c| c.row.holds(id.stream().0, id.seq()))
            .map(|c| Located {
                key: chunk_object_key(&self.root, &inv, &c),
                inv: inv.clone(),
                row: c.row,
            })
            .collect())
    }

    /// The `ichunk` rows under `prefix`, decoded, in key order (writer, then sequence:
    /// both are fixed-width hex). A key that names no writer and sequence is a broken
    /// row.
    async fn chunk_rows(&self, prefix: String) -> Result<Vec<Chunk>, ReadError> {
        self.index
            .scan_prefix(prefix)
            .await?
            .into_iter()
            .map(|(k, v)| {
                let row = decode_row::<ChunkRow>(&k, &v.value)?;
                // `ui/ichunk/<inv>/<writer>/<seq>`
                let mut parts = k.rsplit('/');
                let at = parts.next().zip(parts.next()).and_then(|(s, w)| ids(w, s));
                let at = at.ok_or_else(|| ReadError::Row {
                    key: k.clone(),
                    message: "the key names no writer and sequence".to_owned(),
                })?;
                Ok(Chunk { at, row })
            })
            .collect()
    }

    /// The records of the block of `footer` that holds `key`. A footer with no such
    /// block, for a key the index says the chunk holds, is a broken object.
    async fn block(
        &self,
        object: &ObjectKey,
        footer: &Footer,
        key: &[u8],
    ) -> Result<Vec<ChunkBody>, ReadError> {
        let entry = footer.block_for(key).cloned().unwrap_or_default();
        let stored = self.objects.get_range(object, block_range(&entry)?).await?;
        decode_records(&entry, &stored)
    }
}

/// The object key an `ichunk` row names: the writer and sequence are in the row's key,
/// the day in its value.
fn chunk_object_key(root: &HistoryRoot, inv: &InvKey, c: &Chunk) -> ObjectKey {
    root.chunk(Day::from_number(c.row.day), inv, c.at.writer, c.at.seq)
}

/// Whether `allow` accepts every instance `header` lists, and the list is whole: an
/// empty list (no record names an instance) or one the writer truncated cannot show
/// that every instance is allowed, so it fails closed.
pub(crate) fn all_allow(header: &InvocationHeader, allow: &dyn Fn(&str) -> bool) -> bool {
    !header.instances.is_empty()
        && !header.instances_truncated
        && header.instances.iter().all(|i| allow(i))
}

fn footer_range(row: &ChunkRow) -> Result<ByteRange, FormatError> {
    // A footer range of zero length (a broken row) reads one byte and fails to decode.
    let range = ByteRange::new(row.footer_offset, u64::from(row.footer_len).max(1));
    range.ok_or(FormatError::Truncated("footer range"))
}

fn block_range(entry: &BlockEntry) -> Result<ByteRange, FormatError> {
    ByteRange::new(entry.offset, u64::from(entry.len)).ok_or(FormatError::Truncated("block"))
}

fn decode_tail(tail: &Bytes) -> Result<Footer, ReadError> {
    Ok(container::decode_footer(tail)?.1)
}

pub(crate) fn decode_row<M: Message + Default>(key: &str, value: &[u8]) -> Result<M, ReadError> {
    M::decode(value).map_err(|e| ReadError::Row {
        key: key.to_owned(),
        message: e.to_string(),
    })
}

fn decode_records(entry: &BlockEntry, stored: &[u8]) -> Result<Vec<ChunkBody>, ReadError> {
    container::decode_block(entry, stored)?
        .iter()
        .map(|r| decode_record(r))
        .collect()
}

fn decode_record(bytes: &[u8]) -> Result<ChunkBody, ReadError> {
    let record = ChunkRecord::decode(bytes).map_err(|e| FormatError::Decode {
        what: "chunk record",
        message: e.to_string(),
    })?;
    Ok(record.body.ok_or(FormatError::Decode {
        what: "chunk record",
        message: "empty record".to_owned(),
    })?)
}

fn find_run(block: &[ChunkBody], id: RunId, allow: &dyn Fn(&str) -> bool) -> Option<RunRecord> {
    block.iter().find_map(|b| match b {
        ChunkBody::Run(r) if r.run_id() == id && allow(&r.instance) => Some(r.clone()),
        _ => None,
    })
}

fn find_failed(block: &[ChunkBody], seq: u32) -> Option<FailedRun> {
    block.iter().find_map(|b| match b {
        ChunkBody::Failed(f) if f.seq == seq => Some(f.clone()),
        _ => None,
    })
}

/// Adds the allowed records of one whole chunk to `view`.
fn add_records(
    view: &mut RecordsPage,
    object: &[u8],
    allow: &dyn Fn(&str) -> bool,
) -> Result<(), ReadError> {
    let decoded = container::decode(object)?;
    for record in &decoded.records {
        let body = decode_record(record)?;
        if !allow(body.instance()) {
            continue;
        }
        match body {
            ChunkBody::Run(r) => view.runs.push(r),
            ChunkBody::Failed(f) => view.failed.push(f),
            ChunkBody::Hit(h) => view.hits.push(h),
        }
    }
    Ok(())
}
