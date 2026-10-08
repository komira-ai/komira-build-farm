//! The rollout record's rules: every move the design allows, and no other.

use std::time::Duration;

use kbf_types::{
    Actor, IllegalStep, NodeProgress, NodeStep, Rollout, RolloutId, RolloutState, Selector,
    Strategy, StrategyError, WorkerId,
};

use NodeStep::{
    Cordoned, Done, Draining, Failed, Held, Pending, Qualifying, Quarantined, Rebooting, Updating,
};

const ALL: [NodeStep; 10] = [
    Pending,
    Cordoned,
    Draining,
    Updating,
    Rebooting,
    Qualifying,
    Done,
    Held,
    Quarantined,
    Failed,
];

/// The forward moves of `fleet-updates.md` section 4.2, written out independently of
/// the code: cordon, drain, update, reboot (or not), qualify.
const FORWARD: [(NodeStep, NodeStep); 7] = [
    (Pending, Cordoned),
    (Cordoned, Draining),
    (Draining, Updating),
    (Updating, Rebooting),
    (Updating, Qualifying),
    (Rebooting, Qualifying),
    (Qualifying, Done),
];

/// The steps from which a node may be quarantined (section 3.3): once it has been
/// handed its update.
const QUARANTINABLE: [NodeStep; 3] = [Updating, Rebooting, Qualifying];

/// A node brought to `step` by legal moves.
fn at(step: NodeStep) -> NodeProgress {
    let path: &[NodeStep] = match step {
        Pending => &[],
        Cordoned => &[Cordoned],
        Draining => &[Cordoned, Draining],
        Updating => &[Cordoned, Draining, Updating],
        Rebooting => &[Cordoned, Draining, Updating, Rebooting],
        Qualifying => &[Cordoned, Draining, Updating, Qualifying],
        Done => &[Cordoned, Draining, Updating, Qualifying, Done],
        Held => &[Cordoned, Held],
        Quarantined => &[Cordoned, Draining, Updating, Quarantined],
        Failed => &[Failed],
    };
    let mut p = NodeProgress::pending();
    for &s in path {
        p.advance(s).expect("a legal path");
    }
    assert_eq!(p.step(), step);
    p
}

/// Catches: any illegal node move allowed (skipping the drain, updating from pending,
/// leaving a final step, going backwards, quarantining a node never handed its
/// update, leaving quarantine but to give the node up), any legal one refused, and a
/// refused move that changes the node anyway.
#[test]
fn every_node_move_is_allowed_exactly_when_the_design_says() {
    for from in ALL.into_iter().filter(|s| *s != Held) {
        for to in ALL {
            let legal = match from {
                Quarantined => to == Failed,
                from if from.is_final() => false,
                from => {
                    FORWARD.contains(&(from, to))
                        || matches!(to, Held | Failed)
                        || (to == Quarantined && QUARANTINABLE.contains(&from))
                }
            };
            let mut p = at(from);
            let before = p;
            assert_eq!(p.allows(to), legal, "{from} -> {to}");
            match p.advance(to) {
                Ok(()) => {
                    assert!(legal, "{from} -> {to} was allowed");
                    assert_eq!(p.step(), to);
                }
                Err(e) => {
                    assert!(!legal, "{from} -> {to} was refused");
                    assert_eq!(e, IllegalStep::Node { from, to });
                    assert_eq!(p, before, "a refused move changed the node");
                }
            }
        }
    }
}

/// Catches: a held node that resumes anywhere but where it was held (it would skip its
/// drain or apply), one that cannot be given up, and `held_at` lost or kept wrongly.
#[test]
fn a_held_node_resumes_only_where_it_was_held_or_fails() {
    for held_at in [Pending, Cordoned, Draining, Updating, Rebooting, Qualifying] {
        let mut p = at(held_at);
        assert_eq!(p.held_at(), None);
        p.advance(Held).expect("any unfinished step may be held");
        assert_eq!((p.step(), p.held_at()), (Held, Some(held_at)));
        for to in ALL {
            let legal = to == held_at || to == Failed;
            assert_eq!(p.allows(to), legal, "held at {held_at} -> {to}");
            let mut q = p;
            assert_eq!(q.advance(to).is_ok(), legal, "held at {held_at} -> {to}");
            if legal {
                assert_eq!((q.step(), q.held_at()), (to, None));
            } else {
                assert_eq!(q, p);
            }
        }
    }
}

/// Catches: a final step counted as out of service (it would hold a slot forever), or
/// a held or quarantined node not counted (a broken node would free its slot, section
/// 4.3); and step names other than the design's (section 3.3: `updating`).
#[test]
fn out_of_service_means_started_and_not_finished() {
    for step in ALL {
        let out = !matches!(step, Pending | Done | Failed);
        assert_eq!(step.is_out(), out, "{step}");
        assert_eq!(step.is_final(), matches!(step, Done | Failed), "{step}");
    }
    let names: Vec<String> = ALL.iter().map(ToString::to_string).collect();
    assert_eq!(
        names,
        [
            "pending",
            "cordoned",
            "draining",
            "updating",
            "rebooting",
            "qualifying",
            "done",
            "held",
            "quarantined",
            "failed"
        ]
    );
}

