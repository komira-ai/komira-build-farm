//! The scheduler in a simulated cell: a leader running [`Scheduler`], a control log,
//! and two workers, on a network that delays, duplicates and reorders messages. In the
//! base run one worker is cut off long enough for its leases to expire and be
//! re-dispatched.
//!
//! The leader carries out the scheduler's effects literally: `Commit` sends the record
//! to the log, `Start` sends it to the worker, `Answer` is recorded. The log commits
//! records in arrival order and returns them numbered; the leader feeds them back in
//! log order. Workers heartbeat every 5 s, keep a [`SelfFence`], run every action for
//! 100 s, and resend unacknowledged reports with each heartbeat (a hermetic run keeps
//! its result through a lost connection).
//!
//! The checks, over a seed sweep:
//! - every `Start` a worker receives names a lease the log had already committed;
//! - every operation is answered exactly once, by the lease of the newest grant that
//!   precedes its result in the log;
//! - no self-fenced operation ever runs on two workers at once;
//! - a seed replays to the same trace.
//!
//! Three more runs keep every worker talking, so the grace G never fires, and lose or
//! hide a committed lease another way. Each must still answer every operation once,
//! release every booking, and fence the result of the lost lease:
//! - worker-1 reboots (its runs die) and worker-2's daemon restarts (its runs are
//!   re-adopted), each registering again well inside G;
//! - worker-1 never receives one `Start`, while its session stays up;
//! - worker-1 runs one hermetic lease but leaves it out of every heartbeat's running
//!   set, and reports it late.
//!
//! Two runs lose every `Start` sent to a worker, so each lease granted there is lost
//! (RFC section 5.8: an `INFRA` attempt). With only worker-1 losing them, every
//! operation it lost must be retried on worker-2 and answered by that run. With both
//! workers losing them, every operation must be granted three times and then answered
//! with an `INFRA` failure.
//!
//! Each registration opens a new session, as a new stream does on the wire: a `Start`
//! or acknowledgement sent to an earlier session never arrives, the leader drops
//! heartbeats of a session older than the newest, and a duplicated `Hello` registers
//! once. As on the wire, a `Hello` carries no running set, and a worker resends its
//! `Hello` on the same session when its node report changes; only the first `Hello`
//! of a session registers. Workers send their running set with every heartbeat: runs
//! not ended, and ended runs whose result is not yet acknowledged. The leader passes
//! it to the scheduler.
//!
//! The cell itself is in `cell/mod.rs`.
//!
//! [`Scheduler`]: kbf_sched::Scheduler
//! [`SelfFence`]: kbf_sched::SelfFence

use std::collections::BTreeMap;

use kbf_sched::fence::START_GRACE;
use kbf_sim::{Sim, TraceHash};
use kbf_types::{
    ControlRecord, Failure, FarmTime, FencePolicy, LeaseId, OperationId, Outcome, Resources,
    WorkerId,
};

mod cell;

use cell::{
    Cell, END, Fault, OPS, REBOOT_AT, REGRANT_BOUND, RESTART_AT, RUN_FOR, Run, SEEDS, Scenario,
    leader, log, run, run_scenario, worker, workers,
};

/// Every grant in the log: per operation, each lease with the time it first committed.
fn grants(sim: &Sim<Cell>) -> BTreeMap<OperationId, BTreeMap<LeaseId, FarmTime>> {
    let mut grants: BTreeMap<OperationId, BTreeMap<LeaseId, FarmTime>> = BTreeMap::new();
    for (t, r) in &log(sim).records {
        if let ControlRecord::Lease(g) = r {
            grants
                .entry(g.operation)
                .or_default()
                .entry(g.lease)
                .or_insert(*t);
        }
    }
    grants
}

/// Every grant in the log: per operation, each lease (in lease order) with its worker.
fn grants_on(sim: &Sim<Cell>) -> BTreeMap<OperationId, BTreeMap<LeaseId, WorkerId>> {
    let mut grants: BTreeMap<OperationId, BTreeMap<LeaseId, WorkerId>> = BTreeMap::new();
    for (_, r) in &log(sim).records {
        if let ControlRecord::Lease(g) = r {
            grants
                .entry(g.operation)
                .or_default()
                .insert(g.lease, g.worker.clone());
        }
    }
    grants
}

