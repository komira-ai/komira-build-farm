//! Family F4 of the scheduler simulation catalog (`docs/design/simulation.md`
//! section 5): large randomized fleets with every invariant checked after every input.
//!
//! The world (`f4/world.rs`) is 200 workers drawn from Linux x86-64 at levels v2 to
//! v4, Linux arm64, Macs with one or two Xcode builds, and GPU nodes, fed with arrivals
//! of random size, platform (some no worker satisfies), QoS level and dedup key, at 70
//! to 95 percent of the fleet's CPU. Workers keep a daemon's rules: heartbeats with
//! their running set, results resent until acknowledged, cancels of leases not held,
//! and the self-fence T while cut off. A `Start` reaches its worker after up to 2 s
//! in half the seeds, and only on the stream it was sent on. The control log is in
//! process and commits in proposal order, after up to 3 s in half the seeds (2 to 3 s
//! in F4.2 and F4.4). Repeated keys arrive as dedup joins, and some as twins that may
//! not join (networked, or not to be cached). Each seed also switches each fault kind
//! on or off and draws its rate (the swarm): lost `Start`s, lost heartbeats,
//! duplicated results, reboots, daemon restarts, partitions of up to 2G, node report
//! changes (capacity and Xcode builds), and operator actions.
//!
//! The checker (`f4/check.rs`) runs after every input the scheduler is fed and checks
//! its effects, and the state that input touched, against a reference model: I1 to
//! I14 of the catalog, and L2 and L3 (every operation and worker, untouched ones
//! included, is compared every simulated half minute; its header says which checks
//! run when); and that the accepted result is one the very lease's run produced (I5
//! with a worker model), and that no self-fenced operation runs twice at once (I12).
//! Once arrivals stop, no new fault starts, every dead node returns and every cordon
//! ends, and L1 is checked: every operation finishes within a bound computed from the
//! backlog and the outages left, every waiter is answered once, nothing stays booked
//! or queued. I15 is `a_seed_replays`.
//!
//! A seed is the whole input, and picks its scenario as `seed % 4`:
//!
//! - F4.1 steady state: arrivals only (no worker or operator faults);
//! - F4.2 churn: 5 percent of workers die or return each minute;
//! - F4.3 mass reconnect: every worker reboots or restarts its daemon and comes back
//!   in the same second, just after the handover grace, once or twice (more than one
//!   placement round's worth of work is requeued at once); and once every worker
//!   resends `Hello` on its stream with a changed node report;
//! - F4.4 operator storm: cordon, drain and uncordon at random on a tenth of the fleet,
//!   and one maintenance: a small labelled pool and four other nodes are drained and
//!   go offline for longer than G and the unservable wait; the four are uncordoned
//!   while still offline, just before G, across a short stall of the control log.
//!
//! Each scenario's sweep asserts it reached the situations it exists for (a refusal, a
//! promotion, a lease requeued on a new session, a paused drain, ...), so a check that
//! never fires is noticed.
//!
//! CI runs 16 seeds of 2,000 operations: about 5 s of test time in a debug build on
//! the dev box, under a minute on a CI runner. A failing check prints its seed and a
//! replay command (`--nocapture` so a replay that passes still prints what it
//! reached):
//!
//! ```text
//! KBF_SIM_SEED=<n> KBF_SIM_OPS=<ops> cargo test -p kbf-sched --test f4_fleet -- --ignored --exact replay --nocapture
//! ```
//!
//! The long sweep, 1,000 seeds of 10,000 operations (run it in a release build):
//!
//! ```text
//! cargo test --release -p kbf-sched --test f4_fleet -- --ignored --exact long_sweep
//! ```
//!
//! Not here yet: the catalog's smaller cell version of F4 (20 workers on the
//! `kbf-sim` kernel, for network faults), which builds on the cell nodes family F2
//! moves out of `sim_cell`.

mod f4;

use f4::check::Reach;
use f4::world::{Scenario, World};

const WORKERS: usize = 200;
const CI_SEEDS: u64 = 16;
const CI_OPS: u64 = 2_000;
const LONG_SEEDS: u64 = 1_000;
const LONG_OPS: u64 = 10_000;

fn run(seed: u64, ops: u64) -> World {
    let mut world = World::new(seed, WORKERS, ops);
    world.run();
    world
}

