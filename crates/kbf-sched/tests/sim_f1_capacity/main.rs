//! Simulation family F1 of `docs/design/simulation.md`: capacity, packing and QoS order
//! under contention.
//!
//! The scheduler is fed directly in an in-process world (`world.rs`): every `Commit` is
//! fed straight back as committed, ticks are one second apart, and workers always
//! heartbeat and list what they run. Each scenario (`scenarios.rs`, F1.1 to F1.10) is a
//! generator over that world, swept over seeds. After every input the checker
//! (`check.rs`) compares the scheduler's effects and public state with a shadow written
//! from its contract: I1 to I7, I10, I11, I13 and I14 of the catalog, and L2. At the
//! end of each run it checks L1 (everything finished within the scenario's bound, every
//! waiter answered once, nothing booked or queued) and the F1 base world's rule that no
//! lease is given up. I15 is `seeds_replay`.
//!
//! L1's bound is each scenario's `horizon`, set with a wide margin (F1.4's is 23,200 s),
//! not the catalog's "backlog plus G plus one round". Here L1 proves that the work
//! finishes and that the world quiesces; how soon work is placed is checked by the
//! reference first fit every round (I11), not by L1.
//!
//! Each scenario also asserts that its sweep
//! reached the situations it is about, so a check that cannot fail shows up as a
//! failed sweep, not a green one.
//!
//! A failure names the scenario, the seed, the input step, the invariant and the
//! command that replays it:
//!
//! ```text
//! KBF_SIM_SCENARIO=F1.4 KBF_SIM_SEED=17 cargo test -p kbf-sched --test sim_f1_capacity -- --ignored --exact replay
//! ```
//!
//! The scenarios are run by name (`scenarios::by_name`), so the replay takes the
//! scenario as well as the seed. The CI sweep runs 4 to 48 seeds per scenario. The
//! whole file takes about 3.5 s of test time in a debug build on a quiet dev box (tests
//! in parallel), more on a loaded one, and about 12 s on a CI runner; `seeds_replay`
//! replays two scenarios, not all ten, to stay inside the catalog's 10 s budget. A
//! longer sweep is ignored (300 seeds of every scenario took 61 s in a release build on
//! the dev box):
//!
//! ```text
//! cargo test -p kbf-sched --release --test sim_f1_capacity -- --ignored --exact long_sweep
//! ```
//!
//! (`KBF_SIM_SEEDS=<n>` sets its seeds per scenario; the default is 200.)

mod check;
mod scenarios;
mod world;

use scenarios::{
    BatchUnderInteractive, CapacityShrink, Dedup, EachAxis, ExactPacking, Gpus, JoinAfterFinish,
    LargeBehindSmall, QosOrder, Rounds,
};
use world::{Scenario, run};

/// Runs `make()` once per seed in `seeds`, handing each finished scenario and world to
/// `each`.
fn sweep<S: Scenario>(
    seeds: std::ops::Range<u64>,
    make: impl Fn() -> S,
    mut each: impl FnMut(&S, &world::World),
) {
    for seed in seeds {
        let mut scenario = make();
        let world = run(&mut scenario, seed);
        each(&scenario, &world);
    }
}

#[test]
fn f1_1_exact_packing() {
    let mut full = 0;
    let mut axes = [0; 3];
    let mut exact = 0;
    sweep(0..48, ExactPacking::default, |s, w| {
        full += s.full_at_start;
        exact += w.check.stats.exact_fills;
        for (a, n) in axes.iter_mut().zip(s.extra_on_axis) {
            *a += n;
        }
    });
    assert_eq!(full, 48, "every seed fills its workers exactly at second 0");
    assert!(exact > 0);
    assert!(
        axes.iter().all(|&n| n > 0),
        "one unit more on each axis: {axes:?}"
    );
}

#[test]
fn f1_2_each_axis_full_on_its_own() {
    let (mut memory, mut cpu) = (0, 0);
    sweep(0..8, EachAxis::default, |s, _| {
        memory += s.memory_full_cpu_free;
        cpu += s.cpu_full_memory_free;
    });
    assert!(memory > 0, "no worker was full on memory with CPU free");
    assert!(cpu > 0, "no worker was full on CPU with memory free");
}

#[test]
fn f1_3_whole_gpus() {
    let (mut refused, mut grants) = (0, 0);
    sweep(0..16, Gpus::default, |s, _| {
        refused += s.refused;
        grants += s.gpu_grants;
    });
    assert!(refused > 0, "no request outgrew every worker's GPUs");
    assert!(grants > 0);
}

#[test]
fn f1_4_qos_order_when_saturated() {
    let (mut passed, mut skips, mut refusals, mut promotions) = (0, 0, 0, 0);
    sweep(0..8, QosOrder::default, |s, w| {
        passed += s.interactive_passed_older;
        skips += s.same_level_skips;
        refusals += w.check.stats.refusals;
        promotions += w.check.stats.promotions;
    });
    assert!(
        passed > 0,
        "no interactive operation passed older, less urgent work"
    );
    assert!(
        skips > 0,
        "no operation passed an older one of its level that did not fit"
    );
    assert!(refusals > 0, "the base world refused nothing");
    assert!(promotions > 0, "the swarm's dedup keys promoted nothing");
}

