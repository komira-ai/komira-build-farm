//! The restored-record cache: a record is kept while its header is the one it was made
//! under, and the least recently used leave first when the bound is passed. Each test
//! names the defect it catches.

use std::collections::BTreeSet;
use std::sync::Arc;

use kbf_history::records::InvocationHeader;
use kbf_invocation::{Invocation, InvocationId, RecordKey, Restored, WallTime};

use super::{Cache, Stamp, stamp_of};

fn key(id: &str) -> RecordKey {
    RecordKey::owner(InvocationId::new(id))
}

fn record(id: &str) -> Arc<Invocation> {
    Arc::new(Invocation::restored(
        Restored {
            key: key(id),
            serial: 1 << 63,
            instances: BTreeSet::from(["main".to_owned()]),
            first_seen: WallTime::default(),
            last_seen: WallTime::default(),
            complete: true,
            gap_from: None,
            rows_capped: false,
        },
        Vec::new(),
    ))
}

fn stamp(chunks: u32) -> Stamp {
    stamp_of(&InvocationHeader {
        chunks,
        ..InvocationHeader::default()
    })
}

/// Catches: a record served after history gained a chunk (a page that stops at the
/// first one for as long as it is cached), and a record dropped while its header is
/// unchanged (every read goes to the object store).
#[test]
fn a_record_is_held_only_under_the_header_it_was_made_under() {
    let mut cache = Cache::new(1 << 20);
    let a = record("a");
    cache.put(key("a"), stamp(1), Arc::clone(&a));
    let got = cache.get(&key("a"), stamp(1)).expect("held");
    assert!(
        Arc::ptr_eq(&got, &a),
        "the same record, so its serial holds"
    );
    assert!(cache.get(&key("a"), stamp(2)).is_none());
    assert_eq!(cache.len(), 0, "a stale record is dropped, not kept beside");
    assert!(cache.get(&key("b"), stamp(1)).is_none());
}

/// Catches: no bound (the cache grows with every build read), the newest dropped
/// instead of the oldest, a read that does not count as a use, a record bigger than the
/// bound held anyway, and bytes miscounted when a record is replaced (the cache would
/// drop records that fit, or keep ones that do not).
#[test]
fn the_least_recently_used_leave_first_and_the_bound_holds() {
    let one = record("a").bytes();
    let mut cache = Cache::new(one * 2 + one / 2);
    cache.put(key("a"), stamp(1), record("a"));
    cache.put(key("b"), stamp(1), record("b"));
    // `a` is used, so `b` is the oldest.
    assert!(cache.get(&key("a"), stamp(1)).is_some());
    cache.put(key("c"), stamp(1), record("c"));
    assert_eq!(cache.len(), 2);
    assert!(cache.get(&key("b"), stamp(1)).is_none());
    assert!(cache.get(&key("a"), stamp(1)).is_some());
    assert!(cache.get(&key("c"), stamp(1)).is_some());
    // A record put again replaces its old one: two records still fit.
    cache.put(key("a"), stamp(2), record("a"));
    assert_eq!(cache.len(), 2);
    assert!(cache.get(&key("c"), stamp(1)).is_some());
    // Larger than the whole bound: not held, and what it replaced is gone.
    let mut small = Cache::new(one - 1);
    small.put(key("a"), stamp(1), record("a"));
    assert_eq!(small.len(), 0);
}
