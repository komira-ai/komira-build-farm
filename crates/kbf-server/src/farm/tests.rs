use kbf_front::Cache;
use std::sync::Arc;

use super::*;

/// Catches a server process that reuses an earlier process's term (issue #137:
/// every process granted leases from `(1, 0)`), and one whose term does not order
/// after that of a process started a few milliseconds before. A term is also never
/// 1, the term every server before the fix used.
#[test]
fn each_process_term_is_new_and_later() {
    let first = process_term();
    std::thread::sleep(Duration::from_millis(3));
    let second = process_term();
    assert!(second > first, "{second} does not order after {first}");
    assert!(first >> 16 > 0, "{first} could be a pre-fix term");
}

/// Catches a result the farm settles with no detail that is not a farm fault (a
/// committed `Completed` whose worker report this process never saw) being answered
/// as anything but the fixed INTERNAL "could not run the action", or panicking.
#[test]
fn a_result_without_detail_that_is_no_fault_is_answered_internal() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let answer = kbf_types::Answer {
        operation: kbf_types::OperationId(1),
        lease: kbf_types::LeaseId::new(1, 0),
        waiters: Vec::new(),
        outcome: Outcome::Completed {
            action_result: kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 1),
        },
        memory_runs: Vec::new(),
        farm_runs: Vec::new(),
    };
    let settled = farm.lock().settle(&answer, None);
    assert_eq!(
        settled.finished,
        failed(Code::Internal, "the farm could not run the action")
    );
}

/// Catches a farm fault (Infra, node memory pressure, a lost run) with no detail being
/// answered as the plain INTERNAL, which drops the farm's runs and the operator fix.
#[test]
fn a_result_without_detail_that_is_a_farm_fault_names_the_fault() {
    for failure in [Failure::Infra, Failure::NodeMemoryPressure] {
        let finished = unrun(Outcome::Failed(failure), &[]);
        assert_eq!(finished, fault::finished(&[]), "{failure:?}");
        assert_ne!(
            finished,
            failed(Code::Internal, "the farm could not run the action")
        );
    }
}
