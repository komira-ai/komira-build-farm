//! The client's orders: what it accepts, what it refuses, what it writes, and the gate.
//! Each test names the defect it catches; the planted mutant for each is listed in the
//! pull request.

use std::cell::Cell;
use std::fs;

use super::harness::{NO_LEASES, order, rig_with_helper, status_json, why};
use super::*;
use crate::fixtures::commit;

fn order_file(rig: &super::harness::Rig, id: u64) -> std::path::PathBuf {
    rig.inbox().join("orders").join(format!("{id:020}.json"))
}

/// Catches an activation accepted while a lease is live (running, or finished with a
/// Result the server has not acknowledged: the daemon counts both in `live`), and one
/// that is accepted when the count moves between the first look and the write: nothing is
/// written and the reason is stated. (Mutant: no check; or only the first look.)
#[tokio::test]
async fn an_activation_with_a_lease_live_is_refused() {
    let target = commit(9);
    let rig = rig_with_helper("live", "", &target);
    let busy: &dyn Fn() -> usize = &|| 1;
    let decision = rig
        .handle
        .order(&order(1, Verb::Activate, &target), 7, busy);
    assert_eq!(why(&decision), "1 leases are live; not activating");
    assert_eq!(rig.orders(), Vec::<String>::new());
    assert_eq!(rig.handle.gate(), None);
    // Refused at the first look: the id was not recorded.
    assert!(!rig.dir.join("client.json").exists());
    assert_eq!(rig.rx.borrow().acked_order_id, 0);
    assert_eq!(rig.rx.borrow().state, State::Refused);

    // Quiet at the first look, busy at the second.
    let looks = Cell::new(0);
    let moving = || {
        looks.set(looks.get() + 1);
        looks.get() - 1
    };
    let decision = rig
        .handle
        .order(&order(2, Verb::Activate, &target), 7, &moving);
    assert_eq!(why(&decision), "1 leases are live; not activating");
    assert_eq!(rig.orders(), Vec::<String>::new());
    assert_eq!(rig.handle.gate(), None);

    // With none live it is accepted, and the gate closes.
    assert_eq!(rig.ask(3, Verb::Activate, &target), Decision::Accepted);
    assert_eq!(rig.orders(), vec![format!("{:020}.json", 3)]);
    assert_eq!(
        rig.handle.gate(),
        Some(format!("update to {} ordered", &target[..12]))
    );
    let status = rig.rx.borrow().clone();
    assert_eq!(status.acked_order_id, 3);
    assert!(status.gated);
    assert_eq!(status.refusal, "");
}

/// Catches the order file's content: fixed JSON, no path or command; and a confirmation
/// that is held to the lease check or closes the gate.
#[tokio::test]
async fn the_order_file_is_the_frozen_json() {
    let target = commit(9);
    let rig = rig_with_helper("order-file", &target, &target);
    let busy: &dyn Fn() -> usize = &|| 5;
    let decision = rig
        .handle
        .order(&order(12, Verb::Confirm, &target), 7, busy);
    assert_eq!(
        decision,
        Decision::Accepted,
        "a confirmation takes no lease check"
    );
    let text = fs::read_to_string(order_file(&rig, 12)).expect("order file");
    let expected = format!(
        "{{\"order_id\":12,\"epoch\":7,\"verb\":\"confirm\",\"commit\":\"{target}\",\"window_s\":600}}"
    );
    assert_eq!(text, expected);
    assert_eq!(rig.handle.gate(), None, "a confirmation opens no gate");
}

/// Catches a revert refused for live leases, a revert that leaves the gate closed, a gate
/// that opens on a stale word of the helper (the status file says "reverted" from an
/// earlier try, which proves nothing about this order), and one that opens when the
/// helper's word cannot be read. (Mutant: lift on state alone.)
#[tokio::test]
async fn the_gate_opens_when_the_helper_settles_the_order() {
    let target = commit(9);
    let rig = rig_with_helper("gate", &commit(8), &target);
    // A word left over from an earlier update: reverted at order 1.
    rig.write_status(&status_json("reverted", &commit(8), &target, 1));
    assert_eq!(rig.ask(2, Verb::Activate, &target), Decision::Accepted);
    rig.handle.refresh();
    assert!(
        rig.handle.gate().is_some(),
        "an old word does not open the gate"
    );
    // The helper reports activated: still closed.
    rig.write_status(&status_json("activated_unconfirmed", &target, "", 2));
    assert!(rig.handle.gate().is_some());
    // No word at all: still closed.
    fs::remove_file(rig.dir.join("status.json")).expect("remove");
    rig.handle.refresh();
    assert!(rig.handle.gate().is_some());
    // The helper refused order 2 and this process stays up: open.
    rig.write_status(&status_json("refused", &commit(8), &target, 2));
    assert_eq!(rig.handle.gate(), None);
    assert!(!rig.rx.borrow().gated);

    // A reverted word for order 3 opens it too; a revert order opens it at once, with
    // leases live.
    assert_eq!(rig.ask(3, Verb::Activate, &target), Decision::Accepted);
    rig.write_status(&status_json("reverted", &commit(8), &target, 3));
    assert_eq!(rig.handle.gate(), None);
    assert_eq!(rig.ask(4, Verb::Activate, &target), Decision::Accepted);
    assert!(rig.handle.gate().is_some());
    let busy: &dyn Fn() -> usize = &|| 3;
    let revert = rig
        .handle
        .order(&order(5, Verb::Revert, &commit(8)), 7, busy);
    assert_eq!(revert, Decision::Accepted);
    assert_eq!(rig.handle.gate(), None);
}

