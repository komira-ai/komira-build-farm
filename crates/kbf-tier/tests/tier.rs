//! The running tier over a `FaultFs`: ranges, answers, seals, budget and poisoning.

mod support;

use std::time::Instant;

use kbf_meta::{Epoch, ObjectId};
use kbf_store::{Fault, FaultFs};
use kbf_tier::{TierError, WriteTier, block_name};

use support::{Index, SEAL_AFTER, ack_group, ack_upload, blocks, bytes, open_tier, options};

const LIMIT: u64 = 64;

fn fresh(budget: u64) -> (FaultFs, kbf_tier::Tier<FaultFs>) {
    let fs = FaultFs::new();
    let (tier, rec) = open_tier(&fs, 1, options(budget, LIMIT), &Index::default()).unwrap();
    assert_eq!(rec, kbf_tier::Recovery::default());
    (fs, tier)
}

/// Catches: a read served past a block's committed length (bytes reserved, written
/// and even synced, but never named by a committed `PutStaged`, so never
/// acknowledged), a read of the wrong offset, and a read of an unknown object that
/// does not fail.
#[test]
fn reads_stop_at_the_committed_length() {
    let (_fs, tier) = fresh(1 << 20);
    let mut index = Index::default();
    let now = Instant::now();
    let (object, offset) = ack_group(&tier, &mut index, b"hello", now).unwrap();
    assert_eq!(offset, 0);
    let pending = tier.reserve_group(3, now).unwrap();
    assert_eq!((pending.object(), pending.offset()), (object, 5));
    tier.write(&pending, 0, b"abc").unwrap();
    let pending = tier.sync(pending).unwrap();
    assert_eq!(tier.read(object, 1, 4).unwrap(), b"ello");
    assert!(matches!(
        tier.read(object, 4, 2),
        Err(TierError::NotCommitted {
            end: 6,
            committed: 5,
            ..
        })
    ));
    tier.committed(pending);
    assert_eq!(tier.read(object, 4, 4).unwrap(), b"oabc");
    let unknown = ObjectId::new(Epoch::new(1), 99);
    assert!(matches!(
        tier.read(unknown, 0, 1),
        Err(TierError::UnknownBlock(o)) if o == unknown
    ));
}

/// Catches: a committed length set to the last committed range's end rather than
/// the largest (commits finishing out of order would move it back and hide an
/// acknowledged range).
#[test]
fn out_of_order_commits_never_move_the_committed_length_back() {
    let (_fs, tier) = fresh(1 << 20);
    let now = Instant::now();
    let first = tier.reserve_group(4, now).unwrap();
    let second = tier.reserve_group(4, now).unwrap();
    tier.write(&first, 0, b"1111").unwrap();
    tier.write(&second, 0, b"2222").unwrap();
    let second = tier.sync(second).unwrap();
    let first = tier.sync(first).unwrap();
    let object = second.object();
    tier.committed(second);
    tier.committed(first);
    assert_eq!(tier.read(object, 0, 8).unwrap(), b"11112222");
}

/// Catches: a write that reaches outside its reservation (into a neighbour's range,
/// which could change bytes another caller already acknowledged), including one whose
/// end overflows.
#[test]
fn a_write_outside_its_range_is_refused() {
    let (_fs, tier) = fresh(1 << 20);
    let mut index = Index::default();
    let now = Instant::now();
    let r = tier.reserve_group(4, now).unwrap();
    let (object, _) = ack_group(&tier, &mut index, b"next", now).unwrap();
    assert!(!r.is_empty());
    assert_eq!(r.len(), 4);
    for (offset, data) in [(3, &b"xy"[..]), (5, b""), (u64::MAX, b"z")] {
        assert!(
            matches!(
                tier.write(&r, offset, data),
                Err(TierError::OutOfRange { reserved: 4, .. })
            ),
            "{offset}"
        );
    }
    tier.write(&r, 2, b"xy").unwrap();
    tier.committed(tier.sync(r).unwrap());
    assert_eq!(tier.read(object, 0, 8).unwrap(), b"\0\0xynext");
}

