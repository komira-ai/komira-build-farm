//! `kbf.worker.v1.Segments` over loopback gRPC (`docs/design/write-path.md`, section 4
//! and the "Segment upload (PR 2)" table of section 9): uploads that commit once, the
//! checks that keep a daemon from poisoning the cache, uploads dropped with their
//! connection or when idle, and range reads.

#[path = "segments/support.rs"]
mod support;
#[path = "segments/untracked.rs"]
mod untracked;

use std::time::Duration;

use kbf_front::{
    MAX_RANGE_BYTES, MAX_UPLOAD_RECORDS, MIN_SEGMENT_UPLOAD_BYTES, SegmentsConfig, WriteConfig,
};
use kbf_proto::worker::{BeginRequest, CommitRequest, ObjectRef, ReadRangeRequest, UploadSegment};
use kbf_segments::layout::footer_len;
use kbf_segments::{Footer, IndexEntry};
use tonic::Code;

use support::{Front, Seg, blob, put_all, seg, seg_with, try_put_all};

/// Catches per-segment commits (mutant: commit per segment): an upload of three
/// segments is stored as three objects and indexed by **one** `PutBlobs`, and every
/// blob is present after.
#[tokio::test]
async fn an_upload_of_three_segments_is_one_commit() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let segs: Vec<Seg> = (0..3)
        .map(|s| {
            seg(&(0..5)
                .map(|i| blob(s * 10 + i, 100 + i))
                .collect::<Vec<_>>())
        })
        .collect();
    let mut client = front.client().await;
    let upload = front.begin(&mut client, &segs).await;
    for (i, s) in segs.iter().enumerate() {
        put_all(&mut client, upload, i, s, 64, false).await;
    }
    let reply = client
        .commit(CommitRequest { upload })
        .await
        .expect("commit")
        .into_inner();
    assert_eq!(reply.committed.len(), 15);
    let metrics = front.cache.metrics();
    assert_eq!(metrics.segment_put.count(), 3, "one object per segment");
    assert_eq!(
        metrics.segment_commit.count(),
        1,
        "one PutBlobs for the upload"
    );
    for s in &segs {
        front.assert_present(&s.digests, true).await;
    }
}

/// Catches trusting the daemon's footer (mutant: no SHA-256 check), and a check that
/// skips some record (mutants: only the first record in digest order hashed; the first
/// never hashed): each record in turn, in digest order (first, middle and last), is
/// altered after `Begin`; each such upload's range that completes it, and its commit,
/// are INVALID_ARGUMENT naming that record, and nothing of it is stored or indexed, the
/// good records included. The same upload unaltered then commits, so the refusals
/// were for the altered bytes.
#[tokio::test]
async fn each_record_altered_after_begin_fails_the_commit_and_indexes_nothing() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&[blob(1, 300), blob(2, 300), blob(3, 300), blob(4, 300)]);
    let whole = [s.records.as_slice(), s.footer.as_slice()].concat();
    let footer = Footer::parse(&whole, s.length).expect("footer");
    let entries = footer.entries();
    assert_eq!(entries.len(), 4);
    assert!(
        entries.windows(2).all(|w| w[0].digest < w[1].digest),
        "the footer lists records in digest order"
    );
    let mut client = front.client().await;
    for (i, entry) in entries.iter().enumerate() {
        let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
        let mut altered = s.records.clone();
        altered[usize::try_from(entry.offset).expect("offset")] ^= 0xff;
        let sent = Seg {
            records: altered,
            ..s.clone()
        };
        // Records are checked as their bytes arrive: the range that completes the
        // altered record is refused, and a commit after it is refused too.
        let at_range = try_put_all(&mut client, upload, 0, &sent, 4096, false)
            .await
            .expect("the range completing the altered record is refused");
        let at_commit = client
            .commit(CommitRequest { upload })
            .await
            .expect_err("altered record");
        for refused in [at_range, at_commit] {
            assert_eq!(
                refused.code(),
                Code::InvalidArgument,
                "record {i}: {refused:?}"
            );
            assert!(
                refused
                    .message()
                    .contains(&format!("record at {} ", entry.offset)),
                "record {i} named: {refused:?}"
            );
        }
        front.assert_present(&s.digests, false).await;
        assert_eq!(
            front.cache.metrics().segment_put.count(),
            0,
            "record {i}: nothing stored"
        );
        assert_eq!(
            front.cache.metrics().segment_commit.count(),
            0,
            "record {i}: nothing committed"
        );
    }
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 4096, false).await;
    client
        .commit(CommitRequest { upload })
        .await
        .expect("the unaltered upload");
    front.assert_present(&s.digests, true).await;
}

