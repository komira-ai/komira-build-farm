//! Simulation family F3, the scheduler's half: platform routing, cordon and drain
//! together, including the last worker of a class (`docs/design/simulation.md`
//! section 5, F3; the catalog is komira-ai/komira-build-farm#139). The rollout half
//! (`RolloutDriver` over this scheduler) is `kbf-server`'s `tests/sim_rollout.rs`.
//!
//! The world (`f3_fleet/world.rs`): four workers in name order, `arm` (the only Linux
//! arm64 machine), `mac` (the only Mac, four cores, one Xcode build), `x86-a` and
//! `x86-b`; actions for seven platforms, one of which no worker ever satisfies; an
//! operator; workers that go down (losing their runs) and register again; the Mac's
//! Xcode builds changing under it. The control log is in process.
//!
//! After every input the scheduler is fed, the checker (`f3_fleet/check.rs`) compares
//! it with a reference model: I1 to I11 and I14 of the catalog, with L1 and L2 (the
//! list is in the checker's header). Each scenario below adds its own checks, and each
//! sweep asserts that it reached the situations it is for.
//!
//! Scenarios: F3.1 (the last worker of a platform cordoned), F3.2 (cordoned then
//! silent, and silent then back cordoned), F3.3 (a worker dies while draining; the
//! scheduler's side), F3.4 (a paused drain whose leases end later), F3.7 (a node
//! reboots while cordoned), F3.8 (capabilities change while cordoned), and a swarm that
//! turns every kind of act on or off per seed. F3.5, F3.6 and F3.9 need the rollout
//! driver and are in `kbf-server`.
//!
//! A failing check prints its replay command:
//! `KBF_SIM_SCENARIO=<name> KBF_SIM_SEED=<n> cargo test -p kbf-sched --test f3_fleet -- --ignored --exact replay`.
//! The long sweep: `cargo test -p kbf-sched --release --test f3_fleet -- --ignored --exact swarm_long`.

#[path = "f3_fleet/check.rs"]
mod check;
#[path = "f3_fleet/world.rs"]
mod world;

use kbf_sched::Cordon;
use kbf_sim::{Chance, SimRng};
use kbf_types::{OperationId, WorkerId};

use world::{
    ANY, ARM, Act, DARWIN, GRACE, LINUX, NEVER, NEW_XCODE, OLD_XCODE, Swarm, WAIT, World,
    XCODE_NEW, XCODE_OLD,
};

const SEEDS: u64 = 32;
const MS: u64 = 1_000;

/// The draws a scenario makes for its script: a stream apart from the world's.
fn draws(seed: u64) -> SimRng {
    SimRng::from_seed(seed ^ 0x00f3_5eed)
}

/// Submissions of every satisfiable platform in the background, no other random act.
fn background(until: u64) -> Swarm {
    Swarm {
        submit: Chance::percent(25),
        platforms: vec![ANY, LINUX, DARWIN, ARM, NEVER],
        run: (5, 40),
        until,
        ..Swarm::default()
    }
}

/// The operations for `platform` submitted in `from..to` (seconds).
fn ops_of(world: &World, platform: usize, from: u64, to: u64) -> Vec<OperationId> {
    world
        .platform
        .iter()
        .filter(|(id, p)| **p == platform && (from..to).contains(&submit_time(world, **id)))
        .map(|(id, _)| *id)
        .collect()
}

/// When operation `id` was submitted (seconds).
fn submit_time(world: &World, id: OperationId) -> u64 {
    let n = usize::try_from(id.0).expect("small");
    world
        .acts
        .iter()
        .filter(|(_, act, _)| matches!(act, Act::Submit(..)))
        .nth(n)
        .map(|(t, _, _)| *t)
        .expect("submitted")
}

fn arm() -> WorkerId {
    WorkerId::new("arm")
}

