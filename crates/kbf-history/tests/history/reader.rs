//! The reader: a run id or an invocation to its records, filtered by instance, and the
//! errors a broken index or object gives.

use std::sync::atomic::Ordering;

use kbf_history::container::{self, Header};
use kbf_history::records::{
    ChunkRecord, ChunkRow, ChunkSummary, InvocationHeader, RunOutcome, SECTION_RUN, StreamRow,
    chunk_key,
};
use kbf_history::{
    ChunkCursor, Day, Family, IndexRoot, InvRef, Limits, MAX_PAGE_CHUNKS, ReadError, RunId,
    StreamId, WriterId,
};
use kbf_objstore::ObjectStoreError;
use prost::Message;

use crate::support::{Life, T, all, root, whole};

/// A second into the day, so a time before a stream's first run is the same day.
const B: u64 = T + 1_000;

fn only_a(instance: &str) -> bool {
    instance == "a"
}

fn closed_build(life: &mut Life) -> (InvRef, Vec<kbf_history::records::RunRecord>) {
    let inv = InvRef::named("build");
    let runs = vec![
        life.run(&inv, B, "a", RunOutcome::Ok),
        life.run(&inv, B + 1, "b", RunOutcome::Failed),
        life.run(&inv, B + 2, "a", RunOutcome::TimedOut),
    ];
    life.hit(&inv, B + 3, "a");
    life.hit(&inv, B + 4, "b");
    life.close(&inv);
    life.drain(B + 5, 5);
    (inv, runs)
}

#[test]
fn a_run_reads_back_with_its_failed_section_and_an_ok_run_without() {
    // Catches: a run located in the wrong chunk or block, a failed run whose tails and
    // command are not found, or an ok run given another run's section.
    let mut life = Life::fresh(Limits::default());
    let (inv, runs) = closed_build(&mut life);
    let reader = life.reader();
    for r in &runs {
        let got = futures::executor::block_on(reader.run(r.run_id(), &all))
            .unwrap()
            .unwrap();
        assert_eq!((&got.inv, &got.run), (&inv.key, r));
        assert_eq!(got.failed.is_some(), r.failed());
        if let Some(f) = got.failed {
            assert_eq!(f.seq, r.seq);
            let stderr = f.stderr.unwrap().bytes;
            assert_eq!(stderr, format!("error in {}\n", r.instance).as_bytes());
            assert_eq!(f.command.unwrap().env_names, ["PATH"]);
        }
    }
    // A failed run written without a section reads back with none.
    let other = InvRef::named("bare");
    let minted = life.minter.mint(&other.key, T + 10);
    let id = minted.id;
    let mut r = crate::support::run_record("a", RunOutcome::Failed, T + 10);
    r.set_run_id(id);
    life.queue.offer(kbf_history::Event::Run {
        inv: other.clone(),
        record: Box::new(r.clone()),
        failed: None,
        stream_opened_ms: minted.stream_opened_ms,
    });
    life.close(&other);
    life.step(T + 10);
    let got = futures::executor::block_on(life.reader().run(id, &all))
        .unwrap()
        .unwrap();
    assert_eq!((got.run, got.failed), (r, None));
}

#[test]
fn an_invocation_reads_back_every_record_of_every_chunk() {
    // Catches: a chunk skipped (only the newest read), or a record kind dropped.
    let mut life = Life::fresh(Limits {
        chunk_bytes: 1,
        ..Limits::default()
    });
    let (inv, runs) = closed_build(&mut life);
    let got = whole(&life.reader(), &inv.key, &all).unwrap().unwrap();
    assert!(got.header.chunks >= 4, "{} chunks", got.header.chunks);
    assert_eq!(got.runs, runs);
    assert_eq!((got.failed.len(), got.hits.len()), (2, 2));
    assert!(got.complete);
}

