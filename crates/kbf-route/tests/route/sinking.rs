//! A blob read whole into a sink: checked against its digest, a bad copy discarded and the
//! next owner read, the sink's own failures ending the read.

use std::sync::Arc;

use kbf_route::{BlobSink, IntoError, Router};
use kbf_storage::StorageError;
use kbf_types::Digest;

use super::support::{Cell, Cluster, Hook, active, blob_read_from, map, sha, unavailable, view};

/// A sink in memory that can be made to fail, and counts what it is asked.
#[derive(Default)]
struct Sink {
    held: Vec<u8>,
    writes: usize,
    discards: usize,
    /// The write with this index (from 0) fails.
    fail_write: Option<usize>,
    /// Every discard fails.
    fail_discard: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Refused(&'static str);

impl BlobSink for Sink {
    type Error = Refused;

    async fn write(&mut self, chunk: &[u8]) -> Result<(), Refused> {
        let index = self.writes;
        self.writes += 1;
        if self.fail_write == Some(index) {
            return Err(Refused("write"));
        }
        self.held.extend_from_slice(chunk);
        Ok(())
    }

    async fn discard(&mut self) -> Result<(), Refused> {
        self.discards += 1;
        if self.fail_discard {
            return Err(Refused("discard"));
        }
        self.held.clear();
        Ok(())
    }
}

/// A router over two owners with a hook that is never rate-limited, and a blob whose read
/// order is node 1, then node 2.
fn two_owners() -> (Router, Arc<Cluster>, Arc<Hook>, Vec<u8>, Digest) {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let hook = Arc::new(Hook::default());
    let limit = kbf_route::RepairLimit {
        burst: std::num::NonZeroU32::MAX,
        per_second: std::num::NonZeroU32::MAX,
    };
    let router =
        Router::new(Cell::with(view(&map)), cluster.clone()).with_read_repair(hook.clone(), limit);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    (router, cluster, hook, blob, digest)
}

/// `blob` with its first byte changed: as long as it, and not it.
fn lie(blob: &[u8]) -> Vec<u8> {
    let mut lie = blob.to_vec();
    lie[0] = lie[0].wrapping_add(1);
    lie
}

/// Catches a copy taken for the blob because of its length, a bad copy left in the sink
/// under the good one, and a read that stops at the first owner: node 1 sends a
/// same-length lie of several chunks, node 2 the blob. The sink holds exactly the blob,
/// the lie was discarded once, both nodes were read once, and the hook was told node 1's
/// copy is bad, as `read_blob` tells it.
#[tokio::test]
async fn a_lying_copy_is_discarded_and_the_next_owner_read() {
    let (router, cluster, hook, blob, digest) = two_owners();
    cluster.node(1).hold_as(digest, &lie(&blob));
    cluster.node(2).hold(&blob);

    let mut sink = Sink::default();
    router
        .read_blob_into(&digest, &mut sink)
        .await
        .expect("the blob");
    assert_eq!(sink.held, blob);
    assert_eq!(sink.discards, 1);
    assert_eq!(cluster.node(1).read_count(), 1);
    assert_eq!(cluster.node(2).read_count(), 1);
    assert_eq!(hook.told(), [(digest, 2, vec![1])]);
    assert_eq!(router.read_blob(&digest).await.expect("blob"), blob);
    assert_eq!(hook.told().len(), 2, "read_blob tells the hook the same");
}

/// Catches an owner that does not hold the blob, or fails before its first byte, or breaks
/// part way, taken as the end of the read, and a broken copy left in the sink: the sink
/// gets the blob from node 2 each time. Only the owner that said it lacks the blob is
/// told to the hook; one that broke is down, not missing a copy.
#[tokio::test]
async fn an_owner_without_the_blob_or_that_fails_is_passed_over() {
    for (case, missing_told) in [("absent", true), ("fails", false), ("breaks", false)] {
        let (router, cluster, hook, blob, digest) = two_owners();
        cluster.node(2).hold(&blob);
        match case {
            "absent" => {}
            "fails" => {
                cluster.node(1).hold(&blob);
                cluster.node(1).fail_read(unavailable());
            }
            _ => {
                cluster.node(1).hold(&blob);
                cluster.node(1).break_read(1, unavailable());
            }
        }
        let mut sink = Sink::default();
        router.read_blob_into(&digest, &mut sink).await.expect(case);
        assert_eq!(sink.held, blob, "{case}");
        let discarded = usize::from(case == "breaks");
        assert_eq!(sink.discards, discarded, "{case}");
        let told = if missing_told {
            vec![(digest, 2, vec![1])]
        } else {
            Vec::new()
        };
        assert_eq!(hook.told(), told, "{case}");
    }
}

/// Catches a lie answered when every owner lies, or bytes of it left in the sink: the read
/// is `Corrupt` naming the first owner's bytes, each copy was discarded, the sink holds
/// nothing, and the hook is told nothing (no owner had the blob to repair from).
#[tokio::test]
async fn every_owner_lying_is_corrupt_and_leaves_the_sink_as_it_was() {
    let (router, cluster, hook, blob, digest) = two_owners();
    let lie = lie(&blob);
    cluster.node(1).hold_as(digest, &lie);
    cluster.node(2).hold_as(digest, &lie);

    let mut sink = Sink::default();
    let error = router.read_blob_into(&digest, &mut sink).await;
    match error {
        Err(IntoError::Storage(StorageError::Corrupt { actual, .. })) => {
            assert_eq!(actual, sha(&lie));
        }
        other => panic!("{other:?}"),
    }
    assert!(sink.held.is_empty());
    assert_eq!(sink.discards, 2);
    assert!(hook.told().is_empty());
}

/// Catches a node that sends without end being written on: past one byte over the size
/// nothing more is read or written, and the copy is discarded.
#[tokio::test]
async fn a_copy_that_does_not_stop_is_cut_at_one_byte_past_the_size() {
    let (router, cluster, _, _, digest) = two_owners();
    let produced = cluster.node(1).hold_generated_as(digest, 1000);
    cluster.node(2).hold_generated_as(digest, 1000);

    let mut sink = Sink::default();
    let error = router.read_blob_into(&digest, &mut sink).await;
    assert!(
        matches!(error, Err(IntoError::Storage(StorageError::Corrupt { .. }))),
        "{error:?}"
    );
    let produced = produced.load(std::sync::atomic::Ordering::SeqCst);
    assert!(produced <= 2, "{produced} chunks were read");
    assert!(sink.held.is_empty());
}

/// Catches a sink that fails taken for a failed owner (the next owner would be read into
/// a sink that cannot take bytes, and a write failure would be reported as the farm's):
/// a write that fails ends the read with the sink's error, after one owner, with what was
/// written discarded; a discard that fails after a lie ends it the same way.
#[tokio::test]
async fn a_sink_that_fails_ends_the_read_with_its_error() {
    let (router, cluster, hook, blob, digest) = two_owners();
    cluster.node(1).hold(&blob);
    cluster.node(2).hold(&blob);
    let mut sink = Sink {
        fail_write: Some(1),
        ..Sink::default()
    };
    let error = router.read_blob_into(&digest, &mut sink).await;
    assert!(
        matches!(error, Err(IntoError::Sink(Refused("write")))),
        "{error:?}"
    );
    assert!(sink.held.is_empty());
    assert_eq!(sink.discards, 1);
    assert_eq!(cluster.node(2).read_count(), 0);

    cluster.node(1).hold_as(digest, &lie(&blob));
    let mut sink = Sink {
        fail_discard: true,
        ..Sink::default()
    };
    let error = router.read_blob_into(&digest, &mut sink).await;
    assert!(
        matches!(error, Err(IntoError::Sink(Refused("discard")))),
        "{error:?}"
    );
    assert_eq!(cluster.node(2).read_count(), 0);
    assert!(hook.told().is_empty());
}
