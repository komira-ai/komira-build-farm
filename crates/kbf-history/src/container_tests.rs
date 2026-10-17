use super::*;

fn header() -> Header {
    Header {
        family: Family::InvocationChunk,
        writer: WriterId(0x0102_0304_0506_0708),
        day: Day::from_number(20_261_016),
        seq: 9,
        base_ms: 1_792_108_800_000,
    }
}

/// `n` records whose keys are the big-endian index, given out of order, each `size`
/// bytes of a pattern that compresses poorly.
fn records(n: u32, size: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n)
        .rev()
        .map(|i| {
            let body: Vec<u8> = (0..size)
                .map(|j| (j as u32).wrapping_mul(2_654_435_761).wrapping_add(i) as u8)
                .collect();
            (i.to_be_bytes().to_vec(), body)
        })
        .collect()
}

fn zstd(raw: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(raw, ruzstd::encoding::CompressionLevel::Fastest)
}

#[test]
fn an_object_round_trips_with_records_in_key_order_across_blocks() {
    // Catches: records left in the order given, a record lost at a block boundary, a
    // block index whose offsets or counts are wrong, or a header field misplaced.
    let input = records(40, 5000);
    let enc = encode(&header(), input.clone(), b"summary".to_vec());
    let dec = decode(&enc.bytes).unwrap();
    assert_eq!(dec.header, header());
    assert_eq!(dec.footer.summary, b"summary");
    assert!(
        dec.footer.blocks.len() >= 3,
        "{} blocks",
        dec.footer.blocks.len()
    );
    let mut want = input;
    want.sort();
    let want: Vec<Vec<u8>> = want.into_iter().map(|(_, r)| r).collect();
    assert_eq!(dec.records, want);
    // The index row's range is exactly the footer and trailer.
    let tail = &enc.bytes[enc.footer_offset as usize..];
    assert_eq!(tail.len(), enc.footer_len as usize);
    assert_eq!(
        decode_footer(tail).unwrap(),
        (Family::InvocationChunk, dec.footer.clone())
    );
    // Each block alone decodes to its own records, and starts with its first key.
    let mut total = 0;
    for b in &dec.footer.blocks {
        let got = decode_block(b, block_bytes(&enc.bytes, b).unwrap()).unwrap();
        assert_eq!(got.len(), b.records as usize);
        total += got.len();
    }
    assert_eq!(total, 40);
}

#[test]
fn an_empty_object_has_no_blocks() {
    // Catches: an empty block written for no records (a reader would find a block that
    // holds nothing).
    let enc = encode(&header(), Vec::new(), Vec::new());
    let dec = decode(&enc.bytes).unwrap();
    assert!(dec.footer.blocks.is_empty());
    assert!(dec.records.is_empty());
    assert_eq!(dec.footer.block_for(&[0]), None);
}

#[test]
fn the_block_index_finds_the_block_holding_a_key() {
    // Catches: a search that returns the block after the key's (off by one), or one
    // that answers for a key before the first block.
    let enc = encode(&header(), records(40, 5000), Vec::new());
    let (_, footer) = footer_of(&enc.bytes).unwrap();
    for i in [0u32, 1, 17, 39] {
        let key = i.to_be_bytes();
        let b = footer.block_for(&key).unwrap();
        let recs = decode_block(b, block_bytes(&enc.bytes, b).unwrap()).unwrap();
        let first = u32::from_be_bytes(b.first_key.as_slice().try_into().unwrap());
        assert!(first <= i, "key {i} before block from {first}");
        assert!(
            i < first + recs.len() as u32,
            "key {i} after block from {first}"
        );
    }
    let before = Footer {
        blocks: vec![BlockEntry {
            first_key: vec![5],
            ..BlockEntry::default()
        }],
        summary: Vec::new(),
    };
    assert_eq!(before.block_for(&[4]), None);
}

