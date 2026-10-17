//! The writer's bounds on what a client chooses: no header row for an invocation with
//! no record, at most [`MAX_INSTANCES`] instance names in a header, the stream block
//! refill counted in a step's transactions, and the oldest idle invocation forgotten
//! first when the writer is full.

use std::sync::atomic::Ordering;

use kbf_history::records::{InvState, InvocationHeader, MAX_INSTANCES, RunOutcome};
use kbf_history::{InvRef, Limits};
use prost::Message;

use crate::support::{Index, Life, Objects, T, all, whole};

fn header(life: &Life, inv: &InvRef) -> Option<InvocationHeader> {
    let row = life
        .index
        .inner
        .get(&format!("ui/invid/{}", inv.key))
        .unwrap()?;
    Some(InvocationHeader::decode(row.value.as_slice()).unwrap())
}

#[test]
fn a_close_with_no_records_writes_no_header_row() {
    // Catches: a Close for an invocation with no record and no stored header writing
    // a header row of no instance and the client's record id. No instance means
    // nothing to authorize a caller against, so the row could only leak the id.
    let objects = Objects::new();
    let index = Index::fake();
    let mut life = Life::start(&objects, &index, 1, Limits::default());
    let bare = InvRef::named("bare");
    life.close(&bare);
    life.drain(T, 5);
    assert_eq!(header(&life, &bare), None);
    assert_eq!(life.writer.open_invocations(), 0, "nothing held for it");
    // A close with no new record still closes the header a previous life stored.
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.step(T);
    life.drain(T + 120_000, 5); // cut by age
    assert_eq!(header(&life, &inv).unwrap().state(), InvState::Open);
    let mut second = Life::start(&objects, &index, 2, Limits::default());
    second.close(&inv);
    second.drain(T + 120_001, 5);
    let h = header(&second, &inv).unwrap();
    assert_eq!((h.state(), h.runs), (InvState::Closed, 1));
    assert_eq!(h.instances, ["main"]);
}

#[test]
fn an_invocation_past_64_instances_is_marked_truncated_and_answers_no_caller() {
    // Catches: a header row that grows with every instance name a client sends, or a
    // capped list read as whole (a caller allowed on the 64 listed would be handed the
    // records and counts of the 65th).
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("wide");
    for n in 0..=MAX_INSTANCES as u64 {
        life.run(&inv, T + n, &format!("i{n:03}"), RunOutcome::Ok);
    }
    life.close(&inv);
    life.drain(T + 100, 10);
    let h = header(&life, &inv).unwrap();
    assert_eq!(h.runs, MAX_INSTANCES as u64 + 1);
    assert_eq!(h.instances.len(), MAX_INSTANCES);
    assert!(h.instances_truncated);
    assert_eq!(whole(&life.reader(), &inv.key, &all).unwrap(), None);
}

#[test]
fn a_stream_block_refill_is_one_of_the_steps_transactions() {
    // Catches: the `ui/streams` refill sent on top of `txns_per_step`, so a step that
    // needs a block sends one more index transaction than the bound allows.
    let mut life = Life::fresh(Limits {
        txns_per_step: 1,
        ..Limits::default()
    });
    assert!(life.minter.needs_block());
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.step(T);
    assert_eq!(
        life.index.txns.load(Ordering::SeqCst),
        1,
        "the refill alone"
    );
    assert!(!life.minter.needs_block());
    assert_eq!(life.writer.pending_rows(), 3, "the rows wait a step");
    life.step(T);
    assert_eq!(life.index.txns.load(Ordering::SeqCst), 2);
    assert_eq!(life.writer.pending_rows(), 0);
    // With no transaction to spend, not even the refill is sent.
    let mut none = Life::fresh(Limits {
        txns_per_step: 0,
        ..Limits::default()
    });
    none.step(T);
    assert_eq!(none.index.txns.load(Ordering::SeqCst), 0);
    assert!(none.minter.needs_block());
}

#[test]
fn a_full_writer_forgets_the_idle_invocation_seen_longest_ago() {
    // Catches: room made by forgetting the idle invocation seen most recently (the one
    // likeliest to get another record, which then costs a header read and an eviction
    // of its own) rather than the one seen longest ago.
    let mut life = Life::fresh(Limits {
        invocations: 2,
        ..Limits::default()
    });
    let older = InvRef::named("older");
    let newer = InvRef::named("newer");
    life.run(&older, T, "main", RunOutcome::Ok);
    life.run(&newer, T + 60_000, "main", RunOutcome::Ok);
    life.step(T + 60_000);
    life.drain(T + 180_000, 10); // both cut by age, their rows committed: idle
    assert_eq!(life.writer.open_invocations(), 2);
    let third = InvRef::named("third");
    life.run(&third, T + 180_001, "main", RunOutcome::Ok);
    life.step(T + 180_001);
    assert_eq!(life.writer.open_invocations(), 2);
    assert_eq!(life.writer.drops().invocations, 0, "an idle one made room");
    // The newer one is still held: its next record needs no header read. Had it been
    // forgotten, it would be opened again, its stored header read.
    let gets = life.index.gets.load(Ordering::SeqCst);
    life.run(&newer, T + 180_002, "main", RunOutcome::Ok);
    life.step(T + 180_002);
    assert_eq!(
        life.index.gets.load(Ordering::SeqCst),
        gets,
        "newer was kept"
    );
    assert_eq!(life.writer.drops().invocations, 0);
}
