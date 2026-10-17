//! The wall clock a store that keeps expiry on disk runs on.

use std::time::Duration;

use kbf_state::{Clock, StoreTime, SystemClock};

// Catches a clock whose origin is not the Unix epoch (a restart would change what a
// stored expiry means), and one that steps back.
#[test]
fn readings_are_since_the_epoch_and_never_decrease() {
    let clock = SystemClock::default();
    let first = clock.now();
    assert!(
        first.as_duration() > Duration::from_secs(1_700_000_000),
        "the origin is the Unix epoch, not the clock's creation"
    );
    let mut last = first;
    for _ in 0..1000 {
        let next = clock.now();
        assert!(
            next >= last,
            "a reading went back from {last:?} to {next:?}"
        );
        last = next;
    }
}

// Catches a conversion that loses precision, which a stored expiry would inherit.
#[test]
fn a_reading_round_trips_through_a_duration() {
    let at = Duration::new(1_800_000_000, 123_456_789);
    assert_eq!(StoreTime::from_duration(at).as_duration(), at);
}