#[test]
fn an_invocation_spanning_instances_is_answered_only_when_every_one_is_allowed() {
    // Catches: a header (record id, every instance name, counts, times) returned to a
    // caller allowed on only some of the invocation's instances, with the records of
    // the others filtered out: all-must-allow (ui-data.md section 8) never applied.
    let mut life = Life::fresh(Limits::default());
    let (inv, runs) = closed_build(&mut life);
    let reader = life.reader();
    let view = futures::executor::block_on(reader.invocation(&inv.key, &only_a)).unwrap();
    assert_eq!(view, None);
    let page = futures::executor::block_on(reader.records(&inv.key, None, 1, &only_a)).unwrap();
    assert_eq!(page, None);
    let b = futures::executor::block_on(reader.run(runs[1].run_id(), &only_a)).unwrap();
    assert_eq!(b, None);
    let a = futures::executor::block_on(reader.run(runs[0].run_id(), &only_a)).unwrap();
    assert!(a.is_some(), "a single run is scoped by its own instance");
}

#[test]
fn records_of_an_instance_the_header_does_not_list_are_still_filtered() {
    // Catches: a reader that trusts the header's instance list for every record, so a
    // header that undercounts (restored from an older chunk's footer) hands out the
    // records of an instance the caller may not see. (The invocation's summary is
    // refused here, since its newest chunk names b; the records are read page by page.)
    let mut life = Life::fresh(Limits::default());
    let (inv, _) = closed_build(&mut life);
    let key = IndexRoot::default().invid(&inv.key);
    let row = life.index.inner.get(&key).unwrap().unwrap();
    let mut header = InvocationHeader::decode(row.value.as_slice()).unwrap();
    header.instances = vec!["a".into()];
    life.index.put(&key, header.encode_to_vec());
    let reader = life.reader();
    let summary = futures::executor::block_on(reader.invocation(&inv.key, &only_a));
    assert_eq!(summary.unwrap(), None);
    let got = futures::executor::block_on(reader.records(&inv.key, None, MAX_PAGE_CHUNKS, &only_a))
        .unwrap()
        .unwrap();
    assert_eq!(got.next, None, "one chunk");
    assert!(got.runs.iter().all(|r| r.instance == "a"));
    assert_eq!(got.runs.len(), 2);
    assert!(got.failed.iter().all(|f| f.instance == "a"));
    assert_eq!(got.failed.len(), 1);
    assert_eq!(got.hits.len(), 1);
    assert_eq!(got.hits[0].instance, "a");
}

#[test]
fn ids_and_invocations_history_does_not_hold_read_as_none() {
    // Catches: an unknown id resolved to the nearest stream row of another day or
    // stream, or a sequence past the chunk's range answered with a neighbour.
    let mut life = Life::fresh(Limits::default());
    let (_, runs) = closed_build(&mut life);
    let reader = life.reader();
    let id = runs[0].run_id();
    let unknown = [
        RunId::new(T + 86_400_000, id.stream(), id.seq()),
        RunId::new(id.time_ms(), StreamId(id.stream().0 + 1), 0),
        RunId::new(id.time_ms(), id.stream(), 99),
        // Before the stream's first commit: no row at or before it.
        RunId::new(id.time_ms() - 1, id.stream(), 0),
    ];
    for u in unknown {
        assert_eq!(
            futures::executor::block_on(reader.run(u, &all)).unwrap(),
            None,
            "{u}"
        );
    }
    let nobody = InvRef::named("nobody");
    assert_eq!(whole(&reader, &nobody.key, &all).unwrap(), None);
}

#[test]
fn a_missing_or_newer_object_is_an_error_not_an_empty_answer() {
    // Catches: an expired or unreadable chunk reported as a run that does not exist,
    // or a newer container version misread.
    let mut life = Life::fresh(Limits::default());
    let (inv, runs) = closed_build(&mut life);
    let key = life.objects.keys().remove(0);
    let mut bytes = life.objects.get(&key).to_vec();
    bytes[5] = 2;
    let n = bytes.len();
    bytes[n - 7] = 2;
    life.objects.replace(&key, bytes);
    let reader = life.reader();
    let err = whole(&reader, &inv.key, &all).unwrap_err();
    assert!(err.to_string().contains("version 2"), "{err}");
    let err = futures::executor::block_on(reader.run(runs[0].run_id(), &all)).unwrap_err();
    assert!(matches!(err, ReadError::Format(_)), "{err}");
    life.objects.remove(&key);
    let err = futures::executor::block_on(reader.run(runs[0].run_id(), &all)).unwrap_err();
    assert!(
        matches!(err, ReadError::Object(ObjectStoreError::NotFound(_))),
        "{err}"
    );
}

