//! Failure-log copies: a failed run's logs written to their own object, the row that
//! names it written only after it, and ranges of a log read back from the blocks that
//! hold them. Each test names the defect it catches.

use std::sync::atomic::Ordering;

use kbf_history::container::{self, Header};
use kbf_history::flog::PIECE_BYTES;
use kbf_history::records::{FlogRecord, FlogStream, FlogSummary, ObjRow, flog_key};
use kbf_history::{
    Day, Event, Family, FlogParts, FlogRef, FlogStore, Limits, ReadError, RunId, StreamId, WriterId,
};
use kbf_objstore::ObjectStoreError;
use prost::Message;

use crate::support::{Life, Objects, T, all, root};

fn id(n: u32) -> RunId {
    RunId::new(T + 5, StreamId(0x80_0001), n)
}

/// `len` bytes that tell any two places of a log apart.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

fn store(life: &Life) -> FlogStore<Objects> {
    FlogStore::new(life.objects.clone(), root(), life.writer_id)
}

fn parts<'a>(
    stdout: Option<&'a [u8]>,
    stderr: Option<&'a [u8]>,
    result: Option<&'a [u8]>,
) -> FlogParts<'a> {
    FlogParts {
        instance: "main",
        stdout,
        stderr,
        result,
    }
}

/// Writes the object for `run` and returns its ref, without offering it to the writer.
fn put(life: &Life, run: RunId, parts: &FlogParts<'_>) -> FlogRef {
    let s = store(life);
    futures::executor::block_on(s.put(s.encode(run, parts))).unwrap()
}

/// Writes the object and offers its ref, as the server's tail worker does.
fn land(life: &Life, run: RunId, parts: &FlogParts<'_>) -> FlogRef {
    let r = put(life, run, parts);
    assert!(life.queue.offer(Event::Flog(r.clone())));
    r
}

fn read(life: &Life, run: RunId, stream: FlogStream, first: u64, last: u64) -> Vec<u8> {
    let reader = life.reader();
    let log = futures::executor::block_on(reader.failure_log(run, stream, &all))
        .unwrap()
        .expect("the log is held");
    futures::executor::block_on(reader.read_log(&log, first, last))
        .unwrap()
        .to_vec()
}

#[test]
fn a_failure_log_reads_back_whole_and_by_range_from_its_pieces() {
    // Catches: a piece boundary read off by one (a range that starts or ends on one), a
    // range that returns bytes of another stream or piece, a length that is not the
    // stream's, and a clamp that lets a last byte past the end through.
    let mut life = Life::fresh(Limits::default());
    let out = pattern(PIECE_BYTES * 2 + 1234);
    let err = pattern(100);
    let result = b"an ActionResult";
    let run = id(1);
    land(&life, run, &parts(Some(&out), Some(&err), Some(result)));
    life.drain(T + 10, 5);
    let n = out.len() as u64;
    assert_eq!(read(&life, run, FlogStream::Stdout, 0, u64::MAX), out);
    assert_eq!(read(&life, run, FlogStream::Stderr, 0, 99), err);
    assert_eq!(read(&life, run, FlogStream::Result, 0, 1 << 20), result);
    let piece = PIECE_BYTES as u64;
    for (first, last) in [
        (0, 0),
        (n - 1, n - 1),
        (piece - 1, piece),
        (piece, piece),
        (piece - 1, piece - 1),
        (piece + 5, 2 * piece - 1),
        (3, 2 * piece + 10),
        (2 * piece, n - 1),
    ] {
        assert_eq!(
            read(&life, run, FlogStream::Stdout, first, last),
            out[first as usize..=last as usize],
            "bytes {first}-{last}"
        );
    }
    // A range at or past the end, or backwards, holds nothing.
    for (first, last) in [(n, n + 5), (n + 100, u64::MAX), (7, 3)] {
        assert!(read(&life, run, FlogStream::Stdout, first, last).is_empty());
    }
    let reader = life.reader();
    let log = futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &all))
        .unwrap()
        .unwrap();
    assert_eq!((log.len(), log.is_empty()), (n, false));
    assert_eq!((log.instance(), log.stream()), ("main", FlogStream::Stdout));
}