/// F3.1: the only arm64 Linux worker (and, in even seeds, the only Mac) cordoned for
/// longer than the unservable wait. Its work waits with the cordon reason and is never
/// refused; other platforms run; the uncordon itself places the waiting work.
fn last_of_class_cordoned(seed: u64) -> World {
    let mut d = draws(seed);
    let c = d.between(20, 60);
    let both = seed.is_multiple_of(2);
    let unc = c + WAIT + d.between(30, 200);
    let mut script = vec![
        (c, Act::Cordon("arm")),
        (c + 1, Act::Submit(ARM, 0, 30)),
        (c + 2, Act::Submit(ARM, 1, 30)),
        (unc, Act::Uncordon("arm")),
    ];
    if both {
        script.push((c, Act::Cordon("mac")));
        script.push((c + 3, Act::Submit(DARWIN, 0, 20)));
        script.push((unc, Act::Uncordon("mac")));
    }
    let w = World::new("f3_1", seed, script, background(unc + 10)).run(unc + 400, &mut |_| {});
    let seen = &w.check.seen;
    for id in ops_of(&w, ARM, c, unc) {
        if seen.refused.contains_key(&id) {
            w.check.fail(
                "F3.1",
                &format!("{id}, for the cordoned arm worker, refused"),
            );
        }
    }
    // The uncordon itself places what waited for arm (the reference checks exactly
    // which: arm is first in name order, so other work may take some of its room).
    let by_uncordon = ops_of(&w, ARM, c, unc).iter().any(|id| {
        seen.grants
            .get(id)
            .and_then(|g| g.first())
            .is_some_and(|(at, on, via)| *at == unc * MS && *on == arm() && *via)
    });
    if !by_uncordon {
        w.check.fail(
            "F3.1",
            &format!("the uncordon at {unc} s placed no work that waited for arm"),
        );
    }
    let ran_meanwhile = [ANY, LINUX].iter().any(|p| {
        ops_of(&w, *p, c, unc)
            .iter()
            .any(|id| seen.answered.get(id).is_some_and(|at| *at < unc * MS))
    });
    if !ran_meanwhile {
        w.check
            .fail("F3.1", "no other platform's work ran during the cordon");
    }
    if seen.longest_cordon_wait < WAIT * MS {
        w.check.fail(
            "F3.1",
            &format!(
                "the longest cordon wait was {} ms, under the unservable wait",
                seen.longest_cordon_wait
            ),
        );
    }
    w
}

/// F3.2: the last arm64 worker cordoned, then silent past G (even seeds); or silent,
/// then cordoned, then back (odd seeds). Its work becomes refusable when the worker
/// stops being live and its wait counts from then, not from the cordon: the refusal
/// comes exactly the unservable wait after that, or never if the worker is back
/// (cordoned) before then.
fn cordoned_then_silent(seed: u64) -> World {
    let mut d = draws(seed);
    let silent_first = !seed.is_multiple_of(2);
    let (script, down, up, cordon_at);
    if silent_first {
        let down_at = d.between(20, 60);
        let c = down_at + d.between(1, GRACE);
        let u = down_at + d.between(GRACE + 5, GRACE + 2 * WAIT);
        (down, up, cordon_at) = (down_at, u, c);
        script = vec![
            (down_at, Act::Down("arm")),
            (down_at + 1, Act::Submit(ARM, 0, 30)),
            (down_at + 2, Act::Submit(ARM, 0, 30)),
            (c, Act::Cordon("arm")),
            (u, Act::Up("arm")),
            (u + d.between(10, 60), Act::Uncordon("arm")),
        ];
    } else {
        let c = d.between(20, 60);
        let down_at = c + d.between(10, 120);
        let u = down_at + GRACE + WAIT + d.between(10, 100);
        (down, up, cordon_at) = (down_at, u, c);
        script = vec![
            (c, Act::Cordon("arm")),
            (c + 1, Act::Submit(ARM, 0, 30)),
            (c + 2, Act::Submit(ARM, 0, 30)),
            (down_at, Act::Down("arm")),
            (u, Act::Up("arm")),
            (u + d.between(10, 60), Act::Uncordon("arm")),
        ];
    }
    let mut last_heard = None;
    let mut w = World::new("f3_2", seed, script, background(up + 70)).run(up + 500, &mut |w| {
        if w.t == down {
            last_heard = w.heard.get("arm").copied();
        }
    });
    let not_live = last_heard.expect("probed") + GRACE;
    let refuse_at = not_live + WAIT;
    let watched = ops_of(&w, ARM, down.min(cordon_at), down.max(cordon_at) + 3);
    let mut any_refused = false;
    for id in watched {
        if submit_time(&w, id) >= not_live {
            continue;
        }
        let seen = &w.check.seen;
        let refused = seen.refused.get(&id).copied();
        any_refused |= refused.is_some();
        let want = (up > refuse_at).then_some(refuse_at * MS);
        if refused != want {
            w.check.fail(
                "F3.2",
                &format!(
                    "{id}: refused at {refused:?}, want {want:?} (cordoned at {cordon_at} s, \
                     last heard {} s, back at {up} s)",
                    not_live - GRACE
                ),
            );
        }
    }
    w.check.reach(if any_refused {
        "arm work refused after the silence"
    } else {
        "arm work waited through the silence"
    });
    w
}

