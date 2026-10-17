//! The held instance names. Each test names the defect it catches.

use std::time::{Duration, Instant};

use super::{NAMES_FRESH, NameCache, Names};

/// Catches: names used for ever (a new instance never listed), names thrown away at once
/// (every request reads the index), and a cache that answers before anything was put in it.
/// The clock is an argument, so no test waits.
#[test]
fn names_are_held_for_the_fresh_period_only() {
    let start = Instant::now();
    let mut cache = NameCache::default();
    assert_eq!(cache.get(start), None);
    let names = Names {
        list: vec!["a".to_owned()],
        truncated: true,
    };
    cache.put(start, names.clone());
    assert_eq!(cache.get(start), Some(names.clone()));
    assert_eq!(
        cache.get(start + NAMES_FRESH - Duration::from_millis(1)),
        Some(names)
    );
    assert_eq!(cache.get(start + NAMES_FRESH), None);
    // Put again, it is fresh from then.
    let newer = Names::default();
    cache.put(start + NAMES_FRESH, newer.clone());
    assert_eq!(cache.get(start + NAMES_FRESH), Some(newer));
    // A clock that reads earlier than the put (never so, but a panic is worse) is fresh.
    assert!(cache.get(start).is_some());
}
