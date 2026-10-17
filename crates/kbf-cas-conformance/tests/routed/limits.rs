//! The limits a data plane keeps.

use kbf_transfer::{Cas, TransferConfig, digest_of};

use super::support::{Fake, plane, scratch};

/// Catches requests without a limit: with a limit of two and six reads of different blobs
/// (three into memory, three into files) started together, no more than two are at the
/// node at once, the rest wait, and every read still gets its blob. Catches a limit that
/// is not the configured one.
#[tokio::test]
async fn no_more_requests_than_the_limit_are_out_at_once() {
    let fake = Fake::new(b"four");
    let gate = fake.gate();
    let config = TransferConfig {
        in_flight: 2,
        ..TransferConfig::default()
    };
    let cas = plane(&fake, &config);
    assert_eq!(cas.in_flight(), 2);

    // Reads into memory and into files, of four bytes each. The files are made first, so
    // that a read is held up by nothing but the limit and the node.
    let dir = scratch();
    let mut files = Vec::new();
    for i in 0..6_u8 {
        let path = dir.join(format!("kbf-routed-limit-{i}"));
        files.push(tokio::fs::File::create(path).await.expect("a file"));
    }
    let reads = futures::future::join_all(files.into_iter().enumerate().map(|(i, mut file)| {
        // Reads into memory are of different blobs, so that they do not share a read;
        // every read checks the bytes against the digest, so the node serves each blob
        // under its own.
        let different = [i as u8; 4];
        fake.serve(&different);
        let served = digest_of(b"four");
        let cas = cas.clone();
        async move {
            if i % 2 == 0 {
                cas.get(&digest_of(&different))
                    .await
                    .map(|bytes| (bytes, different.to_vec()))
            } else {
                cas.read_to(&served, &mut file)
                    .await
                    .map(|()| (b"four".to_vec(), b"four".to_vec()))
            }
        }
    }));
    // The reads are polled before this: the ones that could start are at the node.
    let release = async {
        assert_eq!(fake.active(), 2);
        gate.add_permits(100);
    };
    let (answers, ()) = tokio::join!(reads, release);
    for answer in answers {
        let (read, blob) = answer.expect("a blob");
        assert_eq!(read, blob);
    }
    assert_eq!(fake.peak(), 2);
}

/// Catches a limit of zero that would stop everything: it is taken as one.
#[tokio::test]
async fn a_limit_of_zero_is_one() {
    let fake = Fake::new(b"four");
    let config = TransferConfig {
        in_flight: 0,
        ..TransferConfig::default()
    };
    let cas = plane(&fake, &config);
    assert_eq!(cas.in_flight(), 1);
    let digest = digest_of(b"four");
    assert_eq!(cas.get(&digest).await.expect("a blob"), b"four");
}
