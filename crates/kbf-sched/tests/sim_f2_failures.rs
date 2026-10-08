//! Scenario family F2 of the scheduler's simulation catalog
//! (`docs/design/simulation.md`, added in komira-ai/komira-build-farm#139): failures of
//! workers, networks, clocks and servers, on the simulated cell of `sim/cell` (a leader
//! running the scheduler as `kbf-server` does, a log that commits after a random
//! delay, and workers that follow the daemon's rules).
//!
//! | # | Scenario | Test |
//! |---|---|---|
//! | F2.1 | a worker dies mid-lease and never returns (with a slow log) | `f2_1_*` |
//! | F2.2 | heartbeat gaps shorter than G: partitions and one-way losses | `f2_2_*` |
//! | F2.3 | the G boundary: ticks at G - 1 ms, G and G + 1 ms | `f2_3_*` |
//! | F2.4 | a `Start` delayed beyond W | `f2_4_*` |
//! | F2.6 | a worker suspended below T, between T and G, and above G | `f2_6_*` |
//! | F2.7 | the leader's clock paused with its process | `f2_7_*` |
//! | F2.9 | a server restart (issue #137) | `f2_9_*` |
//! | F2.10 | stale, duplicated and reordered session messages | `f2_10_*` |
//! | F2.11 | lost and repeated results and acknowledgements | `f2_11_*` |
//! | F2.12 | two daemons claiming one node id (issue #140) | `f2_12_*` |
//! | F2.13 | leases of another term listed | `f2_13_*` |
//!
//! F2.5 (a `Start` lost on a live session) and F2.8 (a reboot and a daemon restart inside
//! G) are in `sim_cell.rs`.
//!
//! Every input the leader feeds the scheduler is checked at once (`sim/cell/check.rs`:
//! I1 to I7, I14, and the family's R, P and N). At the end of each run: every caller
//! answered exactly once, nothing held, booked or queued (L1); no operation's work ran
//! twice at once (I12; the daemon self-fences every lease today); every `Start` a worker
//! received named a lease its leader's log had committed (I2). Each scenario then checks
//! what it is about, and that its sweep reached the situations it means to reach.
//!
//! A failure prints the scenario, seed, time, invariant and a replay command:
//! `KBF_SIM_SEED=<n> KBF_SIM_SCENARIO=<F2.x> cargo test -p kbf-sched --test sim_f2_failures -- --ignored --exact replay`.
//! The long sweep: `cargo test -p kbf-sched --release --test sim_f2_failures -- --ignored --exact many_seeds`.

#[path = "sim/cell/mod.rs"]
mod cell;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use cell::worker::{End, Took};
use cell::{Ctx, LeaderPlan, LogPlan, Plan, WorkerPlan, World, calm_network, check::CheckStats};
use kbf_sched::fence::{LEASE_GRACE, SELF_FENCE, START_GRACE, START_VALIDITY};
use kbf_sim::{Chance, Faults, SimRng};
use kbf_types::{ControlRecord, Failure, LeaseId, OperationId, Outcome};

/// Seeds per scenario in CI.
const SEEDS: u64 = 64;
/// Seeds per scenario in the long sweep.
const MANY: u64 = 1_000;
const SECOND: u64 = 1_000;
const T_MS: u64 = SELF_FENCE.as_secs() * SECOND;
const G_MS: u64 = LEASE_GRACE.as_secs() * SECOND;
const W_MS: u64 = START_VALIDITY.as_secs() * SECOND;

// The scenarios' windows assume these; a change to the fence constants must revisit
// them. F2.6's middle suspends (42 to 52 s) pass T and end before G even when the
// leader last heard the worker a heartbeat interval before the suspend; F2.4's late
// `Start`s are late by at least W + 1 s.
const _: () = assert!(T_MS < 42 * SECOND && 52 * SECOND + 6 * SECOND < G_MS);
const _: () = assert!(W_MS + SECOND < G_MS);