#[test]
fn a_newer_version_is_refused_in_the_header_and_the_trailer() {
    // Catches: a reader that misreads a newer layout instead of refusing it.
    let enc = encode(&header(), records(2, 10), Vec::new());
    let mut h = enc.bytes.clone();
    h[5] = 2;
    assert_eq!(
        decode(&h),
        Err(FormatError::Version {
            found: 2,
            at: "header"
        })
    );
    let mut t = enc.bytes.clone();
    let n = t.len();
    t[n - 7] = 2;
    let err = decode(&t).unwrap_err();
    assert_eq!(
        err,
        FormatError::Version {
            found: 2,
            at: "trailer"
        }
    );
    assert!(err.to_string().contains("reads version 1"));
}

#[test]
fn bad_magic_unknown_family_and_short_bytes_are_refused() {
    // Catches: arbitrary bytes (another family's object, a truncated read) taken as a
    // chunk.
    let enc = encode(&header(), records(2, 10), Vec::new());
    let n = enc.bytes.len();
    let mut m = enc.bytes.clone();
    m[0] = b'X';
    assert_eq!(decode(&m), Err(FormatError::BadMagic("header")));
    let mut m = enc.bytes.clone();
    m[n - 1] = b'X';
    assert_eq!(decode(&m), Err(FormatError::BadMagic("trailer")));
    let mut f = enc.bytes.clone();
    f[6] = 9;
    assert_eq!(decode(&f), Err(FormatError::Family(9)));
    let mut f = enc.bytes.clone();
    f[n - 6] = 9;
    assert_eq!(decode(&f), Err(FormatError::Family(9)));
    assert_eq!(
        decode(&enc.bytes[..10]),
        Err(FormatError::Truncated("header"))
    );
    assert_eq!(
        decode_footer(&[0; 3]),
        Err(FormatError::Truncated("trailer"))
    );
    assert_eq!(footer_of(&[0; 3]), Err(FormatError::Truncated("trailer")));
}

#[test]
fn a_footer_range_that_is_not_the_footer_is_refused() {
    // Catches: a stale index row's footer range (a few bytes off) read as a footer, or
    // a trailer offset past the end of the object.
    let enc = encode(&header(), records(2, 10), Vec::new());
    let tail = &enc.bytes[enc.footer_offset as usize - 1..];
    assert_eq!(
        decode_footer(tail),
        Err(FormatError::Truncated("footer length"))
    );
    let mut far = enc.bytes.clone();
    let n = far.len();
    far[n - 24..n - 16].copy_from_slice(&u64::MAX.to_be_bytes());
    assert_eq!(
        footer_of(&far),
        Err(FormatError::Truncated("footer offset"))
    );
}

#[test]
fn a_corrupt_footer_or_block_fails_its_crc() {
    // Catches: a flipped byte in the footer or a block returned as data.
    let enc = encode(&header(), records(2, 10), Vec::new());
    let mut f = enc.bytes.clone();
    f[enc.footer_offset as usize] ^= 0xFF;
    assert_eq!(decode(&f), Err(FormatError::Crc("footer")));
    let mut b = enc.bytes.clone();
    b[HEADER_LEN] ^= 0xFF;
    let err = decode(&b).unwrap_err();
    assert_eq!(err, FormatError::Crc("block"));
    assert_eq!(err.to_string(), "history object block failed its CRC-32C");
    // A range of the wrong length is refused before its CRC is computed.
    let (_, footer) = footer_of(&enc.bytes).unwrap();
    let e = &footer.blocks[0];
    let stored = block_bytes(&enc.bytes, e).unwrap();
    assert_eq!(
        decode_block(e, &stored[1..]),
        Err(FormatError::Crc("block"))
    );
}

