//! `Segments` uploads on the local write tier (`docs/design/write-path.md`, sections
//! 4.2 and 6.1): a commit answered once every segment is synced on the tier and its
//! `PutStaged` committed, blobs read from the tier until flushed, and the direct PUT
//! when the tier does not take a segment.
//!
//! The service is called in process over the same [`Farm`] `write_tier.rs` runs: one
//! metadata log that outlives restarts, a tier directory on a `FaultFs`, and a store
//! whose PUTs a test can hold ("the object store is down"). Each test's comment names
//! the defect it is there to catch.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, CacheError, Connections, SegmentsConfig, SegmentsService};
use kbf_meta::BlobAnswer;
use kbf_objstore::{Capabilities, KeyPrefix};
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, CommitRequest, ObjectRef, PutRangeRequest, ReadRangeRequest, UploadSegment,
};
use kbf_segments::layout::encode_footer;
use kbf_segments::{Footer, IndexEntry, SegmentReader, SegmentWriter};
use kbf_types::Digest;
use support::{Farm, TestBlob, TierCache, blobs, later, start};
use tonic::{Code, Request, Status};

use crate::common::faulty::{FaultyStore, GetFault};
use support::Log;

const GIB: u64 = 1 << 30;

type Service = SegmentsService<Log, FaultyStore>;

fn serve(cache: TierCache) -> (Arc<TierCache>, Service) {
    let cache = Arc::new(cache);
    let service = SegmentsService::new(
        Arc::clone(&cache),
        SegmentsConfig::DEFAULT,
        Connections::new(),
    );
    (cache, service)
}

/// A segment as a daemon sends it: its records, and the footer that indexes them.
struct Seg {
    records: Vec<u8>,
    footer: Vec<u8>,
    length: u64,
}

fn seg(blobs: &[TestBlob]) -> Seg {
    let mut writer = SegmentWriter::new(u64::MAX);
    for b in blobs {
        writer.push(&b.bytes).expect("push");
    }
    let bytes = writer.finish();
    let parsed = Footer::parse(&bytes, bytes.len() as u64).expect("footer");
    let records_len = usize::try_from(parsed.records_len()).expect("len");
    let entries: Vec<IndexEntry> = parsed.entries().to_vec();
    let footer = encode_footer(parsed.records_len(), &entries);
    Seg {
        records: bytes[..records_len].to_vec(),
        length: (records_len + footer.len()) as u64,
        footer,
    }
}

/// Begin, PutRange and Commit of `segs`; the Commit gives up after ten seconds, so a
/// commit stuck on a downed store is a failed assertion, not a hung test.
async fn upload(service: &Service, segs: &[Seg]) -> Result<Vec<String>, Status> {
    let begin = BeginRequest {
        segments: segs
            .iter()
            .map(|s| UploadSegment {
                length: s.length,
                footer: s.footer.clone(),
            })
            .collect(),
    };
    let upload = service
        .begin(Request::new(begin))
        .await?
        .into_inner()
        .upload;
    for (i, s) in segs.iter().enumerate() {
        let put = PutRangeRequest {
            upload,
            segment: u32::try_from(i).expect("segment"),
            offset: 0,
            bytes: s.records.clone(),
        };
        service.put_range(Request::new(put)).await?;
    }
    let commit = service.commit(Request::new(CommitRequest { upload }));
    let answered = tokio::time::timeout(Duration::from_secs(10), commit).await;
    let committed = answered.expect("the commit answered while the store is down")?;
    Ok(committed
        .into_inner()
        .committed
        .into_iter()
        .map(|d| d.hash)
        .collect())
}

/// `s` with the first byte of `blob`'s record changed after its footer was made: what a
/// daemon with a bad disk or a bug would send under a digest it computed earlier.
fn altered(s: &Seg, blob: &TestBlob) -> (Seg, usize) {
    let whole = [s.records.as_slice(), s.footer.as_slice()].concat();
    let footer = Footer::parse(&whole, s.length).expect("footer");
    let entry = footer
        .entries()
        .iter()
        .find(|e| e.digest == blob.digest)
        .expect("the blob is in the segment");
    let at = usize::try_from(entry.offset).expect("offset");
    let mut records = s.records.clone();
    records[at] ^= 0xff;
    let seg = Seg {
        records,
        footer: s.footer.clone(),
        length: s.length,
    };
    (seg, at)
}