/// Catches: a seal that does not wait for a range reserved before it (its location
/// would be committed into an object whose footer lacks it), a seal named twice or
/// never, a range given to a sealing block, and a block named for its seal although
/// nothing committed in it.
#[test]
fn a_seal_waits_for_every_reserved_range() {
    let (fs, tier) = fresh(1 << 20);
    let mut index = Index::default();
    let t0 = Instant::now();
    let (object, _) = ack_group(&tier, &mut index, b"aa", t0).unwrap();
    let late = tier.reserve_group(2, t0).unwrap();
    tier.write(&late, 0, b"bb").unwrap();
    let late = tier.sync(late).unwrap();
    assert!(tier.poll_seals(t0 + SEAL_AFTER / 2).is_empty());
    let t1 = t0 + SEAL_AFTER;
    assert!(
        tier.poll_seals(t1).is_empty(),
        "a range is still unanswered"
    );
    let next = tier.reserve_group(2, t1).unwrap();
    assert_ne!(next.object(), object, "a sealing block takes no range");
    index.put_staged(&late, false);
    tier.committed(late);
    assert_eq!(tier.poll_seals(t1), [object]);
    assert_eq!(tier.poll_seals(t1), [object], "named until sealed");
    tier.sealed(object).unwrap();
    assert!(tier.poll_seals(t1).is_empty());
    assert!(matches!(
        tier.sealed(object),
        Err(TierError::NotSealable(o)) if o == object
    ));
    assert_eq!(tier.read(object, 0, 4).unwrap(), b"aabb");

    let empty = next.object();
    drop(next);
    let t2 = t1 + SEAL_AFTER;
    assert!(tier.poll_seals(t2).is_empty());
    assert_eq!(
        fs.contents(&block_name(empty)),
        None,
        "nothing committed: deleted"
    );
    assert!(matches!(tier.sealed(empty), Err(TierError::NotSealable(_))));
}

/// Catches: a grouped block that grows past the segment limit, a full block not
/// sealed until its delay, and a range over the limit accepted.
#[test]
fn a_block_is_sealed_at_the_segment_limit() {
    let (_fs, tier) = fresh(1 << 20);
    let mut index = Index::default();
    let now = Instant::now();
    let (a, _) = ack_group(&tier, &mut index, &bytes(1, 40), now).unwrap();
    let (b, offset) = ack_group(&tier, &mut index, &bytes(2, 40), now).unwrap();
    assert_ne!(a, b, "40 more bytes would pass the limit");
    assert_eq!(offset, 0);
    assert_eq!(tier.poll_seals(now), [a]);
    let (c, _) = ack_group(&tier, &mut index, &bytes(3, 24), now).unwrap();
    assert_eq!(c, b, "24 fits exactly");
    assert_eq!(
        tier.poll_seals(now),
        [a, b],
        "a full block is sealed at once"
    );
    assert!(matches!(
        tier.reserve_group(LIMIT + 1, now),
        Err(TierError::TooLarge {
            len: 65,
            limit: LIMIT
        })
    ));
}

/// Catches: open reservations not counted against the budget (uploads in flight
/// would overrun the disk), a refused range that still counts, bytes not released
/// when an upload fails or a block is unstaged, disk use above the budget, and a
/// range that exactly fills the budget refused (mutant: `>=` for `>`).
#[test]
fn the_budget_counts_open_reservations_and_refuses_the_excess() {
    let (fs, tier) = fresh(100);
    let mut index = Index::default();
    let now = Instant::now();
    let upload = tier.reserve_upload(60).unwrap();
    tier.write(&upload, 0, &bytes(9, 60)).unwrap();
    let (group, _) = ack_group(&tier, &mut index, &bytes(1, 30), now).unwrap();
    assert_eq!(tier.usage().used, 90);
    assert!(matches!(
        tier.reserve_group(20, now),
        Err(TierError::Full {
            want: 20,
            used: 90,
            budget: 100
        })
    ));
    assert!(matches!(
        tier.reserve_upload(11),
        Err(TierError::Full { .. })
    ));
    assert_eq!(tier.usage().used, 90);
    let exact = tier
        .reserve_upload(10)
        .expect("90 used + 10 is exactly the budget");
    assert_eq!(tier.usage().used, 100);
    drop(exact);
    assert_eq!(tier.usage().used, 90);
    let on_disk: usize = blocks(&fs)
        .iter()
        .map(|n| fs.contents(n).unwrap().len())
        .sum();
    assert!(on_disk <= 100, "{on_disk}");

    let name = block_name(upload.object());
    drop(upload);
    assert_eq!(
        fs.contents(&name),
        None,
        "a failed upload's block is deleted"
    );
    assert_eq!(tier.usage().used, 30);
    let (also, _) = ack_group(&tier, &mut index, &bytes(2, 20), now).unwrap();
    assert_eq!(also, group);

    assert!(matches!(
        tier.unstage(group),
        Err(TierError::NotSealable(_))
    ));
    assert_eq!(tier.poll_seals(now + SEAL_AFTER), [group]);
    tier.sealed(group).unwrap();
    tier.unstage(group).unwrap();
    assert_eq!(tier.usage().used, 0);
    assert_eq!(tier.usage().budget, 100);
    assert!(
        fs.contents(&block_name(group)).is_some(),
        "the file is the flusher's"
    );
}

