//! The writer: object first and row second, retries, bounded memory, dropped rows put
//! back with no LIST, stream ids, and when chunks are cut.

use std::sync::atomic::Ordering;

use kbf_history::container;
use kbf_history::records::{
    ChunkSummary, HitHow, HitRecord, InvocationHeader, RunOutcome, RunRecord, SECTION_HIT,
    SECTION_RUN, Timestamps, chunk_key,
};
use kbf_history::run_id::BLOCK_SIZE;
use kbf_history::{Day, Event, InvRef, Limits, PendingLimits, StreamId};
use prost::Message;

use crate::support::{Index, Life, Objects, T, all, digest, failed_section, run_record, whole};

const ICHUNK: &str = "ui/ichunk/";
const STREAM: &str = "ui/stream/";
const INVID: &str = "ui/invid/";

/// Every invocation the index names reads back without a missing object: no row names
/// an object that was never written.
fn assert_no_dangling(life: &Life) {
    let reader = life.reader();
    for key in life.index.keys(INVID) {
        let inv = kbf_history::InvKey::parse(key.trim_start_matches(INVID)).unwrap();
        whole(&reader, &inv, &all).unwrap_or_else(|e| panic!("{key}: {e}"));
    }
}

fn header(life: &Life, inv: &InvRef) -> InvocationHeader {
    let row = life
        .index
        .inner
        .get(&format!("{INVID}{}", inv.key))
        .unwrap()
        .unwrap();
    InvocationHeader::decode(row.value.as_slice()).unwrap()
}

fn chunks(objects: &Objects) -> Vec<String> {
    objects
        .keys()
        .into_iter()
        .filter(|k| k.starts_with("kbf/history/r30/inv/"))
        .collect()
}

#[test]
fn a_failed_put_leaves_no_index_row_and_is_retried_with_the_same_key() {
    // Catches: index rows written before (or without) the object, so a crash or a
    // failed write leaves a row naming an object that does not exist; and a retry that
    // writes under a new key (an orphan and a duplicate).
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.objects.counts().fail_puts.store(1, Ordering::SeqCst);
    life.step(T);
    assert!(
        life.index.keys("ui/i").is_empty(),
        "no row before the object"
    );
    assert!(life.index.keys(STREAM).is_empty());
    assert_eq!(life.writer.drops().put, 1);
    assert_eq!(life.writer.unwritten_chunks(), 1);
    assert!(chunks(&life.objects).is_empty());
    life.step(T + 1);
    assert_eq!(chunks(&life.objects).len(), 1);
    assert_eq!(life.index.keys(ICHUNK).len(), 1);
    assert_eq!(life.index.keys(STREAM).len(), 1);
    assert_eq!(life.objects.counts().puts.load(Ordering::SeqCst), 2);
    assert_no_dangling(&life);
}

#[test]
fn a_crash_after_the_put_leaves_an_orphan_object_and_never_a_dangling_row() {
    // Catches: the same ordering defect seen from the other side: with the index
    // failing and the frontend gone before it recovers, only the object may exist.
    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserve a stream block first, so the next failure hits the rows
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T);
    assert_eq!(chunks(&life.objects).len(), 1);
    assert!(life.index.keys("ui/i").is_empty());
    drop(life.writer);
    let after = Life::start(&life.objects, &life.index, 2, Limits::default());
    assert_no_dangling(&after);
    let got = whole(&after.reader(), &inv.key, &all).unwrap();
    assert_eq!(got, None, "the orphan is not visible without its rows");
}

#[test]
fn a_lost_put_acknowledgement_is_retried_and_already_exists_counts_as_written() {
    // Catches: `AlreadyExists` on a retried write treated as a failure (the chunk
    // would be retried forever and its rows never written).
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.objects.counts().lose_acks.store(1, Ordering::SeqCst);
    life.step(T);
    assert_eq!(
        (life.writer.drops().put, life.writer.unwritten_chunks()),
        (1, 1)
    );
    life.step(T + 1);
    assert_eq!(life.writer.unwritten_chunks(), 0);
    assert_eq!(chunks(&life.objects).len(), 1);
    assert_eq!(life.index.keys(ICHUNK).len(), 1);
}