#[test]
fn broken_index_rows_are_errors() {
    // Catches: an undecodable row read as an empty one (a stream row with no
    // invocation would resolve every run to nothing, silently).
    let mut life = Life::fresh(Limits::default());
    let (inv, runs) = closed_build(&mut life);
    let id = runs[0].run_id();
    let reader = life.reader();
    let stream_key = life
        .index
        .keys(&IndexRoot::default().stream_prefix(id.day(), id.stream()))
        .remove(0);
    let bad_inv = StreamRow {
        inv_key: "nope".into(),
        writer: 1,
    };
    life.index.put(&stream_key, bad_inv.encode_to_vec());
    let err = futures::executor::block_on(reader.run(id, &all)).unwrap_err();
    assert!(err.to_string().contains("not an invocation key"), "{err}");
    life.index.put(&stream_key, vec![0xFF]);
    let err = futures::executor::block_on(reader.run(id, &all)).unwrap_err();
    assert!(matches!(err, ReadError::Row { .. }), "{err}");
    let row_key = life
        .index
        .keys(&IndexRoot::default().ichunk_prefix(&inv.key))
        .remove(0);
    life.index.put(&row_key, vec![0xFF]);
    let err = whole(&reader, &inv.key, &all).unwrap_err();
    assert!(err.to_string().contains(&row_key), "{err}");
    life.index
        .put(&IndexRoot::default().invid(&inv.key), vec![0xFF]);
    let err = whole(&reader, &inv.key, &all).unwrap_err();
    assert!(matches!(err, ReadError::Row { .. }), "{err}");
    // The index itself failing is its own error.
    let as_text = ReadError::from(kbf_state::StoreError::Failed("down".into())).to_string();
    assert!(as_text.contains("down"));
}

/// A header listing `instances`.
fn listing(instances: &[&str]) -> InvocationHeader {
    InvocationHeader {
        instances: instances.iter().map(|&i| i.to_owned()).collect(),
        ..InvocationHeader::default()
    }
}

/// Writes a chunk by hand: `records` under a valid header, with its rows, so a test can
/// plant what no writer writes. Its header row and summary list instance `a`.
fn plant(life: &Life, records: Vec<(Vec<u8>, Vec<u8>)>, row: ChunkRow) -> (InvRef, RunId) {
    let summary = ChunkSummary {
        header: Some(listing(&["a"])),
        ..ChunkSummary::default()
    };
    plant_with(life, records, row, summary.encode_to_vec())
}

fn plant_with(
    life: &Life,
    records: Vec<(Vec<u8>, Vec<u8>)>,
    row: ChunkRow,
    summary: Vec<u8>,
) -> (InvRef, RunId) {
    let inv = InvRef::named("planted");
    let writer = WriterId(77);
    let day = Day::from_ms(T);
    let header = Header {
        family: Family::InvocationChunk,
        writer,
        day,
        seq: 0,
        base_ms: T,
    };
    let enc = container::encode(&header, records, summary);
    let key = root().chunk(day, &inv.key, writer, 0);
    life.objects.replace(key.as_str(), enc.bytes.clone());
    let row = ChunkRow {
        day: day.number(),
        bytes: if row.bytes == u64::MAX {
            0
        } else {
            enc.bytes.len() as u64
        },
        footer_offset: enc.footer_offset,
        footer_len: if row.footer_len == u32::MAX {
            0
        } else {
            enc.footer_len
        },
        ..row
    };
    let stream = StreamId(0x7F_0000);
    life.index.put(
        &IndexRoot::default().ichunk(&inv.key, writer, 0),
        row.encode_to_vec(),
    );
    let s = StreamRow {
        inv_key: inv.key.as_str().into(),
        writer: writer.0,
    };
    life.index.put(
        &IndexRoot::default().stream(day, stream, T),
        s.encode_to_vec(),
    );
    life.index.put(
        &IndexRoot::default().invid(&inv.key),
        listing(&["a"]).encode_to_vec(),
    );
    (inv, RunId::new(T, stream, 0))
}

