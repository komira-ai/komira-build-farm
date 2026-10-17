//! The upload plan, the range cutting and the read coalescing, without a server.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kbf_segments::layout::footer_len;
use kbf_segments::{Footer, IndexEntry};
use kbf_types::{Digest as KbfDigest, DigestFunction};

use super::*;
use crate::source::Source;
use crate::spool::Spool;

/// A blob of `size` bytes whose hash starts with `n`. Its source is empty: planning
/// reads sizes, never bytes.
fn record(n: u8, size: u64) -> Record {
    let mut hash = [0u8; 32];
    hash[0] = n;
    hash[31] = n.wrapping_mul(7);
    Record {
        digest: KbfDigest::new(DigestFunction::Sha256, hash, size),
        crc32c: u32::from(n),
        source: Source::Bytes(bytes::Bytes::new()),
    }
}

/// `n` blobs of `size` bytes.
fn back_to_back(n: u8, size: u64) -> Vec<Record> {
    (0..n).map(|i| record(i, size)).collect()
}

/// Each piece as (first byte of the record's hash, offset into the record, length).
fn shape(pieces: &[(Record, u64, u64)]) -> Vec<(u8, u64, u64)> {
    pieces
        .iter()
        .map(|(r, at, len)| (r.digest.hash[0], *at, *len))
        .collect()
}

/// Catches an upload cut at every segment (mutant: one upload per segment) and a
/// segment over its limit: blobs past one segment's limit fill several segments of
/// one upload, each within the limit.
#[test]
fn segments_fill_to_their_limit_within_one_upload() {
    let limit = 1000 + footer_len(4);
    let uploads = plan(&back_to_back(10, 250), limit, u64::MAX, usize::MAX);
    assert_eq!(uploads.len(), 1);
    let sizes: Vec<usize> = uploads[0].iter().map(|s| s.records.len()).collect();
    assert_eq!(sizes, [4, 4, 2]);
    assert!(uploads[0].iter().all(|s| s.length() <= limit));
}

/// Catches an upload over the server's byte cap or record cap (it would be refused):
/// a new upload starts before either is passed.
#[test]
fn uploads_stay_under_both_caps() {
    let by_bytes = plan(
        &back_to_back(10, 100),
        u64::MAX,
        300 + footer_len(3),
        usize::MAX,
    );
    let counts: Vec<usize> = by_bytes.iter().map(|u| u[0].records.len()).collect();
    assert_eq!(counts, [3, 3, 3, 1]);
    let by_records = plan(&back_to_back(10, 100), u64::MAX, u64::MAX, 4);
    let counts: Vec<usize> = by_records.iter().map(|u| u[0].records.len()).collect();
    assert_eq!(counts, [4, 4, 2]);
}

/// Catches the cap that lost 14,244 outputs to one failed upload: a cold output set of
/// 300 MiB in 1.5 MiB blobs is cut into uploads of at most `SEGMENT_UPLOAD_BYTES`, so one
/// failed upload puts at most that much at risk (mutant: the old 1 GiB cap, one upload).
#[test]
fn a_large_output_set_is_many_small_uploads() {
    let uploads = plan(
        &back_to_back(200, 3 << 19),
        MAX_SEGMENT_BYTES,
        SEGMENT_UPLOAD_BYTES,
        MAX_UPLOAD_RECORDS,
    );
    assert!(uploads.len() >= 10, "{} uploads", uploads.len());
    for upload in &uploads {
        let len: u64 = upload.iter().map(Planned::length).sum();
        assert!(len <= SEGMENT_UPLOAD_BYTES, "an upload of {len} bytes");
    }
}

/// Catches a blob too large for a segment dropped, or packed with others: it is a
/// segment of one record, and the segment after it starts fresh.
#[test]
fn a_blob_over_the_segment_limit_is_a_segment_of_its_own() {
    let records = [record(1, 10), record(2, 5000), record(3, 10)];
    let uploads = plan(&records, 1000, u64::MAX, usize::MAX);
    let sizes: Vec<usize> = uploads[0].iter().map(|s| s.records.len()).collect();
    assert_eq!(sizes, [1, 1, 1]);
    assert_eq!(uploads[0][1].length(), 5000 + footer_len(1));
}

/// Catches a footer the server would refuse (unsorted, wrong offsets or length) and a
/// range read from the wrong blob or the wrong place in it: the footer parses for the
/// segment's length and names each blob at its segment offset; ranges cover the record
/// area in pieces that name the blob and the offset into it.
#[test]
fn footers_parse_and_ranges_map_to_the_spool() {
    // Order 3, 1, 2: the footer is sorted by digest, offsets follow the order given.
    let records = [record(3, 100), record(1, 50), record(2, 70)];
    let segment = &plan(&records, u64::MAX, u64::MAX, usize::MAX)[0][0];
    let footer = Footer::parse(&segment.footer(), segment.length()).expect("parse");
    let offsets: Vec<(u8, u64)> = footer
        .entries()
        .iter()
        .map(|e: &IndexEntry| (e.digest.hash[0], e.offset))
        .collect();
    assert_eq!(offsets, [(1, 100), (2, 150), (3, 0)]);
    assert_eq!(segment.ranges(120), [(0, 120), (120, 220)]);
    // 0..100 is all of blob 3; 100..120 is the start of blob 1.
    assert_eq!(shape(&segment.pieces(0, 120)), [(3, 0, 100), (1, 0, 20)]);
    // 120..220 is the rest of blob 1 and all of blob 2.
    assert_eq!(shape(&segment.pieces(120, 220)), [(1, 20, 30), (2, 0, 70)]);
}

