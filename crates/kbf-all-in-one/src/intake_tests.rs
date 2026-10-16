//! The intake's batches: when they are sent, how many fly at once, and what is stored.
//! The walk's side (one read per file) is `tests/one_read.rs`.

use std::sync::mpsc;
use std::time::Duration;

use kbf_front::{Cache, MAX_UNSTAGED_BLOB_BYTES, SegmentsConfig};
use kbf_server::LocalStore;
use kbf_transfer::{RecordHasher, digest_of, proto_digest};

use super::*;

/// A runtime standing in for the server half, with one worker.
fn server() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime")
}

/// The plane over a new in-memory cache, its calls run on `server`.
fn plane(server: &tokio::runtime::Runtime) -> LocalDataPlane {
    let _context = server.enter();
    let store = LocalStore::new(Arc::new(Cache::memory()), SegmentsConfig::DEFAULT);
    LocalDataPlane::new(store, server.handle().clone())
}

/// Hands `bytes` to `taking` as one file, in two chunks, and finishes it.
fn take(taking: &Taking, bytes: &[u8]) -> kbf_types::Digest {
    let mut file = taking.file(bytes.len() as u64).expect("taken");
    let (first, rest) = bytes.split_at(bytes.len() / 2);
    file.write(first);
    file.write(rest);
    let mut hasher = RecordHasher::new();
    hasher.update(bytes);
    let (digest, crc32c) = hasher.finish();
    file.finish(digest, crc32c);
    digest
}

fn sent(taking: &Taking) -> usize {
    lock(&taking.0.state).uploads.len()
}

/// Catches a batch held until the walk ends, or sent before it is full, and files lost
/// at the end: with a 10-byte batch, a 4-byte file waits, a 6-byte one fills the batch
/// and both are sent at once, a 3-byte one waits for `finish`, which sends it and
/// answers once all three are held (mutants: the size never compared, `finish` not
/// sending the rest). Each file's bytes are the chunks handed over, in order.
#[test]
fn a_full_batch_is_sent_at_once_and_the_rest_at_the_finish() {
    let server = server();
    let plane = plane(&server);
    let taking = Taking::with(plane.clone(), 10, 4);
    let a = take(&taking, b"aaaa");
    assert_eq!(sent(&taking), 0);
    let b = take(&taking, b"bbbbbb");
    assert_eq!(sent(&taking), 1);
    let c = take(&taking, b"ccc");
    assert_eq!(sent(&taking), 1);
    server.block_on(taking.finish()).expect("stored");
    assert_eq!(sent(&taking), 0, "every upload awaited");
    server.block_on(async {
        for (digest, bytes) in [(a, &b"aaaa"[..]), (b, b"bbbbbb"), (c, b"ccc")] {
            assert_eq!(plane.get(&proto_digest(&digest)).await, Ok(bytes.to_vec()));
        }
    });
}

/// Catches a batch of many small files held until it reaches its size: 16,384 files
/// (the server's most records in one upload) are sent as one batch however few bytes
/// they hold (mutant: the record count not compared).
#[test]
fn a_batch_of_the_most_records_is_sent_however_small() {
    let server = server();
    let taking = Taking::with(plane(&server), u64::MAX, 4);
    for n in 0..MAX_UPLOAD_RECORDS - 1 {
        take(&taking, &(n as u64).to_le_bytes());
    }
    assert_eq!(sent(&taking), 0);
    let last = take(&taking, &u64::MAX.to_le_bytes());
    assert_eq!(sent(&taking), 1);
    server.block_on(taking.finish()).expect("stored");
    let plane = plane_of(&taking);
    server.block_on(async {
        assert!(plane.missing(&[proto_digest(&last)]).await.is_empty());
    });
}

fn plane_of(taking: &Taking) -> LocalDataPlane {
    taking.0.plane.clone()
}