#[test]
fn a_dropped_index_row_returns_after_the_backlog_drains_with_no_list() {
    // Catches: a dropped pointer row lost for good (the object kept nowhere), or put
    // back by listing the bucket, which the object store contract does not offer.
    let limits = Limits {
        pending: PendingLimits {
            keys: 3,
            bytes: 1 << 20,
        },
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    life.step(T); // reserve a stream block: the refill is a step's transaction
    let first = InvRef::named("b1");
    let inv = InvRef::named("b2");
    life.run(&first, T, "main", RunOutcome::Ok);
    life.close(&first);
    let r = life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.step(T);
    assert_eq!(
        life.writer.drops().index,
        3,
        "the second chunk's rows did not fit"
    );
    assert_eq!(
        life.writer.pending_rows(),
        3,
        "read back at once: the backlog drained"
    );
    assert_eq!(life.index.keys(ICHUNK).len(), 1);
    life.drain(T + 1, 10);
    assert_eq!(life.index.keys(ICHUNK).len(), 2);
    assert_eq!(life.index.keys(INVID).len(), 2);
    assert_eq!(life.objects.counts().lists.load(Ordering::SeqCst), 0);
    let found = futures::executor::block_on(life.reader().run(r.run_id(), &all)).unwrap();
    assert_eq!(found.unwrap().run, r);
    let got = whole(&life.reader(), &inv.key, &all);
    assert_eq!(
        got.unwrap().unwrap().runs,
        [r],
        "the restored row names the whole object"
    );
}

#[test]
fn an_index_write_that_fails_after_the_put_returns_with_no_list() {
    // Catches: rows of a failed transaction dropped without keeping their object, so
    // the chunk stays invisible though it was written.
    let mut life = Life::fresh(Limits::default());
    life.step(T);
    let inv = InvRef::named("b1");
    let r = life.run(&inv, T, "main", RunOutcome::Failed);
    life.close(&inv);
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T);
    assert_eq!(life.writer.drops().index, 3);
    assert_eq!(life.writer.kept_objects(), 1);
    life.drain(T + 1, 10);
    assert_eq!(life.objects.counts().lists.load(Ordering::SeqCst), 0);
    let found = futures::executor::block_on(life.reader().run(r.run_id(), &all)).unwrap();
    let found = found.unwrap();
    assert_eq!(found.inv, inv.key);
    assert!(found.failed.is_some());
    // The restored header is the one the chunk carried.
    let h = header(&life, &inv);
    assert_eq!((h.runs, h.failed, h.record_id.as_str()), (1, 1, "b1"));
}

#[test]
fn an_index_write_that_landed_but_reported_failure_restores_without_harm() {
    // Catches: a restored row that already exists failing the whole batch for good
    // (its `Absent` check), so the backlog never drains.
    let mut life = Life::fresh(Limits::default());
    life.step(T);
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.index.lose_acks.store(1, Ordering::SeqCst);
    life.step(T);
    assert_eq!(life.writer.kept_objects(), 1);
    life.drain(T + 1, 10);
    assert_eq!(life.index.keys(ICHUNK).len(), 1);
    assert_eq!(
        header(&life, &inv).state(),
        kbf_history::records::InvState::Closed
    );
}

