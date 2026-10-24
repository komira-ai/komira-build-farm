//! The tiered store: puts, write-back, reads, eviction, deletes, listings and restarts.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{Backend, Error, EvictionReport, Maintenance, Op};

use crate::support::{Rig, body, check_ranges, keys, read};

/// Asserts that a second put of a key is refused by the hot tier, that the first object is
/// kept, and that the refused put is not counted in the hot tier's bytes.
#[tokio::test]
async fn a_second_put_of_a_key_is_refused_by_the_hot_tier() {
    let rig = Rig::new();
    let tiered = rig.open(100).await;
    tiered.put("k", body("one")).await.unwrap();
    let again = tiered.put("k", body("two")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(tiered.get("k", None).await.unwrap(), body("one"));
    assert_eq!(tiered.hot_bytes(), 3);
}

/// Asserts that a put is acknowledged by the hot tier alone, is readable at once from it,
/// is copied to the cold tier by the write-back, and is readable from the cold tier after
/// the hot tier has dropped it.
#[tokio::test]
async fn a_put_is_readable_at_once_and_after_write_back_from_cold() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    tiered.put("k", body("value")).await.unwrap();
    assert_eq!(tiered.get("k", None).await.unwrap(), body("value"));
    assert_eq!(keys(&*rig.cold_mem).await, Vec::<String>::new());
    assert_eq!(tiered.pending(), ["k"]);

    let copied = tiered.write_back().await;
    assert_eq!(copied.copied, ["k"]);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("value"));
    assert_eq!(tiered.pending(), Vec::<String>::new());

    let evicted = tiered.evict().await;
    assert_eq!(evicted.evicted, ["k"]);
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());
    assert_eq!(tiered.hot_bytes(), 0);
    assert_eq!(tiered.get("k", None).await.unwrap(), body("value"));
    assert_eq!(tiered.head("k").await.unwrap().size, 5);
}

/// Asserts that range reads are byte-exact from the hot tier, and again from the cold tier
/// after the hot tier has dropped the object: the first and last byte, a middle slice, a
/// range past the end, empty ranges, and an error for a range starting past the end.
#[tokio::test]
async fn range_reads_are_byte_exact_at_both_tiers() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    tiered.put("k", body("0123456789")).await.unwrap();
    check_ranges(&tiered, "k").await;

    let pass = tiered.maintain().await;
    assert_eq!(pass.eviction.evicted, ["k"]);
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());
    check_ranges(&tiered, "k").await;
}

/// Asserts that a read of an object neither tier holds is `NotFound`, for a whole object and
/// for its head.
#[tokio::test]
async fn an_object_neither_tier_holds_is_not_found() {
    let rig = Rig::new();
    let tiered = rig.open(10).await;
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(tiered.get("k", None).await.unwrap_err(), missing);
    assert_eq!(tiered.head("k").await.unwrap_err(), missing);
}

/// Asserts that nothing is evicted before the cold tier holds it: with the cold tier down the
/// puts are still acknowledged, the write-back ends at the first failure, the hot tier grows
/// past its budget and keeps every object; once the cold tier is back and the write-back has
/// run, the least recently used objects are evicted until the hot tier is under its budget.
#[tokio::test]
async fn with_cold_down_nothing_is_evicted_and_afterwards_hot_comes_under_budget() {
    let rig = Rig::new();
    let tiered = rig.open(10).await;
    rig.cold.set_down(true);
    for key in ["a", "b", "c"] {
        tiered.put(key, body("12345678")).await.unwrap();
    }

    let first = tiered.maintain().await;
    assert_eq!(first.write_back.copied, Vec::<String>::new());
    assert_eq!(first.write_back.failed.len(), 1);
    assert_eq!(first.eviction, EvictionReport::default());
    assert_eq!(tiered.hot_bytes(), 24);
    assert_eq!(keys(&*rig.hot_mem).await, ["a", "b", "c"]);
    assert_eq!(tiered.pending(), ["a", "b", "c"]);

    rig.cold.set_down(false);
    let second = tiered.maintain().await;
    assert_eq!(second.write_back.copied, ["a", "b", "c"]);
    assert_eq!(second.eviction.evicted, ["a", "b"]);
    assert_eq!(tiered.hot_bytes(), 8);
    assert_eq!(keys(&*rig.hot_mem).await, ["c"]);
    assert_eq!(keys(&*rig.cold_mem).await, ["a", "b", "c"]);
}