/// Runs the CI seeds of `scenario`, one thread each, and returns what they reached,
/// summed.
fn sweep(scenario: Scenario) -> Reach {
    let seeds = (0..CI_SEEDS).filter(|s| Scenario::of(*s) == scenario);
    let reaches: Vec<Reach> = std::thread::scope(|s| {
        let runs: Vec<_> = seeds
            .map(|seed| s.spawn(move || run(seed, CI_OPS).check.reach))
            .collect();
        runs.into_iter()
            .map(|r| r.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    let mut reach = Reach::new();
    for (what, n) in reaches.iter().flatten() {
        *reach.entry(what).or_default() += n;
    }
    reach
}

#[track_caller]
fn assert_reached(reach: &Reach, wanted: &[&str]) {
    let missing: Vec<&&str> = wanted
        .iter()
        .filter(|what| reach.get(**what).is_none_or(|&n| n == 0))
        .collect();
    assert!(
        missing.is_empty(),
        "the sweep never reached {missing:?}: {reach:#?}"
    );
}

/// What every scenario reaches: grants, answers, refusals (stated and on time), dedup
/// joins, twins that may not join, promotions of queued work, servable work waiting
/// for room, and finished operations dropped once their retention is up.
const EVERY: [&str; 8] = [
    "granted",
    "answered",
    "dropped after the retention",
    "refused",
    "join",
    "non-joinable twin",
    "promotion while queued",
    "servable work waits for room",
];

/// Catches: placement out of queue order or past a free fit (I11), a booking that
/// overflows a worker or drifts from its leases (I6), a refusal early, late or for
/// work a worker could run (I10, L2), a lost or doubled answer (I4), a promotion that
/// does not move the queue (I13, I14), and work left behind at the end (L1).
#[test]
fn f4_1_steady_state() {
    let reach = sweep(Scenario::Steady);
    assert_reached(&reach, &EVERY);
}

/// Catches, besides F4.1's: expiry that is early or late (I3), a result accepted from
/// a lease given up to silence (I5), self-fenced work run twice at once (I12), and
/// refusals checked against the reference verdict as live workers come and go (I10).
#[test]
fn f4_2_churn() {
    let reach = sweep(Scenario::Churn);
    assert_reached(&reach, &EVERY);
    assert_reached(
        &reach,
        &[
            "worker died",
            "worker returned",
            "requeued: worker silent for G",
            "kept: Start sent to a replaced daemon process, inside the handover grace",
            "requeued: Start sent to a replaced daemon process, after the handover grace",
        ],
    );
}

/// Catches, besides F4.1's: a lease lost to a reboot kept until G instead of requeued
/// on the first heartbeat after the handover grace (L3), a re-adopted lease requeued
/// (I3, I12), and a lease requeued twice or held twice (I3).
#[test]
fn f4_3_mass_reconnect() {
    let reach = sweep(Scenario::MassReconnect);
    assert_reached(&reach, &EVERY);
    assert_reached(
        &reach,
        &[
            "mass reconnect",
            "report wave",
            "requeued: Start sent to a replaced daemon process, after the handover grace",
            "full round (PLACEMENT_ROUND grants)",
        ],
    );
}

/// Catches, besides F4.1's: a grant to a cordoned worker (I8), a drain that gives up a
/// lease or states a false drain state (I9), and work only cordoned workers could run
/// refused (I10).
#[test]
fn f4_4_operator_storm() {
    let reach = sweep(Scenario::OperatorStorm);
    assert_reached(&reach, &EVERY);
    assert_reached(
        &reach,
        &[
            "pool maintenance",
            "nodes uncordoned while silent",
            "superseded grant committed",
            "waits for a cordon",
            "a wait for a cordon became refusable",
            "servable again",
            "drained",
            "paused",
            "draining",
        ],
    );
}

/// I15. Catches anything that depends on more than the seed.
#[test]
fn a_seed_replays() {
    let a = run(5, 300).trace();
    assert_eq!(a, run(5, 300).trace());
    assert_ne!(a, run(9, 300).trace());
}

/// Runs the seed in `KBF_SIM_SEED` with `KBF_SIM_OPS` operations (default 2,000).
#[test]
#[ignore = "replays one seed: KBF_SIM_SEED=<n> [KBF_SIM_OPS=<ops>]"]
fn replay() {
    let var = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|v| v.parse::<u64>().expect(name))
    };
    let seed = var("KBF_SIM_SEED").expect("set KBF_SIM_SEED");
    let ops = var("KBF_SIM_OPS").unwrap_or(CI_OPS);
    let world = run(seed, ops);
    println!(
        "seed {seed} ({:?}): drained in {} s; reached {:#?}",
        world.scenario, world.drained_in, world.check.reach
    );
}

#[test]
#[ignore = "long: run with --ignored --release"]
fn long_sweep() {
    let threads = std::thread::available_parallelism().map_or(4, usize::from);
    std::thread::scope(|s| {
        for k in 0..threads {
            s.spawn(move || {
                for seed in (k as u64..LONG_SEEDS).step_by(threads) {
                    run(seed, LONG_OPS);
                }
            });
        }
    });
}