fn holds_run0() -> ChunkRow {
    ChunkRow {
        stream: 0x7F_0000,
        first_seq: 0,
        last_seq: 0,
        runs: 1,
        ..ChunkRow::default()
    }
}

#[test]
fn records_that_are_not_chunk_records_are_errors() {
    // Catches: a record that does not decode, or decodes to nothing, skipped silently.
    let life = Life::fresh(Limits::default());
    let key = chunk_key(SECTION_RUN, 0).to_vec();
    let (inv, id) = plant(&life, vec![(key.clone(), vec![0xFF])], holds_run0());
    let reader = life.reader();
    let err = whole(&reader, &inv.key, &all).unwrap_err();
    assert!(err.to_string().contains("chunk record"), "{err}");
    let err = futures::executor::block_on(reader.run(id, &all)).unwrap_err();
    assert!(err.to_string().contains("chunk record"), "{err}");
    let empty = ChunkRecord { body: None }.encode_to_vec();
    plant(&life, vec![(key, empty)], holds_run0());
    let err = whole(&reader, &inv.key, &all).unwrap_err();
    assert!(err.to_string().contains("empty record"), "{err}");
}

#[test]
fn a_chunk_row_that_disagrees_with_its_object_is_an_error() {
    // Catches: a row whose footer range or length is wrong read as an empty chunk, or
    // a chunk said to hold a run whose footer has no block for it.
    let life = Life::fresh(Limits::default());
    let reader = life.reader();
    let (_, id) = plant(&life, Vec::new(), holds_run0());
    let err = futures::executor::block_on(reader.run(id, &all)).unwrap_err();
    assert!(err.to_string().contains("block"), "{err}");
    let zero_footer = ChunkRow {
        footer_len: u32::MAX,
        ..holds_run0()
    };
    let (_, id) = plant(&life, Vec::new(), zero_footer);
    assert!(futures::executor::block_on(reader.run(id, &all)).is_err());
    let zero_bytes = ChunkRow {
        bytes: u64::MAX,
        ..holds_run0()
    };
    let (inv, _) = plant(&life, Vec::new(), zero_bytes);
    let err = whole(&reader, &inv.key, &all).unwrap_err();
    assert!(err.to_string().contains("chunk row"), "{err}");
}

#[test]
fn an_invocation_reads_its_header_and_newest_summary_in_one_range_read() {
    // Catches: `/v1/invocations/{id}` answered by reading every chunk of the build
    // whole (23 to 80 GETs and every record resident for a large build), or a summary
    // taken from a chunk other than the newest.
    let mut life = Life::fresh(Limits {
        chunk_bytes: 1,
        ..Limits::default()
    });
    let (inv, _) = closed_build(&mut life);
    let reader = life.reader();
    let before = life.objects.counts().gets.load(Ordering::SeqCst);
    let view = futures::executor::block_on(reader.invocation(&inv.key, &all))
        .unwrap()
        .unwrap();
    assert_eq!(
        life.objects.counts().gets.load(Ordering::SeqCst) - before,
        1
    );
    let newest = view.newest.unwrap();
    assert_eq!(
        newest.row.unwrap().last_ms,
        B + 4,
        "the chunk of the last record"
    );
    assert_eq!(newest.header.unwrap().hits, 2);
    assert!(view.complete);
    // An invocation with no chunk (every record shed) has no summary to read.
    let bare = InvRef::named("bare");
    let header = InvocationHeader {
        state: kbf_history::records::InvState::Closed.into(),
        ..listing(&["a"])
    };
    life.index.put(
        &IndexRoot::default().invid(&bare.key),
        header.encode_to_vec(),
    );
    let before = life.objects.counts().gets.load(Ordering::SeqCst);
    let view = futures::executor::block_on(reader.invocation(&bare.key, &all))
        .unwrap()
        .unwrap();
    assert_eq!((view.newest, view.complete), (None, true));
    assert_eq!(life.objects.counts().gets.load(Ordering::SeqCst), before);
}