/// Catches: an upload's block left open after its commit (named for a second seal),
/// its bytes unreadable, and a synced upload whose commit failed left on disk.
#[test]
fn an_upload_block_is_sealed_by_its_commit() {
    let (fs, tier) = fresh(1 << 20);
    let mut index = Index::default();
    let object = ack_upload(&tier, &mut index, b"records!").unwrap();
    assert_eq!(tier.read(object, 0, 8).unwrap(), b"records!");
    assert!(tier.poll_seals(Instant::now() + SEAL_AFTER).is_empty());
    tier.unstage(object).unwrap();

    let r = tier.reserve_upload(4).unwrap();
    tier.write(&r, 0, b"gone").unwrap();
    let synced = tier.sync(r).unwrap();
    let name = block_name(synced.object());
    drop(synced);
    assert_eq!(fs.contents(&name), None, "commit refused: block deleted");
}

/// Catches: a block that keeps acknowledging after a failed sync. A failed
/// `fdatasync` reports a write-back error once, to one caller: a later sync of
/// another range of the same block can return success over lost pages.
#[test]
fn a_failed_sync_poisons_the_block() {
    let (fs, tier) = fresh(1 << 20);
    let now = Instant::now();
    let first = tier.reserve_group(2, now).unwrap();
    let second = tier.reserve_group(2, now).unwrap();
    let object = first.object();
    tier.write(&first, 0, b"aa").unwrap();
    tier.write(&second, 0, b"bb").unwrap();
    fs.fail_at(fs.ops(), Fault::Error);
    let failed = tier.sync(first).unwrap_err();
    assert!(matches!(failed.error, TierError::Io { op: "sync", .. }));
    drop(failed.reservation);
    let poisoned = tier.sync(second).unwrap_err();
    assert!(matches!(poisoned.error, TierError::Poisoned(o) if o == object));
    let next = tier.reserve_group(2, now).unwrap();
    assert!(matches!(tier.write(&next, 0, b"cc"), Ok(())));
    assert_ne!(next.object(), object, "a poisoned block is sealed");
    assert!(matches!(
        tier.write(&poisoned.reservation, 0, b"bb"),
        Err(TierError::Poisoned(_))
    ));
}

/// Catches: the same for a failed directory sync (the block's name may not be
/// durable), and for a failed write.
#[test]
fn a_failed_dir_sync_or_write_poisons_the_block() {
    let (fs, tier) = fresh(1 << 20);
    let r = tier.reserve_upload(2).unwrap();
    tier.write(&r, 0, b"aa").unwrap();
    fs.fail_at(fs.ops() + 1, Fault::Error);
    let failed = tier.sync(r).unwrap_err();
    assert!(matches!(failed.error, TierError::Io { op: "sync_dir", .. }));
    let again = tier.sync(failed.reservation).unwrap_err();
    assert!(matches!(again.error, TierError::Poisoned(_)));

    let r = tier.reserve_upload(2).unwrap();
    fs.fail_at(fs.ops(), Fault::Error);
    assert!(matches!(
        tier.write(&r, 0, b"aa"),
        Err(TierError::Io { op: "write", .. })
    ));
    assert!(matches!(
        tier.sync(r).unwrap_err().error,
        TierError::Poisoned(_)
    ));
}