/// Catches fills still one call per blob, or a run that passes the read limit or
/// reads across a wide gap: close blobs of one object share a run, a far one or one
/// of another object starts another, and a run never grows past the limit.
#[test]
fn coalescing_joins_close_blobs_of_one_object() {
    let object = |seq| ObjectRef {
        epoch: 1,
        seq,
        ..ObjectRef::default()
    };
    let d = |n: u8, size: i64| Digest {
        hash: format!("{n:02x}"),
        size_bytes: size,
    };
    let located = vec![
        (object(1), 0, d(1, 100)),
        (object(1), 150, d(2, 100)),
        (object(1), 10_000, d(3, 100)),
        (object(2), 0, d(4, 100)),
        (object(1), 250, d(5, 1000)),
    ];
    let runs = coalesce(located, 1000, 1200);
    let shape: Vec<(u64, u64, u64, usize)> = runs
        .iter()
        .map(|r| (r.object.seq, r.start, r.end, r.blobs.len()))
        .collect();
    assert_eq!(
        shape,
        [
            (1, 0, 250, 2),
            (1, 250, 1250, 1),
            (1, 10_000, 10_100, 1),
            (2, 0, 100, 1)
        ]
    );
}

/// Catches an empty segment, or an empty upload, sent to the server (mutants: the
/// segment closed at the upload cap pushed whether or not it holds a record; the last
/// segment or upload pushed when empty): when a blob closes a full segment and also
/// passes the upload's byte cap, the new upload starts with that blob and no empty
/// segment is left behind; nothing to upload is no upload.
#[test]
fn a_segment_closed_at_the_upload_cap_leaves_no_empty_segment() {
    let segment_limit = 200 + footer_len(2);
    let upload_bytes = 200 + footer_len(2) + 50;
    let uploads = plan(
        &back_to_back(6, 100),
        segment_limit,
        upload_bytes,
        usize::MAX,
    );
    let shape: Vec<Vec<usize>> = uploads
        .iter()
        .map(|u| u.iter().map(|s| s.records.len()).collect())
        .collect();
    assert_eq!(shape, [[2], [2], [2]]);
    assert!(plan(&[], segment_limit, upload_bytes, usize::MAX).is_empty());
    // A first blob over the upload cap still goes, alone, with no empty upload before
    // it.
    let alone = plan(&back_to_back(1, 500), u64::MAX, 100, usize::MAX);
    let shape: Vec<Vec<usize>> = alone
        .iter()
        .map(|u| u.iter().map(|s| s.records.len()).collect())
        .collect();
    assert_eq!(shape, [[1]]);
}

/// A store that keeps nothing and whose `put` answers the bytes' digest, or, while
/// `lie` is set, another one; `put_chunks` notes each blob's size and gathers it into
/// `put`.
struct Lying {
    lie: AtomicBool,
    chunked: std::sync::Mutex<Vec<i64>>,
}

impl Cas for Lying {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _lease: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        Err(CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        if self.lie.load(Ordering::SeqCst) {
            return Ok(crate::cas::digest_of(b"some other blob"));
        }
        Ok(crate::cas::digest_of(&bytes))
    }

    async fn put_chunks(&self, digest: Digest, mut chunks: Chunks) -> Result<Digest, CasError> {
        self.chunked
            .lock()
            .expect("chunked")
            .push(digest.size_bytes);
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            bytes.extend(chunk.expect("a spool chunk"));
        }
        self.put(bytes).await
    }
}

/// Catches a stored blob trusted without checking the store's answer (mutant: the
/// comparison of `put`'s digest dropped): `put_each` stores a small blob, and one over
/// a megabyte streamed through `put_chunks`, while the store answers their digests,
/// and fails as corrupt, naming the blob, through a store that answers another digest.
/// Also catches a blob over a megabyte read whole (mutant: every blob sent by `put`):
/// only that one reaches `put_chunks`.
/// One store type throughout, so one instantiation of `put_each` runs all of it.
#[tokio::test]
async fn put_each_refuses_a_store_that_answers_another_digest() {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-transfer-segments-unit")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).expect("mkdir");
    let lying = Arc::new(Lying {
        lie: AtomicBool::new(false),
        chunked: std::sync::Mutex::default(),
    });
    let truthful = Arc::new(Spool::create(&dir).expect("spool"));
    truthful
        .put(b"a small output".to_vec())
        .await
        .expect("spooled");
    truthful.put(vec![7; (1 << 20) + 5]).await.expect("spooled");
    put_each(&lying, truthful.outgoing())
        .await
        .expect("the store answered each digest");
    assert_eq!(
        *lying.chunked.lock().expect("chunked"),
        [(1 << 20) + 5],
        "only the blob over a megabyte is streamed"
    );
    lying.lie.store(true, Ordering::SeqCst);
    let spool = Arc::new(Spool::create(&dir).expect("spool"));
    let digest = spool.put(b"an output".to_vec()).await.expect("spooled");
    let failed = put_each(&lying, spool.outgoing())
        .await
        .expect_err("another digest");
    assert_eq!(
        failed,
        CasError::Corrupt(
            label(&digest),
            label(&crate::cas::digest_of(b"some other blob"))
        )
    );
    // The fake's other calls, so none of its lines is left unrun.
    assert_eq!(
        lying.get(&digest).await,
        Err(CasError::Missing(label(&digest)))
    );
    assert!(Arc::ptr_eq(
        &lying.lease(&kbf_types::LeaseId::new(1, 1)),
        &lying
    ));
}