/// Asserts that eviction takes the least recently used object first, and that reading an
/// object from the hot tier makes it the most recently used.
#[tokio::test]
async fn eviction_takes_the_least_recently_used_object_first() {
    let rig = Rig::new();
    let tiered = rig.open(10).await;
    for key in ["a", "b", "c"] {
        tiered.put(key, body("12345678")).await.unwrap();
    }
    tiered.get("a", None).await.unwrap();

    let pass = tiered.maintain().await;
    assert_eq!(pass.eviction.evicted, ["b", "c"]);
    assert_eq!(keys(&*rig.hot_mem).await, ["a"]);
}

/// Asserts that a read from the hot tier of an object the store did not put (it was written
/// to the hot tier by another path) is served and changes nothing the store counts.
#[tokio::test]
async fn a_read_of_an_object_the_store_does_not_track_is_served() {
    let rig = Rig::new();
    let tiered = rig.open(10).await;
    rig.hot_mem.put("stray", body("abc")).await.unwrap();
    assert_eq!(tiered.get("stray", None).await.unwrap(), body("abc"));
    assert_eq!(tiered.hot_bytes(), 0);
}

/// Asserts that a cold object under the same key with the same size and content tag counts
/// as copied, is left as it is, and lets the hot copy be evicted.
#[tokio::test]
async fn a_cold_object_with_the_same_content_counts_as_copied() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    rig.cold_mem.put("a", body("aaaa")).await.unwrap();
    tiered.put("a", body("aaaa")).await.unwrap();

    let pass = tiered.maintain().await;
    assert_eq!(pass.write_back.copied, ["a"]);
    assert_eq!(pass.write_back.clashed, Vec::<String>::new());
    assert_eq!(pass.eviction.evicted, ["a"]);
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());
    assert_eq!(read(&*rig.cold_mem, "a").await, body("aaaa"));
}

/// Asserts that a cold object under the same key with another content tag (the same size) is
/// not treated as copied: it is reported once, the cold object is left as it was, the hot
/// copy is kept and never evicted, later passes leave it alone, other objects are still
/// copied and evicted, and deleting the key clears the report.
#[tokio::test]
async fn a_cold_copy_with_another_tag_is_reported_and_the_hot_copy_kept() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    rig.cold_mem.put("a", body("bbbb")).await.unwrap();
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.put("z", body("zzzz")).await.unwrap();

    let first = tiered.maintain().await;
    assert_eq!(first.write_back.clashed, ["a"]);
    assert_eq!(first.write_back.copied, ["z"]);
    assert_eq!(first.eviction.evicted, ["z"]);
    assert_eq!(keys(&*rig.hot_mem).await, ["a"]);
    assert_eq!(read(&*rig.cold_mem, "a").await, body("bbbb"));
    assert_eq!(tiered.clashed(), ["a"]);
    assert_eq!(tiered.pending(), Vec::<String>::new());
    assert_eq!(tiered.get("a", None).await.unwrap(), body("aaaa"));

    let second = tiered.maintain().await;
    assert_eq!(second, Maintenance::default());

    tiered.delete("a").await.unwrap();
    assert_eq!(tiered.clashed(), Vec::<String>::new());
    assert_eq!(tiered.hot_bytes(), 0);
}

/// Asserts that eviction asks the cold tier again before it removes anything: an object the
/// cold tier lost after the write-back stays in the hot tier, is reported as unconfirmed,
/// becomes pending again, and is copied and then evicted by the next passes.
#[tokio::test]
async fn an_object_the_cold_tier_lost_is_kept_and_copied_again() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.write_back().await;
    rig.cold_mem.delete("a").await.unwrap();

    let pass = tiered.evict().await;
    assert_eq!(pass.unconfirmed, ["a"]);
    assert_eq!(pass.evicted, Vec::<String>::new());
    assert_eq!(tiered.pending(), ["a"]);
    assert_eq!(keys(&*rig.hot_mem).await, ["a"]);

    let again = tiered.maintain().await;
    assert_eq!(again.write_back.copied, ["a"]);
    assert_eq!(again.eviction.evicted, ["a"]);
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());
}

