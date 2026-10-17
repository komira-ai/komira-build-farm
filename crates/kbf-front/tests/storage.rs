//! The cache through the storage boundary: [`CacheStorage`] as an `Arc<dyn Storage>`.
//! The data plane's checks run over it in `kbf-all-in-one` (`tests/storage_seam.rs`);
//! these cover what they do not: the action cache, and that every error answers the
//! gRPC code and message the cache's own error answers. Streamed puts: `tests/storage_stream.rs`.

mod common;

use std::io;
use std::mem::discriminant;
use std::sync::Arc;

use bytes::Bytes;
use common::{Blob, Farm, pseudo_random};
use futures::StreamExt;
use kbf_front::{Cache, CacheError, CacheStorage, MetaLogError, READ_CHUNK_BYTES, SegmentsConfig};
use kbf_meta::{ActionWriteError, Role};
use kbf_objstore::{KeyError, ObjectStoreError};
use kbf_proto::reapi::{ActionResult, OutputFile};
use kbf_segments::{SegmentError, WriteError};
use kbf_storage::{Storage, StorageError};
use kbf_tier::TierError;
use kbf_types::Digest;
use tonic::{Code, Status};

fn storage() -> Arc<dyn Storage> {
    Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ))
}

fn digest_of(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

fn result_with(output: Digest) -> ActionResult {
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(kbf_front::digest_to_proto(&output)),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Catches a call the adapter answers otherwise than the cache: a put that answers
/// another digest than the bytes', a FindMissing out of order or naming a held blob, an
/// absent blob read as anything but NOT_FOUND.
#[tokio::test]
async fn blobs_answer_through_the_trait() {
    let storage = storage();
    let bytes = Bytes::from_static(b"an output");
    let held = storage.put(bytes.clone()).await.expect("put");
    assert_eq!(held, digest_of(&bytes));
    let absent = digest_of(b"never stored");
    let asked = [absent, held];
    assert_eq!(storage.find_missing(&asked).await.expect("find"), [absent]);
    assert_eq!(storage.read_blob(&held).await.expect("read"), bytes);
    assert!(
        matches!(storage.read_blob(&absent).await, Err(StorageError::NotFound(d)) if d == absent)
    );
}

/// Catches an action-cache entry written by a client, an entry prepared over an output
/// that is not held, and a committed entry not served back as it was written.
#[tokio::test]
async fn the_action_cache_answers_through_the_trait() {
    let storage = storage();
    let action = digest_of(b"an action");
    let output = storage
        .put(Bytes::from_static(b"its output"))
        .await
        .expect("put");
    assert!(storage.action_result(&action).await.expect("get").is_none());

    let absent = digest_of(b"an output never stored");
    let refused = storage.prepare_action_result(&result_with(absent)).await;
    assert!(
        matches!(&refused, Err(StorageError::ActionWrite(ActionWriteError::Absent(d))) if *d == [absent]),
        "{refused:?}"
    );

    let result = result_with(output);
    let record = storage
        .prepare_action_result(&result)
        .await
        .expect("prepare");
    let as_client = storage
        .commit_action_record(Role::Client, action, record.clone())
        .await;
    assert!(
        matches!(
            as_client,
            Err(StorageError::ActionWrite(ActionWriteError::NotDaemon))
        ),
        "{as_client:?}"
    );
    assert!(storage.action_result(&action).await.expect("get").is_none());

    let stored = record.result;
    storage
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect("commit");
    assert_eq!(
        storage.action_result(&action).await.expect("get"),
        Some((stored, result))
    );
}

/// Catches the adapter offering no stream where the cache has one (it streams onto the
/// write tier or to the store), or one for an empty blob: a stream for a blob of bytes
/// exists in a runtime, none for 0.
#[tokio::test]
async fn the_cache_offers_a_stream_for_a_blob() {
    let storage = storage();
    let offered = tokio::task::spawn_blocking(move || {
        (storage.stream(1).is_some(), storage.stream(0).is_none())
    })
    .await
    .expect("join");
    assert_eq!(offered, (true, true));
}

/// A blob of three windows and a bit, as a body of 1 MiB pieces.
fn pieces(data: &[u8]) -> futures::stream::BoxStream<'static, io::Result<Bytes>> {
    let pieces: Vec<io::Result<Bytes>> = data
        .chunks(READ_CHUNK_BYTES)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    futures::stream::iter(pieces).boxed()
}

/// Catches a streamed put that stores what it must not or does not store what it must:
/// a blob of more than one read window, streamed in pieces, is present afterwards and
/// reads back byte for byte in chunks of at most [`READ_CHUNK_BYTES`] from three windows
/// (never one buffer of it); bytes that do not hash to the claimed digest store nothing
/// (DigestMismatch); a withdrawn claim answers `None` and stores nothing.
#[tokio::test]
async fn a_streamed_put_stores_a_blob_of_several_windows() {
    let storage = storage();
    let data = pseudo_random(3 * kbf_front::READ_WINDOW_BYTES as usize + 12_345, 4);
    let digest = digest_of(&data);
    let stored = storage
        .put_streamed(
            data.len() as u64,
            pieces(&data),
            Box::pin(async move { Some(digest) }),
        )
        .await
        .expect("streamed");
    assert_eq!(stored, Some(digest));
    assert!(
        storage
            .find_missing(&[digest])
            .await
            .expect("find")
            .is_empty()
    );
    let chunks = read_all(&*storage, &digest, 0, 0).await;
    assert!(chunks.iter().all(|c| c.len() <= READ_CHUNK_BYTES));
    assert_eq!(chunks.concat(), data);

    let mut other = data.clone();
    other[100] ^= 1;
    let other_digest = digest_of(&other);
    let refused = storage
        .put_streamed(
            other.len() as u64,
            pieces(&data),
            Box::pin(async move { Some(other_digest) }),
        )
        .await
        .expect_err("not the claimed bytes");
    assert!(
        matches!(refused, StorageError::DigestMismatch { .. }),
        "{refused:?}"
    );
    assert_eq!(
        storage.find_missing(&[other_digest]).await.expect("find"),
        [other_digest]
    );

    let mut third = data.clone();
    third[200] ^= 1;
    let withdrawn = storage
        .put_streamed(third.len() as u64, pieces(&third), Box::pin(async { None }))
        .await
        .expect("withdrawn");
    assert_eq!(withdrawn, None);
    assert_eq!(
        storage
            .find_missing(&[digest_of(&third)])
            .await
            .expect("find"),
        [digest_of(&third)]
    );
}

/// Catches an error that answers a caller differently through the boundary than the
/// cache's own error did: another gRPC code (a store that could not answer taken as an
/// absent blob, a refused write retried) or another message.
#[test]
fn errors_answer_as_the_cache_answers() {
    let (a, b) = (digest_of(b"a"), digest_of(b"b"));
    let errors = [
        CacheError::NotFound(a),
        CacheError::Unreachable(a),
        CacheError::DigestMismatch {
            claimed: a,
            actual: b,
        },
        CacheError::Invalid("bad".to_owned()),
        CacheError::ActionWrite(ActionWriteError::NotDaemon),
        CacheError::ActionWrite(ActionWriteError::Absent(vec![a])),
        CacheError::ActionWrite(ActionWriteError::Unreachable(vec![a])),
        CacheError::Meta(MetaLogError::Unavailable("no leader".to_owned())),
        CacheError::Store(ObjectStoreError::Unsupported("retention")),
        CacheError::Internal("torn".to_owned()),
        CacheError::from(TierError::UnknownBlock(object_id())),
    ];
    for error in errors {
        let cache = Status::from(error.clone());
        let storage = Status::from(StorageError::from(error));
        assert_eq!(storage.code(), cache.code(), "{}", cache.message());
        assert_eq!(storage.message(), cache.message());
    }
}

/// The three errors of a stored or built thing that is wrong (not the store being down)
/// answer the same code as the cache's own error, INTERNAL, and its message under the
/// `internal: ` the boundary's INTERNAL always carries.
#[test]
fn internal_errors_answer_as_the_cache_answers() {
    let errors = [
        CacheError::Segment(SegmentError::BadMagic),
        CacheError::Pack(WriteError::Full { size: 1 }),
        CacheError::Key(KeyError::Empty),
    ];
    for error in errors {
        let cache = Status::from(error.clone());
        let storage = Status::from(StorageError::from(error));
        assert_eq!(cache.code(), Code::Internal);
        assert_eq!(storage.code(), cache.code());
        assert_eq!(storage.message(), format!("internal: {}", cache.message()));
    }
}

fn object_id() -> kbf_meta::ObjectId {
    kbf_meta::ObjectId::new(kbf_meta::Epoch::new(1), 1)
}

/// `error` as the boundary answers it, and the gRPC code a service sends for that.
fn answer(error: CacheError) -> (StorageError, Code) {
    let storage = StorageError::from(error);
    let code = Status::from(storage.clone()).code();
    (storage, code)
}

fn kind(error: &StorageError) -> std::mem::Discriminant<StorageError> {
    discriminant(error)
}

/// A segment read back from the store that does not parse fails the same way every
/// time it is read, so it is INTERNAL (a broken backend), not UNAVAILABLE: a caller told
/// to retry would be told to retry for ever. Catches `Segment` mapped to `Unavailable`
/// (the mapping before this change) or to `NotFound` (a damaged object taken as absent).
#[test]
fn a_segment_that_does_not_parse_is_internal() {
    let (storage, code) = answer(CacheError::Segment(SegmentError::BadMagic));
    assert_eq!(kind(&storage), kind(&StorageError::Internal(String::new())));
    assert_eq!(
        storage.to_string(),
        "internal: segment: not a segment: wrong magic"
    );
    assert_eq!(code, Code::Internal);
}

/// The write tier's failures are the disk's (a full budget, a failed write, a block gone),
/// which the write path falls back from and a later call may not meet: UNAVAILABLE,
/// with the tier's own words. Catches `Tier` made INTERNAL (paging for a full disk) or
/// absent.
#[test]
fn a_write_tier_failure_is_unavailable() {
    let io = TierError::Io {
        op: "write",
        name: "block".to_owned(),
        source: io::Error::other("no space"),
    };
    let (storage, code) = answer(CacheError::from(io));
    assert_eq!(
        kind(&storage),
        kind(&StorageError::Unavailable(Arc::new(Backend)))
    );
    assert_eq!(storage.to_string(), "write tier: write block: no space");
    assert_eq!(code, Code::Unavailable);
}

/// A blob the cache's own packing refused. A blob over a segment is refused as invalid
/// before it is packed, and a full segment is rotated, so one that reaches a caller is
/// the cache's bug: INTERNAL, not a blip to retry. Catches `Pack` mapped to `Unavailable`.
#[test]
fn a_packing_failure_is_internal() {
    let (storage, code) = answer(CacheError::Pack(WriteError::TooLarge { size: 9, limit: 8 }));
    assert_eq!(kind(&storage), kind(&StorageError::Internal(String::new())));
    assert_eq!(
        storage.to_string(),
        "internal: segment: 9-byte blob exceeds the 8-byte segment limit"
    );
    assert_eq!(code, Code::Internal);
}

/// An object key is the configured prefix and 32 hex digits, so one that cannot be built
/// is a misconfiguration that fails on every retry: INTERNAL, not "worth a retry".
/// Catches `Key` mapped to `Unavailable` (retried for ever) or to `NotFound`.
#[test]
fn a_key_that_cannot_be_built_is_internal() {
    let (storage, code) = answer(CacheError::Key(KeyError::Empty));
    assert_eq!(kind(&storage), kind(&StorageError::Internal(String::new())));
    assert_eq!(
        storage.to_string(),
        "internal: object key: an object key may not be empty"
    );
    assert_eq!(code, Code::Internal);
}

#[derive(Debug, thiserror::Error)]
#[error("the disk is gone")]
struct Backend;

async fn read_all(storage: &dyn Storage, digest: &Digest, offset: u64, limit: u64) -> Vec<Bytes> {
    storage
        .read_range(digest, offset, limit)
        .await
        .expect("read_range")
        .map(|chunk| chunk.expect("chunk"))
        .collect()
        .await
}

/// A blob of two and a half chunks, stored.
async fn big(storage: &dyn Storage) -> (Digest, Vec<u8>) {
    let data = pseudo_random(2 * READ_CHUNK_BYTES + READ_CHUNK_BYTES / 2, 9);
    let digest = storage.put(Bytes::from(data.clone())).await.expect("put");
    (digest, data)
}

/// The memory bound a ranged read can claim: a window of the blob is read, once, and sent
/// as slices of that one buffer (this blob is within one window). Catches the blob collected into one message (a chunk over
/// [`READ_CHUNK_BYTES`]) and each chunk copied out of it (a chunk not starting where
/// the one before ended in memory, so a second copy of the blob across the stream).
#[tokio::test]
async fn chunks_are_bounded_slices_of_one_buffer() {
    let storage = storage();
    let (digest, data) = big(&*storage).await;
    let chunks = read_all(&*storage, &digest, 0, 0).await;
    let lens: Vec<usize> = chunks.iter().map(Bytes::len).collect();
    assert_eq!(
        lens,
        [READ_CHUNK_BYTES, READ_CHUNK_BYTES, READ_CHUNK_BYTES / 2]
    );
    assert_eq!(chunks.concat(), data);
    let starts: Vec<usize> = chunks.iter().map(|c| c.as_ptr() as usize).collect();
    let ends: Vec<usize> = chunks
        .iter()
        .map(|c| c.as_ptr() as usize + c.len())
        .collect();
    assert_eq!(starts[1..], ends[..2], "one buffer, in order");
}

/// Each range is the bytes it names, with the limit 0 meaning to the end: a range inside
/// a chunk, across a chunk boundary, past the end (clamped), the last byte, and from the
/// very end (an empty stream, not an error). Catches an off-by-one at either end, a
/// limit of 0 read as "nothing", and a limit past the end read as an error.
#[tokio::test]
async fn a_range_is_the_bytes_it_names() {
    let storage = storage();
    let (digest, data) = big(&*storage).await;
    let (c, size) = (READ_CHUNK_BYTES, data.len());
    let cases = [
        (0, 0, 0, size),
        (0, size as u64 + 100, 0, size),
        (5, 10, 5, 15),
        (c as u64 - 10, 20, c - 10, c + 10),
        (2 * c as u64, 0, 2 * c, size),
        (size as u64 - 1, 0, size - 1, size),
        (size as u64, 0, size, size),
        (size as u64, 5, size, size),
        (7, c as u64, 7, c + 7),
    ];
    for (offset, limit, start, end) in cases {
        let chunks = read_all(&*storage, &digest, offset, limit).await;
        assert!(chunks.iter().all(|k| k.len() <= c), "{offset} {limit}");
        assert_eq!(chunks.concat(), data[start..end], "{offset} {limit}");
    }
}

/// A reader that lost its stream after some chunks reads on at the offset it reached
/// and gets the rest: the proxy's resume. Catches an offset that restarts, or skips, at
/// a chunk boundary.
#[tokio::test]
async fn a_read_resumes_at_the_offset_it_reached() {
    let storage = storage();
    let (digest, data) = big(&*storage).await;
    let mut first = storage.read_range(&digest, 0, 0).await.expect("read_range");
    let head = first.next().await.expect("a chunk").expect("chunk");
    drop(first);
    let rest = read_all(&*storage, &digest, head.len() as u64, 0).await;
    assert_eq!([head.to_vec(), rest.concat()].concat(), data);
}

/// An offset past the end is OUT_OF_RANGE (the ByteStream `read_offset` rule: a
/// `read_offset` greater than the resource's size), decided from the digest even for a
/// blob not held, and an absent blob read from a valid offset is NOT_FOUND. Catches the
/// offset check missing (a panic or an empty stream), `Invalid` (INVALID_ARGUMENT, the
/// code ByteStream clients do not expect), and the check made after the store is asked.
#[tokio::test]
async fn an_offset_past_the_end_is_out_of_range() {
    let storage = storage();
    let (digest, data) = big(&*storage).await;
    let past = data.len() as u64 + 1;
    let error = storage
        .read_range(&digest, past, 0)
        .await
        .err()
        .expect("past the end");
    assert_eq!(
        error.to_string(),
        format!("read_offset {past} is past the end of {digest}")
    );
    assert_eq!(Status::from(error).code(), Code::OutOfRange);

    let absent = digest_of(b"never stored");
    let (past_absent, from_start) = (
        storage
            .read_range(&absent, 13, 0)
            .await
            .err()
            .expect("past"),
        storage
            .read_range(&absent, 0, 0)
            .await
            .err()
            .expect("absent"),
    );
    assert_eq!(Status::from(past_absent).code(), Code::OutOfRange);
    assert_eq!(Status::from(from_start).code(), Code::NotFound);
}

/// A held blob whose object is gone, or whose bytes are damaged, is UNAVAILABLE as
/// `read_blob` answers it, with its message; and a range that does not touch the damaged
/// byte is refused too, because the blob is checked whole before its last window, and these blobs are one
/// window, so the failure is the call's. Catches
/// bytes sent on a range-only check, and a damaged blob answered NOT_FOUND.
#[tokio::test]
async fn a_damaged_blob_fails_every_range_as_read_blob_does() {
    let farm = Farm::start().await;
    let storage = CacheStorage::new(Arc::clone(&farm.cache), SegmentsConfig::DEFAULT);
    let (gone, damaged) = (
        Blob::new(pseudo_random(4096, 1)),
        Blob::new(pseudo_random(4096, 2)),
    );
    farm.upload(&[&gone]).await;
    farm.delete_object_of(&gone).await;
    farm.upload(&[&damaged]).await;
    farm.corrupt_object_of(&damaged).await;
    // The ranged read comes first, and at an offset clear of the damaged byte: it is the
    // one that finds the object, so it is the one that must check the whole blob.
    for (blob, offset) in [(&gone, 100), (&damaged, 4000), (&gone, 0), (&damaged, 0)] {
        let ranged = storage
            .read_range(&blob.digest, offset, 0)
            .await
            .err()
            .expect("read_range");
        let read = storage
            .read_blob(&blob.digest)
            .await
            .expect_err("read_blob");
        assert_eq!(ranged.to_string(), read.to_string());
        assert_eq!(Status::from(ranged).code(), Code::Unavailable, "{offset}");
    }
}
