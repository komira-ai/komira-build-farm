//! The tiered store when a tier fails.

use tierstore::{Backend, Error, Op, Tiered};

use crate::support::{Rig, body, keys};

/// Asserts that a put the hot tier fails is an error, so it is not acknowledged: nothing is
/// stored or counted, nothing is pending, and the same key can be put once the hot tier
/// answers.
#[tokio::test]
async fn a_put_the_hot_tier_fails_is_not_acknowledged() {
    let rig = Rig::new();
    let tiered = rig.open(100).await;
    rig.hot.fail_next(Op::Put, 1);
    let failed = tiered.put("k", body("value")).await.unwrap_err();
    assert!(failed.is_unavailable());
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());
    assert_eq!(tiered.hot_bytes(), 0);
    assert_eq!(tiered.pending(), Vec::<String>::new());

    tiered.put("k", body("value")).await.unwrap();
    assert_eq!(tiered.pending(), ["k"]);
}

/// Asserts that puts are acknowledged while the cold tier is down, that a write-back pass
/// that fails leaves the object pending and reports the failure, and that a later pass
/// copies it.
#[tokio::test]
async fn puts_are_acknowledged_with_cold_down_and_the_write_back_retries_later() {
    let rig = Rig::new();
    let tiered = rig.open(100).await;
    rig.cold.fail_next(Op::Put, 1);
    tiered.put("a", body("aaaa")).await.unwrap();

    let first = tiered.write_back().await;
    assert_eq!(first.copied, Vec::<String>::new());
    assert_eq!(first.failed.len(), 1);
    assert_eq!(first.failed[0].0, "a");
    assert!(first.failed[0].1.is_unavailable());
    assert_eq!(tiered.pending(), ["a"]);

    let second = tiered.write_back().await;
    assert_eq!(second.copied, ["a"]);
    assert_eq!(second.failed.len(), 0);
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);
}

/// Asserts that a write-back pass that fails on one object for a reason other than an
/// unavailable tier (here the hot tier lost the object) goes on to copy the next ones, and
/// that the failed object stays pending.
#[tokio::test]
async fn a_write_back_failure_that_is_not_an_outage_does_not_stop_the_pass() {
    let rig = Rig::new();
    let tiered = rig.open(100).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.put("b", body("bbbb")).await.unwrap();
    rig.hot_mem.delete("a").await.unwrap();

    let pass = tiered.write_back().await;
    assert_eq!(pass.copied, ["b"]);
    assert_eq!(pass.failed.len(), 1);
    assert_eq!(pass.failed[0].1, Error::NotFound("a".to_owned()));
    assert_eq!(tiered.pending(), ["a"]);
}

/// Asserts that an eviction pass ends at the first failure that says a tier is unavailable
/// (here the hot tier fails a delete), that the object stays, and that a later pass evicts
/// everything the budget asks for.
#[tokio::test]
async fn an_eviction_that_fails_with_an_outage_ends_the_pass_and_is_retried() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.put("b", body("bbbb")).await.unwrap();
    tiered.write_back().await;
    rig.hot.fail_next(Op::Delete, 1);

    let first = tiered.evict().await;
    assert_eq!(first.evicted, Vec::<String>::new());
    assert_eq!(first.failed.len(), 1);
    assert_eq!(first.failed[0].0, "a");
    assert_eq!(keys(&*rig.hot_mem).await, ["a", "b"]);

    let second = tiered.evict().await;
    assert_eq!(second.evicted, ["a", "b"]);
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());
}

/// Asserts that an eviction pass that fails on one object for a reason other than an
/// unavailable tier (the hot tier lost the object) goes on to the next one.
#[tokio::test]
async fn an_eviction_failure_that_is_not_an_outage_does_not_stop_the_pass() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.put("b", body("bbbb")).await.unwrap();
    tiered.write_back().await;
    rig.hot_mem.delete("a").await.unwrap();

    let pass = tiered.evict().await;
    assert_eq!(pass.evicted, ["b"]);
    assert_eq!(pass.failed.len(), 1);
    assert_eq!(pass.failed[0].1, Error::NotFound("a".to_owned()));
}

