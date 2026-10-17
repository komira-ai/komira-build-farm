//! The times the driver writes into a result's `ExecutedActionMetadata`, against the
//! stand-in `crun` (`support/fake.rs`). Buck2 reports an action's execution time
//! as `execution_completed_timestamp - execution_start_timestamp`; a result without
//! them reads as an action that took no time (issue #517).
//!
//! A module of `fake_crun.rs`, not a test binary of its own, for the reason
//! `fake_memory` gives: the coverage job counts the driver's lines per test binary.

use std::time::{Duration, SystemTime};

use prost_types::Timestamp;

use crate::support::Spec;
use crate::support::fake::{Fake, image};

/// How long the action sleeps.
const SLEEP: Duration = Duration::from_millis(300);

fn time(name: &str, t: Option<&Timestamp>) -> SystemTime {
    let t = t.unwrap_or_else(|| panic!("{name} is not set"));
    SystemTime::try_from(*t).expect("a time")
}

/// Catches: any of the eight times the daemon owns left unset (a missing execution
/// pair is the zero execution time Buck2 showed); the times out of their steps' order;
/// an execution window shorter than the action's own sleep (the stamps taken around
/// the wrong step); and a worker window that does not lie within the call to `run`.
#[tokio::test]
async fn a_result_carries_the_times_of_each_step() {
    let fake = Fake::new("clock");
    let mut spec = Spec::new(&image(), "unused by the fake");
    spec.outputs = vec!["out".to_owned()];
    let called = SystemTime::now();
    let result = fake
        .run(
            1,
            &spec,
            &[
                "sleep",
                SLEEP.as_secs_f64().to_string().as_str(),
                ";",
                "write-lines",
                "$UPPER/out",
                "x",
            ],
        )
        .await
        .expect("ran");
    let returned = SystemTime::now();
    assert_eq!(result.exit_code, 0);
    let m = result.execution_metadata.as_ref().expect("metadata");
    let steps = [
        (
            "worker_start",
            time("worker_start", m.worker_start_timestamp.as_ref()),
        ),
        (
            "input_fetch_start",
            time("input_fetch_start", m.input_fetch_start_timestamp.as_ref()),
        ),
        (
            "input_fetch_completed",
            time(
                "input_fetch_completed",
                m.input_fetch_completed_timestamp.as_ref(),
            ),
        ),
        (
            "execution_start",
            time("execution_start", m.execution_start_timestamp.as_ref()),
        ),
        (
            "execution_completed",
            time(
                "execution_completed",
                m.execution_completed_timestamp.as_ref(),
            ),
        ),
        (
            "output_upload_start",
            time(
                "output_upload_start",
                m.output_upload_start_timestamp.as_ref(),
            ),
        ),
        (
            "output_upload_completed",
            time(
                "output_upload_completed",
                m.output_upload_completed_timestamp.as_ref(),
            ),
        ),
        (
            "worker_completed",
            time("worker_completed", m.worker_completed_timestamp.as_ref()),
        ),
    ];
    for pair in steps.windows(2) {
        let ((before, a), (after, b)) = (pair[0], pair[1]);
        assert!(a <= b, "{before} after {after}: {m:?}");
    }
    let start = steps[3].1;
    let completed = steps[4].1;
    let ran = completed.duration_since(start).expect("ordered");
    assert!(
        ran >= SLEEP,
        "execution took {ran:?}, the action slept {SLEEP:?}"
    );
    assert!(called <= steps[0].1, "worker start before the call: {m:?}");
    assert!(
        steps[7].1 <= returned,
        "worker completed after the return: {m:?}"
    );
}