#[test]
fn a_log_the_object_does_not_hold_is_not_found_and_an_empty_one_is_empty() {
    // Catches: a stream the run had no log for answered with another stream's bytes or
    // an error, and an empty log mistaken for a missing one.
    let mut life = Life::fresh(Limits::default());
    let run = id(2);
    land(&life, run, &parts(Some(b""), None, Some(b"r")));
    life.drain(T + 10, 5);
    let reader = life.reader();
    let find = |stream| futures::executor::block_on(reader.failure_log(run, stream, &all)).unwrap();
    assert!(find(FlogStream::Stderr).is_none());
    let empty = find(FlogStream::Stdout).expect("an empty log is held");
    assert_eq!((empty.len(), empty.is_empty()), (0, true));
    assert!(
        futures::executor::block_on(reader.read_log(&empty, 0, 10))
            .unwrap()
            .is_empty()
    );
    assert!(find(FlogStream::Result).is_some());
    // A run with no object at all.
    assert!(
        futures::executor::block_on(reader.failure_log(id(99), FlogStream::Stdout, &all))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_failure_log_is_refused_for_an_instance_the_caller_may_not_read() {
    // Catches: a copy served to a caller allowed on other instances only (the object
    // names the run's instance, and the reader asks `allow` before it reads a byte), and
    // an object that names no instance served to anyone.
    let mut life = Life::fresh(Limits::default());
    let run = id(3);
    land(&life, run, &parts(Some(b"secret"), None, None));
    let anonymous = id(4);
    let s = store(&life);
    let nameless = FlogParts {
        instance: "",
        ..parts(Some(b"x"), None, None)
    };
    let r = futures::executor::block_on(s.put(s.encode(anonymous, &nameless))).unwrap();
    life.queue.offer(Event::Flog(r));
    life.drain(T + 10, 5);
    let reader = life.reader();
    let other = |i: &str| i == "other";
    assert!(
        futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &other))
            .unwrap()
            .is_none()
    );
    let main = |i: &str| i == "main";
    assert!(
        futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &main))
            .unwrap()
            .is_some()
    );
    assert!(
        futures::executor::block_on(reader.failure_log(anonymous, FlogStream::Stdout, &all))
            .unwrap()
            .is_none()
    );
}

#[test]
fn no_flog_row_is_written_before_the_object_or_without_it() {
    // Catches: a row written before its object (a crash, or a failed PUT, would leave a
    // row naming an object that was never written, and a read of it fail for ever), and a
    // row the writer makes with no object put (only a returned ref makes a row).
    let mut life = Life::fresh(Limits::default());
    let run = id(5);
    let s = store(&life);
    // A PUT that fails writes nothing and leaves nothing to offer.
    life.objects.counts().fail_puts.store(1, Ordering::SeqCst);
    let err = futures::executor::block_on(s.put(s.encode(run, &parts(Some(b"x"), None, None))))
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err}");
    life.drain(T + 10, 3);
    assert!(life.objects.keys().is_empty(), "{:?}", life.objects.keys());
    assert!(life.index.keys("ui/obj/").is_empty());
    // A crash after the PUT and before the row: an orphan object, no row, no read.
    let r = put(&life, run, &parts(Some(b"x"), None, None));
    assert_eq!(life.objects.keys(), [r.key.as_str().to_owned()]);
    life.step(T + 11);
    assert!(life.index.keys("ui/obj/").is_empty());
    let reader = life.reader();
    assert!(
        futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &all))
            .unwrap()
            .is_none()
    );
    // The row follows the ref, under the class and family of its object.
    life.queue.offer(Event::Flog(r.clone()));
    life.drain(T + 12, 3);
    let rows = life.index.keys("ui/obj/");
    assert_eq!(rows, [format!("ui/obj/flog/20261016/{run}")]);
    assert_eq!(
        r.key.as_str(),
        format!("kbf/history/r30/flog/20261016/{run}")
    );
}

