//! The check of one received segment, record by record as ranges arrive: the same
//! built segment whatever the order and cut of the ranges, a refusal of a record whose
//! bytes do not match, and no hashing left for the commit.

use kbf_segments::SegmentWriter;
use tonic::Code;

use super::*;

/// A segment of `n` blobs of different sizes (the last one `last` bytes) as `Begin`
/// declares it, and its record bytes.
fn segment(n: usize, last: usize) -> (Declared, Vec<u8>) {
    let mut writer = SegmentWriter::new(u64::MAX);
    for i in 0..n {
        let len = if i + 1 == n { last } else { 100 + i * 37 };
        let blob: Vec<u8> = (0..len).map(|b| (b * 7 + i) as u8).collect();
        writer.push(&blob).expect("push");
    }
    let bytes = writer.finish();
    let footer = kbf_segments::Footer::parse(&bytes, bytes.len() as u64).expect("footer");
    let records_len = usize::try_from(footer.records_len()).expect("len");
    let (mut declared, _) =
        declare(&[(bytes.len() as u64, &bytes[records_len..])]).expect("declare");
    (declared.remove(0), bytes[..records_len].to_vec())
}

/// Sends `records` in ranges of `cut` bytes in the order `order` names (indices of
/// ranges), as `PutRange` does: write, then run the claimed checks.
fn send(declared: &Declared, records: &[u8], cut: usize, order: &[usize]) -> Receiving {
    let mut received = Receiving::new(declared, false, None);
    for &k in order {
        let (start, end) = (k * cut, ((k + 1) * cut).min(records.len()));
        let (_, claimed) = received
            .put(declared, start as u64, &records[start..end])
            .expect("put");
        advance(&claimed, |i, finished| received.next(declared, i, finished));
    }
    received
}

/// Catches an upload on a write tier that still hashes on the ranges' path (a claim
/// handed out, a record refused before the flusher sees it), one that stores a segment
/// as verified when nothing was hashed (the direct PUT would then skip its check), and
/// a footer that loses the declared entries: a record altered after `Begin` makes no
/// claim, no refusal, and the segment builds with the declared CRC-32Cs, unverified.
#[test]
fn a_trusted_upload_hashes_nothing_and_builds_an_unverified_segment() {
    let (declared, records) = segment(5, 300);
    let mut altered = records.clone();
    altered[10] ^= 0xff;
    let mut received = Receiving::new(&declared, true, None);
    let (put, claimed) = received.put(&declared, 0, &altered).expect("put");
    assert_eq!(put, Put::Written);
    assert!(claimed.is_empty(), "{claimed:?}");
    assert!(received.not_ready().is_none());
    assert!(received.first_missing(&declared).is_none());
    let built = received.check(&declared).expect("built");
    assert!(!built.verified);
    assert_eq!(built.entries, declared.entries);
    let mut hashed = Receiving::new(&declared, false, None);
    let (_, claimed) = hashed.put(&declared, 0, &records).expect("put");
    assert!(!claimed.is_empty());
    advance(&claimed, |i, f| hashed.next(&declared, i, f));
    assert!(hashed.check(&declared).expect("built").verified);
}

fn in_order(len: usize, cut: usize) -> Vec<usize> {
    (0..len.div_ceil(cut)).collect()
}

/// Catches an incremental check that depends on how the bytes arrive (a hasher fed a
/// later range first, a record spanning ranges hashed short, a CRC of the wrong
/// bytes): ranges of 97, 1000 and 5000 bytes, in order, reversed and interleaved, give
/// the segment built from the whole buffer, with each entry's CRC-32C that of its
/// record, and the records are all checked before any commit.
#[test]
fn any_cut_and_order_builds_the_same_segment() {
    let (declared, records) = segment(30, 3000);
    let mut whole = Receiving::new(&declared, false, None);
    let (_, claimed) = whole.put(&declared, 0, &records).expect("put");
    advance(&claimed, |i, f| whole.next(&declared, i, f));
    let want = whole.check(&declared).expect("whole");
    for cut in [97, 1000, 5000] {
        let n = in_order(records.len(), cut);
        let reversed: Vec<usize> = n.iter().rev().copied().collect();
        let evens_then_odds: Vec<usize> = n
            .iter()
            .copied()
            .filter(|k| k % 2 == 0)
            .chain(n.iter().copied().filter(|k| k % 2 == 1))
            .collect();
        for order in [n, reversed, evens_then_odds] {
            let received = send(&declared, &records, cut, &order);
            assert!(!received.checking());
            let built = received.check(&declared).expect("built");
            assert_eq!(built.body, want.body, "cut {cut} order {order:?}");
            assert_eq!(built.entries, want.entries, "cut {cut} order {order:?}");
        }
    }
    for (entry, built) in declared.entries.iter().zip(&want.entries) {
        let range = entry.range();
        let record = &records[range.start as usize..range.end as usize];
        assert_eq!(built.crc32c, kbf_segments::crc32c(record));
    }
}

