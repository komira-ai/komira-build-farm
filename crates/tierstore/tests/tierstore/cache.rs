//! The RAM read cache in front of a backend.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{Backend, ByteRange, CachedStore, Error, FaultyBackend, MemoryBackend, Op};

use crate::support::{Rig, body, keys};

/// A cache over a memory backend whose calls are counted.
struct Fixture {
    backend: Arc<FaultyBackend>,
    cached: CachedStore,
}

fn fixture(budget: u64) -> Fixture {
    let backend = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    let cached = CachedStore::new(backend.clone(), budget);
    Fixture { backend, cached }
}

fn range(offset: u64, len: u64) -> Option<ByteRange> {
    Some(ByteRange::new(offset, len))
}

/// Asserts that a repeated read of one range is answered without a call to the backend, that
/// another range of the object and the whole object are reads of their own, and that each
/// is then cached too.
#[tokio::test]
async fn repeated_ranges_are_served_without_a_backend_call() {
    let f = fixture(100);
    f.cached.put("k", body("0123456789")).await.unwrap();
    for _ in 0..3 {
        let got = f.cached.get("k", range(2, 3)).await.unwrap();
        assert_eq!(got, body("234"));
    }
    assert_eq!(f.backend.calls(Op::Get), 1);

    let start = f.cached.get("k", range(0, 2)).await.unwrap();
    assert_eq!(start, body("01"));
    assert_eq!(f.backend.calls(Op::Get), 2);
    let whole = f.cached.get("k", None).await.unwrap();
    assert_eq!(whole, body("0123456789"));
    assert_eq!(f.backend.calls(Op::Get), 3);

    f.cached.get("k", range(0, 2)).await.unwrap();
    f.cached.get("k", None).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 3);
    assert_eq!(f.cached.cached_bytes(), 15);
}

/// Asserts that the cache drops the least recently used entries beyond its byte budget, that
/// reading an entry makes it the most recently used, and that a dropped entry is read from
/// the backend again.
#[tokio::test]
async fn entries_beyond_the_budget_are_dropped_least_recently_used_first() {
    let f = fixture(10);
    f.cached.put("k", body("0123456789ab")).await.unwrap();
    f.cached.get("k", range(0, 4)).await.unwrap();
    f.cached.get("k", range(4, 4)).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 2);
    assert_eq!(f.cached.cached_bytes(), 8);

    f.cached.get("k", range(0, 4)).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 2);
    f.cached.get("k", range(8, 4)).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 3);
    assert_eq!(f.cached.cached_bytes(), 8);

    f.cached.get("k", range(0, 4)).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 3);
    f.cached.get("k", range(4, 4)).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 4);
    assert_eq!(f.cached.cached_bytes(), 8);
}

/// Asserts that a read larger than the whole budget is not cached, and does not push out the
/// entries that are.
#[tokio::test]
async fn a_read_larger_than_the_budget_is_not_cached() {
    let f = fixture(5);
    f.cached.put("k", body("0123456789")).await.unwrap();
    f.cached.get("k", range(0, 2)).await.unwrap();
    f.cached.get("k", None).await.unwrap();
    f.cached.get("k", None).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 3);
    assert_eq!(f.cached.cached_bytes(), 2);
    f.cached.get("k", range(0, 2)).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 3);
}

/// Asserts that an empty read counts as one byte against the budget, so many empty ranges do
/// not grow the cache past it, and that the least recently used one is dropped first.
#[tokio::test]
async fn empty_reads_count_against_the_budget() {
    let f = fixture(2);
    f.cached.put("k", body("0123")).await.unwrap();
    for offset in 0..4 {
        let got = f.cached.get("k", range(offset, 0)).await.unwrap();
        assert_eq!(got, body(""));
    }
    assert_eq!(f.backend.calls(Op::Get), 4);
    assert_eq!(f.cached.cached_bytes(), 2);

    f.cached.get("k", range(3, 0)).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 4);
    f.cached.get("k", range(0, 0)).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), 5);
}

/// Asserts that errors are never cached: a failed read and a read of an absent object are
/// asked of the backend again, and see the object once it is there.
#[tokio::test]
async fn errors_are_never_cached() {
    let f = fixture(100);
    f.cached.put("k", body("value")).await.unwrap();
    f.backend.fail_next(Op::Get, 1);
    let failed = f.cached.get("k", None).await.unwrap_err();
    assert!(failed.is_unavailable());
    assert_eq!(f.cached.get("k", None).await.unwrap(), body("value"));
    assert_eq!(f.backend.calls(Op::Get), 2);

    let missing = Error::NotFound("late".to_owned());
    assert_eq!(f.cached.get("late", None).await.unwrap_err(), missing);
    assert_eq!(f.cached.get("late", None).await.unwrap_err(), missing);
    f.cached.put("late", body("here")).await.unwrap();
    assert_eq!(f.cached.get("late", None).await.unwrap(), body("here"));
    assert_eq!(f.cached.cached_bytes(), 9);
}