/// Catches indexing bytes never received (mutant: no coverage check at commit): a
/// commit with a record's bytes missing is FAILED_PRECONDITION and the upload stays
/// open, so sending the rest lets it commit; and a range overlapping a received one
/// with other bounds is refused (mutant: no overlap check).
#[tokio::test]
async fn a_missing_range_is_refused_at_commit_and_an_overlap_at_once() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&[blob(3, 1000), blob(4, 1000)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    front
        .put(&mut client, upload, 0, 0, &s.records[..1500])
        .await
        .expect("first part");
    let refused = client
        .commit(CommitRequest { upload })
        .await
        .expect_err("a record half sent");
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
    front.assert_present(&s.digests, false).await;

    let overlap = front
        .put(&mut client, upload, 0, 1000, &s.records[1000..1800])
        .await;
    assert_eq!(overlap.expect_err("overlap").code(), Code::InvalidArgument);

    front
        .put(&mut client, upload, 0, 1500, &s.records[1500..])
        .await
        .expect("the rest");
    client
        .commit(CommitRequest { upload })
        .await
        .expect("commit");
    front.assert_present(&s.digests, true).await;
}

/// Catches verified bytes rewritten after the check (mutant: a resend overwrites): a
/// resend of a received range with other bytes is OK and writes nothing, a range sent
/// while the commit runs is FAILED_PRECONDITION, and the indexed records hash to
/// their digests.
#[tokio::test]
async fn a_resend_writes_nothing_and_a_range_during_commit_is_refused() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&[blob(5, 500), blob(6, 500)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 250, false).await;
    let other = vec![0xaa; 250];
    front
        .put(&mut client, upload, 0, 250, &other)
        .await
        .expect("resend is OK");

    front.store.hold();
    let mut committing = client.clone();
    let commit = tokio::spawn(async move { committing.commit(CommitRequest { upload }).await });
    front.store.wait_for_put().await;
    let late = front
        .put(&mut client, upload, 0, 0, &s.records[..250])
        .await;
    assert_eq!(
        late.expect_err("during commit").code(),
        Code::FailedPrecondition
    );
    front.store.release();
    commit.await.expect("task").expect("commit");
    front.assert_present(&s.digests, true).await;
    front.assert_stored_records_check(&s.digests).await;
}

/// Catches a lost-reply retry storing and committing twice, or answered otherwise
/// than the first (mutant: a second Commit refused): two Commits of one upload, the
/// second sent while the first is storing, get the same reply; one object, one
/// `PutBlobs`.
#[tokio::test]
async fn two_commits_of_one_upload_store_once_and_answer_alike() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&[blob(7, 64), blob(8, 64), blob(9, 64)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 1 << 20, false).await;
    front.store.hold();
    let mut first = client.clone();
    let mut second = client.clone();
    let a = tokio::spawn(async move { first.commit(CommitRequest { upload }).await });
    front.store.wait_for_put().await;
    let b = tokio::spawn(async move { second.commit(CommitRequest { upload }).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Wall-clock only: this does not prove the second Commit reached the server before
    // the release, only that, if it did, it was not answered while the PUT was held.
    assert!(
        !b.is_finished(),
        "a second Commit answered during the first's PUT"
    );
    front.store.release();
    let a = a.await.expect("task").expect("first").into_inner();
    let b = b.await.expect("task").expect("second").into_inner();
    assert_eq!(a, b);
    let again = client
        .commit(CommitRequest { upload })
        .await
        .expect("third");
    assert_eq!(again.into_inner(), a, "an answered upload answers the same");
    assert_eq!(front.cache.metrics().segment_put.count(), 1);
    assert_eq!(front.cache.metrics().segment_commit.count(), 1);
}

/// Catches a daemon's CRC-32C reaching the store (mutant: the declared CRC kept): a
/// footer whose CRC for one record is false commits, and the stored footer parses and
/// holds the true CRC, so every record passes `IndexEntry::verify`.
#[tokio::test]
async fn a_false_crc_in_begin_never_reaches_the_stored_footer() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg_with(&[blob(10, 200), blob(11, 200)], |e| IndexEntry {
        crc32c: e.crc32c ^ 1,
        ..e
    });
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 4096, false).await;
    client
        .commit(CommitRequest { upload })
        .await
        .expect("commit");
    front.assert_stored_records_check(&s.digests).await;
}