/// The executing spans of every run of each (leader incarnation, operation).
type Spans = BTreeMap<(u64, OperationId), Vec<(u64, u64, String)>>;

/// The scenarios the sweeps run.
const SCENARIOS: [&str; 11] = [
    "F2.1", "F2.2", "F2.3", "F2.4", "F2.6", "F2.7", "F2.9", "F2.10", "F2.11", "F2.12", "F2.13",
];

fn workers() -> Vec<WorkerPlan> {
    vec![
        WorkerPlan::plain("worker-1", 4),
        WorkerPlan::plain("worker-2", 4),
        WorkerPlan::plain("worker-3", 4),
    ]
}

/// Draws scenario `name`'s plan for `seed`.
fn plan(name: &'static str, seed: u64) -> Plan {
    let mut rng = SimRng::from_seed(seed ^ 0xf2f2_0000);
    let mut plan = Plan {
        ctx: Ctx {
            scenario: name,
            seed,
        },
        faults: calm_network(),
        partitions: Vec::new(),
        leader: LeaderPlan::default(),
        log: LogPlan::PROMPT,
        workers: workers(),
        callers: 24,
        arrive_by: 150 * SECOND,
        end: 800 * SECOND,
    };
    let at = |rng: &mut SimRng, lo: u64, hi: u64| rng.between(lo * SECOND, hi * SECOND);
    match name {
        "F2.1" => {
            // Half the seeds kill worker-1 at a random time; the other half right after
            // it sends a result, which a slow log may commit only after its lease was
            // given up and granted again.
            if seed.is_multiple_of(2) {
                plan.workers[0].die_at = Some(at(&mut rng, 20, 150));
            } else {
                plan.workers[0].die_after_results = Some(rng.between(1, 3));
            }
            plan.leader.boundary_ticks = true;
            plan.log = LogPlan {
                fast: (0, 20),
                slow: Chance::percent(40),
                slow_ms: (55 * SECOND, 70 * SECOND),
            };
        }
        "F2.2" => {
            // One to three gaps of 1 to 59 s, one after another, each a partition, a
            // one-way loss into the leader, or a one-way loss out of it.
            let mut from = at(&mut rng, 10, 60);
            for _ in 0..rng.between(1, 3) {
                let len = at(&mut rng, 1, 59);
                let who = ["worker-1", "worker-2", "worker-3"][rng.below(3) as usize];
                match rng.below(3) {
                    0 => plan.partitions.push((from, from + len, vec![who])),
                    1 => plan.leader.deaf.push((who, from, from + len)),
                    _ => plan.leader.mute.push((who, from, from + len)),
                }
                from += len + at(&mut rng, 5, 60);
            }
        }
        "F2.3" => {
            plan.workers[0].die_at = Some(at(&mut rng, 20, 150));
            plan.leader.boundary_ticks = true;
        }
        "F2.4" => {
            let delay = rng.between(W_MS + SECOND, 2 * G_MS);
            plan.leader.late_start = Some((rng.between(1, 10), delay));
        }
        "F2.6" => {
            let len = match seed % 3 {
                0 => at(&mut rng, 1, 25),
                1 => at(&mut rng, 42, 52),
                _ => at(&mut rng, 70, 120),
            };
            plan.workers[0].freezes = vec![(at(&mut rng, 30, 120), len)];
        }
        "F2.7" => {
            plan.leader.pause = Some((at(&mut rng, 30, 150), at(&mut rng, 5, 120)));
        }
        "F2.9" => {
            plan.leader.restart_at = Some(at(&mut rng, 30, 150));
        }
        "F2.10" => {
            plan.faults = Faults {
                min_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(1_500),
                duplicate: Chance::percent(20),
                reorder: Chance::percent(20),
                ..Faults::default()
            };
            for w in &mut plan.workers {
                w.report_changes = (0..rng.between(3, 6))
                    .map(|_| at(&mut rng, 5, 250))
                    .collect();
                w.reconnects = (0..rng.between(1, 3))
                    .map(|_| at(&mut rng, 5, 250))
                    .collect();
            }
        }
        "F2.11" => {
            plan.leader.lose_reports = Chance::percent(30);
            plan.leader.repeat_acks = Chance::percent(30);
            plan.log = LogPlan {
                fast: (0, 3 * SECOND),
                slow: Chance::never(),
                slow_ms: (0, 0),
            };
            for w in &mut plan.workers {
                w.lose_acks = Chance::percent(30);
                w.repeat_reports = Chance::percent(30);
            }
        }
        "F2.12" => {
            let twin_at = at(&mut rng, 30, 90);
            let mut twin = WorkerPlan::plain("worker-1-twin", 4);
            twin.node = "worker-1";
            twin.boot_at = twin_at;
            plan.workers.push(twin);
            // Half the seeds: the first daemon opens a new stream later and takes the
            // node back. The other half: it dies soon after the twin took over (a node
            // replaced while the old one still ran, then switched off), so the leases
            // it ran are given up only once the twin's heartbeats outlast the handover
            // grace (no silence for G: the node keeps being heard).
            if seed.is_multiple_of(2) {
                plan.workers[0].reconnects = vec![twin_at + at(&mut rng, 20, 60)];
            } else {
                plan.workers[0].die_at = Some(twin_at + at(&mut rng, 1, 20));
            }
        }
        "F2.13" => {
            plan.workers[0].phantoms = vec![
                LeaseId::new(0, 3),
                LeaseId::new(2, 1),
                LeaseId::new(1, 1_000_000),
            ];
        }
        other => panic!("no scenario {other}"),
    }
    plan
}

