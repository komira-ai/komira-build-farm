use std::time::Duration;

use kbf_meta::{Generation, ObjectId, Touch, UnreachableReason};
use kbf_types::FarmTime;

use super::*;

/// The lines of `text` that start with `prefix`.
fn lines<'a>(text: &'a str, prefix: &str) -> Vec<&'a str> {
    text.lines().filter(|l| l.starts_with(prefix)).collect()
}

/// Catches a value on a bound counted in the bucket above it (Prometheus's `le` is
/// "less than or equal"), buckets printed per bucket instead of cumulatively, a
/// `_count` that is not the `+Inf` bucket, and a sum printed in nanoseconds.
#[test]
fn a_histogram_prints_cumulative_buckets_in_seconds() {
    let h = Histogram::new(Unit::Seconds);
    h.observe(1_000_000);
    h.observe(1_000_001);
    h.observe_duration(Duration::from_secs(90));
    let mut out = String::new();
    h.render(&mut out, "x", "");
    let buckets = lines(&out, "x_bucket");
    assert_eq!(buckets.len(), SECONDS_BOUNDS.len() + 1);
    assert_eq!(buckets[0], r#"x_bucket{le="0.001"} 1"#);
    assert_eq!(buckets[1], r#"x_bucket{le="0.0025"} 2"#);
    assert_eq!(buckets[14], r#"x_bucket{le="60"} 2"#);
    assert_eq!(buckets[15], r#"x_bucket{le="+Inf"} 3"#);
    assert_eq!(lines(&out, "x_sum"), ["x_sum 90.002000001"]);
    assert_eq!(lines(&out, "x_count"), ["x_count 3"]);
    assert_eq!((h.count(), h.sum()), (3, 90_002_000_001));
}

/// Catches a label printed without its comma before `le`, or `_sum` and `_count`
/// without the label (two series of one family would then collide), and sizes or
/// counts printed as seconds.
#[test]
fn labels_and_plain_units_print_as_prometheus_reads_them() {
    let h = Histogram::new(Unit::Bytes);
    h.observe(1024);
    h.observe(5000);
    let mut out = String::new();
    h.render(&mut out, "y", r#"command="touch""#);
    assert_eq!(
        lines(&out, "y_bucket")[..3],
        [
            r#"y_bucket{command="touch",le="1024"} 1"#,
            r#"y_bucket{command="touch",le="4096"} 1"#,
            r#"y_bucket{command="touch",le="16384"} 2"#,
        ]
    );
    assert_eq!(lines(&out, "y_sum"), [r#"y_sum{command="touch"} 6024"#]);
    assert_eq!(lines(&out, "y_count"), [r#"y_count{command="touch"} 2"#]);
    let c = Histogram::new(Unit::Count);
    c.observe(3);
    let mut out = String::new();
    c.render(&mut out, "z", "");
    assert_eq!(lines(&out, "z_bucket")[1], r#"z_bucket{le="4"} 1"#);
    assert_eq!(lines(&out, "z_sum"), ["z_sum 3"]);
}

/// Catches a request guard that does not lower the in-flight level when dropped (an
/// error return or a cancelled call would leave it raised for ever), a request
/// counted only when it succeeds, and one that succeeded counted as an error.
#[test]
fn a_request_is_counted_however_it_ends() {
    let requests = Requests::default();
    {
        let mut ok = requests.start();
        let failed = requests.start();
        assert_eq!(requests.in_flight.get(), 2);
        ok.bytes(7);
        failed.bytes(5);
        ok.ok();
    }
    assert_eq!(requests.in_flight.get(), 0);
    assert_eq!(requests.total.get(), 2);
    assert_eq!(requests.errors.get(), 1);
    assert_eq!(requests.bytes.get(), 12);
    assert_eq!(requests.seconds.count(), 2);
}

/// Catches two commands sharing one `command` label (their latencies would be
/// mixed), and a label that is not the command's name.
#[test]
fn each_command_has_its_own_label() {
    let object = ObjectId::new(kbf_meta::Epoch::new(1), 1);
    let commands = [
        Command::Tick(FarmTime::from_millis(1)),
        Command::AllocEpoch,
        Command::PutBlob {
            digest: kbf_segments::sha256(b"a"),
            location: kbf_meta::Location {
                store: kbf_meta::StoreId::CONFIGURED,
                object,
                offset: 0,
            },
        },
        Command::PutBlobs(Vec::new()),
        Command::PutAction {
            role: kbf_meta::Role::Daemon,
            action: kbf_segments::sha256(b"b"),
            record: kbf_meta::ActionRecord {
                result: kbf_segments::sha256(b"c"),
                closure: kbf_meta::Closure::new(),
            },
        },
        Command::Touch(Touch::default()),
        Command::ObjectUnreachable {
            object,
            reason: UnreachableReason::Missing,
        },
        Command::ObjectReachable {
            object,
            generation: Generation::new(1),
        },
        Command::Collect,
        Command::AddTier {
            server: kbf_meta::ServerId::new(1),
            tier: kbf_meta::TierId::new(1),
        },
        Command::PutStaged {
            tier: kbf_meta::TierId::new(1),
            objects: Vec::new(),
        },
        Command::SealStaged { object },
        Command::ObjectFlushed { object },
        Command::FlushConflict { object },
        Command::EvictObjects {
            objects: [object].into(),
        },
        Command::SetWriter(kbf_meta::NodeWriter::new(
            std::num::NonZeroU64::MIN,
            std::num::NonZeroU64::MIN,
        )),
        Command::AdoptSegment {
            object,
            entries: Vec::new(),
        },
        Command::RebuildProgress {
            after: String::new(),
        },
        Command::RebuildDone,
    ];
    let labels: Vec<&str> = commands
        .iter()
        .map(|c| COMMANDS[command_label(c)])
        .collect();
    assert_eq!(labels, COMMANDS);
}

/// Catches a family left out of the scrape, printed twice, or without the `# HELP`
/// and `# TYPE` lines a scraper reads its kind from; and a counter or gauge printed
/// with another family's value.
#[test]
fn every_family_is_printed_once_with_its_type() {
    let metrics = CacheMetrics::default();
    metrics.write.bytes.add(11);
    metrics.write_present.add(12);
    metrics.batch_update.bytes.add(13);
    metrics.batch_update_blobs.add(14);
    metrics.find_missing_digests.add(15);
    metrics.find_missing_missing.add(16);
    let _raised = metrics.batch_update.in_flight.raise();
    metrics.execute_dir_reads.add(17);
    metrics.sched_queued.set(18);
    metrics.sched_leases.set(19);
    metrics.frontend_relay_lost.add(20);
    let _presubmit = metrics.execute_presubmit_in_flight.raise();
    let (_tier, _tier2) = (
        metrics.tier_in_flight.raise(),
        metrics.tier_in_flight.raise(),
    );
    let text = metrics.render();
    let types: Vec<&str> = lines(&text, "# TYPE ")
        .iter()
        .map(|l| l.trim_start_matches("# TYPE "))
        .collect();
    assert_eq!(
        types,
        [
            "kbf_bytestream_write_requests_total counter",
            "kbf_bytestream_write_errors_total counter",
            "kbf_bytestream_write_bytes_total counter",
            "kbf_bytestream_write_in_flight gauge",
            "kbf_bytestream_write_seconds histogram",
            "kbf_bytestream_write_present_total counter",
            "kbf_batch_update_requests_total counter",
            "kbf_batch_update_errors_total counter",
            "kbf_batch_update_bytes_total counter",
            "kbf_batch_update_in_flight gauge",
            "kbf_batch_update_seconds histogram",
            "kbf_batch_update_blobs_total counter",
            "kbf_find_missing_requests_total counter",
            "kbf_find_missing_errors_total counter",
            "kbf_find_missing_in_flight gauge",
            "kbf_find_missing_seconds histogram",
            "kbf_find_missing_digests_total counter",
            "kbf_find_missing_missing_total counter",
            "kbf_store_blobs_seconds histogram",
            "kbf_segment_put_seconds histogram",
            "kbf_segment_footer_read_seconds histogram",
            "kbf_segment_commit_seconds histogram",
            "kbf_segment_blobs histogram",
            "kbf_segment_bytes histogram",
            "kbf_object_get_seconds histogram",
            "kbf_meta_query_seconds histogram",
            "kbf_execute_presubmit_seconds histogram",
            "kbf_execute_walk_seconds histogram",
            "kbf_execute_submit_seconds histogram",
            "kbf_sched_lock_wait_seconds histogram",
            "kbf_tier_wait_seconds histogram",
            "kbf_tier_call_seconds histogram",
            "kbf_execute_presubmit_in_flight gauge",
            "kbf_execute_dir_reads_total counter",
            "kbf_input_directories_cached_total counter",
            "kbf_input_directories_skipped_total counter",
            "kbf_input_directories_joined_total counter",
            "kbf_sched_queued gauge",
            "kbf_sched_leases gauge",
            "kbf_frontend_relay_lost_total counter",
            "kbf_tier_in_flight gauge",
            "kbf_meta_commit_seconds histogram",
            "kbf_execute_requests_total counter",
            "kbf_execute_errors_total counter",
            "kbf_execute_in_flight gauge",
            "kbf_front_open_executions gauge",
            "kbf_front_executes_started_total counter",
            "kbf_execute_queued gauge",
            "kbf_execute_prepare_seconds histogram",
            "kbf_execute_dispatch_seconds histogram",
            "kbf_execute_queued_seconds histogram",
            "kbf_execute_total_seconds histogram",
        ]
    );
    assert_eq!(lines(&text, "# HELP ").len(), types.len());
    for (name, value) in [
        ("kbf_bytestream_write_bytes_total", 11),
        ("kbf_bytestream_write_present_total", 12),
        ("kbf_batch_update_bytes_total", 13),
        ("kbf_batch_update_blobs_total", 14),
        ("kbf_find_missing_digests_total", 15),
        ("kbf_find_missing_missing_total", 16),
        ("kbf_batch_update_in_flight", 1),
        ("kbf_bytestream_write_in_flight", 0),
        ("kbf_execute_dir_reads_total", 17),
        ("kbf_sched_queued", 18),
        ("kbf_sched_leases", 19),
        ("kbf_frontend_relay_lost_total", 20),
        ("kbf_execute_presubmit_in_flight", 1),
        ("kbf_tier_in_flight", 2),
    ] {
        assert_eq!(
            lines(&text, &format!("{name} ")),
            [format!("{name} {value}")]
        );
    }
    let commits = lines(&text, "kbf_meta_commit_seconds_count");
    let expected: Vec<String> = COMMANDS
        .iter()
        .map(|c| format!("kbf_meta_commit_seconds_count{{command=\"{c}\"}} 0"))
        .collect();
    assert_eq!(commits, expected);
}

/// Catches an Execute guard that does not give back `in_flight` and `queued` however
/// the stream ends (a caller that leaves while the operation is queued would leave both
/// raised for ever), a stream counted as answered without its done operation, a
/// queued time observed more than once or when the stream never left `QUEUED`, and a
/// WaitExecution stream that feeds anything but `in_flight`.
#[test]
fn an_execute_is_recorded_however_its_stream_ends() {
    let metrics = Arc::new(ExecuteMetrics::default());

    // Answered: dispatched, queued, executing, done.
    let mut answered = metrics.execute();
    assert_eq!(metrics.in_flight.get(), 1);
    answered.dispatching();
    answered.dispatched(true);
    assert_eq!(metrics.queued.get(), 1);
    answered.update(true, false);
    assert_eq!(metrics.queued.get(), 1, "still QUEUED");
    answered.update(false, false);
    assert_eq!(metrics.queued.get(), 0);
    answered.update(false, true);
    drop(answered);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (1, 0));

    // Left by its caller while queued.
    let mut left = metrics.execute();
    left.dispatching();
    left.dispatched(true);
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (1, 1));
    drop(left);
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (0, 0));
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (2, 1));

    // Refused by the scheduler: its call was timed, and it was never queued.
    let mut refused = metrics.execute();
    refused.dispatching();
    refused.dispatched(false);
    assert_eq!(metrics.queued.get(), 0);
    drop(refused);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (3, 2));

    // A WaitExecution stream is open, and nothing else.
    let waiting = metrics.wait();
    assert_eq!(metrics.in_flight.get(), 1);
    drop(waiting);
    assert_eq!(metrics.in_flight.get(), 0);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (3, 2));

    assert_eq!(metrics.prepare.count(), 3);
    assert_eq!(metrics.dispatch.count(), 3);
    assert_eq!(metrics.queued_seconds.count(), 1);
    assert_eq!(metrics.total.count(), 3);
}

/// Catches `streaming` raising `open_executions` once per call rather than once per
/// guard (a stream that asked twice would leave the gauge raised for ever after its one
/// lowering), and a guard that never streamed lowering it.
#[test]
fn open_executions_is_raised_once_per_guard_and_only_by_a_streaming_one() {
    let metrics = Arc::new(ExecuteMetrics::default());
    let mut streamed = metrics.execute();
    let plain = metrics.wait();
    assert_eq!(metrics.open_executions.get(), 0, "not before streaming()");
    streamed.streaming();
    streamed.streaming();
    assert_eq!(metrics.open_executions.get(), 1);
    drop(plain);
    assert_eq!(
        metrics.open_executions.get(),
        1,
        "a guard that never streamed"
    );
    drop(streamed);
    assert_eq!(metrics.open_executions.get(), 0);
}