/// Catches a dead daemon stalling every write (mutant: the connection's close not
/// watched): an upload whose connection closes after `Begin` gives its memory back at
/// once, long before the idle time.
#[tokio::test]
async fn a_closed_connection_frees_its_upload_at_once() {
    let config = SegmentsConfig {
        idle: Duration::from_secs(3600),
        ..SegmentsConfig::DEFAULT
    };
    let front = Front::start(config).await;
    let s = seg(&[blob(12, 1 << 16)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    assert!(front.cache.bytes_admitted() >= s.length);
    let mut other = front.client().await;
    let refused = other
        .commit(CommitRequest { upload })
        .await
        .expect_err("other connection");
    assert_eq!(
        refused.code(),
        Code::NotFound,
        "an upload belongs to its connection"
    );
    drop(client);
    front.wait_until_no_uploads().await;
    assert_eq!(front.cache.bytes_admitted(), 0);
}

/// Catches unbounded server memory per read (mutant: no length check): a `ReadRange`
/// over 4 MiB, or of nothing, or naming no object, is INVALID_ARGUMENT before the
/// store is asked. Also catches a range whose end overflows `u64` sent to the store
/// (mutant: `ByteRange::new` without its overflow check): such a range of a committed
/// object is NOT_FOUND while the store's next GET would fail, so only a refusal
/// before the store answers NOT_FOUND. A range of an absent object is NOT_FOUND.
#[tokio::test]
async fn a_read_range_over_the_limit_is_refused() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let mut client = front.client().await;
    for length in [MAX_RANGE_BYTES + 1, 0] {
        let refused = client
            .read_range(ReadRangeRequest {
                object: Some(ObjectRef { epoch: 1, seq: 1 }),
                offset: 0,
                length,
            })
            .await
            .expect_err("refused");
        assert_eq!(
            refused.code(),
            Code::InvalidArgument,
            "{length}: {refused:?}"
        );
    }
    let unnamed = client
        .read_range(ReadRangeRequest {
            object: None,
            offset: 0,
            length: 10,
        })
        .await
        .expect_err("no object");
    assert_eq!(unnamed.code(), Code::InvalidArgument, "{unnamed:?}");
    let s = seg(&[blob(70, 300)]);
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 4096, false).await;
    client
        .commit(CommitRequest { upload })
        .await
        .expect("commit");
    let committed = front.locate(&mut client, &s.digests).await[0].object;
    assert!(committed.is_some(), "the blob is located");
    front.store.fail_next_gets(1);
    let past_the_end = client
        .read_range(ReadRangeRequest {
            object: committed,
            offset: u64::MAX - 1,
            length: 10,
        })
        .await
        .expect_err("a range past the largest offset");
    assert_eq!(past_the_end.code(), Code::NotFound, "{past_the_end:?}");
    front.store.fail_next_gets(0);
    let absent = client
        .read_range(ReadRangeRequest {
            object: Some(ObjectRef { epoch: 1, seq: 999 }),
            offset: 0,
            length: 10,
        })
        .await
        .expect_err("no such object");
    assert_eq!(absent.code(), Code::NotFound);
}

/// Catches assuming ranges arrive in order (mutant: a range must start where the last
/// ended): ranges sent last to first commit, and every blob reads back.
#[tokio::test]
async fn ranges_in_reverse_order_commit() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&(0..8).map(|i| blob(20 + i, 333)).collect::<Vec<_>>());
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 100, true).await;
    client
        .commit(CommitRequest { upload })
        .await
        .expect("commit");
    front.assert_present(&s.digests, true).await;
    front.assert_stored_records_check(&s.digests).await;
}

