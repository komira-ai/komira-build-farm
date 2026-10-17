//! The local write tier's metadata (docs/design/write-path.md, sections 6.1 and 6.4):
//! tiers, staged objects, and the answers for staged blobs.

mod common;

use std::collections::BTreeSet;

use common::{DAY, digest, in_segment, mark_missing, meta, object, put, put_action, tick};
use kbf_meta::{
    ActionRecord, Applied, BlobAnswer, BlobWrite, Closure, Command, Epoch, ForeignWriter, Location,
    MetaState, ObjectId, Role, ServerId, StagedPut, StagedState, StoreId, TierError, TierId,
};
use kbf_types::Digest;

const SERVER: ServerId = ServerId::new(1);
const TIER: TierId = TierId::new(7);

/// The index of every command here: no test reads a loss-mark generation.
const INDEX: u64 = 1;

/// A state with [`TIER`] registered to [`SERVER`].
fn tiered() -> MetaState {
    let mut m = meta();
    assert_eq!(tier_cmd(&mut m, add(SERVER, TIER)), Ok(()));
    m
}

fn add(server: ServerId, tier: TierId) -> Command {
    Command::AddTier { server, tier }
}

/// Applies a command answered with [`Applied::Tier`].
fn tier_cmd(m: &mut MetaState, command: Command) -> Result<(), TierError> {
    match m.execute(INDEX, command) {
        Applied::Tier(r) => r,
        other => panic!("applied as {other:?}"),
    }
}

/// Commits one `PutStaged` on `tier`.
fn put_on(
    m: &mut MetaState,
    tier: TierId,
    objects: Vec<StagedPut>,
) -> Result<Vec<BlobWrite>, TierError> {
    match m.execute(INDEX, Command::PutStaged { tier, objects }) {
        Applied::Staged(r) => r,
        other => panic!("PutStaged applied as {other:?}"),
    }
}

/// Commits one `PutStaged` of one object on [`TIER`].
fn stage(m: &mut MetaState, put: StagedPut) -> Result<Vec<BlobWrite>, TierError> {
    put_on(m, TIER, vec![put])
}

/// Object `seq`'s records `blobs` (digest number, offset), committed up to `end`.
fn part(seq: u64, end: u64, blobs: &[(u8, u64)]) -> StagedPut {
    StagedPut {
        object: object(seq),
        end,
        seal: false,
        blobs: blobs.iter().map(|&(n, o)| (digest(n), o)).collect(),
    }
}

fn sealed(put: StagedPut) -> StagedPut {
    StagedPut { seal: true, ..put }
}

fn at(seq: u64, offset: u64) -> Location {
    Location {
        store: StoreId::CONFIGURED,
        object: object(seq),
        offset,
    }
}

fn staged_at(seq: u64, offset: u64) -> BlobAnswer {
    BlobAnswer::Present {
        location: at(seq, offset),
        staged_on: Some(TIER),
    }
}

fn stored_at(location: Location) -> BlobAnswer {
    BlobAnswer::Present {
        location,
        staged_on: None,
    }
}

fn listed(m: &MetaState, seq: u64) -> Vec<(Digest, u64)> {
    m.staged(object(seq))
        .expect("staged")
        .blobs()
        .map(|(d, o)| (*d, o))
        .collect()
}

fn seal(m: &mut MetaState, seq: u64) -> Result<(), TierError> {
    tier_cmd(
        m,
        Command::SealStaged {
            object: object(seq),
        },
    )
}

fn flushed(m: &mut MetaState, seq: u64) -> Result<(), TierError> {
    tier_cmd(
        m,
        Command::ObjectFlushed {
            object: object(seq),
        },
    )
}

fn conflict(m: &mut MetaState, seq: u64) -> Result<(), TierError> {
    tier_cmd(
        m,
        Command::FlushConflict {
            object: object(seq),
        },
    )
}

fn state(m: &MetaState, seq: u64) -> Option<StagedState> {
    m.staged(object(seq)).map(kbf_meta::StagedObject::state)
}

