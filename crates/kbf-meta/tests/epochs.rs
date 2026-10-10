//! Writer epochs, batched blob records, and unreachable marks with a reason and a
//! generation.

mod common;

use common::{
    ANY_INDEX, DAY, EPOCH, digest, in_segment, mark, mark_at, mark_missing, meta, object, put, tick,
};
use kbf_meta::{
    Applied, BlobAnswer, BlobWrite, Command, Epoch, Generation, Location, LossMark, MetaState,
    ObjectId, Retention, StoreId, UnallocatedEpoch, UnreachableReason,
};

fn alloc(m: &mut MetaState) -> Epoch {
    match m.execute(ANY_INDEX, Command::AllocEpoch) {
        Applied::Epoch(e) => e,
        other => panic!("AllocEpoch applied as {other:?}"),
    }
}

fn at_epoch(epoch: u64, seq: u64) -> Location {
    Location {
        store: StoreId::CONFIGURED,
        object: ObjectId::new(Epoch::new(epoch), seq),
        offset: 0,
    }
}

/// Catches: an `AllocEpoch` that hands out an epoch twice (it returns the next epoch
/// without counting past it, or a `Collect` or `Tick` resets the counter). Two writers
/// holding one epoch name the same object keys: with conditional writes every upload of
/// the second fails, without them it overwrites the first's live segments.
#[test]
fn alloc_epoch_strictly_increases_and_collect_does_not_reset_it() {
    let mut m = MetaState::new(Retention::default());
    let mut last = Epoch::new(0);
    for n in 0..1000u32 {
        let e = alloc(&mut m);
        assert!(e > last, "allocation {n}: {e:?} after {last:?}");
        last = e;
        if n % 10 == 0 {
            tick(&mut m, DAY * (n / 10 + 1));
            m.execute(ANY_INDEX, Command::Collect);
        }
    }
    assert_eq!(last, Epoch::new(1000));
}

/// Catches: a record accepted for an object whose epoch was never allocated (epoch 0,
/// or one past the last allocation: an off-by-one in the check), and a refused command
/// that changes the state anyway. A writer that skipped `AllocEpoch` would otherwise
/// write keys another writer will be given.
#[test]
fn objects_of_an_unallocated_epoch_are_refused_and_change_nothing() {
    let mut fresh = MetaState::new(Retention::default());
    let before = fresh.clone();
    assert_eq!(
        fresh.execute(
            ANY_INDEX,
            Command::PutBlob {
                digest: digest(1),
                location: at_epoch(1, 1),
            }
        ),
        Applied::Blob(Err(UnallocatedEpoch(ObjectId::new(Epoch::new(1), 1))))
    );
    assert_eq!(fresh, before);

    let mut m = meta();
    let before = m.clone();
    for epoch in [0, EPOCH.get() + 1, u64::MAX] {
        let location = at_epoch(epoch, 3);
        let refused = Err(UnallocatedEpoch(location.object));
        assert_eq!(
            m.execute(
                ANY_INDEX,
                Command::PutBlob {
                    digest: digest(1),
                    location,
                }
            ),
            Applied::Blob(refused)
        );
        assert_eq!(
            m.execute(
                ANY_INDEX,
                Command::ObjectUnreachable {
                    object: location.object,
                    reason: UnreachableReason::Missing,
                }
            ),
            Applied::Marked(refused.map(|_| ()))
        );
        assert_eq!(
            m.execute(
                ANY_INDEX,
                Command::ObjectReachable {
                    object: location.object,
                    generation: Generation::new(ANY_INDEX),
                }
            ),
            Applied::Marked(refused.map(|_| ()))
        );
    }
    assert_eq!(m, before);
    assert_eq!(m.blob(&digest(1)), BlobAnswer::Absent);
}