#[test]
fn a_put_again_after_a_lost_acknowledgement_is_the_same_success() {
    // Catches: a retried PUT whose acknowledgement was lost (the object landed) failing
    // the copy as already existing, and a second copy under another key.
    let life = Life::fresh(Limits::default());
    let run = id(6);
    life.objects.counts().lose_acks.store(1, Ordering::SeqCst);
    let s = store(&life);
    let p = parts(Some(b"x"), None, None);
    assert!(futures::executor::block_on(s.put(s.encode(run, &p))).is_err());
    let again = futures::executor::block_on(s.put(s.encode(run, &p)));
    // The memory store answers a second write of a held key as already existing.
    assert!(again.is_ok(), "{again:?}");
    assert_eq!(life.objects.keys().len(), 1);
}

#[test]
fn a_dropped_flog_row_comes_back_from_the_object_with_no_list() {
    // Catches: a row lost with a failed transaction never written again (the copy
    // would be unreachable), a restore that LISTs, and a restored row that points
    // anywhere but the footer of the object.
    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserve a stream block, so the failure hits the row
    let run = id(7);
    let out = pattern(PIECE_BYTES + 10);
    let r = land(&life, run, &parts(Some(&out), None, None));
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T + 10);
    assert_eq!(life.writer.kept_objects(), 1, "the object is kept");
    assert!(life.index.keys("ui/obj/").is_empty());
    life.drain(T + 11, 10);
    let row = life
        .index
        .inner
        .get(&format!("ui/obj/flog/20261016/{run}"))
        .unwrap()
        .unwrap();
    let row = ObjRow::decode(row.value.as_slice()).unwrap();
    assert_eq!(
        (row.footer_offset, row.footer_len, row.bytes),
        (r.footer_offset, r.footer_len, r.bytes)
    );
    assert_eq!(life.objects.counts().lists.load(Ordering::SeqCst), 0);
    assert_eq!(read(&life, run, FlogStream::Stdout, 0, u64::MAX), out);
}

/// A kept flog object that is not a good one, restored: the writer queues no row and
/// goes on.
fn restored_from(summary: Vec<u8>, name: &str) {
    restored_as(Family::FailureLog, summary, name);
}

/// [`restored_from`], for an object whose header and trailer say it is of `family`.
fn restored_as(family: Family, summary: Vec<u8>, name: &str) {
    let mut life = Life::fresh(Limits::default());
    life.step(T);
    let run = id(8);
    let header = Header {
        family,
        writer: WriterId(1),
        day: Day::from_number(20_261_016),
        seq: 0,
        base_ms: T,
    };
    let encoded = container::encode(&header, Vec::new(), summary);
    let key = root().flog(run);
    life.objects.replace(key.as_str(), encoded.bytes.clone());
    let r = FlogRef {
        run,
        key,
        footer_offset: encoded.footer_offset,
        footer_len: encoded.footer_len,
        bytes: encoded.bytes.len() as u64,
    };
    life.queue.offer(Event::Flog(r));
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T + 10);
    assert_eq!(life.writer.kept_objects(), 1, "{name}");
    life.drain(T + 11, 10);
    assert!(life.index.keys("ui/obj/").is_empty(), "{name}");
}

#[test]
fn a_kept_chunk_object_that_does_not_describe_a_chunk_restores_no_row() {
    // Catches: a restore that dispatches on the object's family wrongly (a chunk's
    // summary read as a failure log's, or the reverse), and one that panics on, or writes
    // rows for, a chunk whose summary does not decode or names no invocation.
    restored_as(
        Family::InvocationChunk,
        vec![0xff],
        "a summary that does not decode",
    );
    restored_as(
        Family::InvocationChunk,
        kbf_history::records::ChunkSummary::default().encode_to_vec(),
        "a summary that names no invocation",
    );
}