/// The outcome each operation was answered with.
fn outcomes(sim: &Sim<Cell>) -> BTreeMap<OperationId, Outcome> {
    let answers = leader(sim).answers.iter();
    answers.map(|(_, a)| (a.operation, a.outcome)).collect()
}

/// Checks that every operation was answered exactly once, by the lease of the newest
/// grant that precedes its result in the log, and returns the answering leases.
fn assert_answered_once(sim: &Sim<Cell>, seed: u64) -> BTreeMap<OperationId, LeaseId> {
    let mut newest: BTreeMap<OperationId, LeaseId> = BTreeMap::new();
    let mut winner: BTreeMap<OperationId, LeaseId> = BTreeMap::new();
    for (_, r) in &log(sim).records {
        match r {
            ControlRecord::Lease(g) => {
                newest.insert(g.operation, g.lease);
            }
            ControlRecord::Result(res) if newest.get(&res.operation) == Some(&res.lease) => {
                winner.entry(res.operation).or_insert(res.lease);
            }
            _ => {}
        }
    }
    let mut seen = BTreeMap::new();
    for (_, a) in &leader(sim).answers {
        assert!(
            seen.insert(a.operation, a.lease).is_none(),
            "seed {seed}: {} answered twice",
            a.operation
        );
        assert_eq!(
            winner.get(&a.operation),
            Some(&a.lease),
            "seed {seed}: {} answered by {}, which the log had superseded",
            a.operation,
            a.lease
        );
    }
    assert_eq!(seen.len() as u64, OPS, "seed {seed}: unanswered operations");
    seen
}

/// Checks that once every operation is answered, no worker has anything booked.
fn assert_bookings_released(sim: &Sim<Cell>, seed: u64) {
    for (id, _) in workers(sim) {
        let booked = leader(sim).sched.booked(&WorkerId::new(id.as_str()));
        assert_eq!(
            booked,
            Some(Resources::default()),
            "seed {seed}: {id} still booked at the end"
        );
    }
}

/// Checks that no self-fenced operation ran on two workers at once, and returns how
/// many ran more than once.
fn assert_never_twice_at_once(sim: &Sim<Cell>, seed: u64) -> usize {
    let mut spans: BTreeMap<OperationId, Vec<(u64, u64)>> = BTreeMap::new();
    for (_, w) in workers(sim) {
        for run in w.runs.values() {
            if run.fence == FencePolicy::SelfFence {
                let end = run.ended.map_or(END, FarmTime::as_millis);
                spans
                    .entry(run.operation)
                    .or_default()
                    .push((run.started.as_millis(), end));
            }
        }
    }
    let rerun = spans.values().filter(|s| s.len() > 1).count();
    for (op, mut s) in spans {
        s.sort_unstable();
        for pair in s.windows(2) {
            assert!(
                pair[0].1 <= pair[1].0,
                "seed {seed}: {op} ran twice at once: {pair:?}"
            );
        }
    }
    rerun
}

/// Checks that `lost`, a committed lease of `op`, was replaced: `op` was granted again
/// no sooner than the grace after `lost` committed (its `Start` may still have been on
/// its way before then), and answered by a later lease.
fn assert_replaced_after_grace(
    sim: &Sim<Cell>,
    seed: u64,
    answers: &BTreeMap<OperationId, LeaseId>,
    op: OperationId,
    lost: LeaseId,
) {
    let grants = grants(sim);
    let committed = grants[&op][&lost];
    let again = grants[&op].iter().find(|(l, _)| **l > lost);
    assert!(
        again.is_some_and(|(_, t)| *t >= committed.saturating_add(START_GRACE)),
        "seed {seed}: {op} lost {lost} committed at {committed:?}; granted again {again:?}"
    );
    assert!(
        answers[&op] > lost,
        "seed {seed}: {op} answered by {}, the lease it lost",
        answers[&op]
    );
}

/// Catches: a `Start` sent before its lease is committed (at placement, or with the
/// commit in the same breath). The Start then races the log append and, on some seed,
/// a worker holds a lease the log does not know, which a new leader could grant again.
#[test]
fn every_start_names_a_lease_already_committed() {
    for seed in 0..SEEDS {
        let sim = run(seed);
        // A duplicated append commits a record twice; the first commit counts.
        let mut committed: BTreeMap<LeaseId, FarmTime> = BTreeMap::new();
        for (t, r) in &log(&sim).records {
            if let ControlRecord::Lease(g) = r {
                committed.entry(g.lease).or_insert(*t);
            }
        }
        for (id, w) in workers(&sim) {
            for (received, lease) in &w.starts {
                let at = committed.get(lease);
                assert!(
                    at.is_some_and(|c| c <= received),
                    "seed {seed}: {id} received Start for {lease} at {received:?}, committed at {at:?}"
                );
            }
        }
    }
}