/// Catches leaked uploads (mutant: the idle timer never drops one): an upload no call
/// names for the idle time is dropped, its memory returned, nothing indexed, and a
/// later call naming it is NOT_FOUND.
#[tokio::test]
async fn an_idle_upload_is_dropped() {
    let config = SegmentsConfig {
        idle: Duration::from_millis(200),
        ..SegmentsConfig::DEFAULT
    };
    let front = Front::start(config).await;
    let s = seg(&[blob(30, 1000)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    front
        .put(&mut client, upload, 0, 0, &s.records[..500])
        .await
        .expect("part");
    front.wait_until_no_uploads().await;
    assert_eq!(front.cache.bytes_admitted(), 0);
    let late = front
        .put(&mut client, upload, 0, 500, &s.records[500..])
        .await;
    assert_eq!(late.expect_err("dropped").code(), Code::NotFound);
    front.assert_present(&s.digests, false).await;
}

/// Catches a `Begin` the server cannot hold, or one it would wait on forever: a footer
/// that does not parse for its length, overlapping records, and a total over the cap
/// are INVALID_ARGUMENT; an upload of exactly the cap is admitted. Also catches bytes
/// sent in front of a footer read as if absent (mutant: the footer length unchecked):
/// a footer with one byte prepended is INVALID_ARGUMENT.
#[tokio::test]
async fn begin_refuses_what_it_cannot_take() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let mut client = front.client().await;
    let s = seg(&[blob(40, 100), blob(41, 100)]);
    let wrong_length = UploadSegment {
        length: s.length + 1,
        footer: s.footer.clone(),
    };
    let refused = client
        .begin(BeginRequest {
            segments: vec![wrong_length],
        })
        .await
        .expect_err("footer for another length");
    assert_eq!(refused.code(), Code::InvalidArgument);

    let overlapping = seg_with(&[blob(42, 100), blob(43, 100)], |e| IndexEntry {
        offset: e.offset.min(50),
        ..e
    });
    let refused = client
        .begin(BeginRequest {
            segments: vec![overlapping.upload_segment()],
        })
        .await
        .expect_err("overlap");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");

    let mut prefixed = s.upload_segment();
    prefixed.footer.insert(0, 0);
    let refused = client
        .begin(BeginRequest {
            segments: vec![prefixed],
        })
        .await
        .expect_err("a byte in front of the footer");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");

    let empty = client.begin(BeginRequest { segments: vec![] }).await;
    assert_eq!(empty.expect_err("no segment").code(), Code::InvalidArgument);

    // No record is too large; without a write tier one over the in-memory limit is
    // sent back to ByteStream (FAILED_PRECONDITION, which the client falls back at),
    // not refused as too large.
    let huge = support::declared_only(2 * ((5 << 30) + 1) + footer_len(2));
    let refused = client
        .begin(BeginRequest {
            segments: vec![huge],
        })
        .await
        .expect_err("records too large to hold in memory, and no tier");
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
    assert!(
        refused.message().contains("send it by a ByteStream Write"),
        "{refused:?}"
    );

    let cap = SegmentsConfig::DEFAULT.max_bytes;
    let over = support::declared_small(cap + 1);
    let refused = client
        .begin(BeginRequest {
            segments: vec![over],
        })
        .await
        .expect_err("over the cap");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    let at_cap = support::declared_small(cap);
    client
        .begin(BeginRequest {
            segments: vec![at_cap],
        })
        .await
        .expect("an upload of exactly the cap");
}

/// Catches an upload of more records than one commit indexes (mutant: the record
/// limit unchecked): a `Begin` of `MAX_UPLOAD_RECORDS` one-byte records is admitted,
/// and one of that many plus one, split over two segments so the limit is on the
/// upload and not the segment, is INVALID_ARGUMENT.
#[tokio::test]
async fn begin_refuses_more_records_than_the_limit() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let mut client = front.client().await;
    let limit = u32::try_from(MAX_UPLOAD_RECORDS).expect("limit");
    client
        .begin(BeginRequest {
            segments: vec![support::declared_records(limit, 1)],
        })
        .await
        .expect("an upload of exactly the limit");
    let refused = client
        .begin(BeginRequest {
            segments: vec![
                support::declared_records(limit, 1),
                support::declared_records(1, 2),
            ],
        })
        .await
        .expect_err("one record over the limit");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
}

/// How the first upload of [`a_begin_waits_for_the_budget`] gives its budget back.
#[derive(Clone, Copy, Debug)]
enum Release {
    Commit,
    CloseConnection,
}

