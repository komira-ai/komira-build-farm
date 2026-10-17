//! What a storage failure is to a runtime, and what is checked on the way.

use kbf_proto::reapi::Digest;
use kbf_transfer::{Cas, CasError, TransferConfig, digest_of, label};

use super::support::{Fake, plane, scratch};

/// A digest REAPI would never send: not a SHA-256 hash.
fn not_a_digest() -> Digest {
    Digest {
        hash: "not hex".to_owned(),
        size_bytes: 3,
    }
}

/// Catches an outage taken for a missing blob (the runtime would fail the action for an
/// input that is there) and an absent blob taken for an outage (it would be retried for
/// ever): a node that is not holding the blob is `Missing`, one that is down is
/// `Unavailable`, on a read into memory, a read into a file and a write.
#[tokio::test]
async fn only_an_absent_blob_is_missing() {
    let fake = Fake::new(b"whatever");
    let cas = plane(&fake, &TransferConfig::default());
    let digest = digest_of(b"asked for");
    let dir = scratch();
    let mut file = tokio::fs::File::create(dir.join("kbf-routed-errors"))
        .await
        .expect("a file");

    fake.holds.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        cas.get(&digest).await,
        Err(CasError::Missing(label(&digest)))
    );
    assert_eq!(
        cas.read_to(&digest, &mut file).await,
        Err(CasError::Missing(label(&digest)))
    );

    fake.holds.store(true, std::sync::atomic::Ordering::SeqCst);
    fake.down.store(true, std::sync::atomic::Ordering::SeqCst);
    for answer in [
        cas.get(&digest).await.map(drop),
        cas.read_to(&digest, &mut file).await,
        cas.put(b"stored".to_vec()).await.map(drop),
    ] {
        assert!(
            matches!(answer, Err(CasError::Unavailable(..))),
            "{answer:?}"
        );
    }
}

/// Catches a digest that is not one asked of the farm anyway: no store holds it, so a
/// read is `Missing` without a call, and FindMissing names it missing.
#[tokio::test]
async fn a_digest_no_store_could_hold_is_missing_without_a_call() {
    let fake = Fake::new(b"whatever");
    let cas = plane(&fake, &TransferConfig::default());
    let bad = not_a_digest();
    let mut file = tokio::fs::File::create(scratch().join("kbf-routed-bad"))
        .await
        .expect("a file");

    assert_eq!(cas.get(&bad).await, Err(CasError::Missing(label(&bad))));
    assert_eq!(
        cas.read_to(&bad, &mut file).await,
        Err(CasError::Missing(label(&bad)))
    );
    let good = digest_of(b"fine");
    assert_eq!(cas.missing(&[good.clone(), bad.clone()]).await, [good, bad]);
    assert!(fake.find_log().is_empty());
}

/// Catches bytes written to the file that are not checked against the digest: a node
/// that answers other bytes than the blob's is `Corrupt`, naming the digest of what it
/// sent, whether they are another length or the blob's own (a check of the length alone
/// passes the second); a node that answers the blob's bytes is read into the file.
#[tokio::test]
async fn read_to_checks_the_bytes_it_writes() {
    let honest = b"the blob that was asked for".to_vec();
    let digest = digest_of(&honest);
    let path = scratch().join("kbf-routed-read-to");

    for lie in [
        &b"some other bytes entirely"[..],
        &b"the blob that was asked fox"[..],
    ] {
        let fake = Fake::new(lie);
        let cas = plane(&fake, &TransferConfig::default());
        let mut file = tokio::fs::File::create(&path).await.expect("a file");
        let error = cas.read_to(&digest, &mut file).await.expect_err("lies");
        assert_eq!(
            error,
            CasError::Corrupt(label(&digest), label(&digest_of(lie)))
        );
    }
    assert_eq!(
        honest.len(),
        b"the blob that was asked fox".len(),
        "a lie of the same length"
    );

    let fake = Fake::new(&honest);
    let cas = plane(&fake, &TransferConfig::default());
    let mut file = tokio::fs::File::create(&path).await.expect("a file");
    cas.read_to(&digest, &mut file).await.expect("read");
    assert_eq!(std::fs::read(&path).expect("the file"), honest);
}