/// Catches: a staged blob answered as if the store held it (`staged_on` left `None`),
/// so a reader goes to the store, finds nothing and marks a good object unreachable;
/// and a flush that keeps the staged mark, or moves the blob. A blob written straight
/// to the store answers `staged_on: None`.
#[test]
fn a_staged_blob_answers_present_on_its_tier_until_the_flush() {
    let mut m = tiered();
    assert_eq!(
        stage(&mut m, part(5, 300, &[(1, 0), (2, 100)])),
        Ok(vec![BlobWrite::Stored, BlobWrite::Stored])
    );
    put(&mut m, digest(3), in_segment(6, 0));

    assert_eq!(m.blob(&digest(1)), staged_at(5, 0));
    assert_eq!(m.blob(&digest(2)), staged_at(5, 100));
    assert_eq!(m.blob(&digest(3)), stored_at(in_segment(6, 0)));
    let staged = m.staged(object(5)).expect("staged");
    assert_eq!(
        (staged.tier(), staged.committed_len(), staged.state()),
        (TIER, 300, StagedState::Open)
    );
    assert_eq!(
        m.staged_objects().map(|(o, _)| o).collect::<Vec<_>>(),
        [object(5)]
    );

    assert_eq!(seal(&mut m, 5), Ok(()));
    assert_eq!(m.blob(&digest(1)), staged_at(5, 0));
    assert_eq!(flushed(&mut m, 5), Ok(()));
    assert_eq!(m.blob(&digest(1)), stored_at(at(5, 0)));
    assert_eq!(m.blob(&digest(2)), stored_at(at(5, 100)));
    assert!(m.staged(object(5)).is_none());
    assert_eq!(m.staged_objects().count(), 0);
}

/// Catches: the committed length moving backwards when two groups' commits on one
/// block apply out of order (write-path.md section 9, "two groups on one block whose
/// commits finish out of order"): the later group's records would be past the length
/// a restart truncates the block to. Mutant: set the length to `end`.
#[test]
fn commits_applied_out_of_order_never_shorten_the_object() {
    let mut m = tiered();
    stage(&mut m, part(5, 400, &[(4, 300)])).expect("later group first");
    stage(&mut m, part(5, 200, &[(1, 100)])).expect("earlier group second");

    assert_eq!(m.staged(object(5)).expect("staged").committed_len(), 400);
    assert_eq!(m.blob(&digest(4)), staged_at(5, 300));
    assert_eq!(m.blob(&digest(1)), staged_at(5, 100));
    assert_eq!(listed(&m, 5), [(digest(1), 100), (digest(4), 300)]);
}

/// Catches: a `PutStaged` accepted after `SealStaged` (write-path.md section 9, "a late
/// `PutStaged` after `SealStaged` is refused"): the flusher builds the footer from the
/// list it read at the seal, so a blob indexed after it would point into an object
/// whose footer lacks it. Mutant: drop the sealed check.
#[test]
fn a_put_that_adds_to_a_sealed_object_is_refused() {
    let mut m = tiered();
    stage(&mut m, part(5, 100, &[(1, 0)])).expect("open");
    assert_eq!(seal(&mut m, 5), Ok(()));
    let before = m.clone();

    for late in [
        part(5, 200, &[(2, 100)]),
        sealed(part(5, 200, &[(2, 100)])),
        // A new blob below the length is still an addition.
        part(5, 100, &[(2, 0)]),
        // Every blob listed, but past the committed length.
        part(5, 200, &[(1, 0)]),
        // One blob already listed and one new one, within the length: the new one is
        // still an addition (a repeat must list nothing new, not something old).
        part(5, 100, &[(1, 0), (2, 50)]),
    ] {
        assert_eq!(
            stage(&mut m, late.clone()),
            Err(TierError::Sealed(object(5))),
            "{late:?}"
        );
        assert_eq!(m, before, "{late:?} changed the state");
    }
    assert_eq!(m.blob(&digest(2)), BlobAnswer::Absent);
}

