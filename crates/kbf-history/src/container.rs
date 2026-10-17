//! The one container format every history family is stored in.
//!
//! ```text
//! header : magic "KBFH", version u16, family u8, writer u64, day u32, seq u32,
//!          base time ms u64                                          (31 bytes)
//! blocks : each a zstd frame of length-prefixed protobuf records, sorted by the
//!          family's key, cut at BLOCK_RAW_BYTES of raw records
//! footer : protobuf `Footer`: the block index (first key, offset, length, record
//!          count, CRC-32C of the stored block) and the family summary
//! trailer: footer offset u64, footer length u32, CRC-32C of the footer u32,
//!          version u16, family u8, reserved u8, magic "KBFH"         (24 bytes)
//! ```
//!
//! Integers are big-endian. The index row of an object keeps the footer's offset and
//! its length with the trailer, so one exact range read returns both and the footer is
//! checked against the trailer's CRC. A reader refuses a version it does not know, in
//! the header and in the trailer, rather than misread a newer layout.

use std::io::Read;

use prost::Message;

use crate::keys::{Day, Family, WriterId};

/// The magic at both ends of every history object.
pub const MAGIC: [u8; 4] = *b"KBFH";

/// The container version this build writes and reads.
pub const VERSION: u16 = 1;

/// The header's length.
pub const HEADER_LEN: usize = 31;

/// The trailer's length.
pub const TRAILER_LEN: usize = 24;

/// Raw record bytes at which a block is cut. A single larger record gets a block of its
/// own.
pub const BLOCK_RAW_BYTES: usize = 64 * 1024;

/// The most raw bytes a reader inflates one block to: a block is cut at
/// [`BLOCK_RAW_BYTES`] plus one record, and no record is near this.
pub const MAX_BLOCK_RAW_BYTES: usize = 4 * 1024 * 1024;

/// What the header of an object says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The family.
    pub family: Family,
    /// The writer.
    pub writer: WriterId,
    /// The UTC day the object was written (the day in its key).
    pub day: Day,
    /// The writer's sequence for this object.
    pub seq: u32,
    /// The base time of the object's records, milliseconds.
    pub base_ms: u64,
}

/// An entry of the block index.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct BlockEntry {
    /// The first record's key.
    #[prost(bytes = "vec", tag = "1")]
    pub first_key: Vec<u8>,
    /// Where the stored block starts in the object.
    #[prost(uint64, tag = "2")]
    pub offset: u64,
    /// The stored (compressed) length.
    #[prost(uint32, tag = "3")]
    pub len: u32,
    /// Records in the block.
    #[prost(uint32, tag = "4")]
    pub records: u32,
    /// CRC-32C of the stored bytes.
    #[prost(uint32, tag = "5")]
    pub crc32c: u32,
}

/// The footer: the block index and the family summary.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Footer {
    /// The blocks, in key order.
    #[prost(message, repeated, tag = "1")]
    pub blocks: Vec<BlockEntry>,
    /// The family's summary message, encoded.
    #[prost(bytes = "vec", tag = "2")]
    pub summary: Vec<u8>,
}

impl Footer {
    /// The block that holds `key` if any block does: the last whose first key is at or
    /// before it.
    #[must_use]
    pub fn block_for(&self, key: &[u8]) -> Option<&BlockEntry> {
        let after = self
            .blocks
            .partition_point(|b| b.first_key.as_slice() <= key);
        after.checked_sub(1).map(|i| &self.blocks[i])
    }
}

/// An encoded object and where its footer is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    /// The whole object.
    pub bytes: Vec<u8>,
    /// Where the footer starts.
    pub footer_offset: u64,
    /// The footer's length with the trailer.
    pub footer_len: u32,
}