/// Catches: a result accepted from a lease that was superseded (a late result after
/// re-dispatch), a duplicate result answered twice, or an operation never answered.
/// The cut-off worker's hermetic runs finish after their operations were re-dispatched
/// and report late; those reports must lose to the new lease.
#[test]
fn each_operation_answered_once_by_its_newest_grant() {
    for seed in 0..SEEDS {
        let sim = run(seed);
        let answers = assert_answered_once(&sim, seed);
        // The scenario did what it claims: some answer comes from a re-dispatch.
        assert!(answers.values().any(|l| l.seq >= OPS), "seed {seed}");
    }
}

/// Catches: a self-fenced run that outlives the grace G on a cut-off worker (a fence
/// measured from the wrong time, or G not exceeding T), so two copies of networked
/// work run at once.
#[test]
fn self_fenced_operations_never_run_twice_at_once() {
    for seed in 0..SEEDS {
        let sim = run(seed);
        let rerun = assert_never_twice_at_once(&sim, seed);
        assert!(rerun > 0, "seed {seed}: no re-dispatch");
    }
}

/// Catches: anything in the scheduler or this cell that depends on more than the seed
/// (a hashed collection, a clock): two runs of one seed must trace identically.
#[test]
fn a_seed_replays_exactly() {
    let hash = |seed| -> TraceHash { run(seed).trace_hash() };
    assert_eq!(hash(7), hash(7));
    assert_ne!(hash(7), hash(8));
}

/// Catches: a worker that registers again inside G keeping its old leases for good
/// (worker-1 rebooted, so their runs are gone: the operations are never answered and
/// the bookings leak), a re-registration that waits for a grace before letting them go,
/// and one that drops the leases a restarted daemon re-adopted and still runs (they
/// would run twice), as a registration fed the running set the wire's `Hello` lacks
/// would.
#[test]
fn a_worker_that_registers_again_keeps_only_what_it_still_runs() {
    let scenario = Scenario {
        cut: false,
        worker_1: Fault::Reboot(REBOOT_AT),
        worker_2: Fault::Restart(RESTART_AT),
    };
    let reboot = FarmTime::from_millis(REBOOT_AT);
    let restart = FarmTime::from_millis(RESTART_AT);
    for seed in 0..SEEDS {
        let sim = run_scenario(seed, scenario);
        let answers = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        assert_never_twice_at_once(&sim, seed);
        let grants = grants(&sim);

        let killed: Vec<(&LeaseId, &Run)> = worker(&sim, "worker-1")
            .runs
            .iter()
            .filter(|(_, r)| r.ended == Some(reboot))
            .collect();
        assert!(!killed.is_empty(), "seed {seed}: the reboot killed no run");
        for (&lost, run) in killed {
            let op = run.operation;
            let again = grants[&op].iter().find(|(l, _)| **l > lost);
            assert!(
                again.is_some_and(|(_, t)| *t < reboot.saturating_add(REGRANT_BOUND)),
                "seed {seed}: {op} lost {lost} in the reboot; granted again {again:?}"
            );
            assert!(answers[&op] > lost, "seed {seed}: {op} answered by {lost}");
        }

        let adopted: Vec<&Run> = worker(&sim, "worker-2")
            .runs
            .values()
            .filter(|r| r.started < restart)
            .collect();
        assert!(
            !adopted.is_empty(),
            "seed {seed}: worker-2 re-adopted no run"
        );
        for run in adopted {
            let op = run.operation;
            assert_eq!(
                grants[&op].len(),
                1,
                "seed {seed}: {op}, re-adopted by worker-2, was granted again"
            );
        }
    }
}

/// Catches: a committed lease whose `Start` never reached a worker that stays connected
/// kept for good (its operation is never answered and its booking leaks), and one
/// given up before the Start grace, while that `Start` could still arrive.
#[test]
fn a_start_lost_on_a_live_session_is_granted_again_after_the_grace() {
    let scenario = Scenario {
        cut: false,
        worker_1: Fault::DropFirstStart,
        worker_2: Fault::None,
    };
    for seed in 0..SEEDS {
        let sim = run_scenario(seed, scenario);
        let answers = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        assert_never_twice_at_once(&sim, seed);
        let (lost, op) = worker(&sim, "worker-1")
            .dropped
            .first()
            .copied()
            .expect("worker-1 lost a Start");
        assert_replaced_after_grace(&sim, seed, &answers, op, lost);
    }
}

