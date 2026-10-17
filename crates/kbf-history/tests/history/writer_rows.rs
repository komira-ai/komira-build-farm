//! The writer's index rows: a header that did not commit is written again, a stream row
//! is written once per stream, and a chunk's rows share a transaction with its header.

use std::sync::atomic::Ordering;

use kbf_history::records::{InvState, InvocationHeader, RunOutcome};
use kbf_history::{InvRef, Limits};
use prost::Message;

use crate::support::{Life, T, all, whole};

fn header(life: &Life, inv: &InvRef) -> InvocationHeader {
    let row = life
        .index
        .inner
        .get(&format!("ui/invid/{}", inv.key))
        .unwrap()
        .unwrap();
    InvocationHeader::decode(row.value.as_slice()).unwrap()
}

/// A life whose invocation `b1` has one run in a chunk and its header in the index.
fn one_chunk_landed() -> (Life, InvRef) {
    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserve a stream block, so a planted failure hits the rows
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.step(T);
    life.step(T + 120_000); // cut by age
    assert_eq!(header(&life, &inv).runs, 1);
    (life, inv)
}

fn assert_closed_and_complete(life: &mut Life, inv: &InvRef, runs: u64, chunks: u32) {
    let h = header(life, inv);
    assert_eq!(
        (h.state(), h.runs, h.chunks),
        (InvState::Closed, runs, chunks)
    );
    let got = whole(&life.reader(), &inv.key, &all).unwrap().unwrap();
    assert!(got.complete, "a finished build reads complete");
    assert_eq!(life.writer.open_invocations(), 0, "forgotten once saved");
}

#[test]
fn a_final_header_whose_transaction_failed_is_written_again() {
    // Catches: the closing header row dropped with its failed transaction and never
    // written again. A closed invocation gets no later chunk to rewrite it, and the
    // kept chunk's restored header (`Absent`) conflicts with the older header row, so
    // the stored header stayed open with fewer runs and the build read incomplete for
    // good.
    let (mut life, inv) = one_chunk_landed();
    life.run(&inv, T + 120_001, "main", RunOutcome::Ok);
    life.close(&inv);
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T + 120_001);
    assert_eq!(life.writer.kept_objects(), 1);
    life.drain(T + 120_002, 10);
    assert_closed_and_complete(&mut life, &inv, 2, 2);
}

#[test]
fn a_header_alone_whose_transaction_failed_is_written_again() {
    // Catches: the same loss for a close with nothing buffered, whose header row has
    // no object to restore it from.
    let (mut life, inv) = one_chunk_landed();
    life.close(&inv);
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T + 120_001);
    assert_eq!(life.writer.kept_objects(), 0, "no object to keep");
    assert_eq!(
        (life.writer.drops().index, life.writer.pending_rows()),
        (1, 0),
        "counted once, and not queued again while the index is failing"
    );
    assert_eq!(
        life.writer.open_invocations(),
        1,
        "held until its header lands"
    );
    life.drain(T + 120_002, 10);
    assert_closed_and_complete(&mut life, &inv, 1, 1);
}

#[test]
fn a_restored_chunk_of_an_invocation_whose_header_is_unread_is_not_counted_twice() {
    // Catches: a kept chunk's footer header put back for an invocation the writer still
    // holds. While the stored header is unread, the footer carries only what this
    // writer added; restored into an empty row, it is then read as the stored header and
    // the same additions are merged into it again, so the runs and chunks count twice.
    let mut life = Life::fresh(Limits::default());
    life.step(T);
    let inv = InvRef::named("b1");
    life.index.fail_gets.store(true, Ordering::SeqCst);
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T);
    assert_eq!(life.writer.kept_objects(), 1);
    life.step(T + 1); // the kept chunk is read back while the header read still fails
    life.step(T + 2);
    life.index.fail_gets.store(false, Ordering::SeqCst);
    life.drain(T + 3, 10);
    assert_closed_and_complete(&mut life, &inv, 1, 1);
}