#[test]
fn a_kept_flog_object_that_does_not_describe_a_run_restores_no_row() {
    // Catches: a restore that panics on, or writes a row for, an object whose summary
    // does not decode or names no run: such a row would point a read at someone
    // else's object.
    restored_from(vec![0xff], "a summary that does not decode");
    restored_from(
        FlogSummary {
            run_id: vec![1, 2, 3],
            day: 20_261_016,
            instance: "main".into(),
            stdout_len: None,
            stderr_len: None,
            result_len: None,
        }
        .encode_to_vec(),
        "a run id of three bytes",
    );
}

#[test]
fn a_kept_object_whose_footer_does_not_decode_is_left_to_the_sweep() {
    // Catches: a restore that retries a footer that can never decode for ever (the
    // kept list would hold it and block the others), or that panics.
    let mut life = Life::fresh(Limits::default());
    life.step(T);
    let run = id(9);
    let key = root().flog(run);
    life.objects.replace(key.as_str(), vec![0; 64]);
    let r = FlogRef {
        run,
        key,
        footer_offset: 0,
        footer_len: 64,
        bytes: 64,
    };
    life.queue.offer(Event::Flog(r));
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T + 10);
    assert_eq!(life.writer.kept_objects(), 1);
    life.drain(T + 11, 10);
    assert!(life.index.keys("ui/obj/").is_empty());
}

/// A failure-log object made by hand from `records`, with its row, for the reader's
/// errors.
fn handmade(life: &Life, run: RunId, records: &[(FlogStream, u64, Vec<u8>)], len: u64) {
    let header = Header {
        family: Family::FailureLog,
        writer: WriterId(1),
        day: Day::from_number(20_261_016),
        seq: 0,
        base_ms: T,
    };
    let records = records
        .iter()
        .map(|(stream, offset, bytes)| {
            let record = FlogRecord {
                stream: (*stream).into(),
                offset: *offset,
                bytes: bytes.clone(),
            };
            (flog_key(*stream, *offset).to_vec(), record.encode_to_vec())
        })
        .collect();
    let summary = FlogSummary {
        run_id: run.to_bytes().to_vec(),
        day: 20_261_016,
        instance: "main".into(),
        stdout_len: Some(len),
        stderr_len: None,
        result_len: None,
    };
    let encoded = container::encode(&header, records, summary.encode_to_vec());
    life.objects
        .replace(root().flog(run).as_str(), encoded.bytes.clone());
    let row = ObjRow {
        footer_offset: encoded.footer_offset,
        footer_len: encoded.footer_len,
        bytes: encoded.bytes.len() as u64,
    };
    life.index
        .put(&format!("ui/obj/flog/20261016/{run}"), row.encode_to_vec());
}

fn read_err(life: &Life, run: RunId, first: u64, last: u64) -> ReadError {
    let reader = life.reader();
    let log = futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &all))
        .unwrap()
        .unwrap();
    futures::executor::block_on(reader.read_log(&log, first, last)).unwrap_err()
}