fn rollout() -> Rollout {
    rollout_of(&["a", "b"], 1)
}

fn rollout_of(nodes: &[&str], max_unavailable: u32) -> Rollout {
    try_rollout(nodes, max_unavailable).expect("a valid strategy")
}

fn try_rollout(nodes: &[&str], max_unavailable: u32) -> Result<Rollout, StrategyError> {
    Rollout::new(
        RolloutId(7),
        "sha256:abc",
        Selector::Pools(vec!["linux-x86".to_owned()]),
        Strategy {
            max_unavailable,
            ..Strategy::default()
        },
        Actor::new("ci", 1_000),
        nodes.iter().map(|n| WorkerId::new(*n)),
    )
}

/// Catches: an illegal rollout move allowed (restarting a finished rollout, finishing
/// one that never ran, resuming a cancelled one), or a legal one refused.
#[test]
fn every_rollout_move_is_allowed_exactly_when_the_design_says() {
    use RolloutState::{Cancelled, Done, Held, Pending, Running};
    let all = [Pending, Running, Held, Done, Cancelled];
    let legal = [
        (Pending, Running),
        (Running, Held),
        (Running, Done),
        (Held, Running),
        (Pending, Cancelled),
        (Running, Cancelled),
        (Held, Cancelled),
    ];
    for from in all {
        for to in all {
            assert_eq!(
                from.allows(to),
                legal.contains(&(from, to)),
                "{from:?} -> {to:?}"
            );
        }
    }
    let mut r = rollout();
    assert_eq!(
        r.set_state(Done),
        Err(IllegalStep::Rollout {
            from: Pending,
            to: Done
        })
    );
    assert_eq!(r.state(), Pending);
    r.set_state(Running).expect("start");
    assert_eq!(r.state(), Running);
}

/// Catches: a rollout set done while a node is still pending, under way, held or
/// quarantined (the record would say every node is updated when it is not).
#[test]
fn a_rollout_is_done_only_when_every_node_is_done_or_failed() {
    let (a, b) = (WorkerId::new("a"), WorkerId::new("b"));
    let mut r = rollout_of(&["a", "b"], 2);
    r.set_state(RolloutState::Running).expect("start");
    assert_eq!(
        r.set_state(RolloutState::Done),
        Err(IllegalStep::Unfinished {
            node: a.clone(),
            step: Pending
        })
    );
    for step in [Cordoned, Draining, Updating, Qualifying, Done] {
        r.advance(&a, step).expect("a legal step");
    }
    for step in [Cordoned, Draining, Updating, Quarantined] {
        r.advance(&b, step).expect("a legal step");
        let before = r.clone();
        assert_eq!(
            r.set_state(RolloutState::Done),
            Err(IllegalStep::Unfinished {
                node: b.clone(),
                step
            })
        );
        assert_eq!(r, before, "a refused move changed the rollout");
    }
    r.advance(&b, Failed).expect("give b up");
    r.set_state(RolloutState::Done)
        .expect("every node is done or failed");
    assert_eq!(r.state(), RolloutState::Done);
}

/// Catches: `max_unavailable` left to the driver (the record would let a second caller,
/// or a driver bug, take more nodes out than the strategy allows), and held,
/// quarantined or under-way nodes not counted as out.
#[test]
fn the_record_takes_no_more_than_max_unavailable_nodes_out() {
    let (a, b, c) = (WorkerId::new("a"), WorkerId::new("b"), WorkerId::new("c"));
    let mut r = rollout_of(&["a", "b", "c"], 2);
    r.set_state(RolloutState::Running).expect("start");
    r.advance(&a, Cordoned).expect("first out");
    r.advance(&b, Cordoned).expect("second out");
    assert_eq!(r.out_of_service(), 2);
    let before = r.clone();
    assert_eq!(
        r.advance(&c, Cordoned),
        Err(IllegalStep::Unavailable { max_unavailable: 2 })
    );
    assert_eq!(r, before);
    // Out of service until done or failed, whatever the step in between.
    for step in [Draining, Updating, Quarantined] {
        r.advance(&a, step).expect("a legal step");
        assert!(r.advance(&c, Cordoned).is_err(), "{step} counted as back");
    }
    r.advance(&b, Held).expect("hold b");
    assert!(r.advance(&c, Cordoned).is_err(), "held counted as back");
    // A node that is given up frees its slot.
    r.advance(&a, Failed).expect("skip a");
    assert_eq!(r.out_of_service(), 1);
    r.advance(&c, Cordoned).expect("a slot is free");
    // Other moves of nodes already out are not limited.
    r.advance(&c, Draining).expect("c drains");
}

/// Catches: a rollout recorded with `max_unavailable` 0, which could never take a
/// node out and would sit running forever.
#[test]
fn a_strategy_with_no_node_out_is_refused() {
    let refused = try_rollout(&["a"], 0);
    assert_eq!(refused, Err(StrategyError::NoneUnavailable));
    assert_eq!(
        StrategyError::NoneUnavailable.to_string(),
        "max_unavailable is 0: the rollout could never take a node out"
    );
    assert!(try_rollout(&["a"], 1).is_ok());
}

