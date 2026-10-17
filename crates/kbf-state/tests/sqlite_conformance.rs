//! The SQLite store against the conformance suite, one test per clause.

mod common;

use common::SqliteBackend;
use kbf_state::conformance::{
    c1_linearizable_cas, c2_atomic_txn, c3_monotonic_counter, c4_store_clock_expiry,
    c5_ordered_feed, c6_durable_ack, c7_group_commit, c8_bounded_range_scan,
};

#[test]
fn c1_compare_and_set_is_linearizable() {
    c1_linearizable_cas(&SqliteBackend::new("c1"));
}

#[test]
fn c2_transactions_are_atomic() {
    c2_atomic_txn(&SqliteBackend::new("c2"));
}

#[test]
fn c3_counters_are_monotonic() {
    c3_monotonic_counter(&SqliteBackend::new("c3"));
}

#[test]
fn c4_expiry_follows_the_store_clock() {
    c4_store_clock_expiry(&SqliteBackend::new("c4"));
}

#[test]
fn c5_the_feed_resumes_without_skipping() {
    c5_ordered_feed(&SqliteBackend::new("c5"));
}

#[test]
fn c6_acknowledged_writes_survive_a_restart() {
    c6_durable_ack(&SqliteBackend::new("c6"));
}

#[test]
fn c7_a_burst_is_one_group() {
    c7_group_commit(&SqliteBackend::new("c7"));
}

#[test]
fn c8_a_range_scan_is_bounded_and_resumes() {
    c8_bounded_range_scan(&SqliteBackend::new("c8"));
}