/// Asserts that a cold copy replaced after the write-back by other content (the same size) is
/// not trusted: eviction keeps the hot copy, and the next write-back reports the clash.
#[tokio::test]
async fn a_cold_copy_replaced_after_the_write_back_is_not_trusted() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.write_back().await;
    rig.cold_mem.delete("a").await.unwrap();
    rig.cold_mem.put("a", body("xxxx")).await.unwrap();

    let pass = tiered.evict().await;
    assert_eq!(pass.unconfirmed, ["a"]);
    assert_eq!(keys(&*rig.hot_mem).await, ["a"]);

    let copy = tiered.write_back().await;
    assert_eq!(copy.clashed, ["a"]);
    assert_eq!(copy.copied, Vec::<String>::new());
    assert_eq!(keys(&*rig.hot_mem).await, ["a"]);
    assert_eq!(read(&*rig.cold_mem, "a").await, body("xxxx"));
}

/// Asserts that a store opened over the same backends as a stopped one finds the pending
/// write-backs by listing the tiers and finishes them: an object the cold tier holds with the
/// same size is not pending, one it lacks is copied, one it holds at another size is reported
/// as a clash, and the hot tier's bytes are counted again.
#[tokio::test]
async fn a_store_opened_over_the_same_backends_finishes_the_pending_write_backs() {
    let rig = Rig::new();
    let first = rig.open(1000).await;
    first.put("a", body("aaaa")).await.unwrap();
    first.put("b", body("bbbb")).await.unwrap();
    first.put("c", body("cc")).await.unwrap();
    rig.cold_mem.put("a", body("aaaa")).await.unwrap();
    rig.cold_mem.put("c", body("cccccc")).await.unwrap();
    drop(first);

    let second = rig.open(1000).await;
    assert_eq!(second.hot_bytes(), 10);
    assert_eq!(second.pending(), ["b", "c"]);
    let pass = second.write_back().await;
    assert_eq!(pass.copied, ["b"]);
    assert_eq!(pass.clashed, ["c"]);
    assert_eq!(read(&*rig.cold_mem, "b").await, body("bbbb"));
    assert_eq!(read(&*rig.cold_mem, "c").await, body("cccccc"));
    assert_eq!(second.pending(), Vec::<String>::new());
}

/// Asserts that a delete removes both copies, that the key is then not found, that the hot
/// tier's bytes drop, and that deleting a key neither tier holds succeeds.
#[tokio::test]
async fn delete_removes_both_copies() {
    let rig = Rig::new();
    let tiered = rig.open(1000).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.write_back().await;
    assert_eq!(keys(&*rig.hot_mem).await, ["a"]);
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);

    tiered.delete("a").await.unwrap();
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());
    assert_eq!(keys(&*rig.cold_mem).await, Vec::<String>::new());
    assert_eq!(tiered.hot_bytes(), 0);
    let missing = Error::NotFound("a".to_owned());
    assert_eq!(tiered.get("a", None).await.unwrap_err(), missing);
    tiered.delete("a").await.unwrap();
}

/// Asserts that deleting an object only the cold tier holds (the hot tier evicted it)
/// removes it, and a delete of a pending object is not undone by the write-back.
#[tokio::test]
async fn delete_removes_an_evicted_object_and_a_pending_one_stays_deleted() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.maintain().await;
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);
    tiered.delete("a").await.unwrap();
    assert_eq!(keys(&*rig.cold_mem).await, Vec::<String>::new());

    tiered.put("b", body("bbbb")).await.unwrap();
    tiered.delete("b").await.unwrap();
    let pass = tiered.maintain().await;
    assert_eq!(pass, Maintenance::default());
    assert_eq!(keys(&*rig.cold_mem).await, Vec::<String>::new());
}

/// Asserts that a listing is the union of both tiers without duplicates, in key order, with
/// the prefix applied to both.
#[tokio::test]
async fn a_listing_is_the_union_of_both_tiers_without_duplicates() {
    let rig = Rig::new();
    let tiered = rig.open(1000).await;
    tiered.put("x/1", body("one")).await.unwrap();
    tiered.put("x/3", body("three")).await.unwrap();
    rig.cold_mem.put("x/2", body("two")).await.unwrap();
    rig.cold_mem.put("x/3", body("three")).await.unwrap();
    rig.cold_mem.put("y/1", body("other")).await.unwrap();

    assert_eq!(keys(&tiered).await, ["x/1", "x/2", "x/3", "y/1"]);
    let listed = tiered.list("x/").await.unwrap();
    let sizes: Vec<u64> = listed.iter().map(|object| object.size).collect();
    assert_eq!(sizes, [3, 3, 5]);
    let names: Vec<&str> = listed.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(names, ["x/1", "x/2", "x/3"]);
}