/// Every blob of `blobs` is present, and read back whole: by `FindMissing`, by the
/// batch read, and as the daemon does, from `Locate` and `ReadRange`.
async fn assert_readable(cache: &TierCache, service: &Service, blobs: &[TestBlob]) {
    let digests: Vec<Digest> = blobs.iter().map(|b| b.digest).collect();
    assert_eq!(cache.find_missing(&digests).await.expect("find"), []);
    let read = cache.read_blobs(&digests).await.expect("batch");
    let located = service
        .locate(Request::new(kbf_proto::worker::LocateRequest {
            digests: digests.iter().map(kbf_front::digest_to_proto).collect(),
        }))
        .await
        .expect("locate")
        .into_inner()
        .locations;
    for (b, at) in blobs.iter().zip(located) {
        assert_eq!(read[&b.digest].as_ref().expect("batch read"), &b.bytes);
        let object = at.object.expect("located");
        let range = ReadRangeRequest {
            object: Some(object),
            offset: at.offset,
            length: b.bytes.len() as u64,
        };
        let bytes = service
            .read_range(Request::new(range))
            .await
            .expect("read range")
            .into_inner()
            .bytes;
        assert_eq!(bytes, b.bytes.as_ref());
    }
}

fn counter(cache: &TierCache, name: &str) -> String {
    let metrics = cache.render_metrics();
    metrics
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name} ")))
        .unwrap_or_else(|| panic!("no {name} in {metrics}"))
        .to_owned()
}

/// Catches: a `Segments` commit that PUTs while a tier is attached (the cold-upload
/// wait on the object store), and blobs staged for a commit that cannot be found or
/// read until the store takes them. With every PUT held, three segments commit, none
/// reaches the store, three objects are staged, and every blob is present and read
/// back by every read path.
#[tokio::test]
async fn an_upload_commits_on_the_tier_while_the_store_is_down() {
    let farm = Farm::new(Capabilities::default());
    farm.store.hold_puts();
    let (cache, service) = serve(farm.start(GIB).await);
    let all = blobs(1, 6, 4096);
    let segs: Vec<Seg> = all.chunks(2).map(seg).collect();
    let committed = upload(&service, &segs).await.expect("committed");
    let mut want: Vec<_> = all.iter().map(|b| b.digest.hash_hex()).collect();
    let mut got = committed;
    want.sort();
    got.sort();
    assert_eq!(got, want);
    assert!(farm.store.put_keys().is_empty(), "no PUT");
    assert_eq!(farm.store.puts_in_flight(), 0);
    assert_eq!(counter(&cache, "kbf_write_tier_staged_total"), "3");
    for b in &all {
        assert_eq!(farm.log.located(&b.digest).1, cache.tier_id());
    }
    assert_readable(&cache, &service, &all).await;
    assert!(farm.store.put_keys().is_empty(), "reads stayed local");
}

/// Catches: staged upload objects the flusher does not PUT (so the store never gets
/// them), `ObjectFlushed` committed before the PUT, and reads that fail once flushed.
#[tokio::test]
async fn once_the_store_recovers_the_flusher_puts_the_uploaded_objects() {
    let farm = Farm::new(Capabilities::default());
    farm.store.hold_puts();
    let (cache, service) = serve(farm.start(GIB).await);
    let all = blobs(1, 4, 4096);
    let segs: Vec<Seg> = all.chunks(2).map(seg).collect();
    upload(&service, &segs).await.expect("committed");
    let objects: Vec<_> = all.iter().map(|b| farm.log.object_of(&b.digest)).collect();
    assert_eq!(farm.log.applied("object_flushed"), 0);
    farm.store.release_puts();
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2, "{round:?}");
    assert_eq!(farm.log.applied("object_flushed"), 2);
    assert_eq!(farm.store.put_keys().len(), 2);
    for object in &objects {
        assert!(
            farm.store
                .stored(&support::key(&cache, *object))
                .await
                .is_some()
        );
    }
    assert_readable(&cache, &service, &all).await;
}