/// Catches a check that is deferred to the commit (the point of this change): after the
/// last range, every record is already hashed, so `check` hashes nothing and a segment
/// built from records the claims have not run is refused as not checked.
#[test]
fn the_commit_has_nothing_left_to_hash() {
    let (declared, records) = segment(10, 500);
    let mut received = Receiving::new(&declared, false, None);
    received.put(&declared, 0, &records).expect("put");
    // Claimed but never advanced, as a commit that raced its ranges would find it.
    assert!(received.checking());
    let raced = received.check(&declared).expect_err("not checked");
    assert_eq!(raced.code(), Code::FailedPrecondition);
    let done = send(&declared, &records, 64, &in_order(records.len(), 64));
    assert!(!done.checking());
    assert!(done.refusal().is_none());
    assert!(done.check(&declared).is_ok());
}

/// Catches a check that stops refusing (the server must still refuse a segment whose
/// bytes do not match their digests), in either cut: a flipped byte in a record that
/// spans ranges and one in a small record are each refused INVALID_ARGUMENT naming the
/// record, at the range that completes it, and `check` refuses too.
#[test]
fn a_corrupt_record_is_refused_when_its_last_byte_arrives() {
    let (declared, records) = segment(20, 3000);
    for victim in [3, 19] {
        let entry = &declared.entries[victim];
        // The entries are in digest order; find the record by its offset.
        let mut bad = records.clone();
        bad[entry.offset as usize + 1] ^= 1;
        for cut in [97, 5000] {
            let received = send(&declared, &bad, cut, &in_order(bad.len(), cut));
            let refused = received.refusal().expect("refused at the range");
            assert_eq!(refused.code(), Code::InvalidArgument, "cut {cut}");
            assert!(
                refused.message().contains(&entry.digest.to_string()),
                "{refused:?}"
            );
            assert_eq!(
                received.check(&declared).expect_err("check").code(),
                Code::InvalidArgument
            );
        }
    }
}

/// Catches a record that is never hashed or hashed twice when ranges are cut inside it
/// (a hasher restarted per range): one record of 10,000 bytes sent in ranges of 1,000
/// is checked, with the CRC-32C of all its bytes.
#[test]
fn a_record_spanning_many_ranges_is_one_hash() {
    let (declared, records) = segment(1, 10_000);
    let received = send(&declared, &records, 1000, &in_order(records.len(), 1000));
    let built = received.check(&declared).expect("built");
    assert_eq!(built.entries[0].crc32c, kbf_segments::crc32c(&records));
}

/// A segment of an empty blob and a small one, as `Begin` declares it, and its record
/// bytes.
fn with_an_empty_blob() -> (Declared, Vec<u8>) {
    let mut writer = SegmentWriter::new(u64::MAX);
    // The empty record sits at the end of the records: `Begin` refuses one in front of
    // another record, which it would overlap.
    writer.push(b"not empty").expect("push");
    writer.push(b"").expect("push");
    let bytes = writer.finish();
    let footer = kbf_segments::Footer::parse(&bytes, bytes.len() as u64).expect("footer");
    let records_len = usize::try_from(footer.records_len()).expect("len");
    let (mut declared, _) =
        declare(&[(bytes.len() as u64, &bytes[records_len..])]).expect("declare");
    (declared.remove(0), bytes[..records_len].to_vec())
}

/// Catches an empty record that waits for bytes that never come (the commit would find
/// it "not checked"), and an empty record whose declared digest is not the empty
/// digest being accepted (a lying footer): the right one is checked from the start, so
/// the segment builds; one declaring other hash bytes at size 0 is refused at once,
/// naming the record. A trusted upload (on a write tier) checks no record, the empty
/// one included: the lying footer is accepted here, and the flusher drops the record.
#[test]
fn an_empty_record_is_checked_without_bytes_and_a_lying_one_is_refused() {
    let (declared, records) = with_an_empty_blob();
    let received = send(&declared, &records, 4096, &[0]);
    assert!(received.refusal().is_none());
    received.check(&declared).expect("an empty record builds");

    let mut lying = declared;
    let empty = lying
        .entries
        .iter()
        .position(|e| e.digest.size_bytes == 0)
        .expect("the empty record");
    lying.entries[empty].digest.hash[0] ^= 1;
    let refused = Receiving::new(&lying, false, None)
        .refusal()
        .expect("refused at Begin");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        refused
            .message()
            .contains(&lying.entries[empty].digest.to_string()),
        "{refused:?}"
    );
    assert!(Receiving::new(&lying, true, None).refusal().is_none());
}