/// Catches: a retried commit refused because its first apply sealed the object. A
/// segment upload's `PutStaged` seals in the same commit; a forward retried across a
/// leader change applies it twice, and the second must answer as the first did (each
/// blob a duplicate of itself), not fail an upload whose blobs are indexed. Mutant:
/// refuse every `PutStaged` on a sealed object.
#[test]
fn a_repeated_sealing_put_is_accepted_as_a_repeat() {
    let mut m = tiered();
    let upload = sealed(part(5, 300, &[(1, 0), (2, 100)]));
    assert_eq!(
        stage(&mut m, upload.clone()),
        Ok(vec![BlobWrite::Stored, BlobWrite::Stored])
    );
    assert_eq!(state(&m, 5), Some(StagedState::Sealed));
    let once = m.clone();
    assert_eq!(
        stage(&mut m, upload),
        Ok(vec![
            BlobWrite::Duplicate { kept: at(5, 0) },
            BlobWrite::Duplicate { kept: at(5, 100) },
        ])
    );
    assert_eq!(m, once);
}

/// Catches: an object staged again after it left the table. A late or retried
/// `PutStaged` after `ObjectFlushed` would mark a stored object staged, and a restart
/// that finds no block file for it would mark it missing. The same holds for an object
/// marked unreachable, which leaves the table at once (its bytes are not to be
/// trusted, and its block is an orphan for the restart sweep). Mutant: record nothing
/// when an object leaves.
#[test]
fn an_object_that_left_is_never_staged_again() {
    let mut m = tiered();
    stage(&mut m, sealed(part(5, 100, &[(1, 0)]))).expect("upload");
    assert_eq!(flushed(&mut m, 5), Ok(()));
    let before = m.clone();
    assert_eq!(
        stage(&mut m, part(5, 100, &[(1, 0)])),
        Err(TierError::Left(object(5)))
    );
    assert_eq!(m, before);

    stage(&mut m, part(6, 100, &[(2, 0)])).expect("open");
    mark_missing(&mut m, 6);
    assert!(m.staged(object(6)).is_none());
    assert_eq!(m.blob(&digest(2)), BlobAnswer::Unavailable);
    assert_eq!(
        stage(&mut m, part(6, 100, &[(2, 0)])),
        Err(TierError::Unreachable(object(6)))
    );
    common::mark_reachable(&mut m, 6);
    assert_eq!(
        stage(&mut m, part(6, 100, &[(2, 0)])),
        Err(TierError::Left(object(6)))
    );
    assert_eq!(m.blob(&digest(2)), stored_at(at(6, 0)));
}

/// Catches: an object state that moves backwards: a conflict cleared by a seal or a
/// retried sealing put (the flusher would retry and overwrite an operator's view), a
/// flush of an open object (its footer would miss later groups), a conflict or a
/// staging after the flush. Mutants: `SealStaged` sets `Sealed` whatever the state;
/// `ObjectFlushed` accepts an open object.
#[test]
fn an_object_state_never_moves_backwards() {
    let mut m = tiered();
    stage(&mut m, part(5, 100, &[(1, 0)])).expect("open");
    assert_eq!(flushed(&mut m, 5), Err(TierError::NotSealed(object(5))));
    assert_eq!(conflict(&mut m, 5), Err(TierError::NotSealed(object(5))));
    assert_eq!(state(&m, 5), Some(StagedState::Open));

    assert_eq!(seal(&mut m, 5), Ok(()));
    assert_eq!(seal(&mut m, 5), Ok(()));
    assert_eq!(state(&m, 5), Some(StagedState::Sealed));
    assert_eq!(conflict(&mut m, 5), Ok(()));
    assert_eq!(conflict(&mut m, 5), Ok(()));
    assert_eq!(seal(&mut m, 5), Ok(()));
    assert_eq!(state(&m, 5), Some(StagedState::Conflict));
    stage(&mut m, sealed(part(5, 100, &[(1, 0)]))).expect("a repeat");
    assert_eq!(state(&m, 5), Some(StagedState::Conflict));
    assert_eq!(m.blob(&digest(1)), staged_at(5, 0));

    // An operator resolved the conflict: the flusher's ObjectFlushed is accepted.
    assert_eq!(flushed(&mut m, 5), Ok(()));
    assert_eq!(flushed(&mut m, 5), Ok(()));
    assert_eq!(seal(&mut m, 5), Ok(()));
    assert!(m.staged(object(5)).is_none());
    assert_eq!(conflict(&mut m, 5), Err(TierError::Left(object(5))));

    for (name, result) in [
        ("seal", seal(&mut m, 9)),
        ("flushed", flushed(&mut m, 9)),
        ("conflict", conflict(&mut m, 9)),
    ] {
        assert_eq!(result, Err(TierError::NotStaged(object(9))), "{name}");
    }
}