#[test]
fn a_footer_that_does_not_decode_is_refused() {
    // Catches: garbage footer bytes with a matching CRC (a writer bug) decoded as an
    // empty footer.
    let garbage = vec![0xFF, 0xFF, 0xFF];
    let mut tail = garbage.clone();
    tail.extend_from_slice(&0u64.to_be_bytes());
    tail.extend_from_slice(&(garbage.len() as u32).to_be_bytes());
    tail.extend_from_slice(&crc32c::crc32c(&garbage).to_be_bytes());
    tail.extend_from_slice(&VERSION.to_be_bytes());
    tail.push(1);
    tail.push(0);
    tail.extend_from_slice(&MAGIC);
    let err = decode_footer(&tail).unwrap_err();
    assert!(
        matches!(err, FormatError::Decode { what: "footer", .. }),
        "{err}"
    );
}

fn entry_for(stored: &[u8], records: u32) -> BlockEntry {
    BlockEntry {
        first_key: Vec::new(),
        offset: 0,
        len: stored.len() as u32,
        records,
        crc32c: crc32c::crc32c(stored),
    }
}

#[test]
fn a_block_that_is_not_zstd_or_inflates_too_far_is_refused() {
    // Catches: a block inflated without a bound (a small object could take the
    // frontend's memory), or bytes that are not a zstd frame read as records.
    let junk = b"not zstd at all".to_vec();
    let err = decode_block(&entry_for(&junk, 0), &junk).unwrap_err();
    assert!(matches!(err, FormatError::Block(_)), "{err}");
    let bomb = zstd(&vec![0u8; MAX_BLOCK_RAW_BYTES + 1]);
    let err = decode_block(&entry_for(&bomb, 1), &bomb).unwrap_err();
    assert!(err.to_string().contains("inflates past"), "{err}");
    // A frame cut short fails while inflating.
    let whole = zstd(&[7u8; 1000]);
    let cut = &whole[..whole.len() - 3];
    assert!(decode_block(&entry_for(cut, 1), cut).is_err());
}

#[test]
fn records_must_split_as_the_block_entry_says() {
    // Catches: a record length running past the block, a bad varint, or a block whose
    // count disagrees with its entry read as whole.
    let mut raw = Vec::new();
    prost::encoding::encode_varint(10, &mut raw);
    raw.extend_from_slice(b"abc");
    let stored = zstd(&raw);
    assert_eq!(
        decode_block(&entry_for(&stored, 1), &stored),
        Err(FormatError::Truncated("record"))
    );
    let stored = zstd(&[0xFF; 11]);
    let err = decode_block(&entry_for(&stored, 1), &stored).unwrap_err();
    assert!(matches!(
        err,
        FormatError::Decode {
            what: "record length",
            ..
        }
    ));
    let mut raw = Vec::new();
    prost::encoding::encode_varint(1, &mut raw);
    raw.push(b'x');
    let stored = zstd(&raw);
    assert_eq!(
        decode_block(&entry_for(&stored, 2), &stored),
        Err(FormatError::Mismatch("block record count"))
    );
    assert_eq!(
        decode_block(&entry_for(&stored, 1), &stored).unwrap(),
        [b"x".to_vec()]
    );
}

#[test]
fn a_block_outside_the_object_is_refused() {
    // Catches: a block index entry (from a corrupt footer) sliced past the end.
    let e = BlockEntry {
        offset: 10,
        len: 100,
        ..BlockEntry::default()
    };
    assert_eq!(
        block_bytes(&[0; 20], &e),
        Err(FormatError::Truncated("block"))
    );
    let huge = BlockEntry {
        offset: u64::MAX,
        len: 1,
        ..BlockEntry::default()
    };
    assert_eq!(
        block_bytes(&[0; 20], &huge),
        Err(FormatError::Truncated("block"))
    );
}

#[test]
fn a_record_count_past_the_block_is_refused_without_reserving_it() {
    // Catches: room reserved for the footer's record count before the block is split.
    // The CRC guards against corruption, not a crafted object: a count of u32::MAX
    // asked for about 100 GB and aborted the reading process.
    let mut raw = Vec::new();
    prost::encoding::encode_varint(1, &mut raw);
    raw.push(b'x');
    let stored = zstd(&raw);
    assert_eq!(
        decode_block(&entry_for(&stored, u32::MAX), &stored),
        Err(FormatError::Mismatch("block record count"))
    );
}