/// F3.3, the scheduler's side: a worker dies while draining. Its leases are requeued
/// for its silence, at the first tick G after it was last heard, not for the drain;
/// the drain then reads drained, also once it is back.
fn dies_while_draining(seed: u64) -> World {
    let mut d = draws(seed);
    let c = d.between(20, 40);
    let down = c + d.between(5, 30);
    let up = down + GRACE + d.between(5, 100);
    let unc = up + d.between(10, 60);
    let script = vec![
        (1, Act::Submit(ARM, 0, 400)),
        (2, Act::Submit(ARM, 1, 400)),
        (c, Act::Drain("arm", 600)),
        (down, Act::Down("arm")),
        (up, Act::Up("arm")),
        (unc, Act::Uncordon("arm")),
    ];
    let mut last_heard = None;
    let mut drained_away = false;
    let w = World::new("f3_3", seed, script, background(unc + 10)).run(unc + 500, &mut |w| {
        if w.t == down {
            last_heard = w.heard.get("arm").copied();
        }
        if (down..unc).contains(&w.t) && last_heard.is_some_and(|l| w.t >= l + GRACE) {
            drained_away |= w.sched.cordon(&arm()) == Some(&Cordon::Drained);
            if w.sched.cordon(&arm()) != Some(&Cordon::Drained) {
                w.check.fail(
                    "F3.3",
                    &format!(
                        "arm is {:?} after its leases were requeued",
                        w.sched.cordon(&arm())
                    ),
                );
            }
        }
    });
    let expired = (last_heard.expect("probed") + GRACE) * MS;
    let lost: Vec<_> = w
        .check
        .seen
        .given_up
        .iter()
        .filter(|(_, _, on)| *on == arm())
        .collect();
    if lost.len() < 2 || lost.iter().any(|(at, _, _)| *at != expired) {
        w.check.fail(
            "F3.3",
            &format!("arm's leases given up at {lost:?}, want two at {expired} ms"),
        );
    }
    if !drained_away {
        w.check
            .fail("F3.3", "the drain never read drained while arm was away");
    }
    w
}