#[test]
fn a_broken_failure_log_object_is_an_error_not_wrong_bytes() {
    // Catches: an object whose pieces leave a hole, or end before its summary's length,
    // served as a shorter or shifted log; a piece of another stream mixed in; a block
    // the footer does not index served as nothing.
    let life = Life::fresh(Limits::default());
    let piece = PIECE_BYTES as u64;
    // The second piece is missing: bytes piece..2 * piece are a hole.
    let run = id(10);
    handmade(
        &life,
        run,
        &[
            (FlogStream::Stdout, 0, vec![1; PIECE_BYTES]),
            (FlogStream::Stdout, 2 * piece, vec![3; PIECE_BYTES]),
        ],
        3 * piece,
    );
    let e = read_err(&life, run, 0, 2 * piece + 5);
    assert!(
        e.to_string().contains("disagree on the failure-log pieces"),
        "{e}"
    );
    // The summary says more bytes than the pieces hold.
    let run = id(11);
    handmade(&life, run, &[(FlogStream::Stdout, 0, vec![1; 10])], 50);
    let e = read_err(&life, run, 0, 49);
    assert!(e.to_string().contains("failure log"), "{e}");
    // A piece of stderr in the object is not stdout's.
    let run = id(12);
    handmade(
        &life,
        run,
        &[
            (FlogStream::Stdout, 0, vec![1; 10]),
            (FlogStream::Stderr, 0, vec![2; 10]),
        ],
        10,
    );
    let reader = life.reader();
    let log = futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &all))
        .unwrap()
        .unwrap();
    let got = futures::executor::block_on(reader.read_log(&log, 0, 9)).unwrap();
    assert_eq!(got.to_vec(), vec![1; 10]);
    // Several pieces in one block: the ones outside the range are skipped, the one it
    // starts in is cut at its start, and none is read twice.
    let run = id(14);
    handmade(
        &life,
        run,
        &[
            (FlogStream::Stdout, 0, vec![1; 10]),
            (FlogStream::Stdout, 10, vec![2; 10]),
            (FlogStream::Stdout, 20, vec![3; 10]),
        ],
        30,
    );
    let log = futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &all))
        .unwrap()
        .unwrap();
    let got = futures::executor::block_on(reader.read_log(&log, 8, 21)).unwrap();
    let want: Vec<u8> = [vec![1; 2], vec![2; 10], vec![3; 2]].concat();
    assert_eq!(got.to_vec(), want);
    // A range that starts past the first piece skips it, and ends before the last.
    let got = futures::executor::block_on(reader.read_log(&log, 12, 15)).unwrap();
    assert_eq!(got.to_vec(), vec![2; 4]);
    // No block holds the key: the summary names a length over an object with no pieces.
    let run = id(13);
    handmade(&life, run, &[], 10);
    let e = read_err(&life, run, 0, 9);
    assert!(matches!(e, ReadError::Format(_)), "{e}");
}

#[test]
fn a_failure_log_row_or_object_that_is_wrong_is_an_error() {
    // Catches: a row naming another run's object (a copy served under the wrong run id),
    // a row that does not decode, an object that was expired, a footer range of zero
    // bytes and a store that fails, each answered as no log or as bytes.
    let mut life = Life::fresh(Limits::default());
    let (a, b) = (id(20), id(21));
    land(&life, a, &parts(Some(b"a's log"), None, None));
    life.drain(T + 10, 5);
    let reader = life.reader();
    let find = |run| futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &all));
    // b's row names a's object (a's footer range, b's key).
    let row = life
        .index
        .inner
        .get(&format!("ui/obj/flog/20261016/{a}"))
        .unwrap()
        .unwrap();
    life.index
        .put(&format!("ui/obj/flog/20261016/{b}"), row.value.clone());
    life.objects.replace(
        root().flog(b).as_str(),
        life.objects.get(root().flog(a).as_str()).to_vec(),
    );
    let e = find(b).unwrap_err();
    assert!(
        e.to_string().contains("disagree on the failure-log run"),
        "{e}"
    );
    // A row that is not an ObjRow.
    let c = id(22);
    life.index
        .put(&format!("ui/obj/flog/20261016/{c}"), vec![0xff, 0xff]);
    assert!(matches!(find(c).unwrap_err(), ReadError::Row { .. }));
    // A zero-length footer range reads a byte and fails to decode.
    let d = id(23);
    let zero = ObjRow {
        footer_offset: 0,
        footer_len: 0,
        bytes: 0,
    };
    life.index
        .put(&format!("ui/obj/flog/20261016/{d}"), zero.encode_to_vec());
    life.objects.replace(root().flog(d).as_str(), vec![0; 64]);
    assert!(matches!(find(d).unwrap_err(), ReadError::Format(_)));
    // An expired object.
    life.objects.remove(root().flog(a).as_str());
    assert!(matches!(find(a).unwrap_err(), ReadError::Object(_)));
    // A summary that does not decode.
    let e = id(24);
    restored_summary_row(&life, e, vec![0xff]);
    assert!(matches!(find(e).unwrap_err(), ReadError::Format(_)));
    // A store that fails the read.
    let f = id(25);
    land(&life, f, &parts(Some(b"f"), None, None));
    life.drain(T + 11, 5);
    life.objects.counts().fail_gets.store(1, Ordering::SeqCst);
    assert!(matches!(find(f).unwrap_err(), ReadError::Object(_)));
}