/// Catches more batches in flight than the bound, and a walk that waits for a batch
/// that ended without being told: with one batch in flight at most, while the server
/// half's one worker is held the second file's finish does not return, and once the
/// worker is free it does (mutants: no wait, an ended upload not counted down or not
/// signalled, which hangs).
#[test]
fn a_walk_past_the_batches_in_flight_waits_for_one_to_end() {
    let server = server();
    let taking = Arc::new(Taking::with(plane(&server), 1, 1));
    let (hold, held) = mpsc::channel::<()>();
    let (started, running) = mpsc::channel::<()>();
    server.spawn(async move {
        started.send(()).expect("started");
        // Holds the server half's only worker: no upload runs meanwhile.
        let _ = held.recv();
    });
    running.recv().expect("the worker is held");
    take(&taking, b"first");
    assert_eq!(lock(&taking.0.state).in_flight, 1);
    let (done, finished) = mpsc::channel();
    let second = {
        let taking = Arc::clone(&taking);
        std::thread::spawn(move || {
            take(&taking, b"second");
            done.send(()).expect("done");
        })
    };
    assert!(
        finished.recv_timeout(Duration::from_millis(300)).is_err(),
        "the second file was finished with a batch in flight"
    );
    hold.send(()).expect("release the worker");
    finished
        .recv_timeout(Duration::from_secs(30))
        .expect("the second file is finished once the first batch ends");
    second.join().expect("joined");
    server.block_on(taking.finish()).expect("stored");
    assert_eq!(lock(&taking.0.state).in_flight, 0);
}

/// Catches one digest stored twice, and a file left unfinished stored anyway: two files
/// with the same bytes make one batch of one record, and a file handed over but never
/// finished (the walk found it changed) is in no batch (mutants: no `seen` check, an
/// unfinished file added on drop).
#[test]
fn one_digest_is_stored_once_and_an_unfinished_file_not_at_all() {
    let server = server();
    let taking = Taking::with(plane(&server), 1, 4);
    take(&taking, b"same");
    take(&taking, b"same");
    assert_eq!(sent(&taking), 1);
    let mut left = taking.file(4).expect("taken");
    left.write(b"left");
    drop(left);
    assert_eq!(sent(&taking), 1);
    server.block_on(taking.finish()).expect("stored");
    let plane = plane_of(&taking);
    server.block_on(async {
        let left = digest_of(b"left");
        assert_eq!(plane.missing(std::slice::from_ref(&left)).await, [left]);
    });
}

/// Catches a large file held whole in memory, or not taken (mutants: a file over the
/// threshold taken into memory, the refusal restored when there is no write tier): a
/// file of [`MAX_UNSTAGED_BLOB_BYTES`] is taken into memory, and one byte more is taken
/// as a stream, with a write tier and without one, however large.
#[test]
fn a_large_file_is_always_taken() {
    assert_eq!(MAX_UNSTAGED_BLOB_BYTES, 16 << 20);
    let server = server();
    let taking = Taking::new(plane(&server));
    assert!(taking.file(MAX_UNSTAGED_BLOB_BYTES).is_some());
    assert_eq!(sent(&taking), 0, "a small file starts no upload");
    assert!(taking.file(MAX_UNSTAGED_BLOB_BYTES + 1).is_some());
    assert!(taking.file((5 << 30) + 1).is_some());
    assert_eq!(sent(&taking), 2, "each large file streams to the store");
    server
        .block_on(taking.finish())
        .expect("both withdrawn, unfinished");
    let (plane, _handle) = tiered(&server, "intake-large");
    let taking = Taking::new(plane);
    assert!(taking.file(MAX_UNSTAGED_BLOB_BYTES + 1).is_some());
    assert!(taking.file((5 << 30) + 1).is_some());
}

/// Catches a batch whose server half is gone taken for stored, or hanging: the upload
/// never runs, the in-flight count comes back down, and `finish` fails UNAVAILABLE.
#[test]
fn a_server_half_that_is_gone_fails_the_finish() {
    let server = server();
    let taking = Taking::with(plane(&server), 1, 1);
    drop(server);
    take(&taking, b"lost");
    assert_eq!(lock(&taking.0.state).in_flight, 0);
    let here = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let why = here.block_on(taking.finish()).expect_err("gone");
    assert!(
        matches!(&why, CasError::Unavailable(blob, message)
            if blob == "outputs" && message.contains("did not answer")),
        "{why}"
    );
}

/// A fresh scratch directory for one unit test, beside the test binary.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps directory")
        .join("kbf-all-in-one-unit")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The plane over a server started on `server` as `kbf-server` starts one, with a write
