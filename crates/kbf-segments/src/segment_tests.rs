//! Tests for the segment writer, reader and footer checks.

use super::*;
use crate::layout::{FANOUT_LEN, MAGIC, TRAILER_LEN};

fn blobs() -> Vec<Vec<u8>> {
    let mut out = vec![Vec::new(), b"hello".to_vec(), vec![0xa5; 4096]];
    // Enough blobs that several share a first hash byte and exercise the fan-out.
    out.extend((0u32..600).map(|i| format!("blob number {i}").into_bytes()));
    out
}

fn segment_of(blobs: &[Vec<u8>]) -> Vec<u8> {
    let mut w = SegmentWriter::new(MAX_SEGMENT_BYTES);
    for b in blobs {
        w.push(b).unwrap();
    }
    w.finish()
}

/// The footer length of `segment`, from its trailer.
fn footer_len_of(segment: &[u8]) -> usize {
    usize::try_from(Footer::len_from_trailer(&segment[segment.len() - TRAILER_LEN..]).unwrap())
        .unwrap()
}

/// Rewrites the footer CRC so that a test can reach the checks behind it.
fn reseal(segment: &mut [u8]) {
    let n = segment.len();
    let start = n - footer_len_of(segment);
    let crc = crc32c(&segment[start..n - TRAILER_LEN + 16]);
    segment[n - 16..n - 12].copy_from_slice(&crc.to_le_bytes());
}

/// Catches: a writer or reader that loses, misplaces or mislabels a blob; a fan-out or
/// binary search that misses entries in some bucket; a lookup that ignores the size
/// half of the digest; a duplicate push that writes a second record.
#[test]
fn round_trip() {
    let blobs = blobs();
    let mut w = SegmentWriter::new(MAX_SEGMENT_BYTES);
    let digests: Vec<Digest> = blobs.iter().map(|b| w.push(b).unwrap()).collect();
    let len_before = w.len();
    assert_eq!(w.push(&blobs[1]).unwrap(), digests[1]);
    assert_eq!(w.len(), len_before, "a duplicate push adds nothing");
    let segment = w.finish();
    assert_eq!(
        segment.len() as u64,
        len_before,
        "len() predicts the finished size"
    );

    let r = SegmentReader::open(&segment).unwrap();
    assert_eq!(r.footer().entries().len(), blobs.len());
    for (blob, digest) in blobs.iter().zip(&digests) {
        assert_eq!(r.get(digest).unwrap(), Some(blob.as_slice()), "{digest}");
    }
    assert_eq!(r.get(&crate::sha256(b"absent")).unwrap(), None);
    let mut wrong_size = digests[1];
    wrong_size.size_bytes += 1;
    assert_eq!(r.get(&wrong_size).unwrap(), None);
}

/// Catches: a range reader that cannot find the footer from the trailer alone, or a
/// footer parser that needs the records.
#[test]
fn range_reads_need_only_the_footer() {
    let blobs = blobs();
    let segment = segment_of(&blobs);
    let footer_len = footer_len_of(&segment);
    let tail = &segment[segment.len() - footer_len..];
    let footer = Footer::parse(tail, segment.len() as u64).unwrap();
    assert_eq!(footer.records_len() as usize, segment.len() - footer_len);
    let entry = footer.find(&crate::sha256(&blobs[2])).unwrap();
    let range = entry.range();
    entry
        .verify(&segment[range.start as usize..range.end as usize])
        .unwrap();
}

/// Catches: a reader that skips the per-record CRC (the error would then come from the
/// digest check instead), or one whose damage to one record spills into others.
#[test]
fn flipped_byte_in_a_record_is_refused() {
    let blobs = blobs();
    let clean = segment_of(&blobs);
    let footer = SegmentReader::open(&clean).unwrap().footer().clone();
    for entry in footer.entries().iter().filter(|e| e.digest.size_bytes > 0) {
        let mut damaged = clean.clone();
        let at = entry.offset + entry.digest.size_bytes / 2;
        damaged[at as usize] ^= 0x01;
        let r = SegmentReader::open(&damaged).unwrap();
        assert_eq!(
            r.get(&entry.digest),
            Err(SegmentError::RecordCrc {
                digest: entry.digest
            })
        );
        let other = footer
            .entries()
            .iter()
            .find(|e| e.digest != entry.digest)
            .unwrap();
        assert!(r.get(&other.digest).unwrap().is_some());
    }
}

/// Catches: a reader that trusts the CRC alone. A record whose CRC matches but whose
/// bytes are another blob's (a CRC collision, or a writer bug) must still be refused.
#[test]
fn matching_crc_with_wrong_bytes_is_refused() {
    let entry = IndexEntry {
        digest: crate::sha256(b"right"),
        offset: 0,
        crc32c: crc32c(b"wrong"),
    };
    assert_eq!(
        entry.verify(b"wrong"),
        Err(SegmentError::RecordDigest {
            digest: entry.digest
        })
    );
    assert_eq!(
        entry.verify(b"righ"),
        Err(SegmentError::RecordLength {
            digest: entry.digest,
            actual: 4
        })
    );
}