/// A row and an object for `run` whose footer carries `summary` as it is.
fn restored_summary_row(life: &Life, run: RunId, summary: Vec<u8>) {
    let header = Header {
        family: Family::FailureLog,
        writer: WriterId(1),
        day: Day::from_number(20_261_016),
        seq: 0,
        base_ms: T,
    };
    let encoded = container::encode(&header, Vec::new(), summary);
    life.objects
        .replace(root().flog(run).as_str(), encoded.bytes.clone());
    let row = ObjRow {
        footer_offset: encoded.footer_offset,
        footer_len: encoded.footer_len,
        bytes: encoded.bytes.len() as u64,
    };
    life.index
        .put(&format!("ui/obj/flog/20261016/{run}"), row.encode_to_vec());
}

#[test]
fn a_flog_event_belongs_to_no_invocation_and_opens_none() {
    // Catches: a failure-log ref that opens an invocation in the writer (it would hold
    // state, read a header and count against the invocation bound for a run with no
    // chunk), and an event whose size grows the queue's byte count past its fixed share.
    let mut life = Life::fresh(Limits::default());
    let r = put(&life, id(30), &parts(Some(b"x"), None, None));
    let event = Event::Flog(r);
    assert!(event.inv().is_none());
    assert_eq!(event.size(), 64);
    life.queue.offer(event);
    life.step(T + 10);
    assert_eq!(life.writer.open_invocations(), 0);
    assert_eq!(life.writer.drops().invocations, 0);
}

#[test]
fn a_parts_with_nothing_is_empty_and_a_log_is_cut_into_pieces() {
    // Catches: an empty set of parts that is not told so (the caller would put an object
    // with nothing in it for every failed run), and a stream that is not cut at the piece
    // size (a 16 MiB log would be one 16 MiB block, past what a reader inflates).
    assert!(FlogParts::default().is_empty());
    assert!(!parts(None, None, Some(b"")).is_empty());
    assert!(!parts(Some(b""), None, None).is_empty());
    assert!(!parts(None, Some(b""), None).is_empty());
    let life = Life::fresh(Limits::default());
    let out = pattern(PIECE_BYTES * 3);
    let s = store(&life);
    let object = s.encode(id(31), &parts(Some(&out), None, None));
    assert!(!object.is_empty());
    let length = object.len();
    assert_eq!(
        object.key().as_str(),
        format!("kbf/history/r30/flog/20261016/{}", id(31))
    );
    let stored = {
        let r = futures::executor::block_on(s.put(object)).unwrap();
        assert_eq!(
            r.bytes as usize, length,
            "the ref says how long the object is"
        );
        life.objects.get(r.key.as_str()).to_vec()
    };
    assert_eq!(stored.len(), length);
    let decoded = container::decode(&stored).unwrap();
    assert_eq!(decoded.records.len(), 3);
    assert_eq!(decoded.footer.blocks.len(), 3, "a block to a piece");
}

#[test]
fn pieces_that_share_a_block_are_read_with_one_get() {
    // Catches: a range over two pieces whose keys fall in one block reading that block
    // twice (a GET per piece, not per block), and the pieces of a block read in the wrong
    // order or twice into the answer.
    let life = Life::fresh(Limits::default());
    let run = id(15);
    handmade(
        &life,
        run,
        &[
            (FlogStream::Stdout, 0, vec![1; 65_000]),
            (FlogStream::Stdout, 65_000, vec![2; 100]),
        ],
        65_100,
    );
    let reader = life.reader();
    let log = futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &all))
        .unwrap()
        .unwrap();
    let gets = life.objects.counts().gets.load(Ordering::SeqCst);
    let got = futures::executor::block_on(reader.read_log(&log, 0, u64::MAX)).unwrap();
    let want: Vec<u8> = [vec![1; 65_000], vec![2; 100]].concat();
    assert_eq!(got.to_vec(), want);
    assert_eq!(life.objects.counts().gets.load(Ordering::SeqCst) - gets, 1);
}