#[test]
fn records_come_a_bounded_page_of_chunks_at_a_time() {
    // Catches: an invocation's records read in one answer however many chunks it has
    // (an id-less pseudo-invocation holds a writer's whole day), a page past the
    // chunk bound, and a cursor that skips or repeats a chunk.
    let mut life = Life::fresh(Limits {
        chunk_bytes: 1,
        ..Limits::default()
    });
    let inv = InvRef::named("big");
    let runs: Vec<_> = (0..40)
        .map(|n| life.run(&inv, B + n, "a", RunOutcome::Ok))
        .collect();
    life.close(&inv);
    life.drain(B + 40, 5);
    assert_eq!(life.index.keys("ui/ichunk/").len(), 40);
    let reader = life.reader();
    let gets = || life.objects.counts().gets.load(Ordering::SeqCst);
    let before = gets();
    let first = futures::executor::block_on(reader.records(&inv.key, None, 1000, &all))
        .unwrap()
        .unwrap();
    assert_eq!(
        first.runs.len(),
        MAX_PAGE_CHUNKS,
        "one run a chunk, 32 chunks"
    );
    assert_eq!(gets() - before, MAX_PAGE_CHUNKS as u64);
    let cursor = first.next.unwrap();
    let second = futures::executor::block_on(reader.records(&inv.key, Some(&cursor), 5, &all))
        .unwrap()
        .unwrap();
    assert_eq!(second.runs.len(), 5);
    let third =
        futures::executor::block_on(reader.records(&inv.key, second.next.as_ref(), 0, &all))
            .unwrap()
            .unwrap();
    assert_eq!(third.runs.len(), 1, "a page reads at least one chunk");
    let rest = futures::executor::block_on(reader.records(
        &inv.key,
        third.next.as_ref(),
        MAX_PAGE_CHUNKS,
        &all,
    ))
    .unwrap()
    .unwrap();
    assert_eq!(rest.next, None, "the last page says so");
    let all_runs: Vec<_> = [first.runs, second.runs, third.runs, rest.runs].concat();
    assert_eq!(all_runs, runs);
}

#[test]
fn a_newest_summary_that_does_not_decode_is_an_error() {
    // Catches: a broken summary section read as an invocation with no summary.
    let life = Life::fresh(Limits::default());
    let (inv, _) = plant_with(&life, Vec::new(), holds_run0(), vec![0xFF]);
    let err = futures::executor::block_on(life.reader().invocation(&inv.key, &all)).unwrap_err();
    assert!(err.to_string().contains("chunk summary"), "{err}");
}

#[test]
fn a_run_resolves_when_events_arrived_out_of_order_across_a_cut() {
    // Catches: a run located in the first chunk whose sequence range holds it, and no
    // other tried. Minting happens on the request path and the offer to the queue is a
    // separate step, so a later run can land in an earlier chunk: here runs 0 and 2 are
    // cut together by age ([0..2]) and run 1 lands in the next chunk, so the first
    // chunk's range holds run 1 and only the second chunk does.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    let runs: Vec<_> = (0..3)
        .map(|i| {
            let m = life.minter.mint(&inv.key, B + i);
            let mut r = crate::support::run_record("a", RunOutcome::Ok, m.id.time_ms());
            r.set_run_id(m.id);
            (m, r)
        })
        .collect();
    let queue = life.queue.clone();
    let offer = |n: usize| {
        let (m, r) = &runs[n];
        assert!(queue.offer(kbf_history::Event::Run {
            inv: inv.clone(),
            record: Box::new(r.clone()),
            failed: None,
            stream_opened_ms: m.stream_opened_ms,
        }));
    };
    offer(0);
    offer(2);
    life.step(B + 2);
    life.step(B + 120_002); // cut by age: runs 0 and 2
    offer(1);
    life.close(&inv);
    life.drain(B + 120_003, 10);
    assert_eq!(life.index.keys("ui/ichunk/").len(), 2);
    let reader = life.reader();
    for (_, r) in &runs {
        let got = futures::executor::block_on(reader.run(r.run_id(), &all)).unwrap();
        assert_eq!(got.map(|v| v.run).as_ref(), Some(r), "{}", r.run_id());
    }
}