/// Catches: a full tier that fails the upload, or loses a segment; and a fallback that
/// stops counting. At the budget the segments past it are PUT as before and indexed
/// by `PutBlobs`, the commit succeeds, and every blob reads back.
#[tokio::test]
async fn a_tier_at_its_budget_falls_back_to_the_direct_put() {
    let farm = Farm::new(Capabilities::default());
    let (cache, service) = serve(farm.start(10_000).await);
    let all = blobs(1, 3, 4096);
    let segs: Vec<Seg> = all.chunks(1).map(seg).collect();
    let committed = upload(&service, &segs).await.expect("committed");
    assert_eq!(committed.len(), 3);
    assert_eq!(counter(&cache, "kbf_write_tier_staged_total"), "2");
    assert_eq!(counter(&cache, "kbf_write_tier_full_total"), "1");
    assert_eq!(
        farm.store.put_keys().len(),
        1,
        "one segment past the budget"
    );
    assert_eq!(farm.log.applied("put_blobs"), 1);
    assert_readable(&cache, &service, &all).await;
}

/// Catches: an acknowledged upload that a restart loses (the commit answered before
/// the block was synced, or the index not read back at the next start), and a block
/// that is not flushed after the restart. The store is down for the commit; the disk
/// loses what was not synced.
#[tokio::test]
async fn a_committed_upload_survives_a_restart_and_is_flushed_after_it() {
    let farm = Farm::new(Capabilities::default());
    farm.store.hold_puts();
    let (cache, service) = serve(farm.start(GIB).await);
    let all = blobs(1, 4, 4096);
    let segs: Vec<Seg> = all.chunks(2).map(seg).collect();
    upload(&service, &segs).await.expect("committed");
    drop(service);
    drop(cache);

    let cut = farm.fs.crash(0);
    let (cache, startup) = start(&cut, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert!(startup.recovery.lost.is_empty(), "{startup:?}");
    let (cache, service) = serve(cache);
    assert!(farm.store.put_keys().is_empty());
    assert_readable(&cache, &service, &all).await;
    farm.store.release_puts();
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2, "{round:?}");
    assert_readable(&cache, &service, &all).await;
}

/// Catches: an upload to a cache with no tier that is refused or staged somewhere that
/// does not exist, and a `ReadRange` that needs a tier to answer. Without a tier the
/// segment is PUT and indexed by `PutBlobs`, and read back from the store.
#[tokio::test]
async fn without_a_tier_an_upload_is_put_and_read_from_the_store() {
    let farm = Farm::new(Capabilities::default());
    let cache = Cache::open(farm.log.clone(), farm.store.clone(), KeyPrefix::default())
        .await
        .expect("open");
    let (cache, service) = serve(cache);
    let all = blobs(1, 2, 4096);
    upload(&service, &[seg(&all)]).await.expect("committed");
    assert_eq!(farm.store.put_keys().len(), 1);
    assert_eq!(farm.log.applied("put_blobs"), 1);
    assert_eq!(farm.log.applied("put_staged"), 0);
    assert_readable(&cache, &service, &all).await;
}

/// Catches: a failed `PutStaged` answered as a committed upload, and a fallback to the
/// PUT after a commit that may have applied (a retry is the daemon's). The commit is
/// refused, so the upload fails, nothing is PUT and no blob is present.
#[tokio::test]
async fn a_refused_put_staged_fails_the_commit_and_indexes_nothing() {
    let farm = Farm::new(Capabilities::default());
    let (cache, service) = serve(farm.start(GIB).await);
    farm.log.refuse("put_staged", 1);
    let all = blobs(1, 2, 4096);
    let failed = upload(&service, &[seg(&all)]).await;
    assert!(failed.is_err(), "{failed:?}");
    assert!(farm.store.put_keys().is_empty());
    let digests: Vec<Digest> = all.iter().map(|b| b.digest).collect();
    assert_eq!(cache.find_missing(&digests).await.expect("find"), digests);
}

/// Catches: a `ReadRange` of an object neither the tier nor the store holds answered
/// with bytes, or with an error other than NOT_FOUND.
#[tokio::test]
async fn a_range_of_an_object_nobody_holds_is_not_found() {
    let farm = Farm::new(Capabilities::default());
    let (_cache, service) = serve(farm.start(GIB).await);
    let range = ReadRangeRequest {
        object: Some(ObjectRef {
            epoch: 1,
            seq: 999,
            ..ObjectRef::default()
        }),
        offset: 0,
        length: 16,
    };
    let status = service.read_range(Request::new(range)).await;
    assert_eq!(status.expect_err("not found").code(), tonic::Code::NotFound);
}

