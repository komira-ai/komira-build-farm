//! The commit's check without a server: what it builds from received records, and what
//! it answers when the check itself fails.

use kbf_segments::{Footer, SegmentWriter};
use tonic::Code;

use super::*;

/// An upload, committing, of the one segment `declared` describes.
fn committing(declared: Vec<Declared>) -> Arc<Upload> {
    let (done, _) = watch::channel(None);
    Arc::new(Upload {
        owner: None,
        declared,
        phase: Mutex::new(Phase::Committing),
        last_call: Mutex::new(Instant::now()),
        done,
    })
}

/// A segment of two blobs as `Begin` declares it, and its record bytes.
fn declared_segment() -> (Vec<Declared>, Vec<u8>) {
    let mut writer = SegmentWriter::new(u64::MAX);
    writer.push(b"the first record").expect("push");
    writer.push(b"and the second").expect("push");
    let bytes = writer.finish();
    let footer = Footer::parse(&bytes, bytes.len() as u64).expect("footer");
    let records_len = usize::try_from(footer.records_len()).expect("len");
    let (declared, _) = declare(&[(bytes.len() as u64, &bytes[records_len..])]).expect("declare");
    (declared, bytes[..records_len].to_vec())
}

/// Catches a check that drops or reorders records (it builds the stored segment): the
/// received records of a declared segment check, and the built segment indexes both
/// records under the declared footer's entries.
#[tokio::test]
async fn the_check_builds_each_received_segment() {
    let (declared, records) = declared_segment();
    let mut received = Receiving::new(&declared[0], false, None);
    let (put, claimed) = received.put(&declared[0], 0, &records).expect("put");
    assert_eq!(put, Put::Written);
    advance(&claimed, |i, finished| {
        received.next(&declared[0], i, finished)
    });
    let want: Vec<Digest> = declared[0].entries.iter().map(|e| e.digest).collect();
    let built = check_all(committing(declared), vec![received])
        .await
        .expect("checked");
    assert_eq!(built.len(), 1);
    let got: Vec<Digest> = built[0].entries.iter().map(|e| e.digest).collect();
    assert_eq!(got, want);
}

/// Catches a check that panics leaving the upload unanswered (mutant: the join error
/// propagated as a panic of the commit task, so every Commit of the upload waits
/// forever): received buffers that do not match what was declared (the record lies
/// past the buffer, which `Begin` never lets happen) make the check panic, and the
/// answer is INTERNAL naming the failed check.
#[tokio::test]
async fn a_check_that_panics_answers_internal() {
    let (declared, _) = declared_segment();
    let nothing = Declared {
        length: 0,
        records_len: 0,
        entries: Vec::new(),
    };
    let received = Receiving::new(&nothing, false, None);
    let failed = check_all(committing(declared), vec![received])
        .await
        .expect_err("a panicked check");
    assert_eq!(failed.code(), Code::Internal, "{failed:?}");
    assert!(
        failed.message().starts_with("checking an upload failed: "),
        "{failed:?}"
    );
}

/// Catches a hash job that, finishing after its upload was committed, answered or
/// dropped, still touched the records (`check_next` and `refusal_at` must see nothing
/// then): on a receiving upload a corrupt record is found and refused; on a committing,
/// a done and a dropped one both answer `None`.
#[test]
fn a_job_outliving_its_receiving_upload_sees_nothing() {
    let (declared, records) = declared_segment();
    let upload = committing(declared);
    let mut bad = records;
    bad[1] ^= 1;
    let mut received = Receiving::new(&upload.declared[0], false, None);
    let (_, claimed) = received.put(&upload.declared[0], 0, &bad).expect("put");
    *lock(&upload.phase) = Phase::Receiving {
        segments: vec![received],
        _admitted: Admitted::nothing(),
    };
    assert!(upload.refusal_at(0).is_none());
    advance(&claimed, |i, finished| upload.check_next(0, i, finished));
    let refused = upload.refusal_at(0).expect("the corrupt record is refused");
    assert_eq!(refused.code(), Code::InvalidArgument);
    for phase in [Phase::Committing, Phase::Done, Phase::Dropped] {
        *lock(&upload.phase) = phase;
        assert!(upload.refusal_at(0).is_none());
        assert!(upload.check_next(0, 0, None).is_none());
    }
}