/// Catches a commit that goes on while a record is still being checked (it would find
/// the record unchecked or build over bytes not yet hashed), and one that reports a
/// refused record as merely not ready: a range written but not yet checked is
/// FAILED_PRECONDITION; once its check ran, nothing blocks; a corrupt record is
/// INVALID_ARGUMENT.
#[test]
fn a_commit_waits_for_checks_and_stops_at_a_refusal() {
    let (declared, records) = segment(3, 500);
    let mut received = Receiving::new(&declared, false, None);
    let (_, claimed) = received.put(&declared, 0, &records).expect("put");
    let racing = received.not_ready().expect("records are being checked");
    assert_eq!(racing.code(), Code::FailedPrecondition);
    advance(&claimed, |i, finished| {
        received.next(&declared, i, finished)
    });
    assert!(received.not_ready().is_none());

    let mut bad = records;
    bad[1] ^= 1;
    let received = send(&declared, &bad, 4096, &[0]);
    assert_eq!(
        received.not_ready().expect("refused").code(),
        Code::InvalidArgument
    );
}

/// Catches a range that claims a record some task is already advancing (two tasks
/// hashing one record, or its hasher taken twice): a record spanning two ranges whose
/// first range's check has not run is not claimed again by the second range; the first
/// task then carries on over both and the record hashes once, to the right digest.
#[test]
fn a_busy_record_is_not_claimed_again() {
    let (declared, records) = segment(1, 3000);
    let mut received = Receiving::new(&declared, false, None);
    let (_, first) = received.put(&declared, 0, &records[..1000]).expect("put");
    assert_eq!(first.len(), 1);
    let (_, second) = received
        .put(&declared, 1000, &records[1000..])
        .expect("put");
    assert!(second.is_empty(), "claimed twice: {second:?}");
    advance(&first, |i, f| received.next(&declared, i, f));
    assert!(received.refusal().is_none());
    received
        .check(&declared)
        .expect("one hash over both ranges");
}

/// A cache over a memory log and store with an in-memory write tier attached, as a
/// server with a hot store has.
async fn tiered() -> std::sync::Arc<crate::Cache<crate::MemoryMetaLog, kbf_objstore::MemoryStore>> {
    let cache = crate::Cache::open(
        crate::MemoryMetaLog::new(kbf_meta::Retention::default()),
        kbf_objstore::MemoryStore::new(kbf_objstore::Capabilities::default()),
        kbf_objstore::KeyPrefix::default(),
    )
    .await
    .expect("open");
    let (cache, _) = cache
        .attach_tier(
            kbf_store::FaultFs::new(),
            kbf_tier::TierOptions::new(1 << 30),
            kbf_meta::ServerId::new(1),
            None,
        )
        .await
        .expect("a tier");
    std::sync::Arc::new(cache)
}

/// Catches a segment on a write tier held here, or a range whose write failed taken
/// for received (the commit would stage bytes that are not on the tier): its records
/// are not kept in the buffer; while a range is being written the commit is not ready;
/// a range whose write failed is forgotten, so it is missing until sent again; once
/// every range is written the segment is built as its range on the tier, unverified,
/// with nothing in its body.
#[tokio::test]
async fn a_segment_on_the_tier_is_written_there_and_built_as_its_range() {
    let cache = tiered().await;
    let (declared, records) = segment(3, 50);
    let range = cache
        .reserve_range(declared.records_len)
        .await
        .expect("a range");
    let mut received = Receiving::new(&declared, true, Some(range));
    assert!(format!("{received:?}").contains("TierRange"));
    let half = records.len() / 2;
    let (put, claimed) = received.put(&declared, 0, &records[..half]).expect("put");
    assert_eq!((put, claimed), (Put::Written, Vec::new()));
    let (range, writes) = received.tier().expect("on the tier");
    assert!(received.not_ready().is_some(), "a write is running");
    writes.done(0, false);
    received.forget_failed();
    assert!(received.first_missing(&declared).is_some(), "forgotten");
    for (from, to) in [(0, half), (half, records.len())] {
        let (put, _) = received
            .put(&declared, from as u64, &records[from..to])
            .expect("put");
        assert_eq!(put, Put::Written);
        range
            .write(from as u64, &records[from..to])
            .expect("written");
        writes.done(from as u64, true);
    }
    drop(range);
    assert!(received.not_ready().is_none());
    assert!(received.first_missing(&declared).is_none());
    let built = received.check(&declared).expect("built");
    assert!(built.body.is_empty());
    assert!(!built.verified);
    assert_eq!(built.entries.len(), 3);
    let staged = built.staged.expect("its range on the tier");
    let blobs = built.entries.iter().map(|e| (e.digest, e.offset)).collect();
    cache.stage_range(staged, blobs).await.expect("staged");
    for entry in &built.entries {
        let bytes = cache.read_blob(&entry.digest).await.expect("read");
        assert_eq!(kbf_segments::sha256(&bytes), entry.digest);
    }
}