/// Asserts that an eviction whose check of the cold tier fails (the cold tier is
/// unavailable) removes nothing.
#[tokio::test]
async fn an_eviction_whose_cold_check_fails_removes_nothing() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.write_back().await;
    rig.cold.fail_next(Op::Head, 1);

    let pass = tiered.evict().await;
    assert_eq!(pass.evicted, Vec::<String>::new());
    assert_eq!(pass.failed.len(), 1);
    assert!(pass.failed[0].1.is_unavailable());
    assert_eq!(keys(&*rig.hot_mem).await, ["a"]);
}

/// Asserts that a read the hot tier fails with an outage is that error, and is not answered
/// from the cold tier, for a whole object and for a head.
#[tokio::test]
async fn a_hot_outage_is_reported_by_reads_and_heads() {
    let rig = Rig::new();
    let tiered = rig.open(0).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.maintain().await;
    rig.hot.set_down(true);

    let read = tiered.get("a", None).await.unwrap_err();
    assert!(read.is_unavailable());
    let head = tiered.head("a").await.unwrap_err();
    assert!(head.is_unavailable());
    assert_eq!(rig.cold.calls(Op::Get), 0);
}

/// Asserts that a delete the cold tier fails after the hot tier removed its copy is an
/// error, that the retry succeeds, and that a delete the hot tier fails removes nothing.
#[tokio::test]
async fn a_delete_a_tier_fails_is_an_error_and_can_be_retried() {
    let rig = Rig::new();
    let tiered = rig.open(100).await;
    tiered.put("a", body("aaaa")).await.unwrap();
    tiered.write_back().await;

    rig.hot.fail_next(Op::Delete, 1);
    assert!(tiered.delete("a").await.unwrap_err().is_unavailable());
    assert_eq!(keys(&*rig.hot_mem).await, ["a"]);
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);
    assert_eq!(tiered.hot_bytes(), 4);

    rig.cold.fail_next(Op::Delete, 1);
    assert!(tiered.delete("a").await.unwrap_err().is_unavailable());
    assert_eq!(keys(&*rig.hot_mem).await, Vec::<String>::new());
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);
    assert_eq!(tiered.hot_bytes(), 0);

    tiered.delete("a").await.unwrap();
    assert_eq!(keys(&*rig.cold_mem).await, Vec::<String>::new());
}

/// Asserts that a listing fails if either tier fails to list, instead of returning the other
/// tier's keys alone.
#[tokio::test]
async fn a_listing_fails_if_either_tier_fails() {
    let rig = Rig::new();
    let tiered = rig.open(100).await;
    tiered.put("a", body("aaaa")).await.unwrap();

    rig.cold.fail_next(Op::List, 1);
    assert!(tiered.list("").await.unwrap_err().is_unavailable());
    rig.hot.fail_next(Op::List, 1);
    assert!(tiered.list("").await.unwrap_err().is_unavailable());
    assert_eq!(keys(&tiered).await, ["a"]);
}

/// Asserts that a store is not opened if either tier fails to list, since what is pending
/// could not be known.
#[tokio::test]
async fn a_store_is_not_opened_if_either_tier_fails_to_list() {
    let rig = Rig::new();
    rig.hot.fail_next(Op::List, 1);
    let hot_failed = Tiered::open(rig.hot.clone(), rig.cold.clone(), 10).await;
    assert_eq!(hot_failed.err().map(|e| e.is_unavailable()), Some(true));

    rig.cold.fail_next(Op::List, 1);
    let cold_failed = Tiered::open(rig.hot.clone(), rig.cold.clone(), 10).await;
    assert_eq!(cold_failed.err().map(|e| e.is_unavailable()), Some(true));
}