#[test]
fn a_newest_summary_ahead_of_its_header_is_all_must_allow_too() {
    // Catches: an invocation authorized against the stored header alone while its
    // newest chunk's summary, which can land first, names another instance: an
    // A-only caller would get B in `newest.header.instances`, B's run ids in the
    // slowest lists and B's action digests. A previous life stored the header [a]; this
    // life writes a chunk of instance b while its header read fails, so the chunk row
    // lands and the header write waits.
    let objects = crate::support::Objects::new();
    let index = crate::support::Index::fake();
    let mut first = Life::start(&objects, &index, 1, Limits::default());
    first.step(B); // reserve a stream block: the refill is a step's transaction
    let inv = InvRef::named("b1");
    first.run(&inv, B, "a", RunOutcome::Ok);
    first.close(&inv);
    first.step(B);
    let mut second = Life::start(&objects, &index, 2, Limits::default());
    second.step(B);
    index.fail_gets.store(true, Ordering::SeqCst);
    second.run(&inv, B + 1, "b", RunOutcome::Ok);
    second.close(&inv);
    second.step(B + 1);
    index.fail_gets.store(false, Ordering::SeqCst);
    assert_eq!(index.keys("ui/ichunk/").len(), 2, "the chunk row landed");
    let reader = second.reader();
    let view = futures::executor::block_on(reader.invocation(&inv.key, &only_a)).unwrap();
    assert_eq!(view, None);
    let view = futures::executor::block_on(reader.invocation(&inv.key, &all))
        .unwrap()
        .unwrap();
    assert_eq!(view.header.instances, ["a"], "the header still lags");
    let newest = view.newest.unwrap().header.unwrap();
    assert_eq!(newest.instances, ["b"]);
}

#[test]
fn a_header_that_lists_no_instance_or_a_truncated_list_answers_no_caller() {
    // Catches: all-must-allow taken over an empty list (true of every caller), so a
    // header of no instance (a Close with no records, by an earlier writer) hands its
    // client record id to a caller allowed on nothing; and a list the writer capped
    // read as whole, so a caller allowed on the listed instances sees the rest.
    let life = Life::fresh(Limits::default());
    let reader = life.reader();
    let nothing = |_: &str| false;
    let bare = InvRef::named("bare");
    let header = InvocationHeader {
        record_id: "bare".into(),
        ..InvocationHeader::default()
    };
    life.index.put(
        &IndexRoot::default().invid(&bare.key),
        header.encode_to_vec(),
    );
    for allow in [&nothing as &dyn Fn(&str) -> bool, &all] {
        let view = futures::executor::block_on(reader.invocation(&bare.key, allow));
        assert_eq!(view.unwrap(), None);
        let page = futures::executor::block_on(reader.records(&bare.key, None, 1, allow));
        assert_eq!(page.unwrap(), None);
    }
    let wide = InvRef::named("wide");
    let header = InvocationHeader {
        instances_truncated: true,
        ..listing(&["a"])
    };
    life.index.put(
        &IndexRoot::default().invid(&wide.key),
        header.encode_to_vec(),
    );
    let view = futures::executor::block_on(reader.invocation(&wide.key, &all));
    assert_eq!(view.unwrap(), None);
    let page = futures::executor::block_on(reader.records(&wide.key, None, 1, &all));
    assert_eq!(page.unwrap(), None);
}