/// F3.4: a drain whose deadline passes before its leases end stays paused after they
/// end; a new drain then goes to drained at once (even seeds), or drains as its leases
/// end (odd seeds, a new drain while they still run).
fn paused_drain(seed: u64) -> World {
    let mut d = draws(seed);
    let deadline = d.between(5, 60);
    let after_end = seed.is_multiple_of(2);
    let redrain = if after_end {
        310 + d.between(1, 30)
    } else {
        20 + deadline + d.between(1, 80)
    };
    let unc = redrain + 400;
    let script = vec![
        (1, Act::Submit(ARM, 0, d.between(150, 300))),
        (2, Act::Submit(ARM, 0, d.between(150, 300))),
        (3, Act::Submit(ARM, 1, d.between(150, 300))),
        (20, Act::Drain("arm", deadline)),
        (redrain, Act::Drain("arm", 1_000)),
        (unc, Act::Uncordon("arm")),
    ];
    let mut swarm = background(unc + 10);
    swarm.platforms = vec![ANY, LINUX, DARWIN];
    let w = World::new("f3_4", seed, script, swarm).run(unc + 300, &mut |w| {
        if (20 + deadline..redrain).contains(&w.t)
            && !matches!(w.sched.cordon(&arm()), Some(Cordon::Paused { .. }))
        {
            w.check.fail(
                "F3.4",
                &format!("arm is {:?} before the new drain", w.sched.cordon(&arm())),
            );
        }
    });
    let after = w
        .acts
        .iter()
        .find(|(t, act, _)| *t == redrain && matches!(act, Act::Drain(..)))
        .map(|(_, _, c)| c.clone());
    let want_drained = after_end;
    if (after == Some(Some(Cordon::Drained))) != want_drained {
        w.check
            .fail("F3.4", &format!("after the new drain arm is {after:?}"));
    }
    w
}

/// F3.7: a node cordoned or draining reboots (short and long outages); it registers
/// again still cordoned and gets no grant until the uncordon.
fn reboot_while_cordoned(seed: u64) -> World {
    let mut d = draws(seed);
    let c = d.between(20, 60);
    let down = c + d.between(5, 30);
    let up = down + d.between(3, 2 * GRACE);
    let unc = up + d.between(20, 100);
    let cordon = if seed.is_multiple_of(2) {
        Act::Cordon("arm")
    } else {
        Act::Drain("arm", d.between(10, 300))
    };
    let script = vec![
        (c - 5, Act::Submit(ARM, 0, d.between(100, 200))),
        (c, cordon),
        (down, Act::Down("arm")),
        (down + 1, Act::Submit(ARM, 0, 30)),
        (up, Act::Up("arm")),
        (unc, Act::Uncordon("arm")),
    ];
    let mut back_cordoned = false;
    let mut w = World::new("f3_7", seed, script, background(unc + 10)).run(unc + 400, &mut |w| {
        if (up..unc).contains(&w.t) && w.sched.cordon(&arm()).is_none() {
            w.check.fail("F3.7", "arm registered again uncordoned");
        }
        back_cordoned |= w.t == up && w.sched.cordon(&arm()).is_some();
    });
    let granted_on_arm_between = w
        .check
        .seen
        .grants
        .values()
        .flatten()
        .any(|(at, on, _)| *on == arm() && (c * MS..unc * MS).contains(at));
    if granted_on_arm_between {
        w.check.fail("F3.7", "a grant to arm while it was cordoned");
    }
    if back_cordoned {
        w.check.reach(if up - down < GRACE {
            "registered again cordoned after an outage shorter than G"
        } else {
            "registered again cordoned after an outage of G or longer"
        });
    }
    w
}