/// Asserts that a listing keeps an object that moves to the cold tier while the listing is
/// under way: the hot tier's answer comes after the write-back and the eviction, and the
/// cold tier's listing, read after it, has the object.
#[tokio::test(start_paused = true)]
async fn a_listing_keeps_an_object_that_moves_to_the_cold_tier_meanwhile() {
    let rig = Rig::new();
    let tiered = Arc::new(rig.open(0).await);
    tiered.put("k", body("value")).await.unwrap();
    rig.hot.set_delay(Op::List, Some(Duration::from_secs(10)));
    let listing = tokio::spawn({
        let tiered = tiered.clone();
        async move { tiered.list("").await }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    tiered.maintain().await;
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());

    let listed = listing.await.unwrap().unwrap();
    let names: Vec<String> = listed.into_iter().map(|object| object.key).collect();
    assert_eq!(names, ["k"]);
}

/// Asserts that a write-back pass copies the objects pending when it starts and no others, so
/// puts during a pass cannot keep it from ending, and that an object deleted before its turn
/// is left out of the pass without a failure.
#[tokio::test(start_paused = true)]
async fn a_write_back_pass_copies_what_was_pending_when_it_started() {
    let rig = Rig::new();
    let tiered = Arc::new(rig.open(100).await);
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.put("b", body("bbbb")).await.unwrap();
    rig.cold.set_delay(Op::Put, Some(Duration::from_secs(10)));
    let pass = tokio::spawn({
        let tiered = tiered.clone();
        async move { tiered.write_back().await }
    });
    let deletion = tokio::spawn({
        let tiered = tiered.clone();
        async move { tiered.delete("b").await }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    tiered.put("late", body("llll")).await.unwrap();

    let report = pass.await.unwrap();
    deletion.await.unwrap().unwrap();
    assert_eq!(report.copied, ["a"]);
    assert_eq!(report.clashed, Vec::<String>::new());
    assert_eq!(report.failed.len(), 0);
    assert_eq!(tiered.pending(), ["late"]);
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);
}

/// Asserts that a put refused as existing for an object the store does not count (the hot
/// tier holds it from an earlier put that failed after the write landed) starts counting it
/// when the hot tier answers about it, so the write-back copies it; and that when the hot tier
/// does not answer, the put is still refused as existing and the next put counts the object.
#[tokio::test]
async fn a_refused_put_of_an_uncounted_object_starts_counting_it() {
    let rig = Rig::new();
    let tiered = rig.open(100).await;
    rig.hot_mem.put("k", body("value")).await.unwrap();
    rig.hot.fail_next(Op::Head, 1);
    let refused = tiered.put("k", body("value")).await;
    assert_eq!(refused, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(tiered.pending(), Vec::<String>::new());
    assert_eq!(tiered.hot_bytes(), 0);

    let again = tiered.put("k", body("value")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(tiered.pending(), ["k"]);
    assert_eq!(tiered.hot_bytes(), 5);
    let pass = tiered.write_back().await;
    assert_eq!(pass.copied, ["k"]);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("value"));
}

/// Asserts that a background task given a zero interval waits a short while between passes
/// instead of spinning: time goes on, and a put is still copied.
#[tokio::test(start_paused = true)]
async fn the_background_task_does_not_spin_on_a_zero_interval() {
    let rig = Rig::new();
    let tiered = Arc::new(rig.open(1000).await);
    let background = tiered.spawn_background(Duration::ZERO);
    tokio::time::sleep(Duration::from_secs(1)).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);
    drop(background);
}

/// Asserts that the background task copies what was put when it starts, retries a pass that
/// failed after the retry interval, copies a put at once without waiting for the interval,
/// and stops when its handle is dropped.
#[tokio::test(start_paused = true)]
async fn the_background_task_copies_puts_retries_failures_and_stops_when_dropped() {
    let rig = Rig::new();
    let tiered = Arc::new(rig.open(1000).await);
    rig.cold.set_down(true);
    tiered.put("a", body("aaaa")).await.unwrap();
    let background = tiered.spawn_background(Duration::from_secs(5));

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(tiered.pending(), ["a"]);

    rig.cold.set_down(false);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(tiered.pending(), Vec::<String>::new());
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);

    tiered.put("b", body("bbbb")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(tiered.pending(), Vec::<String>::new());
    assert_eq!(keys(&*rig.cold_mem).await, ["a", "b"]);

    drop(background);
    tiered.put("c", body("cccc")).await.unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(tiered.pending(), ["c"]);
}