/// Catches: a reader that accepts an object cut short anywhere in its footer, or a
/// range read that fetched fewer bytes than the footer needs.
#[test]
fn truncated_footer_is_refused() {
    let segment = segment_of(&blobs());
    let footer_len = footer_len_of(&segment);
    for cut in 1..=footer_len {
        let short = &segment[..segment.len() - cut];
        assert!(SegmentReader::open(short).is_err(), "cut {cut}");
    }
    let tail = &segment[segment.len() - footer_len + 1..];
    assert_eq!(
        Footer::parse(tail, segment.len() as u64),
        Err(SegmentError::Truncated {
            needed: footer_len as u64,
            available: footer_len as u64 - 1
        })
    );
    // Records cut from the front leave an intact footer that no longer fits the object.
    assert!(matches!(
        SegmentReader::open(&segment[1..]),
        Err(SegmentError::LengthMismatch { .. })
    ));
}

/// Catches: a reader that skips the magic or version check and so misreads another
/// object, or a future layout, as this one.
#[test]
fn wrong_magic_or_version_is_refused() {
    let segment = segment_of(&blobs());
    let n = segment.len();

    let mut bad_magic = segment.clone();
    bad_magic[n - 1] ^= 0xff;
    assert_eq!(
        SegmentReader::open(&bad_magic).err(),
        Some(SegmentError::BadMagic)
    );
    assert_eq!(&segment[n - 8..], &MAGIC);

    let mut next_version = segment.clone();
    next_version[n - 12..n - 8].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        SegmentReader::open(&next_version).err(),
        Some(SegmentError::UnsupportedVersion(2))
    );

    let mut other_function = segment;
    other_function[n - 20] = 2;
    assert_eq!(
        SegmentReader::open(&other_function).err(),
        Some(SegmentError::UnsupportedDigestFunction(2))
    );
}

/// Catches: a reader that skips the footer CRC, or (behind it) the sort and fan-out
/// checks that keep a lookup from missing a blob the segment holds.
#[test]
fn damaged_index_is_refused() {
    let segment = segment_of(&blobs());
    let n = segment.len();
    let index_start = n - footer_len_of(&segment) + FANOUT_LEN;

    let mut flipped = segment.clone();
    flipped[index_start + 3] ^= 0x10;
    assert!(matches!(
        SegmentReader::open(&flipped),
        Err(SegmentError::FooterCrc { .. })
    ));

    let mut swapped = segment.clone();
    let (a, b) = swapped[index_start..index_start + 2 * ENTRY_LEN].split_at_mut(ENTRY_LEN);
    a.swap_with_slice(b);
    reseal(&mut swapped);
    assert_eq!(
        SegmentReader::open(&swapped).err(),
        Some(SegmentError::IndexOrder { position: 1 })
    );

    let mut fanout = segment.clone();
    let fanout_start = index_start - FANOUT_LEN;
    fanout[fanout_start] = fanout[fanout_start].wrapping_add(1);
    reseal(&mut fanout);
    assert_eq!(
        SegmentReader::open(&fanout).err(),
        Some(SegmentError::FanOut { byte: 0 })
    );

    let mut out_of_bounds = segment;
    let offset_at = index_start + 40;
    out_of_bounds[offset_at..offset_at + 8].copy_from_slice(&(n as u64).to_le_bytes());
    reseal(&mut out_of_bounds);
    assert!(matches!(
        SegmentReader::open(&out_of_bounds),
        Err(SegmentError::RecordOutOfBounds { .. })
    ));
}

/// Catches: a writer that lets a segment exceed its limit, refuses a blob that fits
/// exactly (an off-by-one), or reports a blob that can never fit as merely `Full`.
#[test]
fn writer_respects_the_size_limit() {
    let limit = footer_len(2) + 100;
    let mut w = SegmentWriter::new(limit);
    assert!(w.is_empty());
    w.push(&[1u8; 60]).unwrap();
    assert_eq!(w.push(&[2u8; 41]), Err(WriteError::Full { size: 41 }));
    w.push(&[2u8; 40]).unwrap();
    assert_eq!(w.len(), limit);
    assert_eq!(w.push(&[]), Err(WriteError::Full { size: 0 }));
    assert_eq!(w.finish().len() as u64, limit);

    let one = footer_len(1);
    let mut w = SegmentWriter::new(one + 10);
    assert_eq!(
        w.push(&[0u8; 11]),
        Err(WriteError::TooLarge {
            size: 11,
            limit: one + 10
        })
    );
    w.push(&[0u8; 10]).unwrap();
}

/// Catches: an empty segment that cannot be written or read back, for example a
/// footer length or fan-out check that is off by one at zero entries.
#[test]
fn empty_segment_round_trips() {
    let segment = SegmentWriter::new(MAX_SEGMENT_BYTES).finish();
    assert_eq!(segment.len() as u64, footer_len(0));
    let r = SegmentReader::open(&segment).unwrap();
    assert!(r.footer().entries().is_empty());
    assert_eq!(r.get(&crate::sha256(b"")).unwrap(), None);
}