/// Asserts that a delete drops every cached range of that object and no other object's
/// entries, including the objects whose keys start with its key, and that the object is then
/// not found.
#[tokio::test]
async fn a_delete_drops_that_objects_ranges_only() {
    let f = fixture(100);
    for key in ["a", "ab", "b"] {
        f.cached.put(key, body("0123")).await.unwrap();
        f.cached.get(key, None).await.unwrap();
    }
    f.cached.get("a", range(1, 2)).await.unwrap();
    assert_eq!(f.cached.cached_bytes(), 14);

    f.cached.delete("a").await.unwrap();
    assert_eq!(f.cached.cached_bytes(), 8);
    let missing = Error::NotFound("a".to_owned());
    assert_eq!(f.cached.get("a", None).await.unwrap_err(), missing);

    let before = f.backend.calls(Op::Get);
    f.cached.get("ab", None).await.unwrap();
    f.cached.get("b", None).await.unwrap();
    assert_eq!(f.backend.calls(Op::Get), before);
}

/// Asserts that a delete the backend fails is an error and leaves the cached entries, since
/// the object may still be there.
#[tokio::test]
async fn a_failed_delete_keeps_the_cached_entries() {
    let f = fixture(100);
    f.cached.put("k", body("value")).await.unwrap();
    f.cached.get("k", None).await.unwrap();
    f.backend.fail_next(Op::Delete, 1);
    let failed = f.cached.delete("k").await.unwrap_err();
    assert!(failed.is_unavailable());
    assert_eq!(f.cached.cached_bytes(), 5);
}

/// Asserts that a read that started before a delete does not fill the cache with what it
/// read, even when the object has been put again since.
#[tokio::test(start_paused = true)]
async fn a_read_in_flight_during_a_delete_does_not_fill_the_cache() {
    let f = fixture(100);
    f.cached.put("k", body("old")).await.unwrap();
    f.backend.set_delay(Op::Get, Some(Duration::from_secs(10)));
    let read = f.cached.get("k", None);
    let replace = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        f.cached.delete("k").await.unwrap();
        f.cached.put("k", body("new")).await.unwrap();
    };
    let (read, ()) = tokio::join!(read, replace);
    assert_eq!(read.unwrap(), body("new"));
    assert_eq!(f.cached.cached_bytes(), 0);
}

/// Asserts that two reads of one range that miss at the same time leave one entry, counted
/// once.
#[tokio::test(start_paused = true)]
async fn two_simultaneous_misses_leave_one_entry() {
    let f = fixture(100);
    f.cached.put("k", body("value")).await.unwrap();
    f.backend.set_delay(Op::Get, Some(Duration::from_secs(1)));
    let (one, two) = tokio::join!(f.cached.get("k", None), f.cached.get("k", None));
    assert_eq!(one.unwrap(), body("value"));
    assert_eq!(two.unwrap(), body("value"));
    assert_eq!(f.backend.calls(Op::Get), 2);
    assert_eq!(f.cached.cached_bytes(), 5);
}

/// Asserts that put, head and list go to the backend as they are.
#[tokio::test]
async fn put_head_and_list_pass_through() {
    let f = fixture(100);
    f.cached.put("k", body("value")).await.unwrap();
    assert_eq!(f.cached.head("k").await.unwrap().size, 5);
    assert_eq!(keys(&f.cached).await, ["k"]);
    assert_eq!(keys(&*f.backend).await, ["k"]);
    assert_eq!(f.backend.calls(Op::Head), 1);
    assert_eq!(f.cached.cached_bytes(), 0);
}

/// Asserts that the cache works in front of a tiered store: a repeated range is not read
/// from either tier again, and a delete through the cache removes the object from both
/// tiers.
#[tokio::test]
async fn the_cache_works_in_front_of_a_tiered_store() {
    let rig = Rig::new();
    let tiered = Arc::new(rig.open(100).await);
    let cached = CachedStore::new(tiered.clone(), 100);
    cached.put("k", body("0123456789")).await.unwrap();
    for _ in 0..3 {
        let got = cached.get("k", range(4, 2)).await.unwrap();
        assert_eq!(got, body("45"));
    }
    assert_eq!(rig.hot.calls(Op::Get), 1);
    assert_eq!(rig.cold.calls(Op::Get), 0);

    tiered.write_back().await;
    cached.delete("k").await.unwrap();
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());
    assert_eq!(keys(&*rig.cold_mem).await, Vec::<String>::new());
    assert_eq!(cached.cached_bytes(), 0);
}