/// Runs `name` on `seed` and checks what every run must hold.
fn run(name: &'static str, seed: u64) -> World {
    let world = World::run(plan(name, seed));
    check_world(&world);
    world
}

/// L1, I12 and the workers' side of I2, at the end of a run.
fn check_world(w: &World) {
    let ctx = w.plan.ctx;
    let end = w.end();
    let l = w.leader();
    for (n, c) in l.callers.iter().enumerate() {
        if c.answers.len() != 1 {
            ctx.fail(
                end,
                "L1",
                &format!("caller {n} answered {} times", c.answers.len()),
            );
        }
    }
    let queued: Vec<OperationId> = l.sched.queued().collect();
    if !queued.is_empty() || !l.held_leases().is_empty() {
        let held = l.held_leases();
        ctx.fail(
            end,
            "L1",
            &format!("queued {queued:?}, held {held:?} at the end"),
        );
    }
    for node in l.nodes() {
        if l.sched
            .booked(node)
            .is_some_and(|b| b != kbf_types::Resources::default())
        {
            ctx.fail(end, "L1", &format!("{node} still booked at the end"));
        }
    }

    // Every run has ended (a dead machine's runs end with it), so its spans are closed.
    // The spans of one run never overlap each other: a run is frozen, then goes on.
    let mut spans = Spans::new();
    for (id, worker) in w.workers() {
        for (i, run) in worker.runs.iter().enumerate() {
            if run.end.is_none() {
                ctx.fail(
                    end,
                    "L1",
                    &format!("{id} still runs {} at the end", run.lease),
                );
            }
            for (a, b) in &run.spans {
                let key = (run.incarnation, run.operation);
                let tag = format!("{id} run {i} of {}", run.lease);
                spans
                    .entry(key)
                    .or_default()
                    .push((a.as_millis(), b.as_millis(), tag));
            }
        }
    }
    for ((inc, op), mut s) in spans {
        s.sort_unstable();
        for pair in s.windows(2) {
            if pair[0].1 > pair[1].0 {
                ctx.fail(
                    end,
                    "I12",
                    &format!("{op} of incarnation {inc} ran twice at once: {pair:?}"),
                );
            }
        }
    }

    for (id, worker) in w.workers() {
        for seen in &worker.starts {
            let committed = w.log().entries.iter().any(|e| {
                e.incarnation == seen.incarnation
                    && e.at <= seen.at
                    && matches!(&e.record, ControlRecord::Lease(g) if g.lease == seen.lease)
            });
            if !committed {
                ctx.fail(
                    seen.at,
                    "I2",
                    &format!("{id} received a Start of {} before its commit", seen.lease),
                );
            }
        }
    }
}

