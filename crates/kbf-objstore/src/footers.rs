//! The index of every segment under a prefix, read back from the segments' own footers:
//! what a storage node rebuilds its index from when it has none for objects already in
//! the cold store (a wiped node, a new owner after a join).
//!
//! [`read_footers`] lists the prefix page by page and, for each object, reads the tail
//! that holds the footer: one ranged read of [`FooterReads::tail`] bytes, and a second
//! for the rest of a footer longer than that. It never reads the records. Each object
//! comes back as a [`FooterRead`]: a segment's checked [`Footer`] (whose
//! [`entries`](Footer::entries) are the digest, offset and length of every record), or
//! the reason it was skipped. An object that is too short, torn, foreign or gone is
//! reported and skipped; only a listing failure ends the stream. Nothing here changes
//! an index: the caller decides what the entries mean.

use std::num::{NonZeroU64, NonZeroUsize};

use bytes::{Bytes, BytesMut};
use futures::{Stream, TryStreamExt as _};
use kbf_segments::layout::TRAILER_LEN;
use kbf_segments::{Footer, MAX_SEGMENT_BYTES, SegmentError};

use crate::{
    ByteRange, KeyPrefix, ListToken, ObjectInfo, ObjectKey, ObjectStore, ObjectStoreError, PageSize,
};

/// The longest footer read: a longer one is not a segment kbf wrote (no segment is
/// longer than [`MAX_SEGMENT_BYTES`]), and reading it would hold that many bytes.
const MAX_FOOTER_BYTES: u64 = MAX_SEGMENT_BYTES;

/// How [`read_footers`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FooterReads {
    /// How many objects' footers are read at once.
    pub concurrency: NonZeroUsize,
    /// How many keys each listing call asks for.
    pub page: PageSize,
    /// How many bytes from the end the first read of each object fetches (at least the
    /// trailer's 32): a footer that fits needs no second read. A footer is 1056 bytes
    /// plus 52 per record, so the default 64 KiB holds about 1240 records.
    pub tail: NonZeroU64,
}

impl Default for FooterReads {
    fn default() -> Self {
        Self {
            concurrency: NonZeroUsize::new(16).expect("16 is not zero"),
            page: PageSize::MAX,
            tail: NonZeroU64::new(64 << 10).expect("64 KiB is not zero"),
        }
    }
}

/// What reading one listed object's footer gave.
#[derive(Clone, Debug)]
pub enum FooterRead {
    /// A segment, with its checked footer (boxed: a footer holds a 1 KiB fan-out).
    Segment(Box<SegmentFooter>),
    /// Not read as a segment, and why.
    Skipped(Skipped),
}

/// A segment's footer, read from its tail.
#[derive(Clone, Debug)]
pub struct SegmentFooter {
    /// The segment object.
    pub key: ObjectKey,
    /// Its length, as listed.
    pub size: u64,
    /// The checked footer: [`Footer::entries`] holds every record's digest (whose size
    /// is the record's length), offset and CRC-32C.
    pub footer: Footer,
    /// How many bytes were read to get it: the footer, and at most
    /// [`FooterReads::tail`] bytes before it.
    pub bytes_read: u64,
}

/// A listed object that was not read as a segment.
#[derive(Clone, Debug)]
pub struct Skipped {
    /// The object.
    pub key: ObjectKey,
    /// Its length, as listed.
    pub size: u64,
    /// Why it was skipped.
    pub why: SkipReason,
}

/// Why a listed object was skipped.
#[derive(Clone, Debug, thiserror::Error)]
pub enum SkipReason {
    /// Shorter than a segment trailer: nothing was read.
    #[error("shorter than a segment trailer")]
    Short,
    /// Its tail is not a segment footer kbf reads: a foreign object, a torn or damaged
    /// footer, or a footer longer than any segment kbf writes.
    #[error("not a segment: {0}")]
    NotASegment(SegmentError),
    /// Deleted between the listing and the read.
    #[error("deleted since it was listed")]
    Gone,
    /// The read failed. The object may well be a segment: a caller that must not miss
    /// one reads it again.
    #[error("read failed: {0}")]
    Read(ObjectStoreError),
}