/// Catches: a `PutBlobs` that records the blobs before a refused one (it checks each
/// location as it goes): a batch is one log entry and applies whole or not at all.
#[test]
fn put_blobs_with_one_unallocated_location_records_nothing() {
    let mut m = meta();
    let before = m.clone();
    let bad = at_epoch(EPOCH.get() + 1, 0);
    let applied = m.execute(
        ANY_INDEX,
        Command::PutBlobs(vec![
            (digest(1), in_segment(1, 1)),
            (digest(2), in_segment(1, 2)),
            (digest(3), bad),
        ]),
    );
    assert_eq!(applied, Applied::Blobs(Err(UnallocatedEpoch(bad.object))));
    assert_eq!(m, before);
    assert_eq!(m.blob_count(), 0);
}

/// xorshift64*, so a failing sequence is reproducible from its seed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }
}

/// Catches: a `PutBlobs` that is not exactly the fold of `PutBlob` over its list: one
/// that stops at the first duplicate, skips a blob already listed earlier in the batch,
/// reports outcomes out of order, or heals differently. A model-based check: 200
/// generated histories of batches (with repeated digests, within and across batches),
/// marks, reachable-again and ticks, applied once as `PutBlobs` and once as one
/// `PutBlob` per blob; the outcomes and the states must be equal at every step.
#[test]
fn put_blobs_is_the_fold_of_put_blob() {
    let mut duplicates = 0;
    let mut heals = 0;
    for seed in 1..=200u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let (mut batched, mut single) = (meta(), meta());
        for step in 0..40 {
            match rng.below(6) {
                0 => {
                    let segment = rng.below(4);
                    let reason = if rng.below(2) == 0 {
                        UnreachableReason::Missing
                    } else {
                        UnreachableReason::Corrupt
                    };
                    mark(&mut batched, segment, reason);
                    mark(&mut single, segment, reason);
                }
                1 => {
                    let c = Command::ObjectReachable {
                        object: object(rng.below(4)),
                        generation: Generation::new(ANY_INDEX),
                    };
                    assert_eq!(
                        batched.execute(ANY_INDEX, c.clone()),
                        single.execute(ANY_INDEX, c)
                    );
                }
                2 => {
                    let t = DAY * u32::try_from(step).expect("step");
                    tick(&mut batched, t);
                    tick(&mut single, t);
                }
                _ => {
                    let segment = rng.below(4);
                    let blobs: Vec<_> = (0..rng.below(6))
                        .map(|_| {
                            let n = u8::try_from(rng.below(8)).expect("small");
                            (digest(n), in_segment(segment, n))
                        })
                        .collect();
                    let Applied::Blobs(Ok(outcomes)) =
                        batched.execute(ANY_INDEX, Command::PutBlobs(blobs.clone()))
                    else {
                        panic!("seed {seed} step {step}: PutBlobs refused");
                    };
                    let expected: Vec<BlobWrite> =
                        blobs.iter().map(|&(d, l)| put(&mut single, d, l)).collect();
                    assert_eq!(outcomes, expected, "seed {seed} step {step}: {blobs:?}");
                    for o in &outcomes {
                        match o {
                            BlobWrite::Duplicate { .. } => duplicates += 1,
                            BlobWrite::Healed { .. } => heals += 1,
                            BlobWrite::Stored => {}
                        }
                    }
                }
            }
            assert_eq!(batched, single, "seed {seed} step {step}");
        }
    }
    // The histories must exercise both paths a short-circuit or a reorder would break.
    assert!(
        duplicates > 500 && heals > 50,
        "{duplicates} duplicates, {heals} heals"
    );
}