/// The sum of the checks' counts over a run's incarnations.
fn stats(w: &World) -> CheckStats {
    let l = w.leader();
    let mut s = CheckStats::default();
    for c in l.past.iter().chain(std::iter::once(&l.check)) {
        let t = &c.stats;
        s.steps += t.steps;
        s.requeued_silent += t.requeued_silent;
        s.requeued_at_g += t.requeued_at_g;
        s.kept_before_g += t.kept_before_g;
        s.requeued_earlier_session += t.requeued_earlier_session;
        s.requeued_after_grace += t.requeued_after_grace;
        s.requeued_after_handover += t.requeued_after_handover;
        s.kept_for_handover += t.kept_for_handover;
        s.kept_omitted += t.kept_omitted;
        s.kept_proposed += t.kept_proposed;
        s.answered += t.answered;
        s.answered_failed += t.answered_failed;
        s.named_not_held += t.named_not_held;
        s.foreign_listed += t.foreign_listed;
        s.capacity_resends += t.capacity_resends;
        s.stale_grant_commits += t.stale_grant_commits;
        s.superseded_results += t.superseded_results;
    }
    s
}

fn fenced(w: &World) -> u64 {
    w.workers().map(|(_, x)| x.stats.fenced).sum()
}

/// Asserts a sweep reached a situation: a check that cannot fail is not a check.
#[track_caller]
fn reached(name: &str, what: &str, count: u64) {
    assert!(count > 0, "{name}: no seed of the sweep reached {what}");
}

/// F2.1. Catches: a dead worker's leases kept past G or given up before it (`alive`
/// off by one, or 2G), and, with a slow log, a superseded grant started when its commit
/// comes back after its lease was given up, or a stale result accepted.
#[test]
fn f2_1_a_dead_worker_s_leases_are_requeued_at_g_and_answered_elsewhere() {
    let (mut silent, mut moved, mut stale, mut superseded) = (0, 0, 0, 0);
    for seed in 0..SEEDS {
        let w = run("F2.1", seed);
        let s = stats(&w);
        silent += s.requeued_silent;
        stale += s.stale_grant_commits;
        superseded += s.superseded_results;
        let check = &w.leader().check;
        let dead: Vec<&cell::worker::Run> = w
            .worker("worker-1")
            .runs
            .iter()
            .filter(|r| r.end.is_some_and(|(_, e)| e == End::Died))
            .collect();
        for run in dead {
            let (lease, _) = check.answered[&run.operation];
            assert!(
                lease > run.lease,
                "F2.1 seed {seed}: {} answered by the dead {lease}",
                run.operation
            );
            moved += 1;
        }
    }
    reached("F2.1", "a lease given up for silence", silent);
    reached(
        "F2.1",
        "an operation that died with its worker answered by a later lease",
        moved,
    );
    reached(
        "F2.1",
        "a grant committed after its lease was given up",
        stale,
    );
    reached(
        "F2.1",
        "a result committed after a newer grant of its operation",
        superseded,
    );
}