/// Catches an order that is not replayed safely: the same id again is answered and
/// writes nothing (the file is not rewritten); one below the newest, id 0 and one of
/// another epoch are refused. (Mutant: no floor; no dedup, which rewrites the file.)
#[tokio::test]
async fn an_order_is_deduplicated_and_ordered() {
    let target = commit(9);
    let rig = rig_with_helper("floor", "", &target);
    let path = order_file(&rig, 5);
    assert_eq!(rig.ask(5, Verb::Activate, &target), Decision::Accepted);
    fs::write(&path, "marker").expect("mark");
    let busy: &dyn Fn() -> usize = &|| 9;
    let replay = rig
        .handle
        .order(&order(5, Verb::Activate, &target), 7, busy);
    assert_eq!(
        replay,
        Decision::Replayed,
        "a replay is answered even with leases live"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("file"),
        "marker",
        "and not written again"
    );
    assert_eq!(rig.orders().len(), 1);
    assert_eq!(rig.rx.borrow().acked_order_id, 5);
    assert_eq!(replay.refusal(), None);

    let cases = [
        (4, 7, "order id 4 is not above the newest accepted, 5"),
        (0, 7, "order id 0 is not above the newest accepted, 5"),
        (6, 6, "order 6 is of epoch 6; this session's is 7"),
        (6, 8, "order 6 is of epoch 8; this session's is 7"),
    ];
    for (id, epoch, expected) in cases {
        let mut o = order(id, Verb::Activate, &target);
        o.epoch = epoch;
        assert_eq!(why(&rig.handle.order(&o, 7, NO_LEASES)), expected);
        assert_eq!(rig.orders().len(), 1, "{expected}");
    }
    // Refused orders do not raise the floor: 6 is still welcome.
    assert_eq!(rig.ask(6, Verb::Confirm, &target), Decision::Accepted);
}

/// Catches an order replayed after a restart being refused or rewritten, and the floor
/// being forgotten: it lives in the state file.
#[tokio::test]
async fn the_order_floor_survives_a_restart() {
    let target = commit(9);
    let rig = rig_with_helper("floor-restart", "", &target);
    assert_eq!(rig.ask(7, Verb::Activate, &target), Decision::Accepted);
    let rig = rig.restart();
    assert_eq!(rig.rx.borrow().acked_order_id, 7);
    assert_eq!(rig.ask(7, Verb::Activate, &target), Decision::Replayed);
    assert_eq!(
        why(&rig.ask(6, Verb::Activate, &target)),
        "order id 6 is not above the newest accepted, 7"
    );
    assert_eq!(rig.orders().len(), 1);

    // The id was recorded but the file never written (a crash between the two): the
    // same order is written now, not answered as if it had been.
    fs::remove_file(order_file(&rig, 7)).expect("remove");
    let rig = rig.restart();
    assert_eq!(rig.rx.borrow().acked_order_id, 0);
    assert_eq!(rig.ask(7, Verb::Activate, &target), Decision::Accepted);
    assert_eq!(rig.orders().len(), 1);
}

/// Catches an order for a release this host has never held being written, an order
/// without a verb, and one on a host with no helper (or one whose status cannot be read).
#[tokio::test]
async fn an_order_needs_a_release_the_helper_holds() {
    let held = commit(9);
    let rig = rig_with_helper("holds", &commit(8), &held);
    for wanted in [commit(7), "9".repeat(12)] {
        let decision = rig.ask(1, Verb::Activate, &wanted);
        assert_eq!(
            why(&decision),
            format!("commit {wanted} is neither staged nor installed on this host")
        );
    }
    // The installed one counts, and so does the previous one.
    assert_eq!(rig.ask(1, Verb::Confirm, &commit(8)), Decision::Accepted);
    let previous = format!("\"previous_commit\":\"{}\"", commit(6));
    let status = status_json("idle", &commit(8), "", 0);
    rig.write_status(&status.replace("\"previous_commit\":null", &previous));
    assert_eq!(rig.ask(2, Verb::Revert, &commit(6)), Decision::Accepted);

    let mut no_verb = order(3, Verb::Confirm, &held);
    no_verb.verb = None;
    assert_eq!(
        why(&rig.handle.order(&no_verb, 7, NO_LEASES)),
        "order 3 names no verb"
    );

    fs::remove_file(rig.dir.join("status.json")).expect("remove");
    let decision = rig.ask(4, Verb::Confirm, &held);
    assert_eq!(
        why(&decision),
        "no update helper reports on this host, so it cannot take an order"
    );
    assert_eq!(rig.rx.borrow().state, State::Refused);
}

/// Catches an order accepted when its id or file cannot be recorded: nothing the helper
/// could act on is left behind without the id.
#[tokio::test]
async fn an_order_that_cannot_be_recorded_is_refused() {
    let target = commit(9);
    let rig = rig_with_helper("unrecordable-order", "", &target);
    fs::create_dir(rig.dir.join("client.json.tmp")).expect("block");
    let decision = rig.ask(1, Verb::Activate, &target);
    assert!(
        why(&decision).starts_with("cannot record order 1: "),
        "{decision:?}"
    );
    assert_eq!(rig.orders(), Vec::<String>::new());
    fs::remove_dir(rig.dir.join("client.json.tmp")).expect("unblock");

    fs::remove_dir_all(rig.inbox().join("orders")).expect("remove");
    let decision = rig.ask(2, Verb::Activate, &target);
    assert!(
        why(&decision).starts_with("cannot write order 2: "),
        "{decision:?}"
    );
    assert_eq!(rig.handle.gate(), None, "no gate without an order");
}
