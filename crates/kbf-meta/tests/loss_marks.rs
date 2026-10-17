//! Loss marks (docs/design/architecture-v2.md, section 8): an object marked unreachable
//! while it is staged had its only copy in a block on a write tier (the tier was
//! abandoned, its block was missing or short at a start, or its bytes failed their
//! digest before the flusher PUT them), so the blobs indexed in it are lost, not merely
//! unreachable. [`MetaState::lost_blobs`] counts them now and
//! [`MetaState::blob_losses`] counts every blob ever marked so. Each test's comment
//! names the defect it is there to catch.

mod common;

use std::collections::BTreeSet;

use common::{DAY, collect, digest, in_segment, mark_missing, mark_reachable, meta, object, put};
use kbf_meta::{
    ActionAnswer, ActionRecord, Applied, BlobAnswer, BlobWrite, Closure, Command, MetaState, Miss,
    Role, ServerId, StagedPut, TierId,
};

const SERVER: ServerId = ServerId::new(1);
const TIER: TierId = TierId::new(7);

/// A state with [`TIER`] registered to [`SERVER`].
fn tiered() -> MetaState {
    let mut m = meta();
    let added = m.execute(
        common::ANY_INDEX,
        Command::AddTier {
            server: SERVER,
            tier: TIER,
        },
    );
    assert_eq!(added, Applied::Tier(Ok(())));
    m
}

/// Stages object `seq` on [`TIER`] holding digests `blobs`, one record per 100 bytes.
fn stage(m: &mut MetaState, seq: u64, blobs: &[u8]) -> Vec<BlobWrite> {
    let put = StagedPut {
        object: object(seq),
        end: 100 * blobs.len() as u64,
        seal: false,
        blobs: (0..)
            .zip(blobs)
            .map(|(i, &n)| (digest(n), 100 * i))
            .collect(),
    };
    let staged = m.execute(
        common::ANY_INDEX,
        Command::PutStaged {
            tier: TIER,
            objects: vec![put],
        },
    );
    match staged {
        Applied::Staged(Ok(writes)) => writes,
        other => panic!("PutStaged applied as {other:?}"),
    }
}

/// The gauge and the counter, in that order.
fn counts(m: &MetaState) -> (u64, u64) {
    (m.lost_blobs(), m.blob_losses())
}

/// Catches: a staged object marked unreachable (its tier abandoned) whose blobs are not
/// counted lost; an action-cache entry naming one answered as a hit; a re-upload that
/// does not clear its digest's mark (the gauge stays up); a second mark of the same
/// object counting its blobs again; and a count that is not one per blob.
#[test]
fn a_staged_object_marked_unreachable_loses_its_blobs_until_each_is_uploaded_again() {
    let mut m = tiered();
    stage(&mut m, 1, &[1, 2, 3]);
    stage(&mut m, 2, &[4]);
    let record = ActionRecord {
        result: digest(1),
        closure: Closure::from_iter([digest(4)]),
    };
    common::put_action(&mut m, Role::Daemon, digest(10), record).expect("written");
    assert_eq!(counts(&m), (0, 0), "nothing lost while staged");

    mark_missing(&mut m, 1);
    assert_eq!(counts(&m), (3, 3), "each blob of the abandoned object");
    assert_eq!(m.blob(&digest(1)), BlobAnswer::Unavailable);
    let wanted = [digest(1), digest(4)];
    assert_eq!(m.find_missing(&wanted).missing, [digest(1)]);
    assert_eq!(
        m.action(&digest(10)),
        ActionAnswer::Miss(Miss::Unreachable(digest(1)))
    );

    mark_missing(&mut m, 1);
    assert_eq!(counts(&m), (3, 3), "a second mark is not a second loss");

    let healed = put(&mut m, digest(2), in_segment(9, 2));
    assert!(matches!(healed, BlobWrite::Healed { .. }), "{healed:?}");
    assert_eq!(counts(&m), (2, 3), "the re-uploaded blob is no longer lost");
    put(&mut m, digest(1), in_segment(9, 1));
    assert_eq!(counts(&m), (1, 3));
    assert!(
        matches!(m.action(&digest(10)), ActionAnswer::Hit { .. }),
        "the entry's blobs are all held again"
    );
}

/// Catches: an object marked unreachable after it was flushed counted as lost (the
/// store may still produce it: a 404 can be a misrouted request), and a blob listed in
/// a lost object but indexed in another counted (its copy there is held).
#[test]
fn only_blobs_whose_only_copy_was_on_the_tier_are_lost() {
    let mut m = tiered();
    put(&mut m, digest(5), in_segment(5, 0));
    mark_missing(&mut m, 5);
    assert_eq!(
        counts(&m),
        (0, 0),
        "a stored object is unreachable, not lost"
    );

    stage(&mut m, 1, &[1]);
    let writes = stage(&mut m, 2, &[1, 2]);
    assert!(
        matches!(writes[0], BlobWrite::Duplicate { .. }),
        "{writes:?}"
    );
    mark_missing(&mut m, 2);
    assert_eq!(counts(&m), (1, 1), "digest 1 is held in object 1");
    assert!(matches!(m.blob(&digest(1)), BlobAnswer::Present { .. }));
}

/// Catches: a lost blob left in the gauge after its entry left the index (collected
/// at the end of its retention, or evicted with its object), or after the object was
/// vouched for again ([`Command::ObjectReachable`]); and the counter falling with it.
#[test]
fn a_lost_blob_leaves_the_gauge_when_its_entry_or_its_mark_goes() {
    let mut m = tiered();
    stage(&mut m, 1, &[1, 2]);
    stage(&mut m, 2, &[3]);
    stage(&mut m, 3, &[4]);
    for seq in [1, 2, 3] {
        mark_missing(&mut m, seq);
    }
    assert_eq!(counts(&m), (4, 4));

    mark_reachable(&mut m, 2);
    assert_eq!(counts(&m), (3, 4), "vouched for");

    let evicted = m.execute(
        common::ANY_INDEX,
        Command::EvictObjects {
            objects: BTreeSet::from([object(3)]),
        },
    );
    assert!(matches!(evicted, Applied::Evicted(_)), "{evicted:?}");
    assert_eq!(counts(&m), (2, 4), "evicted");

    common::tick(&mut m, DAY * 9);
    let collected = collect(&mut m);
    assert_eq!(collected.blobs.len(), 3, "{collected:?}");
    assert_eq!(counts(&m), (0, 4), "collected");
}