/// Running totals over a [`read_footers`] stream ([`Totals::add`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Objects listed and read (or skipped).
    pub objects: u64,
    /// Of those, segments.
    pub segments: u64,
    /// Records indexed by those segments.
    pub entries: u64,
    /// Bytes of those records.
    pub record_bytes: u64,
    /// Objects skipped.
    pub skipped: u64,
    /// Bytes read from segments to get their footers.
    pub bytes_read: u64,
}

impl Totals {
    /// Counts `read`.
    pub fn add(&mut self, read: &FooterRead) {
        self.objects += 1;
        match read {
            FooterRead::Segment(s) => {
                self.segments += 1;
                self.entries += s.footer.entries().len() as u64;
                self.record_bytes += s.footer.records_len();
                self.bytes_read += s.bytes_read;
            }
            FooterRead::Skipped(_) => self.skipped += 1,
        }
    }
}

/// Every object under `prefix` with its footer read ([`FooterRead`]), in ascending key
/// order, starting where `from` says (or at the first key): at most
/// [`FooterReads::concurrency`] reads run at a time, and a slow read holds back the
/// results after it, so that when the stream has yielded an object, every object before
/// it has been yielded too. A caller that stops after a key resumes with
/// [`ListToken::after`] that key, as [`listing::objects`](crate::listing::objects) says.
/// The stream ends after the last page, or with the error of a listing call that failed
/// (or of a listing that broke the order that function checks).
pub fn read_footers<'a, S: ObjectStore>(
    store: &'a S,
    prefix: &'a KeyPrefix,
    from: Option<ListToken>,
    reads: FooterReads,
) -> impl Stream<Item = Result<FooterRead, ObjectStoreError>> + Send + 'a {
    crate::listing::objects(store, prefix, from, reads.page)
        .map_ok(move |object| async move { Ok(read_one(store, object, reads.tail).await) })
        .try_buffered(reads.concurrency.get())
}

/// Reads the footer of `object`.
async fn read_one<S: ObjectStore>(store: &S, object: ObjectInfo, tail: NonZeroU64) -> FooterRead {
    let ObjectInfo { key, size } = object;
    match read_footer(store, &key, size, tail).await {
        Ok((footer, bytes_read)) => FooterRead::Segment(Box::new(SegmentFooter {
            key,
            size,
            footer,
            bytes_read,
        })),
        Err(why) => FooterRead::Skipped(Skipped { key, size, why }),
    }
}

/// The footer of the `size`-byte object `key`, and how many bytes were read for it.
async fn read_footer<S: ObjectStore>(
    store: &S,
    key: &ObjectKey,
    size: u64,
    tail: NonZeroU64,
) -> Result<(Footer, u64), SkipReason> {
    if size < TRAILER_LEN as u64 {
        return Err(SkipReason::Short);
    }
    let first = size.min(tail.get().max(TRAILER_LEN as u64));
    let mut bytes = read(store, key, size - first, first).await?;
    let footer_len = Footer::len_from_trailer(&bytes).map_err(SkipReason::NotASegment)?;
    let have = bytes.len() as u64;
    if footer_len > have && footer_len <= size.min(MAX_FOOTER_BYTES) {
        // The rest of the footer, just before the tail already read.
        let rest = read(store, key, size - footer_len, footer_len - have).await?;
        let mut whole = BytesMut::with_capacity(rest.len() + bytes.len());
        whole.extend_from_slice(&rest);
        whole.extend_from_slice(&bytes);
        bytes = whole.freeze();
    }
    // A footer longer than the object or than any segment is still short of its
    // length here, and `parse` refuses it as truncated.
    let footer = Footer::parse(&bytes, size).map_err(SkipReason::NotASegment)?;
    Ok((footer, bytes.len() as u64))
}

/// Reads `len` bytes of `key` from `offset`.
async fn read<S: ObjectStore>(
    store: &S,
    key: &ObjectKey,
    offset: u64,
    len: u64,
) -> Result<Bytes, SkipReason> {
    // The callers ask for at least one byte, ending at or before the object's end, so
    // this range always exists; a store is never asked for an empty one.
    let range = ByteRange::new(offset, len).ok_or(SkipReason::Short)?;
    match store.get_range(key, range).await {
        Ok(bytes) => Ok(bytes),
        Err(ObjectStoreError::NotFound(_)) => Err(SkipReason::Gone),
        Err(e) => Err(SkipReason::Read(e)),
    }
}