/// Catches: a blob indexed at an offset its object's list does not have. An object
/// lists each digest once, at the offset first committed (a second group can write the
/// same digest again before the first commits); a later commit that heals the blob
/// (its entry pointed at an object since marked unreachable) must point it at the
/// listed record, or the index names a location the footer lacks. Mutant: index at the
/// commit's own offset.
#[test]
fn a_digest_committed_twice_is_indexed_where_it_is_listed() {
    let mut m = tiered();
    put(&mut m, digest(1), in_segment(2, 0));
    assert_eq!(
        stage(&mut m, part(5, 100, &[(1, 50)])),
        Ok(vec![BlobWrite::Duplicate {
            kept: in_segment(2, 0)
        }])
    );
    mark_missing(&mut m, 2);
    assert_eq!(
        stage(&mut m, part(5, 400, &[(1, 300)])),
        Ok(vec![BlobWrite::Healed {
            replaced: in_segment(2, 0)
        }])
    );
    assert_eq!(m.blob(&digest(1)), staged_at(5, 50));
    assert_eq!(listed(&m, 5), [(digest(1), 50)]);
}

/// Catches: a `PutStaged` applied in part. Each refusal below names a bad object after
/// a good one; nothing of the good one may be indexed or staged, or a caller told the
/// commit failed would find its blobs half there.
#[test]
fn a_refused_put_changes_nothing() {
    let mut m = tiered();
    stage(&mut m, sealed(part(3, 100, &[(9, 0)]))).expect("a sealed object");
    stage(&mut m, part(4, 100, &[(8, 0)])).expect("an open object");
    let other = TierId::new(8);
    assert_eq!(tier_cmd(&mut m, add(ServerId::new(2), other)), Ok(()));
    put_on(&mut m, other, vec![part(6, 100, &[(7, 0)])]).expect("another tier");
    mark_missing(&mut m, 4);
    let never = ObjectId::new(Epoch::new(2), 1);

    let good = part(5, 100, &[(1, 0)]);
    let cases = [
        (
            "past end",
            part(7, 103, &[(4, 100)]),
            TierError::PastEnd {
                object: object(7),
                digest: digest(4),
            },
        ),
        (
            "overflow",
            part(7, 104, &[(4, u64::MAX)]),
            TierError::PastEnd {
                object: object(7),
                digest: digest(4),
            },
        ),
        (
            "repeated",
            part(5, 100, &[]),
            TierError::Repeated(object(5)),
        ),
        (
            "sealed",
            part(3, 200, &[(2, 100)]),
            TierError::Sealed(object(3)),
        ),
        (
            "other tier",
            part(6, 100, &[]),
            TierError::OtherTier {
                object: object(6),
                tier: other,
            },
        ),
        (
            "unreachable",
            part(4, 100, &[]),
            TierError::Unreachable(object(4)),
        ),
        (
            "unallocated",
            StagedPut {
                object: never,
                ..part(0, 100, &[])
            },
            TierError::Foreign(ForeignWriter(never)),
        ),
    ];
    for (name, bad, error) in cases {
        let before = m.clone();
        assert_eq!(
            stage(&mut m, good.clone()).map(|_| ()),
            Ok(()),
            "{name}: setup"
        );
        m = before.clone();
        assert_eq!(
            put_on(&mut m, TIER, vec![good.clone(), bad]),
            Err(error),
            "{name}"
        );
        assert_eq!(m, before, "{name}: state changed");
        assert_eq!(m.blob(&digest(1)), BlobAnswer::Absent, "{name}");
    }
    // A record that ends exactly at `end` is accepted.
    assert!(stage(&mut m, part(5, 104, &[(4, 100)])).is_ok());

    let before = m.clone();
    assert_eq!(
        put_on(&mut m, TierId::new(99), vec![good]),
        Err(TierError::UnknownTier(TierId::new(99)))
    );
    assert_eq!(m, before);
}