/// Catches a blob read into memory that is not checked against the digest (the router
/// compared the length alone): a node that answers the blob's length of other bytes is
/// `Corrupt`, naming the digest of what it sent, and the bytes are not returned; a node
/// that answers the blob's own bytes is read.
#[tokio::test]
async fn get_checks_the_bytes_it_returns() {
    let honest = b"the blob that was asked for".to_vec();
    let digest = digest_of(&honest);
    let lie = b"the blob that was asked fox";
    assert_eq!(honest.len(), lie.len(), "a lie of the same length");

    let cas = plane(&Fake::new(lie), &TransferConfig::default());
    assert_eq!(
        cas.get(&digest).await,
        Err(CasError::Corrupt(label(&digest), label(&digest_of(lie))))
    );

    let cas = plane(&Fake::new(&honest), &TransferConfig::default());
    assert_eq!(cas.get(&digest).await, Ok(honest));
}

/// Catches a write error lost: a file that cannot be written fails the read with `Write`,
/// whether the error shows on the write (a blob of several chunks, the second write
/// finding the first one failed) or on the flush (one chunk, buffered).
#[tokio::test]
async fn a_file_that_cannot_be_written_fails_the_read() {
    let path = scratch().join("kbf-routed-read-only");
    std::fs::write(&path, b"").expect("a file");
    for blob in [b"1234".to_vec(), b"12345678123456781234".to_vec()] {
        let digest = digest_of(&blob);
        let cas = plane(&Fake::new(&blob), &TransferConfig::default());
        let read_only = std::fs::File::open(&path).expect("open");
        let mut file = tokio::fs::File::from_std(read_only);
        let error = cas
            .read_to(&digest, &mut file)
            .await
            .expect_err("read-only");
        assert!(matches!(error, CasError::Write(..)), "{error:?}");
    }
}

/// Catches a read that breaks for good reported as a short blob or a corrupt one: the
/// node passes a chunk and fails on every attempt, and the read into a file is
/// `Unavailable`, as is a read into memory.
#[tokio::test]
async fn a_read_that_keeps_breaking_is_unavailable() {
    let blob = b"12345678123456781234".to_vec();
    let digest = digest_of(&blob);
    let fake = Fake::new(&blob);
    fake.breaks.store(true, std::sync::atomic::Ordering::SeqCst);
    let cas = plane(&fake, &TransferConfig::default());
    let mut file = tokio::fs::File::create(scratch().join("kbf-routed-breaks"))
        .await
        .expect("a file");

    for answer in [
        cas.read_to(&digest, &mut file).await,
        cas.get(&digest).await.map(drop),
    ] {
        assert!(
            matches!(answer, Err(CasError::Unavailable(..))),
            "{answer:?}"
        );
    }
}

/// Catches a read into a file whose offset cannot be known going ahead anyway: with no
/// offset to cut a bad copy back to, a read into a pipe fails with `Write` before any
/// node is asked.
#[tokio::test]
async fn a_file_that_cannot_seek_fails_before_any_read() {
    let blob = b"1234".to_vec();
    let digest = digest_of(&blob);
    let fake = Fake::new(&blob);
    let cas = plane(&fake, &TransferConfig::default());
    let (_reader, writer) = std::io::pipe().expect("a pipe");
    let pipe = std::fs::File::from(std::os::fd::OwnedFd::from(writer));
    let mut file = tokio::fs::File::from_std(pipe);

    let error = cas.read_to(&digest, &mut file).await.expect_err("a pipe");
    assert!(matches!(error, CasError::Write(..)), "{error:?}");
    assert_eq!(fake.reads.load(std::sync::atomic::Ordering::SeqCst), 0);
}
