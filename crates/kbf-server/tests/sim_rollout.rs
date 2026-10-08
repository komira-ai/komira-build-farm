//! Simulation family F3, the rollout half (`docs/design/simulation.md` section 5, F3;
//! the catalog is komira-ai/komira-build-farm#139): the real [`RolloutDriver`] and
//! [`MemoryRolloutStore`] over a [`Fleet`] implemented on a `kbf_sched::Scheduler`
//! fed with virtual time, with running work, workers that heartbeat, reboot into
//! their update and come back, an operator, and an [`Applier`] that records each
//! hand-over and may refuse it. The driver steps at random seconds.
//!
//! The harness plays the slices after `updating` (not written yet): a node handed its
//! update goes down, comes back, and is recorded `rebooting`, `qualifying` and `done`,
//! then returned to placement.
//!
//! Checked after every input the scheduler is fed and every driver step:
//! - R1 (F3.5): at most `max_unavailable` nodes out of service by the record, and no
//!   more cordoned by the driver;
//! - R2: each node moves `pending -> cordoned -> draining -> updating -> rebooting ->
//!   qualifying -> done` in order, or to `held`;
//! - R3: a node is handed its update only when the record already says `updating`,
//!   the scheduler has it drained, it holds no lease, and it is up;
//! - R4 (F3.9): the driver cordons or drains a node only after the record says so, so
//!   a write the store refuses leaves the node untouched;
//! - R5: a held rollout moves no more, and the driver acts on no node after it;
//! - I8: no grant to a cordoned worker; I9: a drain never kills (a lease is given up
//!   only when its worker went down); each operation answered at most once, and every
//!   one answered once the run ends (L1).
//!
//! Scenarios: F3.3 (a node dies while draining), F3.5 (rollouts over mixed fleets of 4
//! to 12 nodes with `max_unavailable` 1 to 3), F3.6 (an operator uncordons a draining
//! node, cordons others, drains one the rollout has not reached, or uncordons a node
//! between the driver's two reads) and F3.9 (a refused hand-over, a failing store).
//!
//! A failing check prints its replay command (add `KBF_SIM_TRACE=1` and
//! `-- --nocapture` to print the trace):
//! `KBF_SIM_SCENARIO=<name> KBF_SIM_SEED=<n> cargo test -p kbf-server --test sim_rollout -- --ignored --exact replay`.
//! The long sweep: `cargo test -p kbf-server --release --test sim_rollout -- --ignored --exact rollouts_long`.

#[path = "sim_rollout/fleet.rs"]
mod fleet;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_sched::Event;
use kbf_server::rollout::{DriveError, RolloutDriver, RolloutStore, StoreError};
use kbf_sim::{Chance, SimRng};
use kbf_types::{
    Actor, NodeStep, OperationId, Rollout, RolloutState, Selector, Strategy, WorkerId,
};

use fleet::{Cell, FlakyStore, GRACE, ID, SimApplier, SimFleet};