/// F3.8: the Mac's Xcode builds change while it drains; after the uncordon matching
/// uses the new ones. Work for the new build waits for the cordon and runs on the Mac;
/// work for a build it no longer has is refused.
fn caps_change_while_cordoned(seed: u64) -> World {
    let mut d = draws(seed);
    let c = d.between(20, 40);
    let x = c + d.between(5, 40);
    let unc = x + d.between(10, 2 * WAIT);
    let sets: [&[&'static str]; 3] = [&[OLD_XCODE, NEW_XCODE], &[NEW_XCODE], &[OLD_XCODE]];
    let set = sets[usize::try_from(seed % 3).expect("small")];
    let script = vec![
        (1, Act::Submit(DARWIN, 0, d.between(30, 90))),
        (c, Act::Drain("mac", d.between(10, 120))),
        (x, Act::Xcodes(set.to_vec())),
        (x + 1, Act::Submit(XCODE_NEW, 0, 20)),
        (x + 2, Act::Submit(XCODE_NEW, 0, 20)),
        (x + 3, Act::Submit(XCODE_OLD, 0, 20)),
        (unc, Act::Uncordon("mac")),
    ];
    let mut w = World::new("f3_8", seed, script, background(unc + 10)).run(unc + 400, &mut |_| {});
    let seen = &w.check.seen;
    let mac = WorkerId::new("mac");
    let (mut ran, mut refused_lost) = (0, 0);
    for (platform, xcode) in [(XCODE_NEW, NEW_XCODE), (XCODE_OLD, OLD_XCODE)] {
        for id in ops_of(&w, platform, x + 1, x + 4) {
            let has = set.contains(&xcode);
            let ran_on_mac = seen
                .grants
                .get(&id)
                .is_some_and(|g| g.iter().all(|(at, on, _)| *on == mac && *at >= unc * MS))
                && seen.answered.contains_key(&id);
            let refused = seen.refused.contains_key(&id);
            let ok = if has { ran_on_mac } else { refused };
            ran += u64::from(has && ran_on_mac);
            refused_lost += u64::from(!has && refused);
            if !ok {
                w.check.fail(
                    "F3.8",
                    &format!("{id} wants Xcode {xcode}, the Mac has {set:?}: grants {:?}, refused {refused}", seen.grants.get(&id)),
                );
            }
        }
    }
    if ran > 0 {
        w.check
            .reach("work for a build the Mac has ran there after the uncordon");
    }
    if refused_lost > 0 {
        w.check.reach("work for a build the Mac lost refused");
    }
    if ran + refused_lost > 0 {
        w.check.reach(match seed % 3 {
            0 => "Xcode builds changed to old and new",
            1 => "Xcode builds changed to new only",
            _ => "Xcode builds changed to old only",
        });
    }
    w
}

/// Every kind of act, each on or off and at a rate the seed draws.
fn swarm(seed: u64) -> World {
    let mut d = draws(seed);
    let mut rate = |on: u64, lo: u32, hi: u32| {
        if d.below(100) < on {
            Chance::per_million(
                u32::try_from(d.between(u64::from(lo), u64::from(hi))).expect("small"),
            )
        } else {
            Chance::never()
        }
    };
    let swarm = Swarm {
        submit: rate(100, 100_000, 300_000),
        // Weighted so the Mac, four cores for three platforms, keeps up.
        platforms: vec![
            ANY, ANY, LINUX, LINUX, ARM, ARM, NEVER, DARWIN, XCODE_NEW, XCODE_OLD,
        ],
        run: (5, 30),
        operator: rate(85, 10_000, 50_000),
        outage: rate(70, 3_000, 15_000),
        xcodes: rate(50, 2_000, 10_000),
        until: 900,
    };
    World::new("swarm", seed, Vec::new(), swarm).run(900 + 2 * GRACE + WAIT + 600, &mut |_| {})
}

/// A scenario: a seed in, the finished world out.
type Scenario = fn(u64) -> World;

const SCENARIOS: [(&str, Scenario); 7] = [
    ("f3_1", last_of_class_cordoned),
    ("f3_2", cordoned_then_silent),
    ("f3_3", dies_while_draining),
    ("f3_4", paused_drain),
    ("f3_7", reboot_while_cordoned),
    ("f3_8", caps_change_while_cordoned),
    ("swarm", swarm),
];

/// Runs `scenario` over `seeds`; each run checks itself as it goes.
fn sweep(scenario: fn(u64) -> World, seeds: std::ops::Range<u64>) -> Vec<World> {
    seeds.map(scenario).collect()
}

fn reached(worlds: &[World], what: &str) -> u64 {
    worlds
        .iter()
        .map(|w| w.check.seen.reached.get(what).copied().unwrap_or(0))
        .sum()
}

/// Catches: a cordon ignored by placement; a cordon that refuses work; an uncordon
/// that leaves work for the next tick.
#[test]
fn f3_1_last_worker_of_a_platform_cordoned() {
    let worlds = sweep(last_of_class_cordoned, 0..SEEDS);
    assert!(reached(&worlds, "work waits for a cordon") > 0);
}

/// Catches: a wait that keeps its start across a cordon; a verdict that counts a
/// silent cordoned worker as one that could run the work.
#[test]
fn f3_2_cordoned_then_silent_and_the_reverse() {
    let worlds = sweep(cordoned_then_silent, 0..SEEDS);
    // Both answers occur: refused after the silence, and back (cordoned) in time.
    assert!(reached(&worlds, "arm work refused after the silence") > 0);
    assert!(reached(&worlds, "arm work waited through the silence") > 0);
}

/// Catches: a drain that requeues its leases; leases kept on a silent worker.
#[test]
fn f3_3_a_worker_dies_while_draining() {
    let worlds = sweep(dies_while_draining, 0..SEEDS);
    assert!(
        reached(
            &worlds,
            "lease of a cordoned worker given up after it went down"
        ) > 0
    );
}

/// Catches: a paused drain that moves to drained by itself; a deadline off by one.
#[test]
fn f3_4_a_paused_drain_whose_leases_end_later() {
    let worlds = sweep(paused_drain, 0..SEEDS);
    assert!(reached(&worlds, "paused") > 0 && reached(&worlds, "drained") > 0);
}

/// Catches: a cordon tied to the session, lost when the node registers again.
#[test]
fn f3_7_a_node_reboots_while_cordoned() {
    let worlds = sweep(reboot_while_cordoned, 0..SEEDS);
    for what in [
        "registered again cordoned after an outage shorter than G",
        "registered again cordoned after an outage of G or longer",
    ] {
        assert!(reached(&worlds, what) > 0, "F3.7 never reached {what:?}");
    }
}

/// Catches: capabilities a resent `Hello` does not update; a matching memo that
/// serves one platform's matches to another.
#[test]
fn f3_8_capabilities_change_while_cordoned() {
    let worlds = sweep(caps_change_while_cordoned, 0..SEEDS);
    for what in [
        "Xcode builds changed to old and new",
        "Xcode builds changed to new only",
        "Xcode builds changed to old only",
        "work for a build the Mac has ran there after the uncordon",
        "work for a build the Mac lost refused",
    ] {
        assert!(reached(&worlds, what) > 0, "F3.8 never reached {what:?}");
    }
}

/// The swarm reaches every situation the family is for.
#[test]
fn f3_swarm() {
    let worlds = sweep(swarm, 0..SEEDS);
    for what in [
        "cordoned",
        "draining",
        "drained",
        "paused",
        "work waits for a cordon",
        "work waits unservable",
        "refusal",
        "lease of a cordoned worker given up after it went down",
    ] {
        assert!(
            reached(&worlds, what) > 0,
            "the swarm never reached {what:?}"
        );
    }
    let by_uncordon = worlds
        .iter()
        .flat_map(|w| w.check.seen.grants.values().flatten())
        .any(|(_, _, via)| *via);
    assert!(by_uncordon, "no uncordon ever placed work");
}

#[test]
#[ignore = "long: run with --ignored --release"]
fn swarm_long() {
    sweep(swarm, 0..2_000);
}

/// One seed replays to the same trace; another differs. The swarm alone: it turns on
/// every kind of act the scripted scenarios use, on the same world and checker, and
/// replaying every scenario twice cost more than the rest of the file.
#[test]
fn a_seed_replays() {
    let seven = swarm(7).hash;
    assert_eq!(seven, swarm(7).hash);
    assert_ne!(seven, swarm(8).hash);
}

/// Replays one scenario and seed: `KBF_SIM_SCENARIO` and `KBF_SIM_SEED`.
#[test]
#[ignore = "replay: set KBF_SIM_SCENARIO and KBF_SIM_SEED"]
fn replay() {
    let name = std::env::var("KBF_SIM_SCENARIO").expect("KBF_SIM_SCENARIO");
    let seed: u64 = std::env::var("KBF_SIM_SEED")
        .expect("KBF_SIM_SEED")
        .parse()
        .expect("a number");
    let (_, scenario) = SCENARIOS
        .iter()
        .find(|(n, _)| *n == name)
        .expect("a scenario name: f3_1, f3_2, f3_3, f3_4, f3_7, f3_8 or swarm");
    let w = scenario(seed);
    println!("{name} seed {seed}: trace hash {:016x}", w.hash);
}
