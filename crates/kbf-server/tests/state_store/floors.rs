//! What a memory floor (the least memory a fresh submission of an action is booked at,
//! once a run of it passed its own limit) leaves in the state store, and what a farm that
//! takes the store over offers a fresh submission. Two or more farms run over one store,
//! each holding the next writer generation.

use super::fate::{booked, out_of_memory, register_big};
use super::*;

/// Submits action `tag` at 4 GiB, kills its run for memory (the farm raises the booking to
/// 16 GiB and the floor with it), and finishes the rerun. The floor outlives the
/// operation.
async fn raise(farm: &TestFarm, inbox: &mut Inbox, tag: u8) {
    let request = Request {
        resources: Resources::new(1_000, 4 * GIB),
        ..request(tag)
    };
    submit(farm, request).expect("submitted");
    let [(lease, memory)] = booked(inbox)[..] else {
        panic!("one lease expected for action {tag}");
    };
    assert_eq!(memory, 4 * GIB, "action {tag} was floored before its kill");
    let ack = farm.report(&node(), out_of_memory(lease)).await;
    assert!(ack.expect("an acknowledgement").accepted);
    farm.tick().await;
    let [(again, memory)] = booked(inbox)[..] else {
        panic!("one rerun expected for action {tag}");
    };
    assert_eq!(memory, 16 * GIB, "action {tag} was not raised");
    assert!(accepted(farm, again).await);
}

/// The memory a fresh submission of action `tag`, asking 4 GiB, is booked at.
fn offered(farm: &TestFarm, inbox: &mut Inbox, tag: u8) -> u64 {
    let request = Request {
        resources: Resources::new(1_000, 4 * GIB),
        ..request(tag)
    };
    submit(farm, request).expect("submitted");
    let [(_, memory)] = booked(inbox)[..] else {
        panic!("one lease expected for action {tag}");
    };
    memory
}

/// The farm that takes the store over from `old`, keeping at most `bound` floors, with
/// its node registered.
fn handover(disk: &Disk, old: &TestFarm, bound: usize) -> (TestFarm, Inbox) {
    old.fence();
    let successor = disk.generation_farm_bounded(bound);
    let (_, inbox) = register_big(&successor);
    (successor, inbox)
}

/// Catches: a floor that lives only in the old process. An action killed for memory at
/// 4 GiB, finished at 16 GiB and submitted again after the handover is booked at 16 GiB
/// by the successor, not at 4 GiB (one more kill per action at every handover); and an
/// action never killed is not floored. Mutant: the floor row not written.
#[tokio::test(start_paused = true)]
async fn a_floor_is_offered_to_a_fresh_submission_after_a_handover() {
    let disk = Disk::new();
    let old = disk.generation_farm();
    let (_, mut inbox) = register_big(&old);
    raise(&old, &mut inbox, 1).await;
    assert_eq!(disk.rows("floor/"), 1, "a raised floor left no row");

    let (successor, mut inbox) = handover(&disk, &old, kbf_sched::MEMORY_FLOORS);
    assert_eq!(offered(&successor, &mut inbox, 1), 16 * GIB, "no floor");
    assert_eq!(
        offered(&successor, &mut inbox, 2),
        4 * GIB,
        "a floor for all"
    );
}

/// Catches: the same, when the old process dies without a handover.
#[tokio::test(start_paused = true)]
async fn a_floor_is_offered_to_a_fresh_submission_after_a_crash() {
    let mut disk = Disk::new();
    let old = disk.generation_farm();
    let (_, mut inbox) = register_big(&old);
    raise(&old, &mut inbox, 1).await;

    disk.crash(old);
    let successor = disk.generation_farm();
    let (_, mut inbox) = register_big(&successor);
    assert_eq!(offered(&successor, &mut inbox, 1), 16 * GIB, "no floor");
}