/// Catches: a node held while still pending counted as out of service (it never left
/// placement, so it holds no slot, and the rollout could not take another node), and
/// a node held after it was taken out not counted.
#[test]
fn a_node_held_while_pending_holds_no_slot() {
    let (a, b) = (WorkerId::new("a"), WorkerId::new("b"));
    let mut r = rollout_of(&["a", "b"], 1);
    r.set_state(RolloutState::Running).expect("start");
    r.advance(&a, Held).expect("hold a before it is taken out");
    assert!(!r.node(&a).expect("a").is_out());
    assert_eq!(r.out_of_service(), 0);
    r.advance(&b, Cordoned).expect("b may go out");
    r.advance(&b, Held).expect("hold b once out");
    assert!(r.node(&b).expect("b").is_out());
    assert_eq!(r.out_of_service(), 1);
    // Resumed at pending, a still needs a free slot to go out.
    r.advance(&a, Pending).expect("resume a");
    assert_eq!(
        r.advance(&a, Cordoned),
        Err(IllegalStep::Unavailable { max_unavailable: 1 })
    );
}

/// Catches: nodes moved while the rollout is pending, done or cancelled; a held
/// rollout that lets nodes go on (nothing proceeds by itself) or forbids holding and
/// giving up; a node the rollout does not cover; and a refused move that changes it.
#[test]
fn only_a_running_rollout_moves_its_nodes() {
    let (a, ghost) = (WorkerId::new("a"), WorkerId::new("ghost"));
    let mut r = rollout();
    assert_eq!(
        r.advance(&a, Cordoned),
        Err(IllegalStep::NotRunning(RolloutState::Pending))
    );
    r.set_state(RolloutState::Running).expect("start");
    assert_eq!(
        r.advance(&ghost, Cordoned),
        Err(IllegalStep::NotCovered(ghost.clone()))
    );
    r.advance(&a, Cordoned).expect("cordon a");
    let before = r.clone();
    assert!(matches!(
        r.advance(&a, Updating),
        Err(IllegalStep::Node { .. })
    ));
    assert_eq!(r, before);
    r.advance(&a, Draining).expect("drain a");
    r.advance(&a, Updating).expect("update a");

    r.set_state(RolloutState::Held).expect("hold");
    assert_eq!(
        r.advance(&a, Rebooting),
        Err(IllegalStep::NotRunning(RolloutState::Held))
    );
    let mut quarantined = r.clone();
    quarantined
        .advance(&a, Quarantined)
        .expect("a held rollout quarantines a node");
    r.advance(&a, Held).expect("a held rollout holds a node");
    r.advance(&a, Failed).expect("and gives it up");
    assert_eq!(r.node(&a).map(|p| p.step()), Some(Failed));
    assert_eq!(r.node(&ghost), None);
    assert_eq!(r.nodes().len(), 2);

    r.set_state(RolloutState::Cancelled).expect("cancel");
    assert_eq!(
        r.advance(&WorkerId::new("b"), Held),
        Err(IllegalStep::NotRunning(RolloutState::Cancelled))
    );
}

/// Catches: defaults that differ from section 4.1, and a record that does not keep
/// what it was given.
#[test]
fn a_new_rollout_keeps_its_request_and_the_policy_defaults() {
    let r = rollout();
    assert_eq!(
        r.strategy,
        Strategy {
            max_unavailable: 1,
            canary: 1,
            soak: Duration::from_secs(7_200),
            drain_deadline: Duration::from_secs(1_800),
        }
    );
    assert_eq!(
        (r.id.to_string(), r.target.as_str()),
        ("rollout-7".to_owned(), "sha256:abc")
    );
    assert_eq!(
        r.actor,
        Actor {
            who: "ci".to_owned(),
            at_unix_ms: 1_000
        }
    );
    assert_eq!(r.selector, Selector::Pools(vec!["linux-x86".to_owned()]));
    assert_eq!(r.state(), RolloutState::Pending);
    assert!(r.nodes().values().all(|p| *p == NodeProgress::pending()));
    let messages = [
        IllegalStep::Node {
            from: Pending,
            to: Done,
        }
        .to_string(),
        IllegalStep::Rollout {
            from: RolloutState::Done,
            to: RolloutState::Running,
        }
        .to_string(),
        IllegalStep::NotRunning(RolloutState::Pending).to_string(),
        IllegalStep::NotCovered(WorkerId::new("x")).to_string(),
        IllegalStep::Unfinished {
            node: WorkerId::new("x"),
            step: Quarantined,
        }
        .to_string(),
        IllegalStep::Unavailable { max_unavailable: 2 }.to_string(),
    ];
    assert_eq!(
        messages,
        [
            "a node may not move from pending to done",
            "a rollout may not move from Done to Running",
            "a Pending rollout moves no node",
            "the rollout does not cover node x",
            "a rollout is done only when every node is done or failed; x is quarantined",
            "2 node(s) are already out of service, the most the rollout allows",
        ]
    );
}
