//! The rollout record's rules: every move the design allows, and no other.

use std::time::Duration;

use kbf_types::{
    IllegalStep, NodeProgress, NodeStep, Rollout, RolloutId, RolloutState, Selector, Strategy,
    WorkerId,
};

use NodeStep::{Applying, Cordoned, Done, Draining, Failed, Held, Pending, Qualifying, Rebooting};

const ALL: [NodeStep; 9] = [
    Pending, Cordoned, Draining, Applying, Rebooting, Qualifying, Done, Held, Failed,
];

/// The forward moves of `fleet-updates.md` section 4.2, written out independently of
/// the code: cordon, drain, apply, reboot (or not), qualify.
const FORWARD: [(NodeStep, NodeStep); 7] = [
    (Pending, Cordoned),
    (Cordoned, Draining),
    (Draining, Applying),
    (Applying, Rebooting),
    (Applying, Qualifying),
    (Rebooting, Qualifying),
    (Qualifying, Done),
];

/// A node brought to `step` by legal moves.
fn at(step: NodeStep) -> NodeProgress {
    let path: &[NodeStep] = match step {
        Pending => &[],
        Cordoned => &[Cordoned],
        Draining => &[Cordoned, Draining],
        Applying => &[Cordoned, Draining, Applying],
        Rebooting => &[Cordoned, Draining, Applying, Rebooting],
        Qualifying => &[Cordoned, Draining, Applying, Qualifying],
        Done => &[Cordoned, Draining, Applying, Qualifying, Done],
        Held => &[Cordoned, Held],
        Failed => &[Failed],
    };
    let mut p = NodeProgress::pending();
    for &s in path {
        p.advance(s).expect("a legal path");
    }
    assert_eq!(p.step(), step);
    p
}

/// Catches: any illegal node move allowed (skipping the drain, applying from pending,
/// leaving a final step, going backwards), any legal one refused, and a refused move
/// that changes the node anyway.
#[test]
fn every_node_move_is_allowed_exactly_when_the_design_says() {
    for from in ALL.into_iter().filter(|s| *s != Held) {
        for to in ALL {
            let legal =
                !from.is_final() && (FORWARD.contains(&(from, to)) || matches!(to, Held | Failed));
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
    for held_at in [Pending, Cordoned, Draining, Applying, Rebooting, Qualifying] {
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
/// a held node not counted (a broken node would free its slot, section 4.3).
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
            "applying",
            "rebooting",
            "qualifying",
            "done",
            "held",
            "failed"
        ]
    );
}

fn rollout() -> Rollout {
    Rollout::new(
        RolloutId(7),
        "sha256:abc",
        Selector::Pools(vec!["linux-x86".to_owned()]),
        Strategy::default(),
        "ci",
        [WorkerId::new("a"), WorkerId::new("b")],
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
        r.advance(&a, Applying),
        Err(IllegalStep::Node { .. })
    ));
    assert_eq!(r, before);

    r.set_state(RolloutState::Held).expect("hold");
    assert_eq!(
        r.advance(&a, Draining),
        Err(IllegalStep::NotRunning(RolloutState::Held))
    );
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
        (r.id.to_string(), r.target.as_str(), r.actor.as_str()),
        ("rollout-7".to_owned(), "sha256:abc", "ci")
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
    ];
    assert_eq!(
        messages,
        [
            "a node may not move from pending to done",
            "a rollout may not move from Done to Running",
            "a Pending rollout moves no node",
            "the rollout does not cover node x",
        ]
    );
}