/// F2.2. Catches: a lease given up while its worker is heard within G, a self-fenced
/// run outliving T, and a fenced run's ABORTED result not accepted (or answered twice)
/// while the scheduler still held its lease.
#[test]
fn f2_2_gaps_shorter_than_g_requeue_nothing_heard_and_fence_after_t() {
    let (mut fences, mut failed, mut gap_no_requeue) = (0, 0, 0);
    for seed in 0..SEEDS {
        let w = run("F2.2", seed);
        let s = stats(&w);
        fences += fenced(&w);
        failed += s.answered_failed;
        if s.requeued_silent == 0 {
            gap_no_requeue += 1;
        }
        let check = &w.leader().check;
        for (lease, (op, outcome)) in &check.proposals {
            assert_eq!(
                check.answered.get(op),
                Some(&(*lease, *outcome)),
                "F2.2 seed {seed}: the proposed result of {lease} did not answer {op}"
            );
        }
        for (_, worker) in w.workers() {
            for run in worker
                .runs
                .iter()
                .filter(|r| r.end.is_some_and(|(_, e)| e == End::Fenced))
            {
                if check.answered[&run.operation].0 == run.lease {
                    assert_eq!(
                        check.answered[&run.operation].1,
                        Outcome::Failed(Failure::Infra),
                        "F2.2 seed {seed}"
                    );
                }
            }
        }
    }
    reached("F2.2", "a self-fenced run", fences);
    reached("F2.2", "an operation failed by its fenced run", failed);
    reached(
        "F2.2",
        "a run with gaps and nothing requeued",
        gap_no_requeue,
    );
}

/// F2.3. Catches: `alive` measured with `<=` (the lease kept at exactly G) or with
/// any slack, and a lease given up at G - 1 ms.
#[test]
fn f2_3_a_lease_is_kept_at_g_minus_1_ms_and_requeued_at_g() {
    let (mut at_g, mut before) = (0, 0);
    for seed in 0..SEEDS {
        let s = stats(&run("F2.3", seed));
        at_g += s.requeued_at_g;
        before += s.kept_before_g;
    }
    reached("F2.3", "a lease given up at exactly G", at_g);
    reached("F2.3", "a tick at G - 1 ms keeping the leases", before);
}

/// F2.4. Catches: a daemon that acts on a `Start` W or more after the heartbeat it
/// names (it then runs beside the retry), and a scheduler that gives the lease up
/// before `START_GRACE` or never.
#[test]
fn f2_4_a_late_start_is_dropped_and_its_lease_granted_again_after_the_grace() {
    let mut dropped = 0;
    for seed in 0..SEEDS {
        let w = run("F2.4", seed);
        let l = w.leader();
        let Some((lease, op, sent)) = l.stats.late_start else {
            continue;
        };
        let ran = w.workers().any(|(_, x)| {
            x.runs
                .iter()
                .any(|r| r.lease == lease && r.incarnation == 0)
        });
        assert!(!ran, "F2.4 seed {seed}: the late Start of {lease} ran");
        let late = w.workers().any(|(_, x)| {
            x.starts
                .iter()
                .any(|s| s.lease == lease && s.took == Took::Late)
        });
        dropped += u64::from(late);
        let check = &l.check;
        let again = check.leases_of(op).into_iter().find(|x| *x > lease);
        let at = again.and_then(|x| check.granted_at(x));
        assert!(
            at.is_some_and(|t| t >= sent.saturating_add(START_GRACE)),
            "F2.4 seed {seed}: {op} lost {lease} (Start at {sent:?}), granted again at {at:?}"
        );
        assert!(
            check.answered[&op].0 > lease,
            "F2.4 seed {seed}: {op} answered by {lease}"
        );
    }
    reached("F2.4", "a late Start dropped by the daemon", dropped);
}

