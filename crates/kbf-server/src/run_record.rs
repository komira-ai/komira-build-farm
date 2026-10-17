//! What the invocation record keeps of one lease's report (`docs/design/ui-data.md`
//! 4.2, phase U1): the nine REAPI times of the run, its `ResourceUsage` with the
//! cgroup block whole, and for a run killed for memory its booking and, at its own
//! limit, the cap. [`of`] reads the report before it is judged (the judging drops a
//! killed run's result); [`killed`] adds the booking once the scheduler has taken it.

use std::time::SystemTime;

use kbf_invocation::{NumaBytes, ReapiTimes, Run, RunUsage, WallTime};
use kbf_proto::reapi::ExecutedActionMetadata;
use kbf_proto::worker::{self, ResourceUsage};
use kbf_types::{MemoryKill, MemoryRun};
use prost_types::Timestamp;

use crate::stamp::Stamp;

/// What `result`, for a run whose server times are `stamp` and taken in at
/// `received`, measured; `usage` is the `ResourceUsage` its result carried, as
/// [`crate::action_usage::Measured`] decoded it. The times are those its
/// `ActionResult` would carry once stamped ([`Stamp::times`]): the daemon's own,
/// `queued` from the server, and the worker start and completion the server saw when
/// the daemon left them out (a killed or lost run reports none of its own).
pub(crate) fn of(
    result: &worker::Result,
    usage: Option<&ResourceUsage>,
    stamp: Stamp,
    received: SystemTime,
) -> Run {
    let none = ExecutedActionMetadata::default();
    let m = result
        .action_result
        .as_ref()
        .and_then(|r| r.execution_metadata.as_ref())
        .unwrap_or(&none);
    let stamped = stamp.times(m, received);
    Run {
        times: ReapiTimes {
            queued: wall(Some(stamped.queued)),
            worker_start: wall(Some(stamped.worker_start)),
            input_fetch_start: wall(m.input_fetch_start_timestamp),
            input_fetch_completed: wall(m.input_fetch_completed_timestamp),
            execution_start: wall(m.execution_start_timestamp),
            execution_completed: wall(m.execution_completed_timestamp),
            output_upload_start: wall(m.output_upload_start_timestamp),
            output_upload_completed: wall(m.output_upload_completed_timestamp),
            worker_completed: wall(Some(stamped.worker_completed)),
        },
        usage: usage.map(run_usage),
        booked_memory_bytes: None,
        cap_bytes: None,
    }
}

/// `run`, with the booking of `killed`, the scheduler's record of it as a run killed
/// for memory, and its cap for a kill at its own limit.
pub(crate) fn killed(run: Run, killed: &MemoryRun) -> Run {
    Run {
        booked_memory_bytes: Some(killed.booked),
        cap_bytes: match killed.kill {
            MemoryKill::OwnLimit { cap } => Some(cap),
            MemoryKill::NodePressure => None,
        },
        ..run
    }
}

fn run_usage(u: &ResourceUsage) -> RunUsage {
    let none = worker::CgroupUsage::default();
    let c = u.cgroup.as_ref().unwrap_or(&none);
    RunUsage {
        wall_micros: u.wall_micros,
        cpu_usage_micros: c.cpu_usage_micros,
        cpu_user_micros: c.cpu_user_micros,
        cpu_system_micros: c.cpu_system_micros,
        cpu_periods: c.cpu_periods,
        cpu_throttled_periods: c.cpu_throttled_periods,
        cpu_throttled_micros: c.cpu_throttled_micros,
        memory_peak_bytes: c.memory_peak_bytes,
        numa: c
            .numa
            .iter()
            .map(|n| NumaBytes {
                node: n.node,
                bytes: n.bytes,
            })
            .collect(),
    }
}