/// One upload holds most of a 4 KiB budget; a second `Begin` that does not fit must
/// wait in the server (counted by `begins_waiting`) and must not finish while the
/// first is open; once `release` ends the first, the second is admitted, holds its
/// own budget, and commits. Every wait ends on an event; the timeouts only fail.
async fn a_begin_waits_for_the_budget(release: Release) {
    let config = SegmentsConfig {
        max_bytes: 4 << 10,
        idle: Duration::from_secs(3600),
    };
    let writes = WriteConfig {
        bytes_in_flight: 4 << 10,
        ..WriteConfig::DEFAULT
    };
    let front = Front::start_with(config, writes).await;
    let first = seg(&[blob(60, 1400), blob(61, 1400)]);
    let second = seg(&[blob(62, 1500)]);
    assert!(
        first.length + second.length > 4 << 10,
        "the two uploads do not fit together"
    );
    let mut a = front.client().await;
    let held = front.begin(&mut a, std::slice::from_ref(&first)).await;
    assert!(front.cache.bytes_admitted() >= first.length);

    let mut b = front.client().await;
    let segments = vec![second.upload_segment()];
    let waiting = tokio::spawn(async move {
        let begun = b.begin(BeginRequest { segments }).await;
        (b, begun)
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while front.service.begins_waiting() == 0 {
            assert!(
                !waiting.is_finished(),
                "the second Begin finished while the budget was held"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the second Begin waits in the server");
    assert!(!waiting.is_finished(), "the second Begin waits");
    assert_eq!(front.service.open_uploads(), 1);

    match release {
        Release::Commit => {
            put_all(&mut a, held, 0, &first, 4096, false).await;
            a.commit(CommitRequest { upload: held })
                .await
                .expect("the first commits");
            front.assert_present(&first.digests, true).await;
        }
        Release::CloseConnection => drop(a),
    }

    let (mut b, begun) = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("the second Begin is admitted once the first ends")
        .expect("task");
    let upload = begun
        .expect("the second Begin is admitted, not refused")
        .into_inner()
        .upload;
    assert_eq!(front.service.begins_waiting(), 0);
    assert!(front.cache.bytes_admitted() >= second.length);
    put_all(&mut b, upload, 0, &second, 4096, false).await;
    b.commit(CommitRequest { upload })
        .await
        .expect("the second commits");
    front.assert_present(&second.digests, true).await;
    if let Release::CloseConnection = release {
        front.assert_present(&first.digests, false).await;
    }
}

/// Catches a `Begin` refused for load (mutant: `try_acquire` for `acquire`, so
/// concurrent daemons fall back to ByteStream): a second `Begin` waits while the
/// budget is held and is admitted once the first upload commits.
#[tokio::test]
async fn a_begin_waits_for_the_budget_until_the_holder_commits() {
    a_begin_waits_for_the_budget(Release::Commit).await;
}

/// As above, and the budget comes back when the holder's connection closes: the
/// second `Begin` is admitted once the first upload is dropped.
#[tokio::test]
async fn a_begin_waits_for_the_budget_until_the_holder_is_dropped() {
    a_begin_waits_for_the_budget(Release::CloseConnection).await;
}

/// Catches `Locate` answering a blob it does not hold, and `ReadRange` serving other
/// bytes than the object holds: a located blob's range reads back as the blob.
#[tokio::test]
async fn locate_and_read_range_return_a_blob() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&[blob(50, 777), blob(51, 888)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 4096, false).await;
    client
        .commit(CommitRequest { upload })
        .await
        .expect("commit");
    let absent = kbf_segments::sha256(b"never uploaded");
    let mut asked = s.digests.clone();
    asked.push(absent);
    let located = front.locate(&mut client, &asked).await;
    assert!(
        located[2].object.is_none(),
        "an absent blob has no location"
    );
    for (i, digest) in s.digests.iter().enumerate() {
        let at = &located[i];
        let bytes = client
            .read_range(ReadRangeRequest {
                object: at.object,
                offset: at.offset,
                length: digest.size_bytes,
            })
            .await
            .expect("read")
            .into_inner()
            .bytes;
        assert_eq!(kbf_segments::sha256(&bytes), *digest);
    }
}

/// Catches a committing upload dropped with its connection (mutant: the drop ignores a
/// running commit), and a commit tied to the call that started it (mutant: the commit
/// runs in the handler): a connection closed while the commit's PUT is held leaves the
/// upload open until the commit ends, and the commit still indexes every record. Also
/// catches the memory budget returned while the commit still holds the buffers
/// (mutant: the budget dropped before the check and PUT): while the PUT is held the
/// upload's length stays counted.
#[tokio::test]
async fn a_connection_closed_during_the_commit_leaves_the_commit_to_finish() {
    let config = SegmentsConfig {
        idle: Duration::from_secs(3600),
        ..SegmentsConfig::DEFAULT
    };
    let front = Front::start(config).await;
    let s = seg(&[blob(60, 400), blob(61, 400)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 4096, false).await;
    front.store.hold();
    let mut committing = client.clone();
    let call = tokio::spawn(async move { committing.commit(CommitRequest { upload }).await });
    front.store.wait_for_put().await;
    assert!(
        front.cache.bytes_admitted() >= s.length,
        "the budget is held while the commit holds the buffers"
    );
    call.abort();
    let _ = call.await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(front.service.open_uploads(), 1, "dropped while committing");
    front.assert_present(&s.digests, false).await;
    front.store.release();
    front.wait_until_no_uploads().await;
    front.assert_present(&s.digests, true).await;
    assert_eq!(front.cache.bytes_admitted(), 0);
}

/// Catches a segment upload admitted apart from the cache's other uploads (mutant:
/// `Begin` keeps its own memory budget, as before it moved onto `Cache::admit`): with
/// `--write-bytes-in-flight` at 4 KiB and 2 KiB of it admitted elsewhere (what a
/// ByteStream `Write` or a `BatchUpdateBlobs` takes), a `Begin` of 2.5 KiB waits in the
/// server, and is admitted once those 2 KiB are released; its upload then counts
/// against the same bound until it commits. Every wait ends on an event.
#[tokio::test]
async fn a_begin_waits_for_bytes_other_uploads_hold() {
    let config = SegmentsConfig {
        max_bytes: 4 << 10,
        idle: Duration::from_secs(3600),
    };
    let writes = WriteConfig {
        bytes_in_flight: 4 << 10,
        ..WriteConfig::DEFAULT
    };
    let front = Front::start_with(config, writes).await;
    let s = seg(&[blob(70, 2500)]);
    assert!(
        s.length + (2 << 10) > 4 << 10,
        "the two do not fit together"
    );
    let held = front.cache.admit(2 << 10).await;
    assert_eq!(front.cache.bytes_admitted(), 2 << 10);

    let mut client = front.client().await;
    let segments = vec![s.upload_segment()];
    let mut begun = client.clone();
    let waiting = tokio::spawn(async move { begun.begin(BeginRequest { segments }).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while front.service.begins_waiting() == 0 {
            assert!(
                !waiting.is_finished(),
                "the Begin finished while another upload held the bound"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the Begin waits in the server");

    drop(held);
    let upload = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("the Begin is admitted once the other upload ends")
        .expect("task")
        .expect("admitted, not refused")
        .into_inner()
        .upload;
    assert_eq!(front.cache.bytes_admitted(), s.length);
    put_all(&mut client, upload, 0, &s, 4096, false).await;
    client
        .commit(CommitRequest { upload })
        .await
        .expect("commit");
    front.assert_present(&s.digests, true).await;
    assert_eq!(front.cache.bytes_admitted(), 0);
}

/// Catches an upload's segment PUT tried once (mutant: `store_upload` PUTs through the
/// store itself, as before it moved onto the cache's writer): the commit's first PUT
/// fails as a reset connection; the commit still succeeds, after a second PUT under
/// another key, and every record is indexed where the stored footer says.
#[tokio::test]
async fn a_commit_retries_a_failed_put_under_a_new_key() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&[blob(80, 700), blob(81, 900)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 4096, false).await;
    front.store.fail_next_puts(1);
    client
        .commit(CommitRequest { upload })
        .await
        .expect("the commit succeeds on the second PUT");
    let keys = front.store.put_keys();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert_ne!(keys[0], keys[1], "a retry never reuses a key");
    assert_eq!(front.cache.metrics().segment_put.count(), 2);
    front.assert_present(&s.digests, true).await;
    front.assert_stored_records_check(&s.digests).await;
}

/// Catches an upload's segments PUT past `--write-puts-in-flight` (mutant: the PUT
/// slot not taken for an upload's segments): with 2 slots, a commit of three segments
/// whose PUTs are held has exactly 2 at the store. Weak: a 200 ms wall-clock wait, so
/// a third PUT slower than that would go unseen.
#[tokio::test]
async fn an_uploads_segment_puts_take_the_put_slots() {
    let writes = WriteConfig {
        puts_in_flight: std::num::NonZeroUsize::new(2).expect("2"),
        ..WriteConfig::DEFAULT
    };
    let front = Front::start_with(SegmentsConfig::DEFAULT, writes).await;
    let segs: Vec<_> = (0..3).map(|i| seg(&[blob(90 + i, 500)])).collect();
    let mut client = front.client().await;
    let upload = front.begin(&mut client, &segs).await;
    for (i, s) in segs.iter().enumerate() {
        put_all(&mut client, upload, i, s, 4096, false).await;
    }
    front.store.hold();
    let mut committing = client.clone();
    let call = tokio::spawn(async move { committing.commit(CommitRequest { upload }).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while front.store.puts_at_gate() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("two PUTs reach the store");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(front.store.puts_at_gate(), 2, "no more PUTs than slots");
    front.store.release();
    call.await.expect("task").expect("commit");
    for s in &segs {
        front.assert_present(&s.digests, true).await;
    }
}

/// The "a 1 GiB single-record segment commits" row of section 9, scaled: catches a
/// commit that asks for admission again while its `Begin` still holds it (mutant:
/// `commit_task` admits one more byte before storing), which waits forever when the
/// upload is as large as the bound. With the tightest bound `kbf-server`
/// starts with (`--write-bytes-in-flight` equal to the default 5 GiB + 1 MiB cap), a
/// `Begin` of the whole cap (in records a server without a tier may hold) is admitted; with a 64 KiB bound and cap, one record that
/// fills the whole upload is sent and commits, and the bytes come back. Sending 1 GiB
/// would hash and hold gigabytes in every run of the suite.
#[tokio::test]
async fn an_upload_as_large_as_the_bound_is_admitted_and_commits() {
    let tightest = WriteConfig {
        bytes_in_flight: MIN_SEGMENT_UPLOAD_BYTES,
        ..WriteConfig::DEFAULT
    };
    let front = Front::start_with(SegmentsConfig::DEFAULT, tightest).await;
    let mut client = front.client().await;
    let begun = tokio::time::timeout(
        Duration::from_secs(10),
        client.begin(BeginRequest {
            segments: vec![support::declared_small(MIN_SEGMENT_UPLOAD_BYTES)],
        }),
    )
    .await
    .expect("a Begin of the whole cap does not wait");
    begun.expect("admitted");
    assert_eq!(front.cache.bytes_admitted(), MIN_SEGMENT_UPLOAD_BYTES);

    let bound = 64 << 10;
    let config = SegmentsConfig {
        max_bytes: bound,
        idle: Duration::from_secs(3600),
    };
    let writes = WriteConfig {
        bytes_in_flight: bound,
        ..WriteConfig::DEFAULT
    };
    let front = Front::start_with(config, writes).await;
    let record = usize::try_from(bound - kbf_segments::layout::footer_len(1)).expect("len");
    let s = seg(&[blob(100, record)]);
    assert_eq!(s.length, bound, "one record fills the upload");
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 16 << 10, false).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        client.commit(CommitRequest { upload }),
    )
    .await
    .expect("the commit does not wait for admission")
    .expect("commit");
    front.assert_stored_records_check(&s.digests).await;
    assert_eq!(front.cache.bytes_admitted(), 0);
}

/// Catches a range the upload cannot hold being written (mutants: no length check on
/// `PutRange`; a segment index not checked; no bounds check): a range over 4 MiB
/// (inside a record area larger than that), one for a segment the upload lacks, an
/// empty one and one past the record area are INVALID_ARGUMENT. Also catches a coverage check that skips gaps between received
/// ranges (mutant: `from > at` never true): a record whose first and last bytes
/// arrived, but not its middle, is FAILED_PRECONDITION at commit.
#[tokio::test]
async fn put_range_refuses_what_the_upload_cannot_hold() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&[blob(110, 1000)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    let limit = usize::try_from(MAX_RANGE_BYTES).expect("limit");
    let big = seg(&[blob(111, limit + 100)]);
    let big_upload = front.begin(&mut client, std::slice::from_ref(&big)).await;
    let refused = front
        .put(&mut client, big_upload, 0, 0, &big.records[..=limit])
        .await
        .expect_err("a range over 4 MiB inside the records");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    let records = s.records.len() as u64;
    for (what, segment, offset, bytes) in [
        ("no such segment", 1, 0, &s.records[..10]),
        ("empty", 0, 0, &[][..]),
        ("past the records", 0, records - 5, &s.records[..10]),
    ] {
        let refused = front
            .put(&mut client, upload, segment, offset, bytes)
            .await
            .expect_err(what);
        assert_eq!(refused.code(), Code::InvalidArgument, "{what}: {refused:?}");
    }

    front
        .put(&mut client, upload, 0, 0, &s.records[..300])
        .await
        .expect("the start");
    front
        .put(&mut client, upload, 0, 700, &s.records[700..])
        .await
        .expect("the end");
    let refused = client
        .commit(CommitRequest { upload })
        .await
        .expect_err("a record with a hole");
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
    front
        .put(&mut client, upload, 0, 300, &s.records[300..700])
        .await
        .expect("the middle");
    client
        .commit(CommitRequest { upload })
        .await
        .expect("the whole record commits");
    front.assert_present(&s.digests, true).await;
}

/// Catches a store failure on `ReadRange` answered as anything but a retryable error
/// (mutant: every store error is NOT_FOUND, as an absent object is): a range of a
/// stored object whose GET fails as a reset connection is UNAVAILABLE. Also catches
/// a short answer passed on as the range (mutant: the length check dropped): a range
/// past the object's end is NOT_FOUND.
#[tokio::test]
async fn a_store_failure_on_read_range_is_unavailable() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&[blob(120, 600)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 4096, false).await;
    client
        .commit(CommitRequest { upload })
        .await
        .expect("commit");
    let located = front.locate(&mut client, &s.digests).await;
    let object = located[0].object;
    front.store.fail_next_gets(1);
    let failed = client
        .read_range(ReadRangeRequest {
            object,
            offset: located[0].offset,
            length: 600,
        })
        .await
        .expect_err("the store failed");
    assert_eq!(failed.code(), Code::Unavailable, "{failed:?}");
    let overrun = client
        .read_range(ReadRangeRequest {
            object,
            offset: located[0].offset,
            length: s.length + 10,
        })
        .await
        .expect_err("past the object's end");
    assert_eq!(overrun.code(), Code::NotFound, "{overrun:?}");
}

/// Catches an upload dropped while its daemon is still sending (mutant: the idle timer
/// drops at its first deadline, whatever calls came since): with a 1 s idle time, an
/// upload that gets a call every 250 ms for 2 s is still open and commits.
#[tokio::test]
async fn an_upload_with_calls_is_not_idle() {
    let config = SegmentsConfig {
        idle: Duration::from_secs(1),
        ..SegmentsConfig::DEFAULT
    };
    let front = Front::start(config).await;
    let s = seg(&[blob(140, 800)]);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        front
            .put(&mut client, upload, 0, 0, &s.records[..100])
            .await
            .expect("the upload is still open");
    }
    front
        .put(&mut client, upload, 0, 100, &s.records[100..])
        .await
        .expect("the rest");
    client
        .commit(CommitRequest { upload })
        .await
        .expect("commit");
    front.assert_present(&s.digests, true).await;
}

/// Catches indexing what the footer read back does not list (mutant: the read-back's
/// entries not checked against the records put): the store keeps another segment of
/// the same length under the key (a valid footer, other records), so the commit is
/// INTERNAL and nothing is indexed.
#[tokio::test]
async fn a_read_back_without_the_record_fails_the_commit() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let s = seg(&[blob(130, 600)]);
    let other = seg(&[blob(131, 600)]);
    assert_eq!(s.length, other.length);
    let mut client = front.client().await;
    let upload = front.begin(&mut client, std::slice::from_ref(&s)).await;
    put_all(&mut client, upload, 0, &s, 4096, false).await;
    let stored = [other.records.as_slice(), other.footer.as_slice()].concat();
    front.store.replace_next_put(stored.into());
    let refused = client
        .commit(CommitRequest { upload })
        .await
        .expect_err("the read-back lacks the record");
    assert_eq!(refused.code(), Code::Internal, "{refused:?}");
    front.assert_present(&s.digests, false).await;
    front.assert_present(&other.digests, false).await;
}