/// Catches: a store failure on a range the tier does not hold answered as anything but
/// UNAVAILABLE, a short or overlong range passed on as the range, and an offset past
/// the largest a range can start at answered with bytes. Without a tier every range is
/// the store's; the same checks run in `segments.rs` over its own types, and the
/// coverage of the cache's generic read path needs them in this binary too.
#[tokio::test]
async fn a_range_the_tier_does_not_hold_is_answered_by_the_store_or_refused() {
    let farm = Farm::new(Capabilities::default());
    let cache = Cache::open(farm.log.clone(), farm.store.clone(), KeyPrefix::default())
        .await
        .expect("open");
    let (_cache, service) = serve(cache);
    let all = blobs(1, 1, 600);
    let s = seg(&all);
    upload(&service, std::slice::from_ref(&s))
        .await
        .expect("committed");
    let at = service
        .locate(Request::new(kbf_proto::worker::LocateRequest {
            digests: vec![kbf_front::digest_to_proto(&all[0].digest)],
        }))
        .await
        .expect("locate")
        .into_inner()
        .locations
        .remove(0);
    let range = |offset: u64, length: u64| {
        Request::new(ReadRangeRequest {
            object: at.object,
            offset,
            length,
        })
    };
    farm.store.on_get(GetFault::Transport);
    let failed = service.read_range(range(at.offset, 600)).await;
    assert_eq!(failed.expect_err("store failed").code(), Code::Unavailable);
    let overrun = service.read_range(range(at.offset, s.length + 10)).await;
    assert_eq!(overrun.expect_err("overrun").code(), Code::NotFound);
    let past = service.read_range(range(u64::MAX - 1, 10)).await;
    assert_eq!(past.expect_err("past").code(), Code::NotFound);
    let whole = service.read_range(range(at.offset, 600)).await;
    assert_eq!(
        whole.expect("range").into_inner().bytes,
        all[0].bytes.as_ref()
    );
}

/// Catches: a tier-mode upload that hashes on the acknowledgement's path (the server
/// refusing a corrupt record at `PutRange` or `Commit`, the cost the flusher exists to
/// take off it), a flusher that PUTs the corrupt record, one that loses the sound
/// records of the same segment, a drop nobody counts, and a bad digest that stays
/// present. One record of three is altered after `Begin`: the upload is acknowledged;
/// the flush drops the bad record (counted, unavailable), stages the two sound ones
/// again and PUTs them on the next round, with a store object that holds neither the
/// bad digest nor its bytes; a new upload of the good bytes heals it.
#[tokio::test]
async fn a_corrupt_record_is_acknowledged_and_caught_at_the_flush() {
    let farm = Farm::new(Capabilities::default());
    let (cache, service) = serve(farm.start(GIB).await);
    let all = blobs(1, 3, 4096);
    let good = seg(&all);
    let (sent, at) = altered(&good, &all[1]);
    let committed = upload(&service, std::slice::from_ref(&sent))
        .await
        .expect("acknowledged: nothing is hashed on the way");
    assert_eq!(committed.len(), 3);
    assert_eq!(counter(&cache, "kbf_tier_flush_digest_mismatch_total"), "0");
    let object = farm.log.object_of(&all[1].digest);
    assert!(farm.store.put_keys().is_empty());

    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(
        (&round.corrupt, &round.dropped, &round.flushed),
        (&vec![object], &vec![all[1].digest], &vec![])
    );
    assert!(
        farm.store.put_keys().is_empty(),
        "the bad record is not PUT"
    );
    assert_eq!(counter(&cache, "kbf_tier_flush_digest_mismatch_total"), "1");
    assert_eq!(farm.log.answer(&all[1].digest), BlobAnswer::Unavailable);
    let sound = [all[0].clone(), all[2].clone()];
    assert_readable(&cache, &service, &sound).await;
    let digests: Vec<Digest> = all.iter().map(|b| b.digest).collect();
    assert_eq!(
        cache.find_missing(&digests).await.expect("find"),
        [all[1].digest]
    );

    let again = cache.flush_tier(later()).await.expect("round");
    assert_eq!(again.flushed.len(), 1, "{again:?}");
    let puts = farm.store.put_keys();
    assert_eq!(puts.len(), 1, "one object, the sound records");
    let stored = farm
        .store
        .stored(&support::key(&cache, again.flushed[0]))
        .await
        .expect("stored");
    let reader = SegmentReader::open(&stored).expect("a segment");
    for b in &sound {
        assert_eq!(reader.get(&b.digest).expect("get"), Some(&b.bytes[..]));
    }
    assert_eq!(reader.get(&all[1].digest).expect("absent"), None);
    let bad_record = &sent.records[at..at + 4096];
    assert!(
        !stored.windows(4096).any(|w| w == bad_record),
        "the corrupt record's bytes reached the store"
    );
    assert_readable(&cache, &service, &sound).await;

    upload(&service, &[seg(&all[1..2])]).await.expect("healed");
    assert_readable(&cache, &service, &all).await;
}

