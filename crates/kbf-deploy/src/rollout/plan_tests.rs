use super::*;
use crate::rollout::fleet_tests::{commit, run_id};

fn plan(text: &str) -> Plan {
    let inventory = Inventory::parse(text).expect("inventory");
    Plan::new(&inventory, run_id("r1"), commit('b'))
}

fn lines(plan: &Plan) -> Vec<String> {
    plan.steps().iter().map(ToString::to_string).collect()
}

const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// Catches a plan that drifts from the design: lock, backup, then per host the gate,
/// stage, drain, wait, swap, health wait, (replication wait for storage only), confirm
/// and undrain, then unlock.
#[test]
fn a_plan_lists_every_step_in_order() {
    let p = plan("front frontend\ns1 storage-and-daemon\nm1 daemon-mac\n");
    assert_eq!(p.frontend().as_str(), "front");
    assert_eq!(p.hosts().len(), 3);
    assert_eq!(
        lines(&p),
        [
            "lock the fleet as run r1".to_owned(),
            "front: back up the state store".to_owned(),
            "s1: require no under-replicated blob".to_owned(),
            format!("s1: stage {B}"),
            "s1: drain".to_owned(),
            "s1: wait until nothing is in flight".to_owned(),
            format!("s1: swap to {B}"),
            format!("s1: wait until healthy at {B}"),
            "s1: wait until no blob is under-replicated".to_owned(),
            "s1: confirm the swap".to_owned(),
            "s1: undrain".to_owned(),
            "front: require no under-replicated blob".to_owned(),
            format!("front: stage {B}"),
            "front: drain".to_owned(),
            "front: wait until nothing is in flight".to_owned(),
            format!("front: swap to {B}"),
            format!("front: wait until healthy at {B}"),
            "front: confirm the swap".to_owned(),
            "front: undrain".to_owned(),
            "m1: require no under-replicated blob".to_owned(),
            format!("m1: stage {B}"),
            "m1: drain".to_owned(),
            "m1: wait until nothing is in flight".to_owned(),
            format!("m1: swap to {B}"),
            format!("m1: wait until healthy at {B}"),
            "m1: confirm the swap".to_owned(),
            "m1: undrain".to_owned(),
            "unlock the fleet".to_owned(),
        ]
    );
}

/// Catches the rollback step reading as another step: it is not in a plan, only a
/// failure prints it.
#[test]
fn the_rollback_step_prints_and_is_not_planned() {
    let p = plan("front frontend\n");
    let rollback = Step::Rollback {
        host: p.frontend().clone(),
    };
    assert_eq!(rollback.to_string(), "front: roll back");
    assert!(!p.steps().contains(&rollback));
}
