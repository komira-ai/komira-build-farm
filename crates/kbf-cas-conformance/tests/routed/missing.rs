//! FindMissing in calls of the configured size.

use kbf_transfer::{Cas, TransferConfig, digest_of};

use super::support::{Fake, plane, sha, unavailable};

/// Catches digests asked in one call whatever their number, a call that fails failing the
/// others, and a failed call left unanswered: five digests are asked in calls of two,
/// the answer is the absent ones in the order asked, and the digests of a failed call
/// are all answered missing (FindMissing only saves uploads).
#[tokio::test]
async fn digests_are_asked_in_chunks_and_a_failed_chunk_is_all_missing() {
    let fake = Fake::new(b"x");
    let config = TransferConfig {
        find_missing_chunk: 2,
        ..TransferConfig::default()
    };
    let cas = plane(&fake, &config);
    let blobs: Vec<Vec<u8>> = (0..5).map(|i| format!("blob {i}").into_bytes()).collect();
    let digests: Vec<_> = blobs.iter().map(|b| digest_of(b)).collect();
    let held: Vec<_> = [1, 3].iter().map(|i| sha(&blobs[*i])).collect();
    fake.hold_digests(&held);

    let missing = cas.missing(&digests).await;
    let want: Vec<_> = [0, 2, 4].iter().map(|i| digests[*i].clone()).collect();
    assert_eq!(missing, want);
    let calls: Vec<usize> = fake.find_log().iter().map(Vec::len).collect();
    assert_eq!(calls, [2, 2, 1]);

    // The first call fails: its two digests are missing whether held or not.
    fake.fail_next_find(unavailable());
    let missing = cas.missing(&digests).await;
    let want: Vec<_> = [0, 1, 2, 4].iter().map(|i| digests[*i].clone()).collect();
    assert_eq!(missing, want);
}