#[test]
fn a_newest_summary_whose_instances_cannot_be_checked_is_refused() {
    // Catches: a newest summary with no header, an empty instance list or a truncated
    // one passed as allowed: its slowest runs and digests would go to a caller the
    // summary's own instances were never checked against.
    let life = Life::fresh(Limits::default());
    let reader = life.reader();
    let unchecked = [
        None,
        Some(listing(&[])),
        Some(InvocationHeader {
            instances_truncated: true,
            ..listing(&["a"])
        }),
    ];
    for header in unchecked {
        let summary = ChunkSummary {
            header: header.clone(),
            ..ChunkSummary::default()
        };
        let (inv, _) = plant_with(&life, Vec::new(), holds_run0(), summary.encode_to_vec());
        let view = futures::executor::block_on(reader.invocation(&inv.key, &all)).unwrap();
        assert_eq!(view, None, "{header:?}");
    }
    let (inv, _) = plant(&life, Vec::new(), holds_run0());
    let view = futures::executor::block_on(reader.invocation(&inv.key, &all)).unwrap();
    assert!(view.is_some(), "a checked summary of instance a");
}

#[test]
fn a_records_cursor_round_trips_as_writer_and_sequence_text() {
    // Catches: a cursor with no text form (a page could not be resumed over HTTP), one
    // whose text carries the index's key prefix, one that does not resume where its
    // page ended, and malformed text taken as some cursor rather than refused.
    let mut life = Life::fresh(Limits {
        chunk_bytes: 1,
        ..Limits::default()
    });
    let inv = InvRef::named("big");
    for n in 0..40 {
        life.run(&inv, B + n, "a", RunOutcome::Ok);
    }
    life.close(&inv);
    life.drain(B + 40, 10);
    let reader = life.reader();
    let first = futures::executor::block_on(reader.records(&inv.key, None, MAX_PAGE_CHUNKS, &all))
        .unwrap()
        .unwrap();
    let cursor = first.next.unwrap();
    let text = cursor.to_string();
    assert_eq!(text, "0000000000000001-0000001f", "writer 1, chunk 31");
    let parsed: ChunkCursor = text.parse().unwrap();
    assert_eq!(parsed, cursor);
    let resumed = futures::executor::block_on(reader.records(&inv.key, Some(&parsed), 1, &all))
        .unwrap()
        .unwrap();
    assert_eq!(resumed.runs.len(), 1);
    assert_eq!(resumed.runs[0].seq, 32, "the chunk after the page");
    let malformed = [
        "",
        "-",
        "0000000000000001",
        "0000000000000001-",
        "0000000000000001-0000001",
        "0000000000000001-0000001f0",
        "000000000000001-0000001f",
        "0000000000000001-0000001F",
        "000000000000000g-0000001f",
        "0000000000000001_0000001f",
        "0000000000000001-0000001f-",
        "ui/ichunk/0000000000000001/0000001f",
    ];
    for bad in malformed {
        let err = bad.parse::<ChunkCursor>().unwrap_err();
        assert!(
            err.to_string().contains("not a records cursor"),
            "{bad}: {err}"
        );
    }
}

#[test]
fn an_ichunk_row_key_that_names_no_chunk_is_an_error() {
    // Catches: a row key whose writer or sequence does not parse read as chunk 0 of
    // writer 0 (or any chunk), rather than reported as the broken row it is.
    let mut life = Life::fresh(Limits::default());
    let (inv, _) = closed_build(&mut life);
    let row_key = life
        .index
        .keys(&IndexRoot::default().ichunk_prefix(&inv.key))
        .remove(0);
    let value = life.index.inner.get(&row_key).unwrap().unwrap().value;
    let bad = format!(
        "{}0000000000000001/zz",
        IndexRoot::default().ichunk_prefix(&inv.key)
    );
    life.index.put(&bad, value);
    let err = whole(&life.reader(), &inv.key, &all).unwrap_err();
    assert!(matches!(err, ReadError::Row { .. }), "{err}");
    assert!(
        err.to_string().contains("names no writer and sequence"),
        "{err}"
    );
}