/// tier (a hot store) under a scratch directory `name`; the server's handle, which must
/// outlive the plane's calls.
fn tiered(
    server: &tokio::runtime::Runtime,
    name: &str,
) -> (LocalDataPlane, kbf_server::ServerHandle) {
    use clap::Parser as _;
    let dir = scratch(name);
    let args = kbf_server::Args::try_parse_from([
        "kbf-server".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--hot-store=file://{}", dir.join("hot").display()),
    ])
    .expect("flags");
    server.block_on(async {
        let never = || Ok(std::future::pending::<()>());
        let handle = kbf_server::start(&args, never).await.expect("start");
        let plane = LocalDataPlane::new(handle.local(), server.handle().clone());
        (plane, handle)
    })
}

/// Hands `bytes` to `taking` as one file, a megabyte at a time, and finishes it as
/// `digest` and `crc32c`.
fn take_as(taking: &Taking, bytes: &[u8], digest: kbf_types::Digest, crc32c: u32) {
    let mut file = taking.file(bytes.len() as u64).expect("taken");
    for chunk in bytes.chunks(1 << 20) {
        file.write(chunk);
    }
    file.finish(digest, crc32c);
}

/// `len` bytes that differ for each `seed`.
fn patterned(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i % 253) as u8))
        .collect()
}

/// Catches a large file not stored when streamed, or stored twice: a file of
/// one byte over [`MAX_UNSTAGED_BLOB_BYTES`] streamed onto the tier is held after `finish`; the same bytes
/// again in the same walk are not stored again, and in another walk, finding them held,
/// are not committed again; both walks finish cleanly.
#[test]
fn a_large_file_streams_onto_the_tier_and_is_stored_once() {
    let server = server();
    let (plane, _handle) = tiered(&server, "intake-stream");
    let bytes = patterned(
        1,
        usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size"),
    );
    let mut hasher = RecordHasher::new();
    hasher.update(&bytes);
    let (digest, crc32c) = hasher.finish();
    let taking = Taking::new(plane.clone());
    take_as(&taking, &bytes, digest, crc32c);
    take_as(&taking, &bytes, digest, crc32c);
    assert_eq!(sent(&taking), 1, "one digest stored once");
    server.block_on(taking.finish()).expect("stored");
    let again = Taking::new(plane.clone());
    take_as(&again, &bytes, digest, crc32c);
    server.block_on(again.finish()).expect("held already");
    server.block_on(async {
        assert!(plane.missing(&[proto_digest(&digest)]).await.is_empty());
        assert_eq!(plane.get(&proto_digest(&digest)).await, Ok(bytes));
    });
}

/// Catches a streamed file that ends wrong taken for stored: one written past the size
/// it was opened with, and one finished with another CRC-32C, each fail the walk's
/// finish UNAVAILABLE, and neither is held (mutants: a write the tier refused ignored,
/// the server's check skipped).
#[test]
fn a_streamed_file_that_ends_wrong_fails_the_finish() {
    let server = server();
    let (plane, _handle) = tiered(&server, "intake-stream-wrong");
    let size = usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size");
    let bytes = patterned(2, size + 1);
    let mut hasher = RecordHasher::new();
    hasher.update(&bytes);
    let (digest, crc32c) = hasher.finish();
    let taking = Taking::new(plane.clone());
    let mut file = taking.file(size as u64).expect("taken");
    file.write(&bytes[..size]);
    file.write(&bytes[size..]);
    // Nothing more is written once a write failed.
    file.write(&bytes[size..]);
    file.finish(digest, crc32c);
    let why = server
        .block_on(taking.finish())
        .expect_err("written past its size");
    assert!(matches!(why, CasError::Unavailable(..)), "{why}");

    let bytes = patterned(3, size);
    let mut hasher = RecordHasher::new();
    hasher.update(&bytes);
    let (digest, crc32c) = hasher.finish();
    let taking = Taking::new(plane.clone());
    take_as(&taking, &bytes, digest, crc32c ^ 1);
    let why = server
        .block_on(taking.finish())
        .expect_err("another CRC-32C");
    assert!(why.to_string().contains("CRC-32C"), "{why}");
    server.block_on(async {
        assert_eq!(plane.missing(&[proto_digest(&digest)]).await.len(), 1);
    });
}

