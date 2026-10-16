//! The decoded-directory LRU on its own: the cases the walk cannot reach (the same
//! directory inserted twice, a verification for a directory no longer kept).

use std::sync::Arc;

use kbf_types::{Digest, FarmTime};

use super::{Decoded, ENTRY_OVERHEAD, InputTreeConfig, Lru};

fn digest(n: u8) -> Digest {
    kbf_segments::sha256(&[n])
}

fn decoded(files: usize) -> Arc<Decoded> {
    Arc::new(Decoded {
        files: vec![digest(0); files].into_boxed_slice(),
        dirs: Box::default(),
    })
}

fn config(entries: usize, bytes: u64) -> InputTreeConfig {
    InputTreeConfig {
        max_entries: entries.try_into().expect("nonzero"),
        max_bytes: bytes,
        ..InputTreeConfig::DEFAULT
    }
}

/// Catches: a directory inserted twice (two flights racing across an eviction) counted
/// twice in bytes or entered twice in the recency order, so that evicting it later
/// leaves a phantom cost behind; the second insert must replace the first.
#[test]
fn inserting_a_directory_twice_keeps_one_entry_and_one_cost() {
    let mut lru = Lru::new(config(8, u64::MAX));
    lru.insert(digest(1), decoded(2));
    let cost = lru.bytes;
    lru.insert(digest(1), decoded(5));
    let replaced = decoded(5).cost();
    assert_eq!(
        (lru.entries.len(), lru.order.len(), lru.bytes),
        (1, 1, replaced)
    );
    assert_ne!(cost, replaced);
    // An entry replaced by one too dear to keep leaves nothing behind.
    lru.config.max_bytes = replaced - 1;
    lru.insert(digest(1), decoded(5));
    assert_eq!((lru.entries.len(), lru.order.len(), lru.bytes), (0, 0, 0));
}

/// Catches: a verification landing on a directory the bound already evicted, which
/// would invent an entry with no directory (and no cost) behind it.
#[test]
fn a_verification_for_an_evicted_directory_is_dropped() {
    let mut lru = Lru::new(config(1, u64::MAX));
    lru.insert(digest(1), decoded(1));
    lru.insert(digest(2), decoded(1));
    lru.verified(&digest(1), FarmTime::from_millis(9), 0);
    assert_eq!(lru.entries.len(), 1);
    assert!(lru.get(&digest(1)).is_none());
}

/// Catches: a later, shorter verification lowering an earlier longer one, and the
/// byte bound counting less than the directory's cost.
#[test]
fn a_verification_never_lowers_and_bytes_follow_cost() {
    let mut lru = Lru::new(config(8, 2 * ENTRY_OVERHEAD));
    lru.insert(digest(1), decoded(0));
    lru.verified(&digest(1), FarmTime::from_millis(9), 0);
    lru.verified(&digest(1), FarmTime::from_millis(5), 0);
    let (_, verified) = lru.get(&digest(1)).expect("kept");
    assert_eq!(verified.map(|v| v.until), Some(FarmTime::from_millis(9)));
    lru.insert(digest(2), decoded(0));
    lru.insert(digest(3), decoded(0));
    assert_eq!(lru.bytes, 2 * ENTRY_OVERHEAD);
    assert_eq!(lru.entries.len(), 2);
}

/// Catches: a record of a newer loss count replaced by a check that began earlier and
/// ended later (it would make a skip pass on a count that has already been outdated),
/// a record of an older count kept when a check at the new count ends with an earlier
/// `until` (the stale, longer time would survive), and a count not stored at all.
#[test]
fn a_verification_keeps_the_newest_loss_count_and_replaces_older_ones() {
    let at = |lru: &mut Lru| {
        let (_, v) = lru.get(&digest(1)).expect("kept");
        let v = v.expect("verified");
        (v.until.as_millis(), v.losses)
    };
    let mut lru = Lru::new(config(8, u64::MAX));
    lru.insert(digest(1), decoded(0));
    lru.verified(&digest(1), FarmTime::from_millis(90), 3);
    assert_eq!(at(&mut lru), (90, 3));
    // A later count replaces, even with an earlier time.
    lru.verified(&digest(1), FarmTime::from_millis(50), 4);
    assert_eq!(at(&mut lru), (50, 4));
    // An earlier count that ended late changes nothing.
    lru.verified(&digest(1), FarmTime::from_millis(99), 3);
    assert_eq!(at(&mut lru), (50, 4));
    // The same count takes the later time.
    lru.verified(&digest(1), FarmTime::from_millis(70), 4);
    assert_eq!(at(&mut lru), (70, 4));
}