const SEEDS: u64 = 64;
/// A scenario's knobs.
#[derive(Clone, Debug)]
struct Plan {
    nodes: usize,
    max_unavailable: u32,
    drain_deadline: u64,
    store_fails: Chance,
    refuse: Chance,
    /// A node of the rollout dies while draining, for longer than G (F3.3).
    die_while_draining: bool,
    /// The operator's acts (F3.6).
    operator: Operator,
    /// When the rollout part ends at the latest.
    cap: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operator {
    None,
    /// Uncordons the first node seen draining.
    UncordonDraining,
    /// Cordons the spare node (outside the rollout) and drains the last pending one.
    CordonOthers,
    /// Uncordons a node between the driver's read and its read again.
    BetweenReads,
}

struct Run {
    store: FlakyStore,
    cell: Cell,
    held_at: Option<(u64, Rollout)>,
}

/// Runs `plan` for `seed`: a rollout over every node but the last (the spare).
#[allow(clippy::too_many_lines)]
fn run(scenario: &'static str, seed: u64, plan: &Plan) -> Run {
    let mut rng = SimRng::from_seed(seed);
    let cell = RefCell::new(Cell::new((scenario, seed), plan.nodes));
    let store = FlakyStore::new(seed, plan.store_fails);
    let fleet = SimFleet {
        cell: &cell,
        store: &store,
        inject: RefCell::new(None),
    };
    let applier = SimApplier {
        cell: &cell,
        store: &store,
        refuse: RefCell::new((SimRng::from_seed(seed ^ 0xa991), plan.refuse)),
    };
    let driver = RolloutDriver::new(&store, &fleet, &applier);
    let covered: Vec<WorkerId> = cell.borrow().nodes[..plan.nodes - 1].to_vec();
    let spare = cell.borrow().nodes[plan.nodes - 1].clone();
    let strategy = Strategy {
        max_unavailable: plan.max_unavailable,
        drain_deadline: Duration::from_secs(plan.drain_deadline),
        ..Strategy::default()
    };
    let rollout = Rollout::new(
        ID,
        "sha256:set",
        Selector::Nodes(covered.clone()),
        strategy,
        Actor::new("operator", 0),
        covered.clone(),
    )
    .expect("a valid strategy");
    let max = usize::try_from(plan.max_unavailable).expect("small");
    let start_at = 30;
    let mut last: BTreeMap<WorkerId, NodeStep> = BTreeMap::new();
    let mut held_at: Option<(u64, Rollout)> = None;
    let mut calls_at_hold = 0;
    // Harness timers: per node, when it comes back, and when it is qualified.
    let mut back_at: BTreeMap<WorkerId, u64> = BTreeMap::new();
    let mut done_at: BTreeMap<WorkerId, u64> = BTreeMap::new();
    let mut handed_seen = 0;
    let mut died: Option<(WorkerId, u64)> = None;
    let mut operator_done = false;
    // Arrivals and driver steps stop once the rollout is done, held for a minute, or
    // out of time; then every node returns to placement and the work drains (L1).
    let mut stop: Option<u64> = None;
    let mut t = 0;
    while stop.is_none_or(|s| t < s + 900) {
        cell.borrow_mut().t = t;
        let active = stop.is_none();
        if active && rng.chance(Chance::percent(20)) {
            cell.borrow_mut().submit(&mut rng);
        }
        if t == start_at {
            driver.start(rollout.clone()).expect("starts");
            store.arm();
            if plan.operator == Operator::BetweenReads {
                let pick = usize::try_from(rng.below(covered.len() as u64)).expect("small");
                *fleet.inject.borrow_mut() = Some(covered[pick].clone());
            }
        }
        // The harness: nodes handed their update reboot, qualify and return.
        {
            let mut c = cell.borrow_mut();
            for (_, node) in c.handed.clone().into_iter().skip(handed_seen) {
                handed_seen += 1;
                if store
                    .inner
                    .update(ID, &|r| r.advance(&node, NodeStep::Rebooting))
                    .is_ok()
                {
                    c.down(&node);
                    back_at.insert(node, t + rng.between(5, 40));
                }
            }
            for (node, at) in back_at.clone() {
                if at == t {
                    back_at.remove(&node);
                    c.back(&node);
                    if store
                        .inner
                        .update(ID, &|r| r.advance(&node, NodeStep::Qualifying))
                        .is_ok()
                    {
                        done_at.insert(node, t + rng.between(1, 10));
                    }
                }
            }
            for (node, at) in done_at.clone() {
                if at == t {
                    done_at.remove(&node);
                    if store
                        .inner
                        .update(ID, &|r| r.advance(&node, NodeStep::Done))
                        .is_ok()
                    {
                        c.feed(Event::Uncordon { worker: node });
                    }
                }
            }
            if let Some((node, at)) = died.clone()
                && at == t
            {
                c.back(&node);
            }
        }
        // The operator.
        if t > start_at && !operator_done {
            let record = store.record();
            let mut c = cell.borrow_mut();
            let draining = covered
                .iter()
                .find(|n| record.node(n).map(|p| p.step()) == Some(NodeStep::Draining));
            match (plan.operator, draining) {
                (Operator::UncordonDraining, Some(node)) => {
                    c.feed(Event::Uncordon {
                        worker: node.clone(),
                    });
                    c.reach("operator uncordoned a draining node");
                    operator_done = true;
                }
                (Operator::CordonOthers, _) => {
                    c.feed(Event::Cordon {
                        worker: spare.clone(),
                    });
                    let pending = covered
                        .iter()
                        .rev()
                        .find(|n| record.node(n).map(|p| p.step()) == Some(NodeStep::Pending));
                    if let Some(node) = pending {
                        let deadline = c.now().saturating_add(Duration::from_secs(20));
                        c.feed(Event::Drain {
                            worker: node.clone(),
                            deadline,
                        });
                    }
                    operator_done = true;
                }
                _ => {}
            }
        }
        // A node dies while draining (F3.3).
        if plan.die_while_draining && died.is_none() && t > start_at {
            let record = store.record();
            let mut c = cell.borrow_mut();
            let draining = covered.iter().find(|n| {
                record.node(n).map(|p| p.step()) == Some(NodeStep::Draining)
                    && !c.sched.leases_on(n).is_empty()
            });
            if let Some(node) = draining.cloned() {
                c.down(&node);
                died = Some((node, t + GRACE + rng.between(5, 60)));
            }
        }
        cell.borrow_mut().second();
        if active && t > start_at && rng.chance(Chance::percent(20)) {
            match driver.step(ID) {
                Ok(_) => {}
                Err(DriveError::Store(StoreError::Unavailable(_))) => {
                    cell.borrow_mut().reach("store refused a write");
                }
                Err(e) => cell.borrow().fail("driver", &format!("{e}")),
            }
        }
        if t >= start_at {
            let record = store.record();
            let mut c = cell.borrow_mut();
            let out = record.out_of_service();
            let ours = covered
                .iter()
                .filter(|n| c.by_driver.contains(*n) && c.cordoned.contains(*n))
                .count();
            if out > max || ours > max {
                c.fail(
                    "R1",
                    &format!("{out} out by the record, {ours} cordoned by the driver; max {max}"),
                );
            }
            if out == max {
                c.reach("max_unavailable out at once");
                if c.cordoned.iter().any(|n| !c.by_driver.contains(n)) {
                    c.reach("max_unavailable out beside an operator's cordon");
                }
            }
            for (node, progress) in record.nodes() {
                let step = progress.step();
                let before = last.insert(node.clone(), step).unwrap_or(NodeStep::Pending);
                if !in_order(before, step) {
                    c.fail("R2", &format!("{node} moved {before} -> {step}"));
                }
            }
            if let Some((node, back)) = &died
                && t < *back
                && !c.up[node]
                && record.node(node).map(|p| p.step()) != Some(NodeStep::Draining)
            {
                c.fail(
                    "F3.3",
                    &format!("{node} left draining while down: {:?}", record.node(node)),
                );
            }
            match &held_at {
                None if record.state() == RolloutState::Held => {
                    held_at = Some((t, record));
                    calls_at_hold = c.driver_calls;
                    c.reach("rollout held");
                }
                Some((_, then)) if *then != record || c.driver_calls != calls_at_hold => {
                    c.fail(
                        "R5",
                        "a held rollout moved, or the driver acted after the hold",
                    );
                }
                _ => {}
            }
        }
        let record = store.inner.get(ID);
        let all_final = record.is_some_and(|r| r.nodes().values().all(|p| p.step().is_final()));
        let held_long = held_at.as_ref().is_some_and(|(h, _)| t >= h + 60);
        if active && t > start_at && (all_final || held_long || t >= plan.cap) {
            stop = Some(t);
            let mut c = cell.borrow_mut();
            for node in c.nodes.clone() {
                c.back(&node);
                c.feed(Event::Uncordon { worker: node });
            }
        }
        t += 1;
    }
    let cell = cell.into_inner();
    for op in 0..cell.submitted {
        if cell.answered.get(&OperationId(op)) != Some(&1) {
            let id = OperationId(op);
            let cordons: Vec<_> = cell.nodes.iter().map(|n| cell.sched.cordon(n)).collect();
            cell.fail(
                "L1",
                &format!(
                    "{id} answered {:?} times: {:?}, waiting {:?}; cordons {cordons:?}; up {:?}",
                    cell.answered.get(&id),
                    cell.sched.state(id),
                    cell.sched.waiting(id),
                    cell.up
                ),
            );
        }
    }
    Run {
        store,
        cell,
        held_at,
    }
}

/// Whether a node may be seen at `before` and then at `after`.
fn in_order(before: NodeStep, after: NodeStep) -> bool {
    use NodeStep::{Cordoned, Done, Draining, Held, Pending, Qualifying, Rebooting, Updating};
    let order = [
        Pending, Cordoned, Draining, Updating, Rebooting, Qualifying, Done,
    ];
    let rank = |s| order.iter().position(|o| *o == s);
    before == after
        || after == Held
        || matches!((rank(before), rank(after)), (Some(a), Some(b)) if b == a + 1)
}

fn plan(seed: u64) -> Plan {
    let mut d = SimRng::from_seed(seed ^ 0xf35);
    let nodes = usize::try_from(d.between(5, 13)).expect("small");
    let max_unavailable = u32::try_from(d.between(1, 3)).expect("small");
    let per_node = 350 / u64::from(max_unavailable);
    Plan {
        nodes,
        max_unavailable,
        drain_deadline: d.between(150, 300),
        store_fails: Chance::never(),
        refuse: Chance::never(),
        die_while_draining: false,
        operator: Operator::None,
        cap: 600 + nodes as u64 * per_node,
    }
}

fn finished(out: &Run) {
    let record = out.store.record();
    if let Some((node, p)) = record
        .nodes()
        .iter()
        .find(|(_, p)| p.step() != NodeStep::Done)
    {
        out.cell.fail(
            "liveness",
            &format!("{node} ended at {}: {record:?}", p.step()),
        );
    }
}

/// F3.5: rollouts over mixed fleets of 4 to 12 nodes, `max_unavailable` 1 to 3, with
/// running work and the driver stepping at random seconds. Every node is updated.
fn mixed_fleet(seed: u64) -> Run {
    let out = run("f3_5", seed, &plan(seed));
    finished(&out);
    out
}

/// F3.3: a node dies while draining and stays away past G. The driver keeps it at
/// `draining` while it is away and hands it its update once it is back, drained.
fn dies_while_draining(seed: u64) -> Run {
    let mut p = plan(seed);
    p.die_while_draining = true;
    p.drain_deadline = 900;
    let out = run("f3_3", seed, &p);
    finished(&out);
    out
}

/// F3.6: the operator acts during the rollout.
fn operator(seed: u64) -> Run {
    let mut p = plan(seed);
    p.operator = [
        Operator::UncordonDraining,
        Operator::CordonOthers,
        Operator::BetweenReads,
    ][usize::try_from(seed % 3).expect("small")];
    let out = run("f3_6", seed, &p);
    match p.operator {
        Operator::CordonOthers => finished(&out),
        Operator::UncordonDraining | Operator::BetweenReads => {
            let Some((_, record)) = &out.held_at else {
                out.cell.fail("F3.6", "the rollout was not held");
            };
            let held: Vec<_> = record
                .nodes()
                .iter()
                .filter(|(_, p)| p.step() == NodeStep::Held)
                .collect();
            let want = if p.operator == Operator::BetweenReads {
                NodeStep::Updating
            } else {
                NodeStep::Draining
            };
            if held.len() != 1 || held[0].1.held_at() != Some(want) {
                out.cell
                    .fail("F3.6", &format!("held {held:?}, want one held at {want}"));
            }
            if out.cell.handed.iter().any(|(_, n)| n == held[0].0) {
                out.cell.fail("F3.6", "the held node was handed its update");
            }
        }
        Operator::None => unreachable!(),
    }
    out
}

/// F3.9: hand-overs refused (even seeds: the node and the rollout are held), or a store
/// that fails a third of its writes (odd seeds: nothing is done for a failed write, and
/// the rollout still finishes).
fn refused_or_failing(seed: u64) -> Run {
    let mut p = plan(seed);
    if seed.is_multiple_of(2) {
        p.refuse = Chance::percent(50);
    } else {
        p.store_fails = Chance::percent(33);
    }
    let out = run("f3_9", seed, &p);
    if seed.is_multiple_of(2) {
        if out.cell.reached.contains_key("hand-over refused") {
            let Some((_, record)) = &out.held_at else {
                out.cell
                    .fail("F3.9", "a refused hand-over did not hold the rollout");
            };
            let held = record
                .nodes()
                .values()
                .filter(|p| p.held_at() == Some(NodeStep::Updating));
            if held.count() != 1 {
                out.cell
                    .fail("F3.9", &format!("not held at updating: {record:?}"));
            }
        }
    } else {
        finished(&out);
    }
    out
}

fn reached(outs: &[Run], what: &str) -> u64 {
    outs.iter()
        .map(|o| o.cell.reached.get(what).copied().unwrap_or(0))
        .sum()
}

/// Catches: `Rollout::advance` letting one node too many out; a driver that cordons
/// more than its slots; a hand-over to a node that is not drained.
#[test]
fn f3_5_rollout_over_a_mixed_fleet() {
    let outs: Vec<_> = (0..SEEDS).map(mixed_fleet).collect();
    assert!(reached(&outs, "max_unavailable out at once") > 0);
    let maxes: BTreeSet<u32> = (0..SEEDS).map(|s| plan(s).max_unavailable).collect();
    assert_eq!(
        maxes.len(),
        3,
        "the sweep covers max_unavailable 1 to 3: {maxes:?}"
    );
}

/// Catches: a drain that requeues; a hand-over to a disconnected node.
#[test]
fn f3_3_a_node_dies_while_draining() {
    let outs: Vec<_> = (0..SEEDS).map(dies_while_draining).collect();
    assert!(
        reached(
            &outs,
            "lease of a cordoned node given up after it went down"
        ) > 0
    );
}

/// Catches: a driver that hands over without reading the placement again; one that
/// moves on after an operator's uncordon; an operator's cordon counted as the
/// rollout's.
#[test]
fn f3_6_operator_acts_during_a_rollout() {
    let outs: Vec<_> = (0..SEEDS).map(operator).collect();
    for what in [
        "operator uncordoned a draining node",
        "uncordon between the driver's two reads",
        "max_unavailable out beside an operator's cordon",
        "rollout held",
    ] {
        assert!(reached(&outs, what) > 0, "never reached: {what}");
    }
}

/// Catches: a driver that acts on a node before the store kept the step.
#[test]
fn f3_9_refused_hand_over_or_failing_store() {
    let outs: Vec<_> = (0..SEEDS).map(refused_or_failing).collect();
    assert!(reached(&outs, "hand-over refused") > 0);
    assert!(reached(&outs, "store refused a write") > 0);
}

/// A scenario: a seed in, the finished run out.
type Scenario = fn(u64) -> Run;

const SCENARIOS: [(&str, Scenario); 4] = [
    ("f3_3", dies_while_draining),
    ("f3_5", mixed_fleet),
    ("f3_6", operator),
    ("f3_9", refused_or_failing),
];

#[test]
#[ignore = "long: run with --ignored --release"]
fn rollouts_long() {
    for (_, scenario) in SCENARIOS {
        for seed in 0..1_000 {
            scenario(seed);
        }
    }
}

#[test]
fn a_seed_replays() {
    for (name, scenario) in SCENARIOS {
        assert_eq!(scenario(5).cell.hash, scenario(5).cell.hash, "{name}");
    }
    assert_ne!(mixed_fleet(5).cell.hash, mixed_fleet(6).cell.hash);
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
        .expect("a scenario name: f3_3, f3_5, f3_6 or f3_9");
    let out = scenario(seed);
    println!("{name} seed {seed}: trace hash {:016x}", out.cell.hash);
}