/// Catches: a floor raised again that leaves a second row or the old ask: a rerun killed
/// at 16 GiB raises the floor to 32 GiB, and the successor offers 32, from one row.
#[tokio::test(start_paused = true)]
async fn a_floor_raised_again_replaces_its_row() {
    let disk = Disk::new();
    let old = disk.generation_farm();
    let (_, mut inbox) = register_big(&old);
    raise(&old, &mut inbox, 1).await;
    // The fresh submission is floored at 16 GiB, killed there and raised to 32 GiB.
    submit(&old, request(1)).expect("submitted");
    let [(lease, memory)] = booked(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert_eq!(memory, 16 * GIB);
    let ack = old.report(&node(), out_of_memory(lease)).await;
    assert!(ack.expect("an acknowledgement").accepted);
    old.tick().await;
    let [(again, memory)] = booked(&mut inbox)[..] else {
        panic!("one rerun expected");
    };
    assert_eq!(memory, 32 * GIB);
    // Finished: a fresh submission of the same action would otherwise join it.
    assert!(accepted(&old, again).await);
    assert_eq!(disk.rows("floor/"), 1);

    let (successor, mut inbox) = handover(&disk, &old, kbf_sched::MEMORY_FLOORS);
    assert_eq!(offered(&successor, &mut inbox, 1), 32 * GIB);
}

/// Catches: an eviction order that does not survive a reload, and a stamp that restarts
/// at 0 in the successor. With room for two floors: action 9 is raised, then action 1
/// (its key sorts before 9's, so a reload ordered by key would call 1 the older). After a
/// handover action 3 evicts 9, the oldest; after another handover action 4 evicts 1, not
/// 3 (3 was raised after 1, in a later process). Then the cap holds at two floors, and
/// no evicted action is floored again.
#[tokio::test(start_paused = true)]
async fn the_oldest_floor_goes_first_and_the_cap_holds_across_handovers() {
    let disk = Disk::new();
    let a = disk.generation_farm_bounded(2);
    let (_, mut inbox) = register_big(&a);
    raise(&a, &mut inbox, 9).await;
    raise(&a, &mut inbox, 1).await;
    assert_eq!(disk.rows("floor/"), 2);

    let (b, mut inbox) = handover(&disk, &a, 2);
    raise(&b, &mut inbox, 3).await;
    assert_eq!(disk.rows("floor/"), 2, "the eviction left its row");

    let (c, mut inbox) = handover(&disk, &b, 2);
    raise(&c, &mut inbox, 4).await;
    assert_eq!(disk.rows("floor/"), 2, "the eviction left its row");

    let (d, mut inbox) = handover(&disk, &c, 2);
    for (tag, floor) in [(9, 4), (1, 4), (3, 16), (4, 16)] {
        assert_eq!(offered(&d, &mut inbox, tag), floor * GIB, "action {tag}");
    }
}

/// Catches: an evicted floor whose row stays (it comes back when the successor loads the
/// store), after a handover and after a crash.
#[tokio::test(start_paused = true)]
async fn an_evicted_floor_does_not_come_back() {
    for crash in [false, true] {
        let mut disk = Disk::new();
        let old = disk.generation_farm_bounded(1);
        let (_, mut inbox) = register_big(&old);
        raise(&old, &mut inbox, 1).await;
        raise(&old, &mut inbox, 2).await;
        assert_eq!(disk.rows("floor/"), 1, "crash: {crash}");

        let (successor, mut inbox) = if crash {
            disk.crash(old);
            let successor = disk.generation_farm_bounded(1);
            let (_, inbox) = register_big(&successor);
            (successor, inbox)
        } else {
            handover(&disk, &old, 1)
        };
        assert_eq!(
            offered(&successor, &mut inbox, 1),
            4 * GIB,
            "crash: {crash}"
        );
        assert_eq!(
            offered(&successor, &mut inbox, 2),
            16 * GIB,
            "crash: {crash}"
        );
    }
}