#[test]
fn a_restored_row_beside_rows_that_exist_still_lands() {
    // Catches: a batch abandoned at its first `Absent` conflict, so a missing row that
    // shares a transaction with rows that exist (the header, the stream) never lands.
    let limits = Limits {
        chunk_bytes: 1,
        pending: PendingLimits {
            keys: 3,
            bytes: 1 << 20,
        },
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    life.step(T);
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.step(T);
    assert_eq!(
        life.writer.drops().index,
        1,
        "only the second chunk's row was new"
    );
    life.drain(T, 5);
    assert_eq!(life.index.keys(ICHUNK).len(), 2);
    let got = whole(&life.reader(), &inv.key, &all).unwrap().unwrap();
    assert_eq!(got.runs.len(), 2);
}

#[test]
fn kept_objects_are_read_back_a_few_a_step_and_a_failed_read_is_retried() {
    // Catches: an unbounded read-back after an outage (every kept object read in one
    // step), and a kept object forgotten when its read fails.
    let limits = Limits {
        pending: PendingLimits {
            keys: 3,
            bytes: 1 << 20,
        },
        reinserts_per_step: 1,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    life.step(T);
    for n in 0..3 {
        let inv = InvRef::named(&format!("b{n}"));
        life.run(&inv, T, "main", RunOutcome::Ok);
        life.close(&inv);
    }
    life.step(T); // the first chunk's rows apply; one of two kept objects is read back
    assert_eq!(life.writer.drops().index, 6);
    assert_eq!(life.writer.kept_objects(), 1);
    assert_eq!(life.writer.pending_rows(), 3);
    life.objects.counts().fail_gets.store(1, Ordering::SeqCst);
    life.step(T);
    assert_eq!(
        life.writer.kept_objects(),
        1,
        "a failed read keeps the object"
    );
    assert_eq!(
        life.writer.pending_rows(),
        1,
        "only its header, written again from the writer's state"
    );
    life.drain(T, 5);
    assert_eq!(life.index.keys(ICHUNK).len(), 3);
}

#[test]
fn a_kept_object_whose_footer_does_not_decode_is_left_to_the_sweep() {
    // Catches: a corrupt object wedging the read-back (retried forever) or restoring a
    // row from garbage.
    let mut life = Life::fresh(Limits::default());
    life.step(T);
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T);
    assert_eq!(life.writer.kept_objects(), 1);
    let key = chunks(&life.objects).remove(0);
    let len = life.objects.get(&key).len();
    life.objects.replace(&key, vec![0; len]);
    life.drain(T, 5);
    assert!(life.index.keys(STREAM).is_empty());
    assert!(life.index.keys(ICHUNK).is_empty());
}

#[test]
fn a_fallback_stream_reused_after_a_restart_resolves_each_run_to_its_own_chunk() {
    // Catches: a `ui/stream` row keyed without the stream's first commit time (the
    // second life's row would be refused, or would replace the first: last writer
    // wins), so one of the two runs resolves to the other's chunk or to nothing.
    let objects = Objects::new();
    let index = Index::fake();
    index.block_counters.store(true, Ordering::SeqCst);
    let mut first = Life::start(&objects, &index, 1, Limits::default());
    let a_inv = InvRef::named("a");
    first.step(T);
    let a = first.run(&a_inv, T + 1_000, "ia", RunOutcome::Failed);
    first.close(&a_inv);
    first.step(T + 1_000);
    drop(first);
    let mut second = Life::start(&objects, &index, 2, Limits::default());
    let b_inv = InvRef::named("b");
    second.step(T + 5_000);
    let b = second.run(&b_inv, T + 5_000, "ib", RunOutcome::Failed);
    second.close(&b_inv);
    second.step(T + 5_000);
    let (sa, sb) = (a.run_id().stream(), b.run_id().stream());
    assert_eq!(sa, sb, "both lives took the first fallback id");
    assert!(sa.is_fallback());
    assert!(a.run_id() < b.run_id(), "run ids are time ordered");
    assert_eq!(index.keys(STREAM).len(), 2);
    let reader = second.reader();
    for (r, inv, instance) in [(&a, &a_inv, "ia"), (&b, &b_inv, "ib")] {
        let got = futures::executor::block_on(reader.run(r.run_id(), &all))
            .unwrap()
            .unwrap_or_else(|| panic!("{} did not resolve", r.run_id()));
        assert_eq!((&got.inv, &got.run), (&inv.key, r));
        let stderr = got.failed.unwrap().stderr.unwrap().bytes;
        assert_eq!(stderr, format!("error in {instance}\n").as_bytes());
    }
}

#[test]
fn stream_blocks_come_from_the_counter_and_two_writers_never_share_one() {
    // Catches: a writer that never reserves a block (every id would be a fallback id,
    // not durable), or two writers given the same block.
    let objects = Objects::new();
    let index = Index::fake();
    let mut one = Life::start(&objects, &index, 1, Limits::default());
    let mut two = Life::start(&objects, &index, 2, Limits::default());
    one.step(T);
    two.step(T);
    let a = one.minter.mint(&InvRef::named("a").key, T).id.stream();
    let b = two.minter.mint(&InvRef::named("b").key, T).id.stream();
    assert_eq!((a, b), (StreamId(BLOCK_SIZE), StreamId(2 * BLOCK_SIZE)));
    let txns = index.txns.load(Ordering::SeqCst);
    one.step(T);
    assert_eq!(
        index.txns.load(Ordering::SeqCst),
        txns,
        "no refill while a block is fresh"
    );
    // A counter answer with no value is refused, and minting stays on the fallback.
    let lone = Index::fake();
    lone.empty_counters.store(true, Ordering::SeqCst);
    let mut three = Life::start(&objects, &lone, 3, Limits::default());
    three.step(T);
    assert!(
        three
            .minter
            .mint(&InvRef::named("c").key, T)
            .id
            .stream()
            .is_fallback()
    );
}

#[test]
fn a_chunk_is_cut_when_its_oldest_record_has_waited_120_seconds() {
    // Catches: a chunk held past the age bound (a crash would lose more than 120 s of
    // records) or cut before it.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.step(T);
    life.step(T + 119_999);
    assert!(chunks(&life.objects).is_empty());
    life.run(&inv, T + 100_000, "main", RunOutcome::Ok);
    life.step(T + 120_000);
    assert_eq!(chunks(&life.objects).len(), 1, "both runs in one chunk");
    let h = header(&life, &inv);
    assert_eq!((h.runs, h.chunks, h.last_chunk_ms), (2, 1, T + 120_000));
}

#[test]
fn a_chunk_is_cut_at_its_size_on_a_new_day_and_on_a_new_stream() {
    // Catches: a chunk that grows without bound, spans two UTC days (its key names
    // one), or holds two streams (a run id's sequence would find the wrong block).
    let small = Limits {
        chunk_bytes: 1,
        ..Limits::default()
    };
    let mut life = Life::fresh(small);
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.step(T);
    assert_eq!(chunks(&life.objects).len(), 2);

    let mut life = Life::fresh(Limits::default());
    life.run(&inv, T - 1, "main", RunOutcome::Ok);
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.hit(&inv, T + 1, "main");
    life.close(&inv);
    life.step(T);
    let days: Vec<String> = chunks(&life.objects)
        .iter()
        .map(|k| k.split('/').nth(4).unwrap().to_owned())
        .collect();
    assert_eq!(days, ["20261015", "20261016"]);

    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserve a stream block: the refill is a step's transaction
    for stream in [7, 8] {
        let mut r = run_record("main", RunOutcome::Ok, T);
        r.stream = stream;
        life.queue.offer(Event::Run {
            inv: inv.clone(),
            record: Box::new(r),
            failed: None,
            stream_opened_ms: T,
        });
    }
    life.close(&inv);
    life.step(T);
    assert_eq!(chunks(&life.objects).len(), 2);
    assert_eq!(life.index.keys(STREAM).len(), 2);
}

#[test]
fn a_writer_past_its_buffer_bound_drops_the_oldest_and_counts_the_hole() {
    // Catches: unbounded buffers while the object store is down (the frontend's memory
    // would go), a drop that is not counted, or an invocation with a hole shown
    // complete.
    let mut sample = run_record("main", RunOutcome::Ok, T);
    sample.set_run_id(kbf_history::RunId::new(T, StreamId(0x80_0000), 1));
    let size = sample.encoded_len();
    let limits = Limits {
        buffer_bytes: 2 * size + size / 2,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    let old = InvRef::named("old");
    let new = InvRef::named("new");
    life.run(&old, T, "main", RunOutcome::Ok);
    life.step(T);
    life.run(&new, T, "main", RunOutcome::Ok);
    life.run(&new, T, "main", RunOutcome::Ok);
    life.step(T + 1);
    assert_eq!(
        life.writer.drops().buffer,
        1,
        "the oldest invocation's chunk went"
    );
    life.close(&old);
    life.close(&new);
    life.step(T + 2);
    let got = whole(&life.reader(), &old.key, &all).unwrap().unwrap();
    assert!(!got.complete && got.runs.is_empty());
    assert_eq!(got.header.dropped, 1);
    assert_eq!(header(&life, &new).runs, 2);

    // With the store down, cut chunks are shed first, in write order.
    let limits = Limits {
        chunk_bytes: 1,
        buffer_bytes: 2000,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    life.objects
        .counts()
        .fail_puts
        .store(u32::MAX, Ordering::SeqCst);
    for t in 0..10 {
        life.run(&old, T + t, "main", RunOutcome::Ok);
        life.step(T + t);
    }
    let shed = life.writer.drops().buffer;
    assert!(shed > 0);
    assert!(life.writer.unwritten_chunks() < 10);
    // Back up: what survived is the newest runs, and the header counts the rest.
    life.objects.counts().fail_puts.store(0, Ordering::SeqCst);
    life.close(&old);
    life.drain(T + 10, 10);
    let got = whole(&life.reader(), &old.key, &all).unwrap().unwrap();
    let times: Vec<u64> = got.runs.iter().map(|r| r.time_ms).collect();
    let kept = 10 - shed;
    assert_eq!(
        times,
        (T + shed..T + 10).collect::<Vec<_>>(),
        "the oldest went"
    );
    assert_eq!(times.len() as u64, kept);
    assert_eq!(got.header.dropped, shed);
}

#[test]
fn an_invocation_left_open_by_a_crash_reads_incomplete_from_its_last_chunk() {
    // Catches: an invocation whose writer died shown complete, or with no gap time.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.step(T);
    life.step(T + 120_000);
    life.run(&inv, T + 130_000, "main", RunOutcome::Ok);
    life.step(T + 130_000);
    drop(life.writer);
    let after = Life::start(&life.objects, &life.index, 2, Limits::default());
    let got = whole(&after.reader(), &inv.key, &all).unwrap().unwrap();
    assert!(!got.complete);
    assert_eq!(got.gap_from_ms, Some(T + 120_000));
    assert_eq!(
        got.runs.len(),
        1,
        "the unflushed run is lost, the flushed one kept"
    );
}

#[test]
fn a_restarted_writer_continues_the_header_and_reopens_a_closed_invocation() {
    // Catches: a new writer life overwriting an invocation's header with its own
    // counts only, or a closed invocation that takes records while still closed.
    let objects = Objects::new();
    let index = Index::fake();
    let inv = InvRef::named("b1");
    let mut first = Life::start(&objects, &index, 1, Limits::default());
    first.step(T); // reserve a stream block: the refill is a step's transaction
    first.run(&inv, T, "a", RunOutcome::Ok);
    first.run(&inv, T, "a", RunOutcome::Failed);
    first.close(&inv);
    first.step(T);
    let mut second = Life::start(&objects, &index, 2, Limits::default());
    second.run(&inv, T + 10, "b", RunOutcome::Ok);
    second.step(T + 10);
    second.step(T + 130_000);
    let h = header(&second, &inv);
    assert_eq!((h.runs, h.failed, h.chunks), (3, 1, 2));
    assert_eq!(h.instances, ["a", "b"]);
    assert_eq!(h.state(), kbf_history::records::InvState::Open);
    second.close(&inv);
    second.step(T + 130_001);
    let got = whole(&second.reader(), &inv.key, &all).unwrap().unwrap();
    assert!(got.complete && got.gap_from_ms.is_none());
    assert_eq!(
        (got.runs.len(), got.failed.len(), got.header.chunks),
        (3, 1, 2)
    );

    // A header that cannot be read is read again: the stored one is never replaced by
    // a fresh count, and what the writer added meanwhile is merged into it.
    let mut third = Life::start(&objects, &index, 3, Limits::default());
    index.fail_gets.store(true, Ordering::SeqCst);
    third.run(&inv, T + 20, "c", RunOutcome::Ok);
    third.step(T + 20);
    third.step(T + 140_000);
    assert_eq!(chunks(&objects).len(), 3, "the chunk is written meanwhile");
    assert_eq!(
        header(&third, &inv).runs,
        3,
        "the stored header is untouched"
    );
    index.fail_gets.store(false, Ordering::SeqCst);
    third.step(T + 140_001);
    third.step(T + 140_002);
    let h = header(&third, &inv);
    assert_eq!((h.runs, h.failed, h.chunks), (4, 1, 3));
    assert_eq!(h.instances, ["a", "b", "c"]);
    assert_eq!(h.first_seen_ms, T);
    assert_eq!(h.state(), kbf_history::records::InvState::Open);
}

#[test]
fn closed_idle_and_stale_invocations_are_forgotten_and_held_ones_kept() {
    // Catches: per-invocation state kept for ever (memory grows with every build), or
    // state forgotten while a chunk of it is still unwritten (its header would be lost).
    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserve a stream block: the refill is a step's transaction
    let closed = InvRef::named("closed");
    let open = InvRef::named("open");
    let stale = InvRef::named("stale");
    life.run(&closed, T, "main", RunOutcome::Ok);
    life.close(&closed);
    life.run(&open, T, "main", RunOutcome::Ok);
    life.run(&stale, T - 3 * 86_400_000, "main", RunOutcome::Ok);
    life.step(T + 200_000);
    assert_eq!(
        life.writer.open_invocations(),
        2,
        "buffered records keep their state"
    );
    life.step(T + 320_000);
    assert_eq!(
        life.writer.open_invocations(),
        1,
        "only the recent open one stays"
    );
    let held = InvRef::named("held");
    life.objects.counts().fail_puts.store(1, Ordering::SeqCst);
    life.run(&held, T, "main", RunOutcome::Ok);
    life.close(&held);
    life.step(T + 320_001);
    assert_eq!(
        life.writer.open_invocations(),
        2,
        "an unwritten chunk keeps its state"
    );
    life.step(T + 320_002);
    assert_eq!(life.writer.open_invocations(), 1);
    // Closing an invocation with nothing buffered writes its header alone.
    life.close(&open);
    life.step(T + 320_003);
    assert_eq!(
        header(&life, &open).state(),
        kbf_history::records::InvState::Closed
    );
    assert_eq!(life.writer.open_invocations(), 0);
}

#[test]
fn index_transactions_are_bounded_per_step() {
    // Catches: the writer flushing its whole backlog in one step, past the background
    // lane's rows per transaction and transactions per step.
    let limits = Limits {
        txn_rows: 1,
        txns_per_step: 2,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    life.step(T);
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    let before = life.index.txns.load(Ordering::SeqCst);
    life.step(T);
    assert_eq!(life.index.txns.load(Ordering::SeqCst) - before, 2);
    assert_eq!(life.writer.pending_rows(), 1);
    life.step(T);
    assert_eq!(life.writer.pending_rows(), 0);
}

#[test]
fn hits_and_id_less_runs_are_kept_with_their_invocation() {
    // Catches: a hits-only chunk given a stream row (no run can name it), or an
    // id-less run with no way back to it.
    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserve a stream block: the refill is a step's transaction
    let inv = InvRef::named("hits");
    life.hit(&inv, T, "main");
    life.hit(&inv, T + 1, "main");
    life.close(&inv);
    let idless = InvRef::idless(life.writer_id, Day::from_ms(T));
    let r = life.run(&idless, T + 2, "main", RunOutcome::Failed);
    life.close(&idless);
    life.step(T + 2);
    assert_eq!(
        life.index.keys(STREAM).len(),
        1,
        "the hits-only chunk has none"
    );
    let h = header(&life, &inv);
    assert_eq!((h.hits, h.runs), (2, 0));
    let got = whole(&life.reader(), &inv.key, &all).unwrap().unwrap();
    let rows: Vec<u32> = got.hits.iter().map(|h| h.row).collect();
    assert_eq!(rows, [0, 1]);
    let found = futures::executor::block_on(life.reader().run(r.run_id(), &all))
        .unwrap()
        .unwrap();
    assert_eq!(found.inv, idless.key);
    let ih = header(&life, &idless);
    assert!(ih.idless && ih.record_id.is_empty());
    let _: RunRecord = found.run;
}

#[test]
fn a_dropped_header_with_no_object_is_counted_and_not_kept() {
    // Catches: a header-only write (an invocation closed with nothing buffered) put on
    // the read-back list though no object can restore it, or dropped uncounted.
    let limits = Limits {
        pending: PendingLimits {
            keys: 1,
            bytes: 1 << 20,
        },
        txns_per_step: 0,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    let full = InvRef::named("full");
    let empty = InvRef::named("empty");
    life.run(&full, T, "main", RunOutcome::Ok);
    life.close(&full);
    life.step(T);
    assert_eq!(
        life.writer.drops().index,
        2,
        "the chunk's rows past the stream row"
    );
    // A header written alone: a stored header closed with nothing buffered.
    life.index.put(
        &format!("{INVID}{}", empty.key),
        InvocationHeader {
            instances: vec!["main".into()],
            ..InvocationHeader::default()
        }
        .encode_to_vec(),
    );
    life.close(&empty);
    life.step(T);
    assert_eq!(life.writer.drops().index, 3, "and the header");
    assert_eq!(life.writer.kept_objects(), 1, "only the chunk is kept");
}

#[test]
fn a_hits_only_chunk_is_restored_without_a_stream_row() {
    // Catches: a stream row invented for a chunk with no runs when its rows are put
    // back (it would claim stream 0 for an invocation that has none).
    let mut life = Life::fresh(Limits::default());
    life.step(T);
    let inv = InvRef::named("hits");
    life.hit(&inv, T, "main");
    life.close(&inv);
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T);
    assert_eq!(life.writer.kept_objects(), 1);
    life.drain(T, 5);
    assert_eq!(life.index.keys(ICHUNK).len(), 1);
    assert!(life.index.keys(STREAM).is_empty());
    assert_eq!(header(&life, &inv).hits, 1);
}

#[test]
fn a_kept_chunk_restored_after_a_newer_header_leaves_the_header_alone() {
    // Catches: a restored header row written over the live one (`Cond::Any` where the
    // restore needs `Absent`). The header in a kept chunk's footer was taken when the
    // chunk was cut, so it counts fewer runs and chunks and may say open; written over
    // the header a later chunk applied, it moves the invocation backwards and a closed
    // build reads incomplete.
    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserve a stream block, so the planted failure hits the rows
    let inv = InvRef::named("b1");
    life.run(&inv, T + 1, "main", RunOutcome::Ok);
    life.step(T + 1);
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T + 120_001); // the first chunk is cut by age; its rows fail
    assert_eq!(life.writer.kept_objects(), 1);
    life.run(&inv, T + 120_002, "main", RunOutcome::Ok);
    life.close(&inv);
    // The second chunk's rows apply; with nothing pending the first chunk is read back.
    life.step(T + 120_002);
    assert_eq!(life.writer.kept_objects(), 0);
    life.drain(T + 120_003, 10);
    assert_eq!(
        life.index.keys(ICHUNK).len(),
        2,
        "the kept chunk's row came back"
    );
    let h = header(&life, &inv);
    assert_eq!((h.runs, h.chunks), (2, 2));
    assert_eq!(h.state(), kbf_history::records::InvState::Closed);
    let got = whole(&life.reader(), &inv.key, &all).unwrap().unwrap();
    assert!(
        got.complete,
        "a closed build with every record reads complete"
    );
    assert_eq!(got.runs.len(), 2);
}

#[test]
fn a_chunk_footer_carries_its_digest_table_and_slowest_runs() {
    // Catches: a chunk written without its digest table (the `/actions/{digest}` hit
    // lookup would find nothing), a table that names failed-run sections or is not
    // sorted by (prefix, key), and slowest lists left out of the summary or swapped.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    // Slower to execute, faster on the wall clock.
    let a = life.run(&inv, T, "main", RunOutcome::Ok);
    // Faster to execute, slower on the wall clock, and failed (with a section).
    let minted = life.minter.mint(&inv.key, T + 1);
    let mut b = run_record("main", RunOutcome::Failed, T + 1);
    b.timestamps = Some(Timestamps {
        queued_us: 0,
        execution_start_us: 10,
        execution_completed_us: 110,
        worker_completed_us: 10_000,
        ..Timestamps::default()
    });
    b.set_run_id(minted.id);
    assert!(life.queue.offer(Event::Run {
        inv: inv.clone(),
        record: Box::new(b.clone()),
        failed: Some(failed_section("main")),
        stream_opened_ms: minted.stream_opened_ms,
    }));
    // A hit whose digest sorts before the runs', though its record comes after them.
    assert!(life.queue.offer(Event::Hit {
        inv: inv.clone(),
        record: HitRecord {
            time_ms: T + 2,
            instance: "main".into(),
            action: Some(digest(0)),
            how: HitHow::CacheHit.into(),
            row: 0,
        },
    }));
    life.close(&inv);
    life.step(T + 2);
    let object = life.objects.get(&chunks(&life.objects).remove(0));
    let (_, footer) = container::footer_of(&object).unwrap();
    let summary = ChunkSummary::decode(footer.summary.as_slice()).unwrap();
    let digests: Vec<(u64, Vec<u8>)> = summary
        .digests
        .iter()
        .map(|d| (d.prefix, d.key.clone()))
        .collect();
    let p1 = digest(1).prefix16();
    assert_eq!(
        digests,
        [
            (0, chunk_key(SECTION_HIT, 0).to_vec()),
            (p1, chunk_key(SECTION_RUN, a.seq).to_vec()),
            (p1, chunk_key(SECTION_RUN, b.seq).to_vec()),
        ],
        "runs and hits with an action, none for the failed section, sorted"
    );
    let ids = |list: &[kbf_history::records::Slow]| -> Vec<Vec<u8>> {
        list.iter().map(|s| s.run_id.clone()).collect()
    };
    let (ia, ib) = (
        a.run_id().to_bytes().to_vec(),
        b.run_id().to_bytes().to_vec(),
    );
    assert_eq!(ids(&summary.slowest_execution), [ia.clone(), ib.clone()]);
    assert_eq!(ids(&summary.slowest_wall), [ib, ia]);
}

#[test]
fn an_invocation_forgotten_and_seen_again_by_the_same_writer_keeps_both_chunks() {
    // Catches: chunk keys numbered per invocation from 0, so a writer that forgot a
    // closed invocation and then took a late record of it writes chunk 0 again: the
    // write answers `AlreadyExists` (taken for a retry that landed), the new `ichunk`
    // row fails `Absent` (taken for a row that is there), and the record is lost with
    // no drop counted while the header counts it.
    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserve a stream block: the refill is a step's transaction
    let inv = InvRef::named("b1");
    let first = life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.step(T);
    assert_eq!(
        life.writer.open_invocations(),
        0,
        "the closed invocation is forgotten"
    );
    let late = life.run(&inv, T + 1_000, "main", RunOutcome::Ok);
    life.step(T + 1_000);
    life.step(T + 121_000);
    assert_eq!(chunks(&life.objects).len(), 2);
    assert_eq!(life.index.keys(ICHUNK).len(), 2);
    let reader = life.reader();
    for r in [&first, &late] {
        let got = futures::executor::block_on(reader.run(r.run_id(), &all))
            .unwrap()
            .unwrap_or_else(|| panic!("{} did not resolve", r.run_id()));
        assert_eq!(&got.run, r);
    }
    let got = whole(&reader, &inv.key, &all).unwrap().unwrap();
    assert_eq!(got.runs, [first, late]);
    assert_eq!((got.header.runs, got.header.chunks), (2, 2));
    assert_eq!(life.writer.drops(), kbf_history::Drops::default());

    // An open invocation forgotten as stale and seen again on a later day: the object
    // key carries the day, the `ichunk` row key does not, so the rows must not collide.
    let mut life = Life::fresh(Limits::default());
    let before = T - 2 * 86_400_000;
    let old = life.run(&inv, before, "main", RunOutcome::Ok);
    life.step(before);
    life.step(before + 120_000);
    life.step(T);
    assert_eq!(
        life.writer.open_invocations(),
        0,
        "the stale invocation is forgotten"
    );
    let new = life.run(&inv, T, "main", RunOutcome::Ok);
    life.step(T);
    life.step(T + 120_000);
    assert_eq!(life.index.keys(ICHUNK).len(), 2);
    let reader = life.reader();
    for r in [&old, &new] {
        let got = futures::executor::block_on(reader.run(r.run_id(), &all)).unwrap();
        assert_eq!(got.map(|v| v.run).as_ref(), Some(r));
    }
}

#[test]
fn index_transactions_retried_on_conflicts_count_against_the_step_bound() {
    // Catches: a batch retried once per `Absent` conflict within one step, each retry
    // another synced transaction. Restored rows that exist already are the common case
    // (a write that landed but reported failure), so one step could send a transaction
    // per row, far past the background lane's bound.
    let mut life = Life::fresh(Limits::default());
    life.step(T);
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.index.lose_acks.store(1, Ordering::SeqCst);
    life.step(T);
    assert_eq!(life.writer.kept_objects(), 1);
    life.step(T + 1); // the kept object's three rows, all present, are queued again
    assert_eq!(life.writer.pending_rows(), 3);
    let mut steps = 0;
    while life.writer.pending_rows() > 0 {
        let before = life.index.txns.load(Ordering::SeqCst);
        life.step(T + 2);
        let sent = life.index.txns.load(Ordering::SeqCst) - before;
        assert!(sent <= 1, "{sent} transactions in one step");
        steps += 1;
        assert!(steps <= 5, "the conflicting rows never drained");
    }
    assert_eq!(steps, 3, "one conflicting row leaves the batch per step");
    assert_eq!(life.index.keys(ICHUNK).len(), 1);
    assert_eq!(
        life.writer.drops().index,
        3,
        "only the failed write's rows counted"
    );
}

#[test]
fn a_stream_row_is_keyed_by_the_stream_open_time_whatever_order_events_arrive() {
    // Catches: a `ui/stream` row keyed by whichever run of the stream the writer took
    // first. Events reach the queue from concurrent requests in no fixed order, so a
    // later run first keys the row past the earlier run's time, and that run's id no
    // longer resolves (or, with a fallback id reused across lives, resolves to the
    // other life's chunk).
    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserve a stream block: the refill is a step's transaction
    let inv = InvRef::named("b1");
    let m0 = life.minter.mint(&inv.key, T);
    let m1 = life.minter.mint(&inv.key, T + 5);
    let mut runs = Vec::new();
    for m in [m0, m1] {
        let mut r = run_record("main", RunOutcome::Ok, m.id.time_ms());
        r.set_run_id(m.id);
        runs.push((m, r));
    }
    // The later run reaches the queue first.
    for (m, r) in runs.iter().rev() {
        assert!(life.queue.offer(Event::Run {
            inv: inv.clone(),
            record: Box::new(r.clone()),
            failed: None,
            stream_opened_ms: m.stream_opened_ms,
        }));
    }
    life.close(&inv);
    life.step(T + 5);
    let reader = life.reader();
    for (_, r) in &runs {
        let got = futures::executor::block_on(reader.run(r.run_id(), &all)).unwrap();
        assert_eq!(got.map(|v| v.run).as_ref(), Some(r), "{}", r.run_id());
    }
}

#[test]
fn stored_headers_are_read_a_bounded_number_a_step() {
    // Catches: one blocking index read per new invocation inside the drain, so a queue
    // full of distinct client ids becomes that many reads in one step.
    let limits = Limits {
        header_reads_per_step: 1,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    for n in 0..3 {
        let inv = InvRef::named(&format!("b{n}"));
        life.run(&inv, T, "main", RunOutcome::Ok);
        life.close(&inv);
    }
    let mut reads = Vec::new();
    for t in 0..4 {
        let before = life.index.gets.load(Ordering::SeqCst);
        life.step(T + t);
        reads.push(life.index.gets.load(Ordering::SeqCst) - before);
    }
    assert_eq!(reads, [1, 1, 1, 0]);
    assert_eq!(
        life.index.keys(INVID).len(),
        3,
        "each header written once read"
    );
    assert_eq!(life.writer.open_invocations(), 0);
}

#[test]
fn open_invocations_are_bounded_forgetting_idle_ones_first() {
    // Catches: per-invocation state bounded only by client input (every fresh id held
    // until it closes or the day ends), an idle invocation kept while a new one is
    // refused, and a refusal that is not counted.
    let limits = Limits {
        invocations: 2,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    let idle = InvRef::named("idle");
    let busy = InvRef::named("busy");
    life.run(&idle, T, "main", RunOutcome::Ok);
    life.step(T);
    life.step(T + 120_000); // written and applied: open, but idle
    life.run(&busy, T + 120_000, "main", RunOutcome::Ok);
    life.step(T + 120_000);
    assert_eq!(life.writer.open_invocations(), 2);
    let fresh = InvRef::named("fresh");
    life.run(&fresh, T + 120_001, "main", RunOutcome::Ok);
    life.step(T + 120_001);
    assert_eq!(life.writer.open_invocations(), 2, "the idle one made room");
    assert_eq!(life.writer.drops().invocations, 0);
    // Both held invocations have records buffered: a fourth is refused and counted.
    let refused = InvRef::named("refused");
    life.run(&refused, T + 120_002, "main", RunOutcome::Ok);
    life.close(&refused);
    life.step(T + 120_002);
    assert_eq!(life.writer.drops().invocations, 2, "its run and its close");
    assert_eq!(life.writer.open_invocations(), 2);
    // The forgotten invocation continues its stored header when it is seen again.
    life.close(&busy);
    life.close(&fresh);
    life.step(T + 120_003);
    life.run(&idle, T + 120_004, "main", RunOutcome::Ok);
    life.close(&idle);
    life.drain(T + 120_004, 5);
    let h = header(&life, &idle);
    assert_eq!((h.runs, h.chunks), (2, 2));
    assert!(
        life.index
            .inner
            .get(&format!("{INVID}{}", refused.key))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_closed_invocation_whose_header_row_is_pending_is_not_forgotten() {
    // Catches: an invocation forgotten while its header row still waits in the pending
    // rows; a late record would reload the older header from the index and its write
    // would replace the pending one, losing the counts it held.
    let limits = Limits {
        txns_per_step: 0,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.step(T);
    assert_eq!(life.writer.unwritten_chunks(), 0);
    assert_eq!(life.writer.pending_rows(), 3);
    assert_eq!(life.writer.open_invocations(), 1);
}