/// Catches: a corrupt record read back from the tier before its flush and served (the
/// server verifies the block it reads), and one that is then flushed anyway. The read
/// is refused, its object is marked corrupt and the flusher has nothing left to do.
#[tokio::test]
async fn a_corrupt_staged_record_is_not_served_before_the_flush() {
    let farm = Farm::new(Capabilities::default());
    let (cache, service) = serve(farm.start(GIB).await);
    let all = blobs(1, 2, 4096);
    let (sent, _) = altered(&seg(&all), &all[0]);
    upload(&service, &[sent]).await.expect("acknowledged");
    let object = farm.log.object_of(&all[0].digest);
    let read = cache.read_blob(&all[0].digest).await;
    assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    assert_eq!(
        farm.log.unreachable(object),
        Some(kbf_meta::UnreachableReason::Corrupt)
    );
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round, kbf_front::FlushRound::default());
    assert!(farm.store.put_keys().is_empty());
}

/// Catches: a segment the tier does not take PUT unverified (the corrupt record in the
/// object store), a daemon's false CRC-32C copied into the stored footer, and a
/// refusal that is not the client's (INVALID_ARGUMENT). The tier is smaller than the
/// segment: an altered record is refused with nothing PUT, and a segment whose footer
/// declared false CRC-32Cs is stored with true ones.
#[tokio::test]
async fn a_segment_the_tier_does_not_take_is_hashed_before_its_put() {
    let farm = Farm::new(Capabilities::default());
    let (cache, service) = serve(farm.start(1000).await);
    let all = blobs(1, 2, 4096);
    let good = seg(&all);
    let (sent, _) = altered(&good, &all[0]);
    let refused = upload(&service, &[sent]).await;
    let refused = refused.expect_err("a corrupt record on the direct path");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    assert!(farm.store.put_keys().is_empty(), "nothing PUT");
    assert_eq!(counter(&cache, "kbf_write_tier_full_total"), "1");
    let digests: Vec<Digest> = all.iter().map(|b| b.digest).collect();
    assert_eq!(cache.find_missing(&digests).await.expect("find"), digests);

    let whole = [good.records.as_slice(), good.footer.as_slice()].concat();
    let parsed = Footer::parse(&whole, good.length).expect("footer");
    let lying: Vec<IndexEntry> = parsed
        .entries()
        .iter()
        .map(|e| IndexEntry {
            crc32c: 0xdead_beef,
            ..*e
        })
        .collect();
    let lie = Seg {
        footer: encode_footer(parsed.records_len(), &lying),
        ..good
    };
    upload(&service, &[lie]).await.expect("committed");
    assert_eq!(farm.store.put_keys().len(), 1);
    let object = farm.log.object_of(&all[0].digest);
    let stored = farm
        .store
        .stored(&support::key(&cache, object))
        .await
        .expect("stored");
    let reader = SegmentReader::open(&stored).expect("a segment");
    for b in &all {
        assert_eq!(
            reader.get(&b.digest).expect("crc checked"),
            Some(&b.bytes[..])
        );
    }
}

/// Catches a large record held whole in memory when the write tier has no room for it
/// (mutant: the whole-body fallback kept for it), or refused for its size (mutant: the
/// refusal restored): at the tier's budget, a `Begin` declaring a record one byte over
/// `MAX_UNSTAGED_BLOB_BYTES` is answered FAILED_PRECONDITION, at which the client sends
/// it by ByteStream; one declaring records of at most that size is admitted (held in
/// memory, as before).
#[tokio::test]
async fn a_large_record_a_full_tier_cannot_take_is_sent_to_bytestream() {
    let farm = Farm::new(Capabilities::default());
    let (_cache, service) = serve(farm.start(10_000).await);
    let over = kbf_front::MAX_UNSTAGED_BLOB_BYTES + 1;
    let entry = kbf_segments::IndexEntry {
        digest: Digest::new(kbf_types::DigestFunction::Sha256, [5; 32], over),
        offset: 0,
        crc32c: 0,
    };
    let large = UploadSegment {
        length: over + kbf_segments::layout::footer_len(1),
        footer: kbf_segments::layout::encode_footer(over, &[entry]),
    };
    let refused = service
        .begin(tonic::Request::new(BeginRequest {
            segments: vec![large],
        }))
        .await
        .expect_err("sent to ByteStream");
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
    assert!(
        refused.message().contains("send it by a ByteStream Write")
            && refused.message().contains("16777216"),
        "{refused:?}"
    );
    let all = blobs(1, 2, 4096);
    upload(&service, &[seg(&all)]).await.expect("small records");
}