/// F2.6. Catches: a resumed worker whose old contact keeps its runs going (no fence
/// first), a suspend below T that changes anything, the ABORTED results of runs fenced
/// at a resume inside G not accepted, and those of a resume after G accepted.
#[test]
fn f2_6_a_suspended_worker_fences_first_on_resume() {
    let (mut short, mut mid, mut long) = (0, 0, 0);
    for seed in 0..SEEDS {
        let w = run("F2.6", seed);
        let s = stats(&w);
        let w1 = w.worker("worker-1");
        let check = &w.leader().check;
        let fenced_runs: Vec<&cell::worker::Run> = w1
            .runs
            .iter()
            .filter(|r| r.end.is_some_and(|(_, e)| e == End::Fenced))
            .collect();
        match seed % 3 {
            0 => {
                // A short suspend fences nothing and loses no lease to silence. Only a
                // `Start` that arrived during it is late, dropped, and its lease given up
                // after the grace, as any late `Start` is.
                assert_eq!(
                    w1.stats.fenced, 0,
                    "F2.6 seed {seed}: a short suspend fenced"
                );
                assert_eq!(s.requeued_silent, 0, "F2.6 seed {seed}");
                let late = w1.starts.iter().filter(|x| x.took == Took::Late).count() as u64;
                assert!(s.requeued_after_grace <= late, "F2.6 seed {seed}");
                short += 1;
            }
            1 => {
                assert_eq!(s.requeued_silent, 0, "F2.6 seed {seed}: requeued inside G");
                for run in &fenced_runs {
                    assert_eq!(
                        check.answered[&run.operation],
                        (run.lease, Outcome::Failed(Failure::Infra)),
                        "F2.6 seed {seed}: the fenced {} not answered by its ABORTED result",
                        run.lease
                    );
                }
                mid += w1.stats.fenced_on_resume;
            }
            _ => {
                for run in &fenced_runs {
                    assert!(
                        check.answered[&run.operation].0 > run.lease,
                        "F2.6 seed {seed}"
                    );
                }
                long += u64::from(s.requeued_silent > 0 && w.leader().stats.refused_results > 0);
            }
        }
    }
    reached("F2.6", "a suspend below T", short);
    reached("F2.6", "a resume between T and G that fenced", mid);
    reached(
        "F2.6",
        "a resume after G whose stale results were refused",
        long,
    );
}

/// F2.7. Catches: a scheduler that reads time other than from its inputs, and so
/// gives up leases the moment a paused leader resumes.
#[test]
fn f2_7_a_paused_leader_requeues_only_after_g_of_its_own_time() {
    let mut fences = 0;
    for seed in 0..SEEDS {
        let w = run("F2.7", seed);
        assert_eq!(w.leader().stats.resumed, 1, "F2.7 seed {seed}");
        fences += fenced(&w);
    }
    reached("F2.7", "a pause long enough for workers to fence", fences);
}

/// F2.9. Catches (issue #137): a restarted server that grants a lease id an old
/// process granted to the same worker, and accepts the old run's result for the new
/// operation (I5). Before the fix every process granted leases of term 1, and seed 0
/// already failed I5. The sweep must reach a worker that held a lease of the old
/// process when the new one welcomed it (and dropped it), and a sequence number both
/// processes granted to one worker: the collision the fix makes harmless.
#[test]
fn f2_9_after_a_server_restart_every_answer_comes_from_its_own_lease() {
    let (mut dropped, mut collided) = (0, 0);
    for seed in 0..SEEDS {
        let w = run("F2.9", seed);
        assert_eq!(w.leader().stats.restarts, 1, "F2.9 seed {seed}");
        for (_, worker) in w.workers() {
            dropped += worker.stats.superseded;
            let seqs = |inc: u64| -> BTreeSet<u64> {
                let starts = worker.starts.iter().filter(|s| s.incarnation == inc);
                starts.map(|s| s.lease.seq).collect()
            };
            collided += seqs(0).intersection(&seqs(1)).count() as u64;
        }
    }
    reached(
        "F2.9",
        "a lease of the old process dropped on a Welcome",
        dropped,
    );
    reached(
        "F2.9",
        "a sequence number both processes granted to one worker",
        collided,
    );
}

/// F2.10. Catches: a resent `Hello` (`Capacity`) that opens a session (a `Start` in
/// flight is then given up and run twice), and a scheduler that gives up a lease a
/// heartbeat of its own session leaves out before `START_GRACE`.
#[test]
fn f2_10_stale_session_messages_requeue_nothing() {
    let (mut resent, mut replaced, mut kept) = (0, 0, 0);
    for seed in 0..SEEDS {
        let w = run("F2.10", seed);
        let s = stats(&w);
        resent += s.capacity_resends;
        replaced += w.leader().stats.replaced_beats;
        kept += s.kept_omitted;
    }
    reached("F2.10", "a resent Hello fed as Capacity", resent);
    reached("F2.10", "a replaced stream's heartbeat dropped", replaced);
    reached("F2.10", "a lease left out inside its grace and kept", kept);
}

