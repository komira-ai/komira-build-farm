use super::*;

/// Catches: a percentile below the true value, or more than 1/16 above it (a
/// sub-bucket too wide, or the bottom of the sub-bucket reported); a value below
/// 16 not exact; a percentile above the largest value; a rank off by one. Every
/// value from 0 to 5,000 and powers of two near the top of the range, against the
/// sorted list.
#[test]
fn a_percentile_is_never_below_the_true_value_and_at_most_a_sixteenth_above() {
    let mut values: Vec<u64> = (0..5_000).collect();
    values.extend((10..64).map(|p| (1u64 << p) + 12_345));
    values.push(u64::MAX);
    for &v in &values {
        let top = sub_bucket_top(sub_bucket(v));
        assert!(top >= v, "{v}: top {top}");
        assert!(top - v <= v / 16, "{v}: top {top}");
        if v < 16 {
            assert_eq!(top, v);
        }
    }
    // Sub-buckets are in value order, so ranks walk in order.
    assert!(
        values
            .windows(2)
            .all(|w| sub_bucket(w[0]) <= sub_bucket(w[1]))
    );
    let mut h = Histogram::default();
    let sample: Vec<u64> = (1..=1_000).map(|i| i * 37 % 1_009 + 3).collect();
    for &v in &sample {
        h.record(v);
    }
    let mut sorted = sample.clone();
    sorted.sort_unstable();
    for percent in [1, 50, 90, 99, 100] {
        let rank = (percent * 1_000usize).div_ceil(100);
        let exact = sorted[rank - 1];
        let got = h.percentile(percent as u64).expect("values");
        assert!(
            got >= exact && got - exact <= exact / 16,
            "p{percent}: {got} vs {exact}"
        );
    }
    assert_eq!(h.percentile(100), Some(*sorted.last().expect("values")));
    assert_eq!(
        (h.min(), h.max()),
        (Some(sorted[0]), sorted.last().copied())
    );
    assert_eq!(h.sum(), sample.iter().sum::<u64>());
    assert_eq!(h.count(), 1_000);
}

/// Catches: an empty histogram answering a value, and one value not being every
/// percentile of a histogram that holds only it.
#[test]
fn an_empty_histogram_has_no_percentiles_and_one_value_is_all_of_them() {
    let mut h = Histogram::default();
    assert_eq!((h.percentile(50), h.min(), h.max()), (None, None, None));
    h.record(1_000_003);
    assert_eq!(h.percentile(1), Some(1_000_003));
    assert_eq!(h.percentile(100), Some(1_000_003));
    h.record(7);
    assert_eq!(
        (h.min(), h.max(), h.percentile(50)),
        (Some(7), Some(1_000_003), Some(7))
    );
}

/// Catches: two variants given one name, or a name missing for a variant (each
/// name is listed once, and the lists are complete by the exhaustive matches).
#[test]
fn every_requeue_reason_and_failure_class_has_its_own_name() {
    use kbf_types::LostWhy;
    let requeues = [
        RequeueReason::Silent,
        RequeueReason::Replaced,
        RequeueReason::Reconnected,
        RequeueReason::NotStarted,
        RequeueReason::SessionSwitch,
        RequeueReason::OutOfMemory {
            booked: 1,
            raised: 2,
        },
        RequeueReason::NodeMemoryPressure,
        RequeueReason::Restarted,
        RequeueReason::Lost(LostWhy::Fenced),
        RequeueReason::Infra,
    ];
    let mut names: Vec<&str> = requeues.iter().map(requeue_name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), requeues.len());
    let failures = [
        Failure::Timeout,
        Failure::Infra,
        Failure::Invalid,
        Failure::OutOfMemory,
        Failure::NodeMemoryPressure,
        Failure::MissingInput,
        Failure::Lost(LostWhy::DaemonRestarted),
    ];
    let mut names: Vec<&str> = failures.into_iter().map(failure_name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), failures.len());
}

/// Catches: a requeue reason whose name [`REQUEUE_NAMES`] does not list (a variant
/// added, its name given in `requeue_name` but not here: `?requeue_reason=` would
/// answer 400 for a reason attempts show), or a listed name no reason has. The match
/// below stops compiling when a variant is added, so the list of reasons here cannot
/// go stale either.
#[test]
fn requeue_names_lists_every_reason_s_name() {
    use kbf_types::LostWhy;
    let reasons = [
        RequeueReason::Silent,
        RequeueReason::Replaced,
        RequeueReason::Reconnected,
        RequeueReason::NotStarted,
        RequeueReason::SessionSwitch,
        RequeueReason::OutOfMemory {
            booked: 1,
            raised: 2,
        },
        RequeueReason::NodeMemoryPressure,
        RequeueReason::Restarted,
        RequeueReason::Lost(LostWhy::Fenced),
        RequeueReason::Infra,
    ];
    let index = |r: &RequeueReason| match r {
        RequeueReason::Silent => 0,
        RequeueReason::Replaced => 1,
        RequeueReason::Reconnected => 2,
        RequeueReason::NotStarted => 3,
        RequeueReason::SessionSwitch => 4,
        RequeueReason::OutOfMemory { .. } => 5,
        RequeueReason::NodeMemoryPressure => 6,
        RequeueReason::Restarted => 7,
        RequeueReason::Lost(_) => 8,
        RequeueReason::Infra => 9,
    };
    let indexes: Vec<usize> = reasons.iter().map(index).collect();
    assert_eq!(indexes, (0..REQUEUE_NAMES.len()).collect::<Vec<_>>());
    let names: Vec<&str> = reasons.iter().map(requeue_name).collect();
    assert_eq!(names, REQUEUE_NAMES);
}
