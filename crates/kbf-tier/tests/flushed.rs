//! What the disk layer does once an object is flushed: the block becomes a flushed copy
//! (`<epoch>-<seq>.seg`), copies are read and evicted least recently read first, a
//! corrupt block is discarded, and the unflushed exposure is measured.

mod support;

use std::time::{Duration, Instant};

use kbf_store::{FaultFs, Fs};
use kbf_tier::{TierError, WriteTier, block_name, seg_name};

use support::{Index, ack_group, ack_upload, bytes, open_tier, options};

const LIMIT: u64 = 1 << 20;

/// Seals the open grouped block at `now + SEAL_AFTER`, as `SealStaged` would.
fn seal(tier: &dyn WriteTier, index: &mut Index, now: Instant) {
    for object in tier.poll_seals(now + support::SEAL_AFTER) {
        index.seal(object);
        tier.sealed(object).unwrap();
    }
}

/// Catches: a flushed block left counting against the budget, renamed without the
/// directory synced, or no longer readable; and a block flushed before it was sealed
/// (an open block could then take more ranges into an object the store already holds).
#[test]
fn a_flushed_block_becomes_a_copy_that_reads_and_stops_counting() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    let (object, at) = ack_group(&tier, &mut index, &bytes(1, 10), now).unwrap();
    assert!(matches!(
        tier.flushed(object),
        Err(TierError::NotSealable(o)) if o == object
    ));
    seal(&tier, &mut index, now);
    tier.flushed(object).unwrap();
    assert_eq!(fs.contents(&block_name(object)), None);
    assert_eq!(fs.contents(&seg_name(object)), Some(bytes(1, 10)));
    assert_eq!(tier.read(object, at, 10).unwrap(), bytes(1, 10));
    let usage = tier.usage();
    assert_eq!((usage.used, usage.flushed), (0, 10));
    let cut = fs.crash_reordered(0, 0);
    assert!(
        cut.contents(&seg_name(object)).is_some(),
        "the rename is synced"
    );
}

/// Catches: eviction of the most recently read copy, a read that does not count as
/// use, eviction of a block not yet flushed (its only copy), and a reservation refused
/// while copies could make room.
#[test]
fn copies_are_evicted_least_recently_read_first_and_blocks_never() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, options(30, LIMIT), &index).unwrap();
    let a = ack_upload(&tier, &mut index, &bytes(1, 10)).unwrap();
    let b = ack_upload(&tier, &mut index, &bytes(2, 10)).unwrap();
    tier.flushed(a).unwrap();
    tier.flushed(b).unwrap();
    let staged = ack_upload(&tier, &mut index, &bytes(3, 10)).unwrap();
    assert_eq!(tier.read(a, 0, 10).unwrap(), bytes(1, 10));
    let (next, _) = ack_group(&tier, &mut index, &bytes(4, 10), now).unwrap();
    assert!(fs.contents(&seg_name(a)).is_some(), "read last");
    assert_eq!(fs.contents(&seg_name(b)), None, "least recently read");
    assert!(matches!(
        tier.read(b, 0, 10),
        Err(TierError::UnknownBlock(_))
    ));
    ack_group(&tier, &mut index, &bytes(5, 10), now).unwrap();
    assert_eq!(fs.contents(&seg_name(a)), None, "evicted to make room");
    let full = ack_group(&tier, &mut index, &bytes(6, 10), now);
    assert!(matches!(full, Err(TierError::Full { .. })), "{full:?}");
    assert!(fs.contents(&block_name(staged)).is_some());
    assert!(fs.contents(&block_name(next)).is_some());
    assert_eq!(tier.usage().evicted, 2);
}

/// Catches: a discarded (corrupt) block that keeps its file or its budget, and a
/// discard of a block still taking ranges.
#[test]
fn a_discarded_block_is_deleted_and_stops_counting() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    let (object, _) = ack_group(&tier, &mut index, &bytes(1, 10), now).unwrap();
    assert!(matches!(
        tier.discard(object),
        Err(TierError::NotSealable(_))
    ));
    seal(&tier, &mut index, now);
    tier.discard(object).unwrap();
    assert_eq!(fs.contents(&block_name(object)), None);
    assert_eq!(tier.usage().used, 0);
    assert!(matches!(
        tier.read(object, 0, 1),
        Err(TierError::UnknownBlock(_))
    ));
}