/// Catches the whole-body path's limit lost without a write tier: a file of exactly
/// [`MAX_UNSTAGED_BLOB_BYTES`] is kept in memory and stored as a batch.
#[test]
fn without_a_tier_a_file_at_the_threshold_is_stored_from_memory() {
    let server = server();
    let plane = plane(&server);
    let bytes = patterned(4, usize::try_from(MAX_UNSTAGED_BLOB_BYTES).expect("size"));
    let taking = Taking::new(plane.clone());
    let digest = take(&taking, &bytes);
    server.block_on(taking.finish()).expect("stored");
    server.block_on(async {
        assert!(plane.missing(&[proto_digest(&digest)]).await.is_empty());
    });
}

/// Catches a large file refused without a write tier, stored twice, or stored when it
/// should not be (mutants: the refusal restored, no `seen` check on the store's path, a
/// file left unfinished completed): without a tier, a file one byte over
/// [`MAX_UNSTAGED_BLOB_BYTES`] streams to the store and is held after `finish`; the same
/// bytes again in the same walk are withdrawn, and in another walk, finding them held,
/// are withdrawn too; a file dropped unfinished stores nothing; every walk finishes
/// cleanly.
#[test]
fn without_a_tier_a_large_file_streams_to_the_store_and_is_stored_once() {
    let server = server();
    let plane = plane(&server);
    let size = usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size");
    let bytes = patterned(5, size);
    let mut hasher = RecordHasher::new();
    hasher.update(&bytes);
    let (digest, crc32c) = hasher.finish();
    let taking = Taking::new(plane.clone());
    take_as(&taking, &bytes, digest, crc32c);
    take_as(&taking, &bytes, digest, crc32c);
    let left = patterned(6, size);
    let mut file = taking.file(size as u64).expect("taken");
    file.write(&left[..1 << 20]);
    drop(file);
    server.block_on(taking.finish()).expect("stored");
    let again = Taking::new(plane.clone());
    take_as(&again, &bytes, digest, crc32c);
    server.block_on(again.finish()).expect("held already");
    server.block_on(async {
        assert!(plane.missing(&[proto_digest(&digest)]).await.is_empty());
        assert_eq!(plane.get(&proto_digest(&digest)).await, Ok(bytes));
        let left = digest_of(&left);
        assert_eq!(plane.missing(std::slice::from_ref(&left)).await, [left]);
    });
}

/// Catches a file streamed to the store taken for stored when its bytes are not its
/// digest's, or a walk left waiting on an upload that failed (mutants: the digest check
/// skipped, a failed upload's queue written to forever): without a tier, a file
/// finished under another digest fails the walk's finish and is not held; one written
/// far past its size fails it too, the walk's later writes dropped once the upload
/// stopped taking them.
#[test]
fn without_a_tier_a_streamed_file_that_ends_wrong_fails_the_finish() {
    let server = server();
    let plane = plane(&server);
    let size = usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size");
    let bytes = patterned(7, size);
    let claimed = digest_of(&patterned(8, size));
    let claimed = kbf_types::Digest::parse(
        kbf_types::DigestFunction::Sha256,
        &kbf_transfer::label(&claimed),
    )
    .expect("a digest");
    let taking = Taking::new(plane.clone());
    take_as(&taking, &bytes, claimed, 0);
    let why = server
        .block_on(taking.finish())
        .expect_err("another digest");
    assert!(matches!(why, CasError::Unavailable(..)), "{why}");
    server.block_on(async {
        let asked = [proto_digest(&claimed)];
        assert_eq!(plane.missing(&asked).await, asked);
    });

    let taking = Taking::new(plane.clone());
    let mut file = taking.file(size as u64).expect("taken");
    let chunk = vec![1u8; 1 << 20];
    for _ in 0..size / chunk.len() + 16 {
        file.write(&chunk);
    }
    file.finish(claimed, 0);
    let why = server
        .block_on(taking.finish())
        .expect_err("written past its size");
    assert!(matches!(why, CasError::Unavailable(..)), "{why}");
}
