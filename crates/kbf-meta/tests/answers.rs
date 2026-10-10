//! Present, absent and unavailable: what the index says about one blob.

mod common;

use common::{DAY, digest, in_segment, mark_missing, mark_reachable, meta, put, tick};
use kbf_meta::{BlobAnswer, BlobWrite};
use kbf_types::Digest;

/// Catches: NOT_FOUND for a blob the farm holds. When the object holding a blob is
/// unreachable the answer must be UNAVAILABLE (retry later), never absent: a client
/// told NOT_FOUND treats the blob as lost and fails or re-runs work. Also catches an
/// answer that stays unavailable after the object is reachable again.
#[test]
fn held_but_unreachable_is_unavailable_never_absent() {
    let mut m = meta();
    let held = digest(1);
    put(&mut m, held, in_segment(7, 1));
    assert_eq!(m.blob(&held), BlobAnswer::Present(in_segment(7, 1)));

    mark_missing(&mut m, 7);
    assert_eq!(m.blob(&held), BlobAnswer::Unavailable);
    assert_eq!(m.blob(&digest(2)), BlobAnswer::Absent);

    mark_reachable(&mut m, 7);
    assert_eq!(m.blob(&held), BlobAnswer::Present(in_segment(7, 1)));
}

/// Catches: an index keyed on the hash alone. A request with the right hash and the
/// wrong size must be absent, never another blob's bytes.
#[test]
fn wrong_size_is_absent() {
    let mut m = meta();
    let held = digest(3);
    put(&mut m, held, in_segment(1, 0));
    let wrong = Digest::new(held.function, held.hash, held.size_bytes + 1);
    assert_eq!(m.blob(&wrong), BlobAnswer::Absent);
}

/// Catches: `FindMissingBlobs` that omits a digest (buck2 reads an omitted digest as
/// present), drops a duplicate, reports a held-but-unreachable blob present (the client
/// would then rely on bytes nobody can read), or touches what it reports missing.
#[test]
fn find_missing_lists_every_digest_not_present() {
    let mut m = meta();
    let (present, absent, lost) = (digest(1), digest(2), digest(3));
    put(&mut m, present, in_segment(1, 0));
    put(&mut m, lost, in_segment(2, 0));
    mark_missing(&mut m, 2);
    tick(&mut m, DAY * 2);

    let answer = m.find_missing(&[absent, present, lost, absent]);
    assert_eq!(answer.missing, vec![absent, lost, absent]);
    assert_eq!(answer.touch.blobs, [present].into());
    assert!(answer.touch.actions.is_empty());
}

/// Catches: an upload of a held-but-unreachable blob that keeps the dead location (the
/// re-upload FindMissingBlobs asked for would never heal it), and an upload of a
/// present blob that moves the entry to the redundant new copy.
#[test]
fn reupload_heals_an_unreachable_entry_and_keeps_a_reachable_one() {
    let mut m = meta();
    let d = digest(4);
    put(&mut m, d, in_segment(1, 4));
    let big = in_segment(9, 0);
    assert_eq!(
        put(&mut m, d, big),
        BlobWrite::Duplicate {
            kept: in_segment(1, 4)
        }
    );
    assert_eq!(m.blob(&d), BlobAnswer::Present(in_segment(1, 4)));

    mark_missing(&mut m, 1);
    assert_eq!(
        put(&mut m, d, big),
        BlobWrite::Healed {
            replaced: in_segment(1, 4)
        }
    );
    assert_eq!(m.blob(&d), BlobAnswer::Present(big));
    assert_eq!(put(&mut m, digest(5), big), BlobWrite::Stored);
    assert_eq!(m.blob_count(), 2);
}