/// F2.11. Catches: a result proposed twice for one holding, a lease with a proposed
/// result given up when a heartbeat leaves it out, and a lost result never resent.
#[test]
fn f2_11_lost_and_repeated_results_are_proposed_once() {
    let (mut lost, mut acks, mut kept) = (0, 0, 0);
    for seed in 0..SEEDS {
        let w = run("F2.11", seed);
        lost += w.leader().stats.lost_reports;
        acks += w.workers().map(|(_, x)| x.stats.lost_acks).sum::<u64>();
        kept += stats(&w).kept_proposed;
    }
    reached("F2.11", "a lost Result", lost);
    reached("F2.11", "a lost ResultAck", acks);
    reached(
        "F2.11",
        "a lease with a proposed result left out and kept",
        kept,
    );
}

/// F2.12. Catches: `Start`s or acknowledgements going to a replaced stream, and the
/// replaced daemon's work running beside its retry (issue #140): a lease whose `Start`
/// went to the other daemon requeued on the newer daemon's first heartbeat that leaves
/// it out, while the older one, no longer acknowledged, runs it until its fence (I12),
/// or kept past the handover grace (R).
#[test]
fn f2_12_two_daemons_claiming_one_node_id() {
    let (mut replaced, mut kept, mut handed_over) = (0, 0, 0);
    for seed in 0..SEEDS {
        let w = run("F2.12", seed);
        replaced += w.leader().stats.replaced_beats;
        let s = stats(&w);
        kept += s.kept_for_handover;
        handed_over += s.requeued_after_handover;
    }
    reached("F2.12", "a replaced daemon's heartbeat dropped", replaced);
    reached(
        "F2.12",
        "a replaced daemon's lease left out and kept inside the handover grace",
        kept,
    );
    reached(
        "F2.12",
        "a replaced daemon's lease given up after the handover grace",
        handed_over,
    );
}

/// F2.13. Catches: `not_held` naming a lease of another term, or one of this term this
/// scheduler never granted, for cancelling.
#[test]
fn f2_13_leases_of_other_terms_are_never_cancelled() {
    let mut foreign = 0;
    for seed in 0..SEEDS {
        foreign += stats(&run("F2.13", seed)).foreign_listed;
    }
    reached(
        "F2.13",
        "a heartbeat listing a lease of another term",
        foreign,
    );
}

/// Catches: anything in the scheduler or the cell that depends on more than the seed.
#[test]
fn a_seed_replays_exactly() {
    for name in SCENARIOS {
        let hash = |seed| World::run(plan(name, seed)).sim.trace_hash();
        assert_eq!(hash(7), hash(7), "{name}");
        assert_ne!(hash(7), hash(8), "{name}");
    }
}

/// Runs one seed of one scenario with every check:
/// `KBF_SIM_SEED=<n> KBF_SIM_SCENARIO=<F2.x> cargo test -p kbf-sched --test sim_f2_failures -- --ignored --exact replay`.
#[test]
#[ignore = "replays the seed in KBF_SIM_SEED of the scenario in KBF_SIM_SCENARIO"]
fn replay() {
    let seed: u64 = std::env::var("KBF_SIM_SEED")
        .expect("KBF_SIM_SEED")
        .parse()
        .expect("a seed");
    let name = std::env::var("KBF_SIM_SCENARIO").expect("KBF_SIM_SCENARIO");
    let name = SCENARIOS
        .into_iter()
        .find(|s| *s == name)
        .expect("a scenario of this file");
    run(name, seed);
}

/// Every scenario on many more seeds:
/// `cargo test -p kbf-sched --release --test sim_f2_failures -- --ignored --exact many_seeds`.
#[test]
#[ignore = "long: run with --ignored --release"]
fn many_seeds() {
    for name in SCENARIOS {
        for seed in 0..MANY {
            run(name, seed);
        }
    }
}