/// Catches: two servers sharing a tier id, a server whose tier is replaced while its
/// staged objects are still on the old one (their blocks would never be recovered or
/// flushed), and a retired id registered again (a stale `TIER` file on a remounted disk
/// would pass the start check). A server's current tier registered again is a no-op.
/// Only the server's own old tier counts: an object staged on another server's tier
/// must not block it (mutant: refuse while any object is staged on any tier, which
/// would refuse one server's --hot-store-abandon whenever another has unflushed data).
#[test]
fn one_tier_per_server_and_an_id_is_never_reused() {
    let mut m = tiered();
    let next = TierId::new(8);
    let other = ServerId::new(3);
    let other_tier = TierId::new(9);
    assert_eq!(m.tier(SERVER), Some(TIER));
    assert_eq!(m.tier(ServerId::new(2)), None);
    assert_eq!(tier_cmd(&mut m, add(SERVER, TIER)), Ok(()));
    assert_eq!(
        tier_cmd(&mut m, add(ServerId::new(2), TIER)),
        Err(TierError::TierTaken {
            tier: TIER,
            server: SERVER
        })
    );

    stage(&mut m, part(5, 100, &[(1, 0)])).expect("open");
    assert_eq!(
        tier_cmd(&mut m, add(SERVER, next)),
        Err(TierError::StillStaged {
            server: SERVER,
            tier: TIER
        })
    );
    assert_eq!(m.tier(SERVER), Some(TIER));

    // Another server's tier holds a staged object through all of this.
    assert_eq!(tier_cmd(&mut m, add(other, other_tier)), Ok(()));
    put_on(&mut m, other_tier, vec![part(20, 100, &[(3, 0)])]).expect("other open");

    // What --hot-store-abandon does: mark every staged object missing, then register.
    mark_missing(&mut m, 5);
    assert_eq!(tier_cmd(&mut m, add(SERVER, next)), Ok(()));
    assert_eq!(m.tier(SERVER), Some(next));
    assert_eq!(m.tier(other), Some(other_tier));
    assert_eq!(state(&m, 20), Some(StagedState::Open));
    assert_eq!(
        tier_cmd(&mut m, add(ServerId::new(2), TIER)),
        Err(TierError::TierRetired(TIER))
    );
    assert_eq!(
        stage(&mut m, part(6, 100, &[(2, 0)])),
        Err(TierError::UnknownTier(TIER))
    );
}

/// Catches: the record of objects that left the table kept forever (it would grow with
/// every flush) or dropped too soon (a late `PutStaged` could stage a flushed object
/// again). It is kept for the blob retention: `min_ttl` plus the touch quantum.
#[test]
fn objects_that_left_are_forgotten_after_the_blob_retention() {
    let mut m = tiered();
    stage(&mut m, sealed(part(5, 100, &[(1, 0)]))).expect("upload");
    assert_eq!(flushed(&mut m, 5), Ok(()));

    tick(&mut m, DAY * 8);
    common::collect(&mut m);
    assert_eq!(
        stage(&mut m, part(5, 100, &[])),
        Err(TierError::Left(object(5)))
    );
    assert_eq!(seal(&mut m, 5), Ok(()));

    tick(&mut m, DAY * 8 + common::HOUR);
    common::collect(&mut m);
    assert_eq!(seal(&mut m, 5), Err(TierError::NotStaged(object(5))));
    assert!(stage(&mut m, part(5, 100, &[])).is_ok());
}