/// Catches: a closed block that takes more ranges (verifier follow-up: a `PutStaged`
/// into an object marked unreachable while its block was open is refused, so every
/// upload grouped into it would fail until the seal delay), a close that never lets
/// the block seal, and a close of a block other than the open one.
#[test]
fn a_closed_block_takes_no_more_ranges_and_seals() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    let upload = ack_upload(&tier, &mut index, &bytes(1, 10)).unwrap();
    let (object, _) = ack_group(&tier, &mut index, &bytes(2, 10), now).unwrap();
    tier.close(upload);
    let (same, _) = ack_group(&tier, &mut index, &bytes(3, 10), now).unwrap();
    assert_eq!(
        same, object,
        "closing another block leaves the open one open"
    );
    tier.close(object);
    assert_eq!(
        tier.poll_seals(now),
        [object],
        "sealed at once, not after the delay"
    );
    let (next, _) = ack_group(&tier, &mut index, &bytes(4, 10), now).unwrap();
    assert_ne!(next, object, "a closed block took a range");
    index.seal(object);
    tier.sealed(object).unwrap();
    tier.discard(object).unwrap();
    assert_eq!(fs.contents(&block_name(object)), None);
    assert_eq!(tier.usage().used, 20, "the upload's block and the new one");
}

/// Catches: a restart that forgets its flushed copies (they would fill the disk
/// uncounted) or deletes them as orphans, and one that keeps more than the budget.
#[test]
fn a_restart_keeps_flushed_copies_within_the_budget() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    let a = ack_upload(&tier, &mut index, &bytes(1, 10)).unwrap();
    let b = ack_upload(&tier, &mut index, &bytes(2, 10)).unwrap();
    for object in [a, b] {
        tier.flushed(object).unwrap();
        index.tiers.get_mut(&support::TIER).unwrap().remove(&object);
    }
    drop(tier);
    let (tier, rec) = open_tier(&fs.restart(), 2, options(1 << 20, LIMIT), &index).unwrap();
    assert_eq!(rec.flushed, [a, b]);
    assert_eq!(rec.removed, Vec::<String>::new());
    assert_eq!(tier.usage().flushed, 20);
    assert_eq!(tier.read(b, 0, 10).unwrap(), bytes(2, 10));
    let small = fs.restart();
    let (tier, _) = open_tier(&small, 3, options(15, LIMIT), &index).unwrap();
    assert_eq!(tier.usage().flushed, 10);
    assert_eq!(
        small
            .list()
            .unwrap()
            .iter()
            .filter(|n| n.ends_with(".seg"))
            .count(),
        1
    );
}

/// Catches: unflushed bytes that count reserved but uncommitted bytes or flushed
/// ones, and an oldest age taken from the newest block.
#[test]
fn unflushed_counts_committed_bytes_and_the_oldest_block() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let t0 = Instant::now();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    assert_eq!(tier.unflushed(t0).oldest, None);
    let (old, _) = ack_group(&tier, &mut index, &bytes(1, 10), t0).unwrap();
    let _uncommitted = tier.reserve_group(5, t0).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let young = ack_upload(&tier, &mut index, &bytes(2, 7)).unwrap();
    let at = Instant::now() + Duration::from_secs(60);
    let u = tier.unflushed(at);
    assert_eq!((u.bytes, u.blocks), (17, 2));
    assert_eq!(u.oldest, Some(at.duration_since(t0)));
    tier.flushed(young).unwrap();
    let u = tier.unflushed(at);
    assert_eq!((u.bytes, u.blocks), (10, 1));
    assert_ne!(old, young);
}

/// Catches: a failed rename reported as a kept copy (a read would look for a file
/// that is not there, and the budget would count it), and a block left counting after
/// its object was flushed; and a failed directory sync after the rename unreported.
#[test]
fn a_flush_whose_rename_or_sync_fails_is_reported_and_stops_counting() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    let a = ack_upload(&tier, &mut index, &bytes(1, 10)).unwrap();
    let b = ack_upload(&tier, &mut index, &bytes(2, 10)).unwrap();
    fs.fail_at(fs.ops(), kbf_store::Fault::Error);
    assert!(matches!(
        tier.flushed(a),
        Err(TierError::Io { op: "rename", .. })
    ));
    assert!(
        fs.contents(&block_name(a)).is_some(),
        "left for the next start's sweep"
    );
    assert_eq!(fs.contents(&seg_name(a)), None);
    assert_eq!((tier.usage().used, tier.usage().flushed), (10, 0));
    fs.fail_at(fs.ops() + 1, kbf_store::Fault::Error);
    assert!(matches!(
        tier.flushed(b),
        Err(TierError::Io { op: "sync_dir", .. })
    ));
    assert_eq!(tier.read(b, 0, 10).unwrap(), bytes(2, 10), "renamed, kept");
    assert_eq!((tier.usage().used, tier.usage().flushed), (0, 10));
}