/// Catches: a `Corrupt` mark downgraded to `Missing` by a later 404 (a later re-probe
/// would then find the object and serve its bad bytes again), a mark that drops its
/// reason, and a reachable-again that leaves the mark.
#[test]
fn a_corrupt_mark_is_never_downgraded_to_missing() {
    let mut m = meta();
    put(&mut m, digest(1), in_segment(1, 1));
    put(&mut m, digest(2), in_segment(2, 2));

    mark_missing(&mut m, 1);
    assert_eq!(m.unreachable(object(1)), Some(UnreachableReason::Missing));
    mark(&mut m, 1, UnreachableReason::Corrupt);
    assert_eq!(m.unreachable(object(1)), Some(UnreachableReason::Corrupt));

    mark(&mut m, 2, UnreachableReason::Corrupt);
    mark_missing(&mut m, 2);
    assert_eq!(m.unreachable(object(2)), Some(UnreachableReason::Corrupt));
    assert_eq!(m.blob(&digest(2)), BlobAnswer::Unavailable);

    m.execute(
        ANY_INDEX,
        Command::ObjectReachable {
            object: object(2),
            generation: Generation::new(ANY_INDEX),
        },
    );
    assert_eq!(m.unreachable(object(2)), None);
    assert_eq!(m.blob(&digest(2)), BlobAnswer::Present(in_segment(2, 2)));
    assert_eq!(m.unreachable(object(3)), None);
}

fn reachable(m: &mut MetaState, segment: u64, generation: Generation) {
    let applied = m.execute(
        ANY_INDEX,
        Command::ObjectReachable {
            object: object(segment),
            generation,
        },
    );
    assert_eq!(applied, Applied::Marked(Ok(())));
}

/// Catches: an `ObjectUnreachable` that does not stamp its own log index as the mark's
/// generation, and one that stamps it only when the reason rises: a second mark of the
/// same reason must restamp too, or a reachable-again read before it clears it.
#[test]
fn every_applied_mark_stamps_its_log_index_whether_or_not_the_reason_rises() {
    let mut m = meta();
    let mark_of = |m: &MetaState| m.loss_mark(object(1));
    mark_at(&mut m, 5, 1, UnreachableReason::Missing);
    let missing = UnreachableReason::Missing;
    let at = |n| {
        Some(LossMark {
            reason: missing,
            generation: Generation::new(n),
        })
    };
    assert_eq!(mark_of(&m), at(5));
    mark_at(&mut m, 9, 1, missing);
    assert_eq!(mark_of(&m), at(9));
    mark_at(&mut m, 12, 1, UnreachableReason::Corrupt);
    mark_at(&mut m, 14, 1, missing);
    assert_eq!(
        mark_of(&m),
        Some(LossMark {
            reason: UnreachableReason::Corrupt,
            generation: Generation::new(14),
        })
    );
    assert_eq!(m.loss_mark(object(2)), None);
}

/// Catches: an `ObjectReachable` that clears the mark whatever its generation, and a mark
/// that keeps its first generation when a later mark of the same reason applies (the
/// mutant that stamps only when the reason rises). A reachable-again delayed past a newer
/// loss mark would clear it, and every server would answer present for bytes that are
/// gone. Two cases: the newer mark has the same reason as the one the prober read (only
/// this case catches the stamp-on-rise mutant), and it has a higher one.
#[test]
fn a_delayed_object_reachable_leaves_a_newer_mark() {
    for (newer, name) in [
        (UnreachableReason::Missing, "same reason"),
        (UnreachableReason::Corrupt, "higher reason"),
    ] {
        let mut m = meta();
        put(&mut m, digest(1), in_segment(1, 1));
        mark_at(&mut m, 5, 1, UnreachableReason::Missing);
        // A prober reads the generation, finds the object, and sends ObjectReachable;
        // before it commits, a newer read marks the object again.
        let read = m.loss_mark(object(1)).expect("marked").generation;
        mark_at(&mut m, 9, 1, newer);
        let before = m.clone();
        reachable(&mut m, 1, read);
        assert_eq!(
            m, before,
            "{name}: the delayed ObjectReachable changed the state"
        );
        assert_eq!(
            m.loss_mark(object(1)),
            Some(LossMark {
                reason: newer,
                generation: Generation::new(9),
            }),
            "{name}"
        );
        assert_eq!(m.blob(&digest(1)), BlobAnswer::Unavailable, "{name}");

        // One that names the current generation clears it.
        reachable(&mut m, 1, Generation::new(9));
        assert_eq!(m.loss_mark(object(1)), None, "{name}");
        assert_eq!(
            m.blob(&digest(1)),
            BlobAnswer::Present(in_segment(1, 1)),
            "{name}"
        );
    }
}