/// Catches: an error message that names the wrong tier or server, which would send an
/// operator reading a refused start to the wrong directory or host.
#[test]
fn a_refusal_names_the_tier_and_the_server() {
    let taken = TierError::TierTaken {
        tier: TIER,
        server: SERVER,
    };
    assert_eq!(taken.to_string(), "tier 7 is registered to server 1");
    let still = TierError::StillStaged {
        server: SERVER,
        tier: TIER,
    };
    assert_eq!(
        still.to_string(),
        "server 1's tier 7 still holds staged objects"
    );
}

/// Catches a hit check that probes the store for an object still staged on a tier
/// (the store does not hold it yet, so every such hit would turn into a miss), that
/// probes one object once per blob in it, or that leaves out the result blob's object
/// or a closure blob's. `stored_objects` lists each stored object once, no staged one,
/// and nothing for an action with no entry.
#[test]
fn stored_objects_lists_each_stored_object_of_an_entry_once_and_no_staged_one() {
    let mut m = tiered();
    put(&mut m, digest(1), in_segment(6, 0));
    put(&mut m, digest(2), in_segment(6, 1));
    put(&mut m, digest(3), in_segment(8, 0));
    stage(&mut m, part(5, 300, &[(4, 0)])).expect("staged");
    let record = kbf_meta::ActionRecord {
        result: digest(1),
        closure: kbf_meta::Closure::from_iter([digest(2), digest(3), digest(4)]),
    };
    common::put_action(&mut m, kbf_meta::Role::Daemon, digest(20), record).expect("cached");
    assert_eq!(
        m.stored_objects(&digest(20))
            .into_iter()
            .collect::<Vec<_>>(),
        [object(6), object(8)]
    );
    assert!(m.stored_objects(&digest(21)).is_empty());
}

/// Catches an action-cache entry whose result's object is not held back for its
/// closure's (architecture-v2 section 8: closure blobs flush before the entry), one held
/// back for blobs the store already holds or that share its own object, a wait that
/// outlives the flush it waits for, and waits answered for an object never staged.
#[test]
fn a_result_object_waits_for_the_staged_objects_of_its_closure() {
    let mut m = tiered();
    stage(&mut m, part(1, 200, &[(2, 0), (4, 100)])).expect("result's object");
    stage(&mut m, sealed(part(2, 100, &[(3, 0)]))).expect("output's object");
    put(&mut m, digest(5), in_segment(9, 0));
    let record = ActionRecord {
        result: digest(2),
        closure: Closure::from_iter([digest(3), digest(4), digest(5)]),
    };
    assert_eq!(m.flush_waits(object(1)), BTreeSet::new());
    put_action(&mut m, Role::Daemon, digest(1), record).expect("written");
    assert_eq!(m.flush_waits(object(1)), BTreeSet::from([object(2)]));
    assert_eq!(m.flush_waits(object(2)), BTreeSet::new());

    let refused = ActionRecord {
        result: digest(3),
        closure: Closure::from_iter([digest(2), digest(6)]),
    };
    assert!(put_action(&mut m, Role::Daemon, digest(7), refused).is_err());
    assert_eq!(
        m.flush_waits(object(2)),
        BTreeSet::new(),
        "a refused write orders"
    );

    assert_eq!(
        tier_cmd(&mut m, Command::ObjectFlushed { object: object(2) }),
        Ok(())
    );
    assert_eq!(m.flush_waits(object(1)), BTreeSet::new());
    assert_eq!(
        m.flush_waits(object(99)),
        BTreeSet::new(),
        "an object never staged waits for none"
    );
}
