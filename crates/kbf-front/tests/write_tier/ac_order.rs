//! The flusher's order between an action-cache entry and its closure
//! (`docs/design/architecture-v2.md`, section 8, AC): the object holding an entry's
//! result blob is flushed only after every object on the same tier holding a blob of
//! its closure. The entry is readable from the tier at once; what the order keeps is
//! the store: it never holds a result whose outputs it does not hold yet, so a tier
//! lost after a partial flush leaves outputs without a result (a miss), never a
//! result without its outputs.
//!
//! A module of `write_tier.rs`, so the flusher is measured as one instantiation with the
//! rest of the tier's tests. Each test's comment names the defect it is there to catch.

use kbf_front::FLUSH_BACKOFF_FIRST;
use kbf_meta::{ObjectId, Role, StagedState};
use kbf_objstore::Capabilities;
use kbf_proto::reapi::{ActionResult, Digest, OutputFile};
use prost::Message;

use super::GIB;
use super::common::faulty::PutFault;
use super::support::{self, Farm, SEGMENT, TestBlob, key, later};

/// The tier's objects for one action-cache entry, laid out so that id order (the
/// flusher's own order) would flush the result first: `result` is a grouped block
/// opened before `output`'s block of its own, and the entry's result blob is staged
/// into it after the output is committed.
struct Entry {
    /// The grouped block holding the entry's result blob.
    result: ObjectId,
    /// The block holding the entry's one output.
    output: ObjectId,
}

async fn entry(farm: &Farm, cache: &support::TierCache) -> Entry {
    // Opens a grouped block, kept open (unsealed) until `later()`.
    let early = TestBlob::new(1, 4096);
    cache
        .store_blobs(vec![early.verified()])
        .await
        .expect("stored");
    let group = farm.log.object_of(&early.digest);
    // An output over the segment limit: a block of its own, sealed at once.
    let len = usize::try_from(SEGMENT).expect("size") + 1;
    let output = TestBlob::new(2, len);
    cache
        .store_blobs(vec![output.verified()])
        .await
        .expect("stored");
    let output_object = farm.log.object_of(&output.digest);
    let result = ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(Digest {
                hash: output.digest.hash_hex(),
                size_bytes: i64::try_from(output.digest.size_bytes).expect("size"),
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let action = TestBlob::new(3, 64);
    cache
        .write_action_result(Role::Daemon, action.digest, &result)
        .await
        .expect("daemon write");
    let result_digest = kbf_segments::sha256(&result.encode_to_vec());
    let result_object = farm.log.object_of(&result_digest);
    // The layout the tests rely on: the result in the older, still open, grouped block.
    assert_eq!(
        result_object, group,
        "the result blob joined the open block"
    );
    assert!(
        result_object < output_object,
        "id order puts the result first"
    );
    assert_eq!(farm.log.staged_state(group), Some(StagedState::Open));
    assert_eq!(
        farm.log.staged_state(output_object),
        Some(StagedState::Sealed)
    );
    Entry {
        result: result_object,
        output: output_object,
    }
}

/// Catches a flusher that PUTs sealed objects in id order alone: the object holding an
/// action-cache entry's result reaches the store before the object holding its output,
/// so a tier lost between the two PUTs leaves a stored result naming an output no store
/// holds.
#[tokio::test]
async fn the_output_object_is_put_before_the_result_object() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let entry = entry(&farm, &cache).await;

    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.sealed, vec![entry.result]);
    assert_eq!(round.flushed, vec![entry.output, entry.result]);
    assert_eq!(
        farm.store.put_keys(),
        vec![key(&cache, entry.output), key(&cache, entry.result)],
        "the closure's object is PUT first"
    );
}

/// Catches a flusher that flushes an entry's result while the object holding its
/// output failed and waits out its backoff: the store would hold the result alone for
/// as long as the store keeps failing that object.
#[tokio::test]
async fn the_result_object_waits_while_the_output_object_fails() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let entry = entry(&farm, &cache).await;
    farm.store.on_put(PutFault::Fail);

    let t0 = later();
    let round = cache.flush_tier(t0).await.expect("round");
    assert_eq!(round.flushed, Vec::<ObjectId>::new(), "{round:?}");
    assert_eq!(round.retrying, vec![(entry.output, FLUSH_BACKOFF_FIRST)]);
    assert_eq!(
        farm.log.staged_state(entry.result),
        Some(StagedState::Sealed)
    );
    assert_eq!(farm.store.put_keys(), vec![key(&cache, entry.output)]);

    let round = cache
        .flush_tier(t0 + FLUSH_BACKOFF_FIRST)
        .await
        .expect("round");
    assert_eq!(round.flushed, vec![entry.output, entry.result]);
    assert_eq!(farm.log.staged_state(entry.result), None, "flushed");
}

/// Catches a flusher that, with the output object waiting out its backoff and so not
/// in the round, flushes the result object it was never ordered after: the round after
/// the failure, still inside the backoff, must PUT nothing and leave both sealed, and
/// must not report a retry for the result, which was never attempted.
#[tokio::test]
async fn the_result_object_is_not_attempted_while_the_output_waits_out_its_backoff() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let entry = entry(&farm, &cache).await;
    farm.store.on_put(PutFault::Fail);

    let t0 = later();
    cache.flush_tier(t0).await.expect("round");
    let puts = farm.store.put_keys();

    let round = cache
        .flush_tier(t0 + FLUSH_BACKOFF_FIRST / 2)
        .await
        .expect("round");
    assert_eq!(round.flushed, Vec::<ObjectId>::new(), "{round:?}");
    assert!(round.retrying.is_empty(), "{round:?}");
    assert_eq!(farm.store.put_keys(), puts, "no PUT inside the backoff");
    for object in [entry.output, entry.result] {
        assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    }
}

/// A gate that holds the digests larger than a segment (the entry's output) while
/// `output_ours` is false, and flushes everything else.
#[derive(Debug)]
struct OutputGate(std::sync::atomic::AtomicBool);

impl kbf_storage::flush::FlushGate for OutputGate {
    fn flushes(&self, d: &kbf_types::Digest) -> bool {
        d.size_bytes <= SEGMENT || self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Catches the gate and the closure order composed wrongly: a result object this node
/// flushes, whose closure object another node flushes, must not be PUT while that
/// closure object is still staged here (the gate removes the output from the round, and
/// a flusher that treats a missing wait as satisfied would PUT the result alone). Once
/// the gate hands the output to this node, the closure order applies as without a gate:
/// output, then result.
#[tokio::test]
async fn a_result_waits_for_a_closure_object_the_gate_holds_elsewhere() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let farm = Farm::new(Capabilities::default());
    let gate = std::sync::Arc::new(OutputGate(AtomicBool::new(false)));
    let cache = farm.start(GIB).await.with_flush_gate(gate.clone());
    let entry = entry(&farm, &cache).await;

    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, Vec::<ObjectId>::new(), "{round:?}");
    assert!(
        farm.store.put_keys().is_empty(),
        "no PUT of the result alone"
    );
    for object in [entry.output, entry.result] {
        assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    }

    gate.0.store(true, Ordering::SeqCst);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, vec![entry.output, entry.result]);
    assert_eq!(
        farm.store.put_keys(),
        vec![key(&cache, entry.output), key(&cache, entry.result)]
    );
}