/// Catches a range the write tier failed to write taken for received (the commit would
/// stage bytes that are not there), or a failed write that cannot be sent again: with
/// the tier's write failing once, the `PutRange` fails UNAVAILABLE and the commit
/// refuses the record as not wholly received; the same range sent again to a new upload
/// is written and the upload commits, its blobs read back.
#[tokio::test]
async fn a_range_the_tier_failed_to_write_is_not_received() {
    let farm = Farm::new(Capabilities::default());
    let (cache, service) = serve(farm.start(GIB).await);
    let all = blobs(1, 2, 4096);
    let s = seg(&all);
    let begin = || BeginRequest {
        segments: vec![UploadSegment {
            length: s.length,
            footer: s.footer.clone(),
        }],
    };
    let upload = service
        .begin(Request::new(begin()))
        .await
        .expect("begun")
        .into_inner()
        .upload;
    let open = format!("{service:?}");
    assert!(
        open.contains("TierRange"),
        "the records go to the tier: {open}"
    );
    // The next filesystem call is the range's write.
    farm.fs.fail_at(farm.fs.ops(), kbf_store::Fault::Error);
    let put = PutRangeRequest {
        upload,
        segment: 0,
        offset: 0,
        bytes: s.records.clone(),
    };
    let failed = service
        .put_range(Request::new(put))
        .await
        .expect_err("not written");
    assert_eq!(failed.code(), Code::Unavailable, "{failed:?}");
    assert!(failed.message().contains("write tier"), "{failed:?}");
    let refused = service
        .commit(Request::new(CommitRequest { upload }))
        .await
        .expect_err("nothing received");
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
    upload_and_read(&cache, &service, &all, &[s]).await;
}

/// Uploads `segs` and checks every one of `all` reads back.
async fn upload_and_read(cache: &TierCache, service: &Service, all: &[TestBlob], segs: &[Seg]) {
    upload(service, segs).await.expect("committed");
    for b in all {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
}

/// Catches a segment that found no room on the tier at `Begin` PUT at its commit even
/// though the tier has room by then: the first upload fills the tier, the second's
/// `Begin` gets no range (its records are held in memory), a flush frees the tier, and
/// the second's commit stages its segment, with no PUT of its own.
#[tokio::test]
async fn a_segment_with_no_room_at_begin_is_staged_at_commit_once_there_is() {
    let farm = Farm::new(Capabilities::default());
    let (cache, service) = serve(farm.start(10_000).await);
    let first = blobs(1, 2, 4096);
    upload(&service, &[seg(&first)])
        .await
        .expect("first upload");
    let second = blobs(3, 1, 4096);
    let s = seg(&second);
    let begin = BeginRequest {
        segments: vec![UploadSegment {
            length: s.length,
            footer: s.footer.clone(),
        }],
    };
    let id = service
        .begin(Request::new(begin))
        .await
        .expect("begin")
        .into_inner()
        .upload;
    let put = PutRangeRequest {
        upload: id,
        segment: 0,
        offset: 0,
        bytes: s.records.clone(),
    };
    service.put_range(Request::new(put)).await.expect("range");
    let round = cache.flush_tier(later()).await.expect("flush");
    assert_eq!(round.flushed.len(), 1, "{round:?}");
    let commit = service.commit(Request::new(CommitRequest { upload: id }));
    commit.await.expect("committed");
    let (_, staged_on) = farm.log.located(&second[0].digest);
    assert_eq!(staged_on, cache.tier_id(), "staged at the commit");
    assert_eq!(
        farm.store.put_keys().len(),
        1,
        "only the first upload's flush"
    );
    assert_readable(&cache, &service, &second).await;
}