#[test]
fn f1_5_a_large_request_behind_small_ones() {
    let (mut passed, mut waited) = (0, 0);
    sweep(0..4, LargeBehindSmall::default, |s, _| {
        passed += s.passed_over;
        waited += s.waited_whole_stream;
    });
    // Today's behaviour, not a failure: the small ones pass it while they keep coming.
    assert!(passed > 0);
    assert!(
        waited > 0,
        "the large request never waited out the whole stream"
    );
}

#[test]
fn f1_6_batch_under_steady_interactive() {
    let mut waits = 0;
    sweep(0..4, BatchUnderInteractive::default, |s, _| {
        waits += s.batch_waits
    });
    assert!(waits > 0, "batch never waited behind interactive work");
}

#[test]
fn f1_7_dedup_and_promotion() {
    let (mut joins, mut promotions, mut unjoinable) = (0, 0, 0);
    let (mut other, mut networked, mut dnc) = (0, 0, 0);
    sweep(0..8, Dedup::default, |s, w| {
        joins += w.check.stats.joins;
        promotions += w.check.stats.promotions;
        unjoinable += w.check.stats.unjoinable_twins;
        other += s.other_instance;
        networked += s.networked;
        dnc += s.do_not_cache;
    });
    assert!(
        joins > 0 && promotions > 0,
        "joins {joins}, promotions {promotions}"
    );
    assert!(
        unjoinable > 0,
        "no networked or do_not_cache twin met an unfinished one"
    );
    assert!(other > 0 && networked > 0 && dnc > 0);
}

#[test]
fn f1_8_a_join_after_the_twin_finished() {
    let (mut answered, mut refused, mut rejoins) = (0, 0, 0);
    sweep(0..16, JoinAfterFinish::default, |s, w| {
        answered += s.after_answer;
        refused += s.after_refusal;
        rejoins += w.check.stats.rejoins_after_finish;
    });
    assert!(
        answered > 0 && refused > 0,
        "after an answer {answered}, a refusal {refused}"
    );
    assert_eq!(rejoins, answered + refused);
}

#[test]
fn f1_9_capacity_shrinks_below_bookings_then_grows() {
    let (mut shrunk, mut skips, mut grown) = (0, 0, 0);
    sweep(0..8, CapacityShrink::default, |s, w| {
        shrunk += s.shrunk_below_bookings;
        skips += w.check.stats.overbooked_skips;
        grown += s.grants_into_grown_room;
    });
    assert!(shrunk > 0, "no capacity fell below its bookings");
    assert!(skips > 0, "no request was kept off an overbooked worker");
    assert!(grown > 0, "no grant used room that grew in the same second");
}

#[test]
fn f1_10_more_than_one_round() {
    let (mut full, mut past_cut, mut unservable, mut refused) = (0, 0, 0, 0);
    sweep(0..4, Rounds::default, |s, w| {
        full += w.check.stats.full_rounds;
        past_cut += w.check.stats.verdicts_past_cut;
        unservable += s.unservable;
        refused += w.check.stats.refusals;
    });
    assert_eq!(full, 8, "two full rounds per seed");
    // Each unservable request is behind both full rounds' cuts.
    assert_eq!(past_cut, 2 * unservable, "verdicts checked past the cut");
    assert_eq!(
        refused, unservable,
        "every unservable request refused, nothing else"
    );
}

#[test]
fn seeds_replay() {
    // Two of the cheaper scenarios: one with dedup joins and promotions, one with
    // capacity changes. Every scenario runs on the same world and checker, so the
    // others replay by the same code; three runs of all ten would cost more than the
    // rest of the file.
    for name in ["F1.7", "F1.9"] {
        let hash = |seed| {
            let mut s = scenarios::by_name(name).unwrap();
            run(s.as_mut(), seed).trace_hash()
        };
        let three = hash(3);
        assert_eq!(three, hash(3), "{name}: seed 3 replays");
        assert_ne!(three, hash(4), "{name}: seeds 3 and 4 differ");
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Replays one seed of one scenario: `KBF_SIM_SCENARIO` and `KBF_SIM_SEED`.
#[test]
#[ignore = "replay: set KBF_SIM_SCENARIO and KBF_SIM_SEED"]
fn replay() {
    let name = env("KBF_SIM_SCENARIO").expect("set KBF_SIM_SCENARIO, e.g. F1.4");
    let seed: u64 = env("KBF_SIM_SEED")
        .expect("set KBF_SIM_SEED")
        .parse()
        .expect("KBF_SIM_SEED is a number");
    let mut scenario = scenarios::by_name(&name).expect("a scenario name, F1.1 to F1.10");
    let world = run(scenario.as_mut(), seed);
    println!(
        "{name} seed {seed}: {} inputs, {} grants, trace {:016x}",
        world.inputs,
        world.check.stats.grants,
        world.trace_hash()
    );
}

/// Many more seeds of every scenario than CI runs.
#[test]
#[ignore = "long: run with --ignored --release"]
fn long_sweep() {
    let seeds: u64 = env("KBF_SIM_SEEDS").map_or(200, |s| s.parse().expect("a number"));
    for name in scenarios::NAMES {
        for seed in 0..seeds {
            let mut scenario = scenarios::by_name(name).unwrap();
            run(scenario.as_mut(), seed);
        }
    }
}
