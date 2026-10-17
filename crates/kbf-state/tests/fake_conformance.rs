//! The in-memory fake against the conformance suite: the correct fake passes every
//! clause, and a fake with one deliberate defect fails the clause that defect breaks.
//! The `expected` text is the start of that clause's assertion message, so each test
//! also pins the reason it fails.

use kbf_state::conformance::{
    c1_linearizable_cas, c2_atomic_txn, c3_monotonic_counter, c4_store_clock_expiry,
    c5_ordered_feed, c6_durable_ack, c7_group_commit, c8_bounded_range_scan, run_all,
};
use kbf_state::fake::{FakeBackend, Flaw};

#[test]
fn the_correct_fake_passes_every_clause() {
    run_all(&FakeBackend::new(Flaw::None));
}

// Catches a compare-and-set that checks and writes in separate critical sections.
#[test]
#[should_panic(expected = "C1 race")]
fn non_linearizable_cas_fails_c1() {
    c1_linearizable_cas(&FakeBackend::new(Flaw::NonLinearizableCas));
}

// Catches a transaction that applies operations until one fails.
#[test]
#[should_panic(expected = "C2 aborted")]
fn partial_transaction_fails_c2() {
    c2_atomic_txn(&FakeBackend::new(Flaw::PartialTxn));
}

// Catches a counter kept outside the transaction: an aborted increment is not rolled
// back, so the next value skips it.
#[test]
#[should_panic(expected = "C2 counter: a counter incremented in an aborted transaction changed")]
fn a_counter_that_survives_an_abort_fails_c2() {
    c2_atomic_txn(&FakeBackend::new(Flaw::CounterSurvivesAbort));
}

// Catches a counter that is not durable, so a value is reissued after restart.
#[test]
#[should_panic(expected = "C3 restart")]
fn forgotten_counter_fails_c3() {
    c3_monotonic_counter(&FakeBackend::new(Flaw::CounterNotDurable));
}

// Catches expiry judged by a clock that is not the store's current one.
#[test]
#[should_panic(expected = "C4 expiry")]
fn caller_clock_expiry_fails_c4() {
    c4_store_clock_expiry(&FakeBackend::new(Flaw::CallerClockExpiry));
}

// Catches a key that is still live at the reading that equals its expiry: the boundary
// is the one place a `<` and a `<=` differ, and the other C4 steps step over it.
#[test]
#[should_panic(expected = "C4 boundary: a key whose ttl ends exactly now is expired, and get")]
fn a_key_live_at_its_exact_expiry_fails_c4() {
    c4_store_clock_expiry(&FakeBackend::new(Flaw::InclusiveExpiry));
}

// Catches a feed that advances past a revision that has not committed: the poller
// that saw the later revision first never sees the earlier one.
#[test]
#[should_panic(expected = "C5 interleaved")]
fn feed_past_an_uncommitted_revision_fails_c5() {
    c5_ordered_feed(&FakeBackend::new(Flaw::FeedPastPending));
}

// Catches an acknowledgement sent before the data is durable.
#[test]
#[should_panic(expected = "C6 durable")]
fn ack_before_sync_fails_c6() {
    c6_durable_ack(&FakeBackend::new(Flaw::AckBeforeSync));
}

// Catches writes that pay a sync each, so a burst misses the commit window.
#[test]
#[should_panic(expected = "C7 latency")]
fn ungrouped_writes_fail_c7() {
    c7_group_commit(&FakeBackend::new(Flaw::Ungrouped));
}

// Catches a batch that pays two syncs: 4 ms of store time is inside the window, so the
// latency bound passes and only the sync count fails.
#[test]
#[should_panic(expected = "C7 syncs")]
fn a_double_sync_fails_c7_on_the_sync_count() {
    c7_group_commit(&FakeBackend::new(Flaw::DoubleSync));
}

// Catches a range scan that returns every key of its range, past its limit.
#[test]
#[should_panic(expected = "C8 limit")]
fn a_scan_that_ignores_its_limit_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanIgnoresLimit));
}

// Catches a range scan that begins at the prefix whatever lower bound it is given.
#[test]
#[should_panic(expected = "C8 lower")]
fn a_scan_that_ignores_its_lower_bound_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanIgnoresLower));
}

// Catches a resume token that resumes at the last key of its page instead of after it,
// so the key is returned twice.
#[test]
#[should_panic(expected = "C8 resume")]
fn a_scan_that_resumes_at_its_token_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanResumesAtToken));
}

// Catches a range scan that runs past the last key under its prefix.
#[test]
#[should_panic(expected = "C8 prefix")]
fn a_scan_past_its_prefix_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanPastPrefix));
}

// Catches a forward scan that runs on past its upper bound.
#[test]
#[should_panic(expected = "C8 upper: an included upper key is the last row")]
fn a_scan_that_ignores_its_upper_bound_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanIgnoresUpper));
}

// Catches a forward scan that returns the key of an excluded upper bound.
#[test]
#[should_panic(expected = "C8 upper: an excluded upper key is not returned")]
fn a_scan_that_includes_an_excluded_upper_key_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanIncludesExcludedUpper));
}

// Catches a reverse scan built as the forward page reversed: the first keys of the
// range, not the last (`ORDER BY key LIMIT n` then reversing the rows).
#[test]
#[should_panic(expected = "C8 reverse: a reverse page holds the last keys")]
fn a_reverse_scan_of_the_forward_page_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanReversesForwardPage));
}

// Catches a reverse scan whose token resumes at the last key of its page, so the key is
// returned twice.
#[test]
#[should_panic(expected = "C8 reverse resume")]
fn a_reverse_scan_that_resumes_at_its_token_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanReverseResumesAtToken));
}

// Catches a backend that applies its limit before its expiry filter (a natural SQL
// mistake): the expired `c8/bb` takes a row's place, so the page is short and has lost
// its token.
#[test]
#[should_panic(expected = "C8 expiry")]
fn a_scan_that_counts_expired_keys_toward_its_limit_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanCountsExpired));
}

// Catches a last page that still carries a token, so a caller fetches an empty page and
// a UI shows a "next" link to nothing.
#[test]
#[should_panic(expected = "C8 resume: a page that reached the end of the range has no token")]
fn a_scan_with_a_token_on_its_last_page_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanTokenOnLastPage));
}

// Catches a resumed page served from the first page's snapshot: a key committed ahead
// of the token after the first page is missing.
#[test]
#[should_panic(expected = "C8 concurrent keys")]
fn a_scan_that_skips_keys_committed_ahead_of_its_token_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanSkipsAhead));
}

// Catches a resumed page that shows a key's value from the first page's time, not its
// own.
#[test]
#[should_panic(expected = "C8 concurrent value")]
fn a_scan_that_shows_stale_values_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanStaleValues));
}

// Catches a page whose rows are read one at a time, so a commit between two of them
// splits the page across commits; the store's contract is one snapshot per page.
#[test]
#[should_panic(expected = "C8 page snapshot")]
fn a_scan_that_reads_each_row_on_its_own_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanReadsPerRow));
}

// The two error types say what failed.
#[test]
fn errors_say_what_failed() {
    let conflict = kbf_state::Conflict {
        index: 2,
        key: "k".to_owned(),
    };
    assert_eq!(
        conflict.to_string(),
        "operation 2 on \"k\" failed its precondition"
    );
    let failure = kbf_state::StoreError::Failed("disk full".to_owned());
    assert_eq!(failure.to_string(), "state store failure: disk full");
}