/// A protobuf timestamp on the wall clock, to the microsecond; `None` for none, or
/// one before the epoch or past `u64` microseconds.
fn wall(t: Option<Timestamp>) -> Option<WallTime> {
    let t = t?;
    let micros = i128::from(t.seconds) * 1_000_000 + i128::from(t.nanos) / 1_000;
    u64::try_from(micros).ok().map(WallTime::from_unix_micros)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use kbf_proto::reapi::ActionResult;
    use kbf_proto::worker::{CgroupUsage, NumaNodeBytes};
    use kbf_types::WorkerId;

    use super::*;

    fn secs(n: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(n)
    }

    fn at(n: u64) -> Option<WallTime> {
        Some(WallTime::from_unix_micros(n * 1_000_000))
    }

    /// Catches: a REAPI time dropped or copied into the wrong field (each of the
    /// daemon's eight has its own second), `queued` not the server's, a cgroup
    /// counter dropped or swapped, or the NUMA list lost.
    #[test]
    fn every_time_and_every_cgroup_counter_is_kept() {
        let ts = |n| Some(Timestamp::from(secs(n)));
        let usage = ResourceUsage {
            wall_micros: 2_000_000,
            cgroup: Some(CgroupUsage {
                cpu_usage_micros: Some(11),
                cpu_user_micros: Some(12),
                cpu_system_micros: Some(13),
                cpu_periods: Some(14),
                cpu_throttled_periods: Some(15),
                cpu_throttled_micros: Some(16),
                memory_peak_bytes: Some(17),
                numa: vec![NumaNodeBytes { node: 1, bytes: 18 }],
            }),
            ..ResourceUsage::default()
        };
        let result = worker::Result {
            action_result: Some(ActionResult {
                execution_metadata: Some(ExecutedActionMetadata {
                    queued_timestamp: ts(99),
                    worker_start_timestamp: ts(2),
                    input_fetch_start_timestamp: ts(3),
                    input_fetch_completed_timestamp: ts(4),
                    execution_start_timestamp: ts(5),
                    execution_completed_timestamp: ts(6),
                    output_upload_start_timestamp: ts(7),
                    output_upload_completed_timestamp: ts(8),
                    worker_completed_timestamp: ts(9),
                    auxiliary_metadata: vec![usage.to_any()],
                    ..ExecutedActionMetadata::default()
                }),
                ..ActionResult::default()
            }),
            ..worker::Result::default()
        };
        let stamp = Stamp {
            queued: secs(1),
            started: secs(50),
        };
        let decoded = result
            .action_result
            .as_ref()
            .and_then(ResourceUsage::in_result);
        let run = of(&result, decoded.as_ref(), stamp, secs(60));
        let times = run.times.named().map(|(_, t)| t);
        assert_eq!(
            times,
            [
                at(1),
                at(2),
                at(3),
                at(4),
                at(5),
                at(6),
                at(7),
                at(8),
                at(9)
            ]
        );
        let u = run.usage.expect("usage");
        assert_eq!(u.wall_micros, 2_000_000);
        assert_eq!(
            [
                u.cpu_usage_micros,
                u.cpu_user_micros,
                u.cpu_system_micros,
                u.cpu_periods,
                u.cpu_throttled_periods,
                u.cpu_throttled_micros,
                u.memory_peak_bytes
            ],
            [11, 12, 13, 14, 15, 16, 17].map(Some)
        );
        assert_eq!(u.numa, [NumaBytes { node: 1, bytes: 18 }]);
    }

    /// Catches: a report without a result (a killed or lost run) given no times, or
    /// times other than the server's (queued, Start sent, taken in); and usage made
    /// up for a result that carries none.
    #[test]
    fn a_report_without_a_result_has_the_server_s_times_only() {
        let run = of(
            &worker::Result::default(),
            None,
            Stamp {
                queued: secs(1),
                started: secs(2),
            },
            secs(3),
        );
        assert_eq!(
            run.times,
            ReapiTimes {
                queued: at(1),
                worker_start: at(2),
                worker_completed: at(3),
                ..ReapiTimes::default()
            }
        );
        assert_eq!(run.usage, None);
    }

    /// Catches: a cap set for a busy node's kill, or not set for an own-limit kill,
    /// and the booking dropped.
    #[test]
    fn a_kill_keeps_its_booking_and_only_an_own_limit_kill_its_cap() {
        let run = |kill| {
            killed(
                Run::default(),
                &MemoryRun {
                    worker: WorkerId::new("n"),
                    booked: 8,
                    kill,
                },
            )
        };
        let own = run(MemoryKill::OwnLimit { cap: 64 });
        assert_eq!(
            (own.booked_memory_bytes, own.cap_bytes),
            (Some(8), Some(64))
        );
        let busy = run(MemoryKill::NodePressure);
        assert_eq!((busy.booked_memory_bytes, busy.cap_bytes), (Some(8), None));
    }

    /// Catches: a time before the epoch read as a huge or wrapped one.
    #[test]
    fn a_time_before_the_epoch_is_none() {
        let before = Timestamp {
            seconds: -1,
            nanos: 0,
        };
        assert_eq!(wall(Some(before)), None);
        assert_eq!(wall(None), None);
        let micro = Timestamp {
            seconds: 2,
            nanos: 3_999,
        };
        assert_eq!(
            wall(Some(micro)),
            Some(WallTime::from_unix_micros(2_000_003))
        );
    }
}