#[test]
fn a_log_of_many_blocks_is_read_in_groups_and_comes_back_whole() {
    // Catches: more blocks than one group of parallel reads holds being cut short or read
    // out of order (34 pieces are two groups), and a range across the group boundary
    // losing a piece.
    let mut life = Life::fresh(Limits::default());
    let out = pattern(PIECE_BYTES * 34 + 7);
    let run = id(16);
    land(&life, run, &parts(Some(&out), None, None));
    life.drain(T + 10, 5);
    assert_eq!(read(&life, run, FlogStream::Stdout, 0, u64::MAX), out);
    let piece = PIECE_BYTES;
    let (first, last) = (piece * 31 + 100, piece * 33 + 100);
    assert_eq!(
        read(&life, run, FlogStream::Stdout, first as u64, last as u64),
        out[first..=last]
    );
}

#[test]
fn a_range_of_a_log_whose_blocks_fail_is_an_error_of_the_kind_that_failed() {
    // Catches: a block that cannot be fetched, one that fails its checksum and a piece that
    // does not decode answered as a shorter log, an empty one or a panic, and the kinds
    // mixed (the store's failure said to be the object's).
    let mut life = Life::fresh(Limits::default());
    let run = id(40);
    land(&life, run, &parts(Some(b"stdout bytes"), None, None));
    life.drain(T + 10, 5);
    let reader = life.reader();
    let log = futures::executor::block_on(reader.failure_log(run, FlogStream::Stdout, &all))
        .unwrap()
        .unwrap();
    // The store fails the read of the block (the footer was read already).
    life.objects.counts().fail_gets.store(1, Ordering::SeqCst);
    let e = futures::executor::block_on(reader.read_log(&log, 0, 5)).unwrap_err();
    assert!(matches!(e, ReadError::Object(_)), "{e}");
    // A bit flipped in the stored block: its CRC says so.
    let key = root().flog(run);
    let mut bytes = life.objects.get(key.as_str()).to_vec();
    bytes[container::HEADER_LEN + 3] ^= 0x01;
    life.objects.replace(key.as_str(), bytes);
    let e = futures::executor::block_on(reader.read_log(&log, 0, 5)).unwrap_err();
    assert!(matches!(e, ReadError::Format(_)), "{e}");
    assert!(e.to_string().contains("CRC"), "{e}");
    // A block whose record is not a piece.
    let other = id(41);
    let header = Header {
        family: Family::FailureLog,
        writer: WriterId(1),
        day: Day::from_number(20_261_016),
        seq: 0,
        base_ms: T,
    };
    let summary = FlogSummary {
        run_id: other.to_bytes().to_vec(),
        day: 20_261_016,
        instance: "main".into(),
        stdout_len: Some(4),
        stderr_len: None,
        result_len: None,
    };
    let records = vec![(flog_key(FlogStream::Stdout, 0).to_vec(), vec![0xff, 0xff])];
    let encoded = container::encode(&header, records, summary.encode_to_vec());
    life.objects
        .replace(root().flog(other).as_str(), encoded.bytes.clone());
    let row = ObjRow {
        footer_offset: encoded.footer_offset,
        footer_len: encoded.footer_len,
        bytes: encoded.bytes.len() as u64,
    };
    life.index.put(
        &format!("ui/obj/flog/20261016/{other}"),
        row.encode_to_vec(),
    );
    let e = read_err(&life, other, 0, 3);
    assert!(e.to_string().contains("failure-log record"), "{e}");
}