/// Catches: a block file that cannot be created, or read, reported as success, and
/// a reservation of one tier accepted by another.
#[test]
fn io_errors_and_foreign_reservations_are_reported() {
    let (fs, tier) = fresh(1 << 20);
    let mut index = Index::default();
    let now = Instant::now();
    fs.fail_at(fs.ops(), Fault::Error);
    assert!(matches!(
        tier.reserve_group(1, now),
        Err(TierError::Io { op: "create", .. })
    ));
    fs.fail_at(fs.ops(), Fault::Error);
    assert!(matches!(
        tier.reserve_upload(1),
        Err(TierError::Io { op: "create", .. })
    ));
    assert_eq!(tier.usage().used, 0, "a failed create counts nothing");
    let (object, _) = ack_group(&tier, &mut index, b"x", now).unwrap();
    fs.fail_at(fs.ops(), Fault::Error);
    assert!(matches!(
        tier.read(object, 0, 1),
        Err(TierError::Io { op: "read", .. })
    ));

    let other_fs = FaultFs::new();
    let (other, _) = open_tier(&other_fs, 7, options(1 << 20, LIMIT), &Index::default()).unwrap();
    let foreign = other.reserve_upload(1).unwrap();
    assert!(matches!(
        tier.write(&foreign, 0, b"f"),
        Err(TierError::UnknownBlock(_))
    ));
    assert!(matches!(
        tier.sync(foreign).unwrap_err().error,
        TierError::UnknownBlock(_)
    ));
    drop(tier);
    let r = other.reserve_upload(1).unwrap();
    drop(other);
    drop(r);
}

/// Catches: error messages that lose the numbers an operator needs, and a Debug form
/// that does not name the tier.
#[test]
fn errors_and_debug_say_what_happened() {
    let (_fs, tier) = fresh(1);
    let o = ObjectId::new(Epoch::new(3), 4);
    let t = kbf_tier::TierId::new(5);
    let io = std::io::Error::other("disk");
    let cases = [
        (
            TierError::Full {
                want: 2,
                used: 1,
                budget: 3,
            },
            "write tier full: 2 bytes wanted, 1 of 3 used",
        ),
        (
            TierError::TooLarge { len: 9, limit: 8 },
            "a grouped range of 9 bytes passes the segment limit of 8",
        ),
        (
            TierError::OutOfRange {
                offset: 1,
                len: 2,
                reserved: 2,
            },
            "2 bytes at 1 pass a reservation of 2",
        ),
        (
            TierError::NotCommitted {
                object: o,
                end: 7,
                committed: 6,
            },
            "object 3/4: bytes to 7 asked, 6 committed",
        ),
        (
            TierError::UnknownBlock(o),
            "object 3/4 has no block being written in this tier",
        ),
        (
            TierError::Poisoned(o),
            "object 3/4: an earlier write or sync of its block failed",
        ),
        (
            TierError::NotSealable(o),
            "object 3/4 is not ready for that step",
        ),
        (
            TierError::WrongTier {
                expected: t,
                found: None,
            },
            "write tier directory names no tier, expected tier 5, and objects are staged on tier 5",
        ),
        (
            TierError::OtherTier { found: t },
            "write tier directory belongs to tier 5, which has objects staged",
        ),
        (
            TierError::FutureBlock { name: "n".into() },
            "block file n is from this writer or a later one",
        ),
        (
            TierError::Io {
                op: "sync",
                name: "n".into(),
                source: io,
            },
            "sync n: disk",
        ),
    ];
    for (error, want) in cases {
        assert_eq!(error.to_string(), want);
    }
    let debug = format!("{tier:?}");
    assert!(debug.starts_with("Tier {"), "{debug}");
    assert!(debug.contains("budget: 1"), "{debug}");
}

/// Catches: a grouped block that keeps taking ranges past its seal delay when no
/// poll ran in between (a busy tier would never seal by age, and its blocks would
/// wait unflushed until the segment limit).
#[test]
fn a_range_after_the_seal_delay_goes_to_a_new_block() {
    let (_fs, tier) = fresh(1 << 20);
    let mut index = Index::default();
    let t0 = Instant::now();
    let (a, _) = ack_group(&tier, &mut index, b"a", t0).unwrap();
    let (same, _) = ack_group(&tier, &mut index, b"b", t0 + SEAL_AFTER / 2).unwrap();
    assert_eq!(same, a);
    let (b, offset) = ack_group(&tier, &mut index, b"c", t0 + SEAL_AFTER).unwrap();
    assert_ne!(b, a);
    assert_eq!(offset, 0);
    assert_eq!(tier.poll_seals(t0 + SEAL_AFTER), [a]);
}