#[test]
fn a_header_past_the_backlog_bound_is_written_again_and_counted_once() {
    // Catches: a header refused by a full backlog lost for good, or counted again on
    // every step it waits for room.
    let limits = Limits {
        pending: kbf_history::PendingLimits {
            keys: 1,
            bytes: 1 << 20,
        },
        txns_per_step: 0,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    let a = InvRef::named("a");
    let b = InvRef::named("b");
    // Headers written alone: stored headers closed with nothing buffered.
    for inv in [&a, &b] {
        let stored = InvocationHeader {
            instances: vec!["main".into()],
            ..InvocationHeader::default()
        };
        life.index
            .put(&format!("ui/invid/{}", inv.key), stored.encode_to_vec());
    }
    life.close(&a);
    life.close(&b);
    life.step(T);
    assert_eq!(life.writer.drops().index, 1, "b's header found no room");
    life.step(T + 1);
    assert_eq!(life.writer.drops().index, 1, "and is not counted again");
    assert_eq!(life.writer.pending_rows(), 1);
    assert_eq!(life.writer.open_invocations(), 2);
}

#[test]
fn a_stream_row_is_queued_once_per_stream_not_once_per_chunk() {
    // Catches: every chunk of a stream queueing its `ui/stream` row again. The key is
    // the same for every chunk, so from the second chunk on the `Absent` row always
    // conflicts, and each conflict costs a transaction of the step's budget: one
    // redundant row a step delays every row batched with it, and at load the backlog
    // grows until rows are dropped.
    let (mut life, inv) = one_chunk_landed();
    for k in 1..=3u64 {
        let t = T + k * 200_000;
        life.run(&inv, t, "main", RunOutcome::Ok);
        life.step(t);
        let before = life.index.txns.load(Ordering::SeqCst);
        life.step(t + 120_000); // cut by age, written and applied
        let sent = life.index.txns.load(Ordering::SeqCst) - before;
        assert_eq!(
            (sent, life.writer.pending_rows()),
            (1, 0),
            "chunk {k}: one transaction, nothing left"
        );
    }
    assert_eq!(life.index.keys("ui/stream/").len(), 1);
    assert_eq!(life.index.keys("ui/ichunk/").len(), 4);
}

#[test]
fn a_chunk_s_rows_and_its_header_share_a_transaction_under_a_backlog() {
    // Catches: index rows batched in key order. Every `ui/ichunk` row sorts before every
    // `ui/invid` row, so with a backlog past one transaction a chunk row lands steps
    // before the header written with it, and a reader sees a chunk the header does not
    // account for.
    let limits = Limits {
        txn_rows: 3,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    life.step(T);
    let invs = [InvRef::named("a"), InvRef::named("b")];
    for inv in &invs {
        life.run(inv, T, "main", RunOutcome::Ok);
        life.close(inv);
    }
    life.step(T + 1);
    assert_eq!(life.writer.pending_rows(), 3);
    for inv in &invs {
        let chunk = life.index.keys(&format!("ui/ichunk/{}/", inv.key));
        let head = life.index.keys(&format!("ui/invid/{}", inv.key));
        assert_eq!(
            chunk.len(),
            head.len(),
            "{}: chunk row without its header",
            inv.key
        );
    }
    life.step(T + 2);
    assert_eq!(life.index.keys("ui/invid/").len(), 2);
}

#[test]
fn a_batch_whose_rows_all_exist_leaves_the_rest_of_the_budget_to_the_next_batch() {
    // Catches: a step that stops once conflicts have emptied a batch, though budget is
    // left: restored pointer rows that landed already (a lost acknowledgement) would
    // hold back the rows behind them for a step each.
    let limits = Limits {
        txn_rows: 2,
        txns_per_step: 4,
        ..Limits::default()
    };
    let (mut life, inv) = {
        let mut life = Life::fresh(limits);
        life.step(T);
        let inv = InvRef::named("b1");
        life.run(&inv, T, "main", RunOutcome::Ok);
        life.close(&inv);
        (life, inv)
    };
    life.index.lose_acks.store(1, Ordering::SeqCst);
    life.step(T); // two rows land, reported failed; the header lands after them
    assert_eq!(life.writer.kept_objects(), 1);
    life.step(T + 1); // the closed invocation is forgotten: all three rows come back
    assert_eq!(life.writer.pending_rows(), 3);
    life.step(T + 2);
    assert_eq!(
        life.writer.pending_rows(),
        0,
        "three conflicts over two batches in one step"
    );
    assert_closed_and_complete(&mut life, &inv, 1, 1);
}