/// Catches: a flushed copy deleted under the tier still counted (its bytes would
/// hold budget no file holds), or its read answered as anything but "not here" (the
/// reader then goes to the store).
#[test]
fn a_copy_deleted_under_the_tier_stops_counting() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    let a = ack_upload(&tier, &mut index, &bytes(1, 10)).unwrap();
    tier.flushed(a).unwrap();
    fs.remove(&seg_name(a)).unwrap();
    assert!(matches!(
        tier.read(a, 0, 10),
        Err(TierError::UnknownBlock(_))
    ));
    assert_eq!(tier.usage().flushed, 0);
}

/// Catches: a read past a block's committed length served (bytes never acknowledged),
/// a read of an object the tier does not hold answered with bytes, and a disk error
/// on a block or a copy taken for "not here" (the reader would then treat an
/// unreadable staged block like a flushed one) instead of an I/O error.
#[test]
fn reads_serve_committed_bytes_and_report_disk_errors() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    let (block, at) = ack_group(&tier, &mut index, &bytes(1, 10), now).unwrap();
    assert_eq!(tier.read(block, at, 10).unwrap(), bytes(1, 10));
    assert!(matches!(
        tier.read(block, at, 11),
        Err(TierError::NotCommitted {
            end: 11,
            committed: 10,
            ..
        })
    ));
    let elsewhere = kbf_meta::ObjectId::new(kbf_meta::Epoch::new(9), 9);
    assert!(matches!(
        tier.read(elsewhere, 0, 1),
        Err(TierError::UnknownBlock(_))
    ));
    fs.fail_at(fs.ops(), kbf_store::Fault::Error);
    assert!(matches!(
        tier.read(block, at, 10),
        Err(TierError::Io { op: "read", .. })
    ));

    let copy = ack_upload(&tier, &mut index, &bytes(2, 10)).unwrap();
    tier.flushed(copy).unwrap();
    fs.fail_at(fs.ops(), kbf_store::Fault::Error);
    assert!(matches!(
        tier.read(copy, 0, 10),
        Err(TierError::Io { op: "read", .. })
    ));
    assert_eq!(
        tier.read(copy, 0, 10).unwrap(),
        bytes(2, 10),
        "still counted"
    );
}

/// Catches: a block deleted under the tier (a disk replaced while running) read as
/// anything but "not here": the reader must not get an I/O error it would retry, nor
/// bytes.
#[test]
fn a_block_deleted_under_the_tier_reads_as_not_here() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    let object = ack_upload(&tier, &mut index, &bytes(1, 10)).unwrap();
    fs.remove(&block_name(object)).unwrap();
    assert!(matches!(
        tier.read(object, 0, 10),
        Err(TierError::UnknownBlock(_))
    ));
}

/// Catches a tier whose node keeps its local copies in the one cache
/// (`kbf_storage::local_cache`) still keeping a flushed copy: a second copy of every
/// flushed blob on the same disk (the "two directories" mutant). Without flushed
/// copies, a flushed block is deleted, stops counting, is read by no one (a reader goes
/// to the store), and a reservation does not see it.
#[test]
fn without_flushed_copies_a_flushed_block_is_deleted() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let options = options(1 << 20, LIMIT).without_flushed_copies();
    assert!(!options.flushed_copies);
    let (tier, _) = open_tier(&fs, 1, options, &index).unwrap();
    let (object, at) = ack_group(&tier, &mut index, &bytes(1, 10), now).unwrap();
    seal(&tier, &mut index, now);
    tier.flushed(object).unwrap();
    assert_eq!(fs.contents(&block_name(object)), None);
    assert_eq!(fs.contents(&seg_name(object)), None);
    assert!(matches!(
        tier.read(object, at, 10),
        Err(TierError::UnknownBlock(o)) if o == object
    ));
    let usage = tier.usage();
    assert_eq!((usage.used, usage.flushed), (0, 0));
    let other = ack_upload(&tier, &mut index, &bytes(2, 10)).unwrap();
    fs.fail_at(fs.ops(), kbf_store::Fault::Error);
    assert!(matches!(
        tier.flushed(other),
        Err(TierError::Io { op: "remove", .. })
    ));
    assert!(
        fs.contents(&block_name(other)).is_some(),
        "left for the next start's sweep"
    );
    assert_eq!(fs.contents(&seg_name(other)), None);
    assert_eq!(tier.usage().used, 0);
}
