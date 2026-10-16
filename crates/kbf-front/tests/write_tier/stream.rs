//! A record streamed onto the write tier as its bytes are read (`Cache::stream`): what
//! the in-process plane stores a large output with, written chunk by chunk to a block
//! of its own, checked at its end, committed as one `PutStaged`, and flushed by the
//! flusher, which hashes it before the PUT. Each test's comment names the defect it is
//! there to catch.

use std::sync::Arc;

use super::support::{Farm, TestBlob, later};
use kbf_front::{Cache, CacheError};
use kbf_meta::BlobAnswer;
use kbf_objstore::Capabilities;

use super::GIB;

/// Catches a streamed record that is not staged, not readable, or not flushed: one
/// written in three chunks is located on the tier once committed, reads back as its
/// bytes with nothing PUT yet, and is PUT once by the flusher.
#[tokio::test]
async fn a_streamed_record_is_staged_read_back_and_flushed() {
    let farm = Farm::new(Capabilities::default());
    let cache = Arc::new(farm.start(GIB).await);
    let blob = TestBlob::new(1, 300_000);
    let mut stream = cache.stream(blob.bytes.len() as u64).expect("a stream");
    for chunk in blob.bytes.chunks(128 << 10) {
        stream.append(chunk).expect("appended");
    }
    let crc = kbf_segments::crc32c(&blob.bytes);
    let streamed = stream.finish(blob.digest, crc).expect("finished");
    assert_eq!(streamed.digest(), blob.digest);
    assert_eq!(farm.log.answer(&blob.digest), BlobAnswer::Absent, "not yet");
    streamed.commit().await.expect("committed");
    assert_eq!(farm.log.located(&blob.digest).1, cache.tier_id());
    assert_eq!(
        cache.read_blob(&blob.digest).await.expect("read"),
        blob.bytes
    );
    assert!(farm.store.put_keys().is_empty(), "staged, not PUT");
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 1, "{round:?}");
    assert_eq!(farm.store.put_keys().len(), 1);
}

/// Catches a stream that ends unchecked (mutants: the length check, the CRC-32C check
/// skipped): a digest naming one byte more than was written, the stream ended short of
/// what it was made for, and a CRC-32C other than the bytes' are each refused as
/// invalid, and nothing is stored.
#[tokio::test]
async fn a_stream_whose_end_does_not_match_its_bytes_is_refused() {
    let farm = Farm::new(Capabilities::default());
    let cache = Arc::new(farm.start(GIB).await);
    let blob = TestBlob::new(2, 10_000);
    let crc = kbf_segments::crc32c(&blob.bytes);
    let longer = kbf_types::Digest::new(
        kbf_types::DigestFunction::Sha256,
        [3; 32],
        blob.bytes.len() as u64 + 1,
    );
    let short = blob.bytes.len() as u64 - 1;
    let cases: [(&str, &[u8], kbf_types::Digest, u32); 3] = [
        ("a longer digest", &blob.bytes, longer, crc),
        (
            "fewer bytes than made for",
            &blob.bytes[..short as usize],
            kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [4; 32], short),
            crc,
        ),
        ("another CRC-32C", &blob.bytes, blob.digest, crc ^ 1),
    ];
    for (what, bytes, digest, crc) in cases {
        let mut stream = cache.stream(blob.bytes.len() as u64).expect("a stream");
        stream.append(bytes).expect("appended");
        let refused = stream.finish(digest, crc).map(drop);
        assert!(
            matches!(refused, Err(CacheError::Invalid(_))),
            "{what}: {refused:?}"
        );
        assert_eq!(farm.log.answer(&digest), BlobAnswer::Absent, "{what}");
    }
    let mut stream = cache.stream(4).expect("a stream");
    let past = stream
        .append(b"five!")
        .expect_err("past the reserved range");
    assert!(matches!(past, CacheError::Tier(_)), "{past:?}");
}

/// Catches a stream made where it cannot be stored, or refused for its size: without a
/// tier, for an empty blob, and past the tier's budget (counted as a write the tier did
/// not take) there is no stream; a blob over 5 GiB gets one when the tier has room.
#[tokio::test]
async fn there_is_no_stream_without_a_tier_room_or_a_legal_size() {
    assert!(Arc::new(Cache::memory()).stream(10).is_none(), "no tier");
    let farm = Farm::new(Capabilities::default());
    let cache = Arc::new(farm.start(10_000).await);
    assert!(cache.stream(0).is_none(), "empty");
    assert!(cache.stream(20_000).is_none(), "past the budget");
    let metrics = cache.render_metrics();
    assert!(
        metrics.contains("\nkbf_write_tier_full_total 1\n"),
        "{metrics}"
    );
    let roomy = Farm::new(Capabilities::default());
    let cache = Arc::new(roomy.start(8 * GIB).await);
    assert!(cache.stream((5 << 30) + 1).is_some(), "over 5 GiB");
}