/// Catches: a committed lease the worker's running set leaves out kept past the Start
/// grace (its operation waits on a run nobody accounts for), and the result that worker
/// reports late for it proposed or accepted over the re-grant.
#[test]
fn a_lease_missing_from_the_running_set_is_granted_again_and_its_late_result_fenced() {
    let scenario = Scenario {
        cut: false,
        worker_1: Fault::HideFirstHermetic,
        worker_2: Fault::None,
    };
    for seed in 0..SEEDS {
        let sim = run_scenario(seed, scenario);
        let answers = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        assert_never_twice_at_once(&sim, seed);
        let w1 = worker(&sim, "worker-1");
        let hidden = w1.hidden.expect("worker-1 hid a lease");
        let run = &w1.runs[&hidden];
        // The scenario did what it claims: the hidden run finished and its late report
        // reached the leader.
        assert_eq!(
            run.ended,
            Some(run.started.saturating_add(RUN_FOR)),
            "seed {seed}"
        );
        assert!(!w1.unacked.contains_key(&hidden), "seed {seed}");
        assert_replaced_after_grace(&sim, seed, &answers, run.operation, hidden);
        let proposed = log(&sim)
            .records
            .iter()
            .any(|(_, r)| matches!(r, ControlRecord::Result(res) if res.lease == hidden));
        assert!(
            !proposed,
            "seed {seed}: the late result of {hidden} was proposed"
        );
    }
}

/// Catches: a lease lost on a worker that drops every `Start` retried on that same
/// worker although worker-2 has room (today first fit sends it back to worker-1 for
/// ever, and its waiters are never answered). Each operation worker-1 lost must be
/// granted there once, then on worker-2, and answered by worker-2's run.
#[test]
fn a_worker_that_drops_every_start_loses_its_operations_to_another() {
    let scenario = Scenario {
        cut: false,
        worker_1: Fault::DropEveryStart,
        worker_2: Fault::None,
    };
    for seed in 0..SEEDS {
        let sim = run_scenario(seed, scenario);
        let answers = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        assert_never_twice_at_once(&sim, seed);
        let lost = &worker(&sim, "worker-1").dropped;
        assert!(!lost.is_empty(), "seed {seed}: worker-1 lost no Start");
        let grants = grants_on(&sim);
        let outcomes = outcomes(&sim);
        for &(lease, op) in lost {
            let on: Vec<(LeaseId, &str)> = grants[&op]
                .iter()
                .map(|(l, w)| (*l, w.as_str()))
                .collect();
            let [(first, "worker-1"), (second, "worker-2")] = on.as_slice() else {
                panic!("seed {seed}: {op} lost {lease} on worker-1; granted {on:?}");
            };
            assert_eq!(*first, lease, "seed {seed}: {op}");
            assert_eq!(answers[&op], *second, "seed {seed}: {op}");
            assert!(
                matches!(outcomes[&op], Outcome::Completed { .. }),
                "seed {seed}: {op} answered {:?}",
                outcomes[&op]
            );
        }
    }
}

/// Catches: an operation whose every lease is lost retried for ever (its waiters are
/// never answered), and an infra budget off by one. Each operation must be granted
/// exactly three times, then answered with an `INFRA` failure by its third lease.
#[test]
fn an_operation_that_loses_every_start_fails_after_three_attempts() {
    let scenario = Scenario {
        cut: false,
        worker_1: Fault::DropEveryStart,
        worker_2: Fault::DropEveryStart,
    };
    for seed in 0..SEEDS {
        let sim = run_scenario(seed, scenario);
        let answers = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        let outcomes = outcomes(&sim);
        for (op, leases) in grants_on(&sim) {
            assert_eq!(leases.len(), 3, "seed {seed}: {op} granted {leases:?}");
            let third = leases.last_key_value().map(|(l, _)| *l);
            assert_eq!(Some(answers[&op]), third, "seed {seed}: {op}");
            assert_eq!(
                outcomes[&op],
                Outcome::Failed(Failure::Infra),
                "seed {seed}: {op}"
            );
        }
    }
}