/// Why bytes are not a history object this build reads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// Shorter than a header and a trailer, or a range runs past the end.
    #[error("history object truncated: {0}")]
    Truncated(&'static str),
    /// The magic is missing.
    #[error("not a history object: bad magic in the {0}")]
    BadMagic(&'static str),
    /// A version this build does not read.
    #[error("history object version {found} in the {at}; this build reads version {VERSION}")]
    Version {
        /// The version found.
        found: u16,
        /// Header or trailer.
        at: &'static str,
    },
    /// A family byte this build does not know.
    #[error("history object family {0} is unknown to this build")]
    Family(u8),
    /// Two parts of the object disagree.
    #[error("history object parts disagree on the {0}")]
    Mismatch(&'static str),
    /// A checksum did not match.
    #[error("history object {0} failed its CRC-32C")]
    Crc(&'static str),
    /// A record, the footer or a summary did not decode.
    #[error("history object {what} does not decode: {message}")]
    Decode {
        /// What was decoded.
        what: &'static str,
        /// The decoder's message.
        message: String,
    },
    /// A block does not inflate, or inflates past [`MAX_BLOCK_RAW_BYTES`].
    #[error("history object block: {0}")]
    Block(String),
}

impl FormatError {
    pub(crate) fn decode(what: &'static str, e: &prost::DecodeError) -> FormatError {
        FormatError::Decode {
            what,
            message: e.to_string(),
        }
    }
}

/// Encodes an object of `header`'s family from `records` (each a sort key and an
/// encoded protobuf message; sorted here by key, stably) and the family `summary`.
#[must_use]
pub fn encode(header: &Header, mut records: Vec<(Vec<u8>, Vec<u8>)>, summary: Vec<u8>) -> Encoded {
    records.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.push(header.family.number());
    out.extend_from_slice(&header.writer.0.to_be_bytes());
    out.extend_from_slice(&header.day.number().to_be_bytes());
    out.extend_from_slice(&header.seq.to_be_bytes());
    out.extend_from_slice(&header.base_ms.to_be_bytes());

    let mut blocks = Vec::new();
    let mut raw = Vec::new();
    let mut first_key: Option<Vec<u8>> = None;
    let mut count = 0u32;
    for (key, record) in records {
        if first_key.is_none() {
            first_key = Some(key);
        }
        prost::encoding::encode_varint(record.len() as u64, &mut raw);
        raw.extend_from_slice(&record);
        count += 1;
        if raw.len() >= BLOCK_RAW_BYTES {
            blocks.push(flush_block(&mut out, &mut raw, first_key.take(), count));
            count = 0;
        }
    }
    if count > 0 {
        blocks.push(flush_block(&mut out, &mut raw, first_key.take(), count));
    }

    let footer = Footer { blocks, summary }.encode_to_vec();
    let footer_offset = out.len() as u64;
    out.extend_from_slice(&footer);
    out.extend_from_slice(&footer_offset.to_be_bytes());
    // A footer is a block index and a summary, far below 4 GiB.
    out.extend_from_slice(&(footer.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32c::crc32c(&footer).to_be_bytes());
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.push(header.family.number());
    out.push(0);
    out.extend_from_slice(&MAGIC);
    Encoded {
        footer_len: (footer.len() + TRAILER_LEN) as u32,
        bytes: out,
        footer_offset,
    }
}

fn flush_block(
    out: &mut Vec<u8>,
    raw: &mut Vec<u8>,
    first_key: Option<Vec<u8>>,
    records: u32,
) -> BlockEntry {
    let stored = ruzstd::encoding::compress_to_vec(
        raw.as_slice(),
        ruzstd::encoding::CompressionLevel::Fastest,
    );
    raw.clear();
    let entry = BlockEntry {
        first_key: first_key.unwrap_or_default(),
        offset: out.len() as u64,
        // A block is 64 KiB of raw records plus one record, compressed.
        len: stored.len() as u32,
        records,
        crc32c: crc32c::crc32c(&stored),
    };
    out.extend_from_slice(&stored);
    entry
}

/// Reads the header at the start of `bytes`.
///
/// # Errors
/// [`FormatError`] when the bytes are short, the magic or family is wrong, or the
/// version is not [`VERSION`].
pub fn decode_header(bytes: &[u8]) -> Result<Header, FormatError> {
    let h = bytes
        .get(..HEADER_LEN)
        .ok_or(FormatError::Truncated("header"))?;
    if h[..4] != MAGIC {
        return Err(FormatError::BadMagic("header"));
    }
    let version = u16::from_be_bytes([h[4], h[5]]);
    if version != VERSION {
        return Err(FormatError::Version {
            found: version,
            at: "header",
        });
    }
    let family = Family::from_number(h[6]).ok_or(FormatError::Family(h[6]))?;
    Ok(Header {
        family,
        writer: WriterId(be_u64(&h[7..15])),
        day: Day::from_number(be_u32(&h[15..19])),
        seq: be_u32(&h[19..23]),
        base_ms: be_u64(&h[23..31]),
    })
}

/// Reads the footer from `tail`: the bytes from the footer's offset to the end of the
/// object (the range an index row names), checked against the trailer.
///
/// # Errors
/// [`FormatError`] when the trailer is short or wrong, the version is unknown, the
/// footer fails its CRC or does not decode.
pub fn decode_footer(tail: &[u8]) -> Result<(Family, Footer), FormatError> {
    let at = tail
        .len()
        .checked_sub(TRAILER_LEN)
        .ok_or(FormatError::Truncated("trailer"))?;
    let t = &tail[at..];
    if t[20..24] != MAGIC {
        return Err(FormatError::BadMagic("trailer"));
    }
    let version = u16::from_be_bytes([t[16], t[17]]);
    if version != VERSION {
        return Err(FormatError::Version {
            found: version,
            at: "trailer",
        });
    }
    let family = Family::from_number(t[18]).ok_or(FormatError::Family(t[18]))?;
    let len = be_u32(&t[8..12]) as usize;
    if len != at {
        return Err(FormatError::Truncated("footer length"));
    }
    let footer = &tail[..at];
    if crc32c::crc32c(footer) != be_u32(&t[12..16]) {
        return Err(FormatError::Crc("footer"));
    }
    let footer = Footer::decode(footer).map_err(|e| FormatError::decode("footer", &e))?;
    Ok((family, footer))
}

/// The footer of a whole object, with its offset checked against the trailer.
///
/// # Errors
/// As [`decode_footer`], and when the trailer's offset does not end at the trailer.
pub fn footer_of(object: &[u8]) -> Result<(Family, Footer), FormatError> {
    let at = object
        .len()
        .checked_sub(TRAILER_LEN)
        .ok_or(FormatError::Truncated("trailer"))?;
    let offset = usize::try_from(be_u64(&object[at..at + 8])).unwrap_or(usize::MAX);
    let tail = object
        .get(offset..)
        .ok_or(FormatError::Truncated("footer offset"))?;
    decode_footer(tail)
}

/// The records of one stored block (`stored`, as read at `entry`'s range), each an
/// encoded protobuf message.
///
/// # Errors
/// [`FormatError`] when the block fails its CRC, does not inflate within
/// [`MAX_BLOCK_RAW_BYTES`], or its records do not split as the entry says.
pub fn decode_block(entry: &BlockEntry, stored: &[u8]) -> Result<Vec<Vec<u8>>, FormatError> {
    if stored.len() != entry.len as usize || crc32c::crc32c(stored) != entry.crc32c {
        return Err(FormatError::Crc("block"));
    }
    let mut source = stored;
    let decoder = ruzstd::decoding::StreamingDecoder::new(&mut source)
        .map_err(|e| FormatError::Block(e.to_string()))?;
    let mut raw = Vec::new();
    decoder
        .take(MAX_BLOCK_RAW_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| FormatError::Block(e.to_string()))?;
    if raw.len() > MAX_BLOCK_RAW_BYTES {
        return Err(FormatError::Block(format!(
            "inflates past {MAX_BLOCK_RAW_BYTES} bytes"
        )));
    }
    let mut rest = raw.as_slice();
    // The count is the footer's claim, checked only after the split: reserve no more
    // than the block can hold (each record takes at least its length byte).
    let mut records = Vec::with_capacity((entry.records as usize).min(raw.len()));
    while !rest.is_empty() {
        let len = prost::encoding::decode_varint(&mut rest)
            .map_err(|e| FormatError::decode("record length", &e))?;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len > rest.len() {
            return Err(FormatError::Truncated("record"));
        }
        let (record, after) = rest.split_at(len);
        records.push(record.to_vec());
        rest = after;
    }
    if records.len() != entry.records as usize {
        return Err(FormatError::Mismatch("block record count"));
    }
    Ok(records)
}

/// A whole object, decoded: its header, footer and every record in key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The header.
    pub header: Header,
    /// The footer.
    pub footer: Footer,
    /// Every record, in key order.
    pub records: Vec<Vec<u8>>,
}

/// Decodes a whole object.
///
/// # Errors
/// [`FormatError`] as each part's decoder says, and when a block lies outside the
/// object.
pub fn decode(object: &[u8]) -> Result<Decoded, FormatError> {
    let header = decode_header(object)?;
    // Each end's family byte is checked on its own; one family exists, so they agree.
    let (_, footer) = footer_of(object)?;
    let mut records = Vec::new();
    for entry in &footer.blocks {
        let stored = block_bytes(object, entry)?;
        records.extend(decode_block(entry, stored)?);
    }
    Ok(Decoded {
        header,
        footer,
        records,
    })
}

/// The stored bytes of `entry` in a whole object.
///
/// # Errors
/// [`FormatError::Truncated`] when the block lies outside the object.
pub fn block_bytes<'a>(object: &'a [u8], entry: &BlockEntry) -> Result<&'a [u8], FormatError> {
    let start = usize::try_from(entry.offset).unwrap_or(usize::MAX);
    start
        .checked_add(entry.len as usize)
        .and_then(|end| object.get(start..end))
        .ok_or(FormatError::Truncated("block"))
}

fn be_u32(b: &[u8]) -> u32 {
    let mut a = [0; 4];
    a.copy_from_slice(b);
    u32::from_be_bytes(a)
}

fn be_u64(b: &[u8]) -> u64 {
    let mut a = [0; 8];
    a.copy_from_slice(b);
    u64::from_be_bytes(a)
}

#[cfg(test)]
#[path = "container_tests.rs"]
mod tests;
