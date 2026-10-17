//! The service and its client, end to end over a loopback connection to an in-memory
//! store ([`fake::Mem`]); the roles are in [`roles`], the map generation in
//! [`generation`].
//!
//! Everything here is about the wire: what a caller of [`RemoteStorage`] sees must be
//! what a caller of the store itself would see, and what a raw REAPI client sends that
//! no [`RemoteStorage`] would is refused with the code the protocol names.

mod catch_up;
mod every_call;
pub(crate) mod fake;
mod generation;
mod in_process;
mod open_writes;
pub(crate) mod roles;
mod stale_map;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use fake::{Mem, pair, patterned, serve, serve_odd, sha};
use futures::{StreamExt as _, stream};
use kbf_meta::{ActionRecord, ActionWriteError, Role};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, ReadRequest, WriteRequest, WriteResponse,
};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    self, ActionResult, BatchReadBlobsRequest, BatchUpdateBlobsRequest, FindMissingBlobsRequest,
    GetChunkMappingRequest, GetTreeRequest, RegisterChunkMappingRequest, SpliceBlobRequest,
    SplitBlobRequest,
};
use kbf_proto::storage::action_store_client::ActionStoreClient;
use kbf_proto::storage::{CommitRequest, PrepareRequest, Record, Writer, refusal};
use kbf_types::Digest;
use tonic::transport::Channel;
use tonic::{Code, Status};

use super::wire::{self, to_proto};
use super::{MAX_FIND_MISSING_DIGESTS, MAX_MESSAGE_BYTES, RemoteStorage, STREAM_FROM_BYTES};
use crate::{READ_CHUNK_BYTES, Storage, StorageError};

/// Collects a read: the sizes of its chunks and its bytes.
async fn collect(mut stream: crate::ReadStream) -> (Vec<usize>, Vec<u8>) {
    let mut sizes = Vec::new();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("a chunk");
        sizes.push(chunk.len());
        bytes.extend_from_slice(&chunk);
    }
    (sizes, bytes)
}

/// A write of the named resource, in these messages.
fn write_of(name: &str, parts: &[(&[u8], bool)]) -> Vec<WriteRequest> {
    let mut at = 0;
    parts
        .iter()
        .enumerate()
        .map(|(i, (data, finish))| {
            let request = WriteRequest {
                resource_name: if i == 0 {
                    name.to_owned()
                } else {
                    String::new()
                },
                write_offset: at,
                finish_write: *finish,
                data: data.to_vec(),
            };
            at += data.len() as i64;
            request
        })
        .collect()
}

async fn raw_write(
    channel: &Channel,
    requests: Vec<WriteRequest>,
) -> Result<WriteResponse, Status> {
    let mut client = ByteStreamClient::new(channel.clone());
    client
        .write(stream::iter(requests))
        .await
        .map(tonic::Response::into_inner)
}

fn upload_name(d: &Digest) -> String {
    format!("uploads/u-1/blobs/{}/{}", d.hash_hex(), d.size_bytes)
}

/// Catches a service or client that loses, repeats or reorders bytes, ignores the
/// offset or the limit, mis-sizes a chunk (the blob is three messages at 2.5 MiB, each at
/// most `READ_CHUNK_BYTES`), or answers a blob other than the one put: the empty blob, a
/// small one and a large one go in with `put` and come out of `read_blob` equal, and
/// ranges of them come out of `read_range` as the store would cut them.
#[tokio::test]
async fn blobs_round_trip_through_the_wire() {
    let (_mem, remote, _) = pair().await;
    let large = patterned(2 * READ_CHUNK_BYTES + READ_CHUNK_BYTES / 2);
    for bytes in [Vec::new(), b"small".to_vec(), large.clone()] {
        let digest = remote.put(Bytes::from(bytes.clone())).await.expect("put");
        assert_eq!(digest, sha(&bytes));
        assert_eq!(
            remote.read_blob(&digest).await.expect("read").to_vec(),
            bytes
        );
    }
    let digest = sha(&large);
    let (sizes, bytes) = collect(remote.read_range(&digest, 0, 0).await.expect("all")).await;
    assert_eq!(bytes, large);
    assert_eq!(
        sizes,
        [READ_CHUNK_BYTES, READ_CHUNK_BYTES, READ_CHUNK_BYTES / 2]
    );
    let from = READ_CHUNK_BYTES + 3;
    let (_, bytes) = collect(
        remote
            .read_range(&digest, from as u64, 10)
            .await
            .expect("cut"),
    )
    .await;
    assert_eq!(bytes, large[from..from + 10]);
    let (_, bytes) = collect(
        remote
            .read_range(&digest, from as u64, 0)
            .await
            .expect("tail"),
    )
    .await;
    assert_eq!(bytes, large[from..]);
    let at_end = remote
        .read_range(&digest, digest.size_bytes, 0)
        .await
        .expect("at the end");
    assert_eq!(collect(at_end).await, (Vec::new(), Vec::new()));
}

/// Catches a read that collects the blob before it sends a byte (an endless store would
/// never answer; a streaming service answers its first chunks at once and stops when the
/// client does).
#[tokio::test]
async fn a_read_streams_and_is_not_collected() {
    let (mem, remote, _) = pair().await;
    mem.endless.store(true, Ordering::SeqCst);
    let digest = sha(b"endless");
    let mut chunks = remote.read_range(&digest, 0, 0).await.expect("answered");
    let first = tokio::time::timeout(Duration::from_secs(10), chunks.next())
        .await
        .expect("the first chunk arrives while the read goes on");
    assert_eq!(first.expect("a chunk").expect("ok").len(), 1024);
}

/// Catches an error that loses its code on the way: a blob not held must be NOT_FOUND
/// (the caller uploads it), an offset past the end OUT_OF_RANGE with the offset, a
/// store that cannot answer UNAVAILABLE (never "absent"), a store whose records disagree
/// INTERNAL, a malformed request INVALID_ARGUMENT, and a read that fails after its first
/// chunk an error in the stream, not a short blob.
#[tokio::test]
async fn errors_keep_their_codes_across_the_wire() {
    let (mem, remote, channel) = pair().await;
    let held = remote.put(Bytes::from_static(b"held")).await.expect("put");
    let absent = sha(b"never stored");
    let error = remote
        .read_range(&absent, 0, 0)
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(error, StorageError::NotFound(d) if d == absent),
        "{error:?}"
    );
    let error = remote.read_blob(&absent).await.expect_err("refused");
    assert!(
        matches!(error, StorageError::NotFound(d) if d == absent),
        "{error:?}"
    );
    let error = remote.read_range(&held, 9, 0).await.err().expect("refused");
    assert!(
        matches!(error, StorageError::OutOfRange { digest, offset: 9 } if digest == held),
        "{error:?}"
    );

    // Raw: a negative offset is OUT_OF_RANGE, a negative limit INVALID_ARGUMENT, and a
    // name that is no blob INVALID_ARGUMENT.
    let mut raw = ByteStreamClient::new(channel.clone());
    let name = format!("blobs/{}/{}", held.hash_hex(), held.size_bytes);
    for (read_offset, read_limit, resource_name, code) in [
        (-1, 0, name.as_str(), Code::OutOfRange),
        (0, -1, name.as_str(), Code::InvalidArgument),
        (0, 0, "nothing", Code::InvalidArgument),
    ] {
        let status = raw
            .read(ReadRequest {
                resource_name: resource_name.to_owned(),
                read_offset,
                read_limit,
            })
            .await
            .expect_err("refused");
        assert_eq!(
            status.code(),
            code,
            "{resource_name} {read_offset} {read_limit}"
        );
    }

    // A read that fails after its first chunk.
    let big = remote
        .put(Bytes::from(patterned(2 * READ_CHUNK_BYTES)))
        .await
        .expect("put");
    *mem.fail_reading.lock().expect("lock") = Some(StorageError::Unreachable(big));
    let mut stream = remote.read_range(&big, 0, 0).await.expect("begins");
    assert_eq!(
        stream.next().await.expect("first").expect("ok").len(),
        READ_CHUNK_BYTES
    );
    let failed = stream.next().await.expect("second").expect_err("fails");
    assert!(matches!(failed, StorageError::Unavailable(_)), "{failed:?}");
    *mem.fail_reading.lock().expect("lock") = None;

    // Whatever the store fails with.
    let cases = [
        (
            StorageError::Unavailable(Arc::new(std::fmt::Error)),
            Code::Unavailable,
        ),
        (StorageError::Internal("torn".to_owned()), Code::Internal),
        (
            StorageError::Invalid("bad".to_owned()),
            Code::InvalidArgument,
        ),
    ];
    for (injected, code) in cases {
        *mem.fail.lock().expect("lock") = Some(injected);
        let error = remote.find_missing(&[held]).await.expect_err("fails");
        let expected = match code {
            Code::Unavailable => matches!(error, StorageError::Unavailable(_)),
            Code::Internal => matches!(error, StorageError::Internal(_)),
            _ => matches!(error, StorageError::Invalid(_)),
        };
        assert!(expected, "{code:?}: {error:?}");
        let error = remote
            .put(Bytes::from_static(b"x"))
            .await
            .expect_err("fails");
        assert_eq!(Status::from(error).code(), code);
    }
}

/// Catches every status code of the client's reading of a failed call changed: the
/// table of what an in-process caller would have seen.
#[test]
fn a_status_becomes_the_error_a_store_would_give() {
    use super::client::lift;
    let d = sha(b"x");
    let at = Some((d, 4));
    let lifted = |code: Code, at| lift(&Status::new(code, "m"), at);
    assert!(matches!(lifted(Code::NotFound, at), StorageError::NotFound(x) if x == d));
    assert!(matches!(
        lifted(Code::OutOfRange, at),
        StorageError::OutOfRange { digest, offset: 4 } if digest == d
    ));
    for code in [
        Code::InvalidArgument,
        Code::PermissionDenied,
        Code::Unauthenticated,
    ] {
        assert!(
            matches!(lifted(code, at), StorageError::Invalid(m) if m == "m"),
            "{code:?}"
        );
    }
    // Without a blob to name, NOT_FOUND and OUT_OF_RANGE are the node's own confusion.
    for code in [
        Code::Internal,
        Code::Unimplemented,
        Code::NotFound,
        Code::OutOfRange,
    ] {
        assert!(
            matches!(lifted(code, None), StorageError::Internal(m) if m == "m"),
            "{code:?}"
        );
    }
    // The codes that say the node could not be reached or did not answer in time: the
    // only ones a primary may answer a write over, with one copy and a repair owed.
    for code in [Code::Unavailable, Code::DeadlineExceeded, Code::Unknown] {
        let error = lifted(code, at);
        assert!(matches!(error, StorageError::Unavailable(_)), "{code:?}");
        assert_eq!(error.to_string(), "storage node: m");
    }
    // The codes a node that was reached answers with when it declines: refusals, which are
    // never outages (a primary that took them for one would answer a write the replica
    // refused with a single copy).
    for code in [
        Code::Aborted,
        Code::DataLoss,
        Code::ResourceExhausted,
        Code::Cancelled,
        Code::AlreadyExists,
        Code::FailedPrecondition,
    ] {
        let error = lifted(code, at);
        assert!(
            matches!(&error, StorageError::Internal(m) if m.contains("m") && m.contains(&format!("{code:?}"))),
            "{code:?}: {error:?}"
        );
    }
}

/// Catches FindMissing answering in another order than asked, dropping a held-but-empty
/// blob into the missing, passing a malformed digest or another digest function, and a
/// batch call answered when the node does not serve it.
#[tokio::test]
async fn find_missing_answers_in_order_and_refuses_what_it_cannot() {
    let (_mem, remote, channel) = pair().await;
    let held = remote.put(Bytes::from_static(b"held")).await.expect("put");
    let (a, b) = (sha(b"a"), sha(b"b"));
    let empty = sha(b"");
    let asked = [b, held, a, empty];
    assert_eq!(remote.find_missing(&asked).await.expect("answer"), [b, a]);

    let mut cas = ContentAddressableStorageClient::new(channel.clone());
    let ask = |digest_function, blob_digests| FindMissingBlobsRequest {
        instance_name: String::new(),
        blob_digests,
        digest_function,
    };
    let bad = reapi::Digest {
        hash: "nope".to_owned(),
        size_bytes: 1,
    };
    let negative = reapi::Digest {
        hash: "ab".repeat(32),
        size_bytes: -1,
    };
    for request in [
        ask(
            reapi::digest_function::Value::Sha1 as i32,
            vec![to_proto(&a)],
        ),
        ask(0, vec![bad]),
        ask(0, vec![negative]),
    ] {
        let status = cas.find_missing_blobs(request).await.expect_err("refused");
        assert_eq!(status.code(), Code::InvalidArgument);
    }
    let explicit = ask(
        reapi::digest_function::Value::Sha256 as i32,
        vec![to_proto(&a)],
    );
    assert_eq!(
        cas.find_missing_blobs(explicit)
            .await
            .expect("ok")
            .into_inner()
            .missing_blob_digests
            .len(),
        1
    );

    let refused = [
        cas.batch_update_blobs(BatchUpdateBlobsRequest::default())
            .await
            .map(drop),
        cas.batch_read_blobs(BatchReadBlobsRequest::default())
            .await
            .map(drop),
        cas.get_tree(GetTreeRequest::default()).await.map(drop),
        cas.split_blob(SplitBlobRequest::default()).await.map(drop),
        cas.splice_blob(SpliceBlobRequest::default())
            .await
            .map(drop),
        cas.get_chunk_mapping(GetChunkMappingRequest::default())
            .await
            .map(drop),
        cas.register_chunk_mapping(stream::iter([RegisterChunkMappingRequest::default()]))
            .await
            .map(drop),
    ];
    for answer in refused {
        assert_eq!(answer.expect_err("not served").code(), Code::Unimplemented);
    }
}

/// Catches a Write that accepts what it must not: no message, a name that is no upload,
/// more bytes than the digest names, a stream that stops
/// without `finish_write`, fewer bytes than named, other bytes than named (all
/// INVALID_ARGUMENT, and nothing stored); and one that does not accept what it must: a
/// correct write is acknowledged with the blob's size.
#[tokio::test]
async fn a_write_is_checked_against_its_digest() {
    let (mem, _remote, channel) = pair().await;
    let blob = b"twelve bytes";
    let digest = sha(blob);
    let name = upload_name(&digest);
    let wrong = [b'x'; 12];
    let cases = [
        ("no message", Vec::new()),
        ("not an upload", write_of("blobs/x", &[(blob, true)])),
        (
            "more than named",
            write_of(&name, &[(blob, false), (b"!", true)]),
        ),
        ("no finish", write_of(&name, &[(blob, false)])),
        ("fewer than named", write_of(&name, &[(&blob[..5], true)])),
        ("other bytes", write_of(&name, &[(&wrong, true)])),
    ];
    for (what, requests) in cases {
        let status = raw_write(&channel, requests).await.expect_err(what);
        assert_eq!(status.code(), Code::InvalidArgument, "{what}: {status}");
    }
    assert!(!mem.blobs.lock().expect("lock").contains_key(&digest));
    let acknowledged = raw_write(
        &channel,
        write_of(&name, &[(&blob[..5], false), (&blob[5..], true)]),
    )
    .await
    .expect("a correct write");
    assert_eq!(acknowledged.committed_size, 12);
    assert_eq!(
        mem.blobs.lock().expect("lock").get(&digest).map(Bytes::len),
        Some(12)
    );
}

/// Catches a Write whose stream ends early that is answered with anything but
/// INVALID_ARGUMENT naming how it ended: a stream with no message at all, and one that
/// stops before `finish_write` (after one message, and after two). Nothing is stored, and
/// the same blob written whole afterwards is.
#[tokio::test]
async fn a_write_that_ends_early_says_how() {
    let (mem, _remote, channel) = pair().await;
    let blob = b"twelve bytes";
    let digest = sha(blob);
    let name = upload_name(&digest);
    let cases = [
        ("empty stream", Vec::new(), "write stream sent no message"),
        (
            "one message",
            write_of(&name, &[(&blob[..5], false)]),
            "write stream ended without finish_write",
        ),
        (
            "two messages",
            write_of(&name, &[(&blob[..5], false), (&blob[5..8], false)]),
            "write stream ended without finish_write",
        ),
    ];
    for (what, requests, text) in cases {
        let status = raw_write(&channel, requests).await.expect_err(what);
        assert_eq!(status.code(), Code::InvalidArgument, "{what}: {status}");
        assert_eq!(status.message(), text, "{what}");
    }
    assert_eq!(mem.puts.load(Ordering::SeqCst), 0, "something was stored");
    assert!(mem.blobs.lock().expect("lock").is_empty());
    let whole = raw_write(&channel, write_of(&name, &[(blob, true)])).await;
    assert_eq!(whole.expect("a whole write").committed_size, 12);
}

/// Catches a Write that reads and stores a blob the node already holds (the cheapest
/// upload is the one skipped), and QueryWriteStatus that reports a blob not held as
/// complete or one held as not.
#[tokio::test]
async fn a_held_blob_is_not_written_again() {
    let (mem, remote, channel) = pair().await;
    let blob = Bytes::from_static(b"stored once");
    let digest = remote.put(blob.clone()).await.expect("put");
    assert_eq!(mem.puts.load(Ordering::SeqCst), 1);
    let again = raw_write(&channel, write_of(&upload_name(&digest), &[(&blob, true)])).await;
    assert_eq!(again.expect("acknowledged").committed_size, 11);
    assert_eq!(
        mem.puts.load(Ordering::SeqCst),
        1,
        "the second write stored nothing"
    );

    let mut raw = ByteStreamClient::new(channel);
    let status = |d: Digest| {
        let mut raw = raw.clone();
        async move {
            let name = upload_name(&d);
            raw.query_write_status(QueryWriteStatusRequest {
                resource_name: name,
            })
            .await
            .map(tonic::Response::into_inner)
        }
    };
    let held = status(digest).await.expect("status");
    assert_eq!((held.complete, held.committed_size), (true, 11));
    let absent = status(sha(b"absent")).await.expect("status");
    assert_eq!((absent.complete, absent.committed_size), (false, 0));
    let refused = raw
        .query_write_status(QueryWriteStatusRequest {
            resource_name: "nothing".to_owned(),
        })
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), Code::InvalidArgument);
}

/// Catches uploads that hold more than `write_bytes_in_flight` at once (two 6-byte blobs
/// under a bound of 8 must not both be inside the store's put together), and a blob
/// bigger than the bound that waits for room it can never have (it takes the whole
/// bound and goes through).
#[tokio::test]
async fn concurrent_writes_wait_for_room() {
    let mem = Arc::new(Mem::default());
    *mem.put_takes.lock().expect("lock") = Duration::from_millis(100);
    let channel = serve(&mem, 8).await;
    let remote = RemoteStorage::new(channel, 0);
    let (a, b) = (Bytes::from_static(b"aaaaaa"), Bytes::from_static(b"bbbbbb"));
    let (first, second) = tokio::join!(remote.put(a), remote.put(b));
    first.expect("first");
    second.expect("second");
    assert_eq!(mem.most_active.load(Ordering::SeqCst), 1);
    let big = Bytes::from(patterned(100));
    assert_eq!(
        remote
            .put(big.clone())
            .await
            .expect("larger than the bound"),
        sha(&big)
    );
}

/// Catches the action cache losing something on the wire: a miss that becomes a hit or
/// the reverse, the result blob's digest, a record's closure, a refusal that loses the
/// blob it names (the three kinds), a client that commits (refused, whatever the store
/// says), and a writer or record or result that is not named.
#[tokio::test]
async fn the_action_cache_round_trips() {
    let (_mem, remote, channel) = pair().await;
    let action = sha(b"action");
    assert!(remote.action_result(&action).await.expect("miss").is_none());

    let result = ActionResult {
        stdout_raw: b"out".to_vec(),
        exit_code: 2,
        ..ActionResult::default()
    };
    let record = remote
        .prepare_action_result(&result)
        .await
        .expect("prepared");
    assert_eq!(record.result, sha(b"out"));
    assert_eq!(record.closure.len(), 2);

    let refused = remote
        .commit_action_record(Role::Client, action, record.clone())
        .await
        .expect_err("a client never writes");
    assert!(matches!(
        refused,
        StorageError::ActionWrite(ActionWriteError::NotDaemon)
    ));
    remote
        .commit_action_record(Role::Daemon, action, record.clone())
        .await
        .expect("committed");
    let (digest, got) = remote
        .action_result(&action)
        .await
        .expect("hit")
        .expect("a hit");
    assert_eq!((digest, got.exit_code), (record.result, 2));

    for (code, expected) in [(99, "absent"), (98, "unreachable")] {
        let lacking = ActionResult {
            exit_code: code,
            ..ActionResult::default()
        };
        let error = remote
            .prepare_action_result(&lacking)
            .await
            .expect_err("refused");
        match (expected, error) {
            ("absent", StorageError::ActionWrite(ActionWriteError::Absent(d))) => {
                assert_eq!(d, [sha(b"missing")]);
            }
            ("unreachable", StorageError::ActionWrite(ActionWriteError::Unreachable(d))) => {
                assert_eq!(d, [sha(b"unreadable")]);
            }
            (_, other) => panic!("{expected}: {other:?}"),
        }
    }

    // Raw: what a client of the wire could send that the client does not.
    let mut raw = ActionStoreClient::new(channel);
    let record_of = |r: &ActionRecord| Record {
        result: Some(to_proto(&r.result)),
        closure: r.closure.iter().map(to_proto).collect(),
    };
    let commit = |writer: Writer, record: Option<Record>| CommitRequest {
        map_generation: 0,
        action: Some(to_proto(&action)),
        record,
        writer: writer as i32,
    };
    let unwritten = commit(Writer::Unspecified, Some(record_of(&record)));
    let no_record = commit(Writer::Daemon, None);
    let bad_record = commit(
        Writer::Daemon,
        Some(Record {
            result: None,
            closure: Vec::new(),
        }),
    );
    let bad_closure = commit(
        Writer::Daemon,
        Some(Record {
            result: Some(to_proto(&record.result)),
            closure: vec![reapi::Digest::default()],
        }),
    );
    for request in [unwritten, no_record, bad_record, bad_closure] {
        let status = raw.commit(request).await.expect_err("refused");
        assert_eq!(status.code(), Code::InvalidArgument);
    }
    let no_action = CommitRequest {
        action: None,
        ..commit(Writer::Daemon, Some(record_of(&record)))
    };
    assert_eq!(
        raw.commit(no_action).await.expect_err("refused").code(),
        Code::InvalidArgument
    );
    let no_result = PrepareRequest {
        map_generation: 0,
        result: None,
    };
    assert_eq!(
        raw.prepare(no_result).await.expect_err("refused").code(),
        Code::InvalidArgument
    );
}

/// Catches a store failure on the action cache that is answered in the message (a miss,
/// or a refusal) rather than as the status its error has.
#[tokio::test]
async fn an_action_cache_failure_is_a_status() {
    let (mem, remote, _) = pair().await;
    *mem.fail.lock().expect("lock") = Some(StorageError::Internal("torn".to_owned()));
    let action = sha(b"action");
    let record = ActionRecord {
        result: sha(b"r"),
        closure: kbf_meta::Closure::new(),
    };
    for error in [
        remote
            .action_result(&action)
            .await
            .map(drop)
            .expect_err("fails"),
        remote
            .prepare_action_result(&ActionResult::default())
            .await
            .map(drop)
            .expect_err("fails"),
        remote
            .commit_action_record(Role::Daemon, action, record)
            .await
            .expect_err("fails"),
    ] {
        assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
    }
}

/// Catches an action-store request sent without the map generation in its field (section
/// 6's STALE_MAP could not work), and a client that believes a node that contradicts
/// itself: a result digest or record that is not a digest, an empty answer to Prepare, a
/// read of the wrong size, a write acknowledged as no bytes.
#[tokio::test]
async fn a_client_does_not_trust_a_node_that_contradicts_itself() {
    let (odd, channel) = serve_odd().await;
    let remote = RemoteStorage::new(channel, 9);
    let action = sha(b"a");
    let invalid = |e: StorageError| matches!(e, StorageError::Invalid(_));
    assert!(invalid(
        remote
            .action_result(&action)
            .await
            .map(drop)
            .expect_err("bad digest")
    ));
    let empty = remote
        .prepare_action_result(&ActionResult::default())
        .await
        .map(drop);
    assert!(matches!(
        empty.expect_err("no outcome"),
        StorageError::Internal(_)
    ));
    for exit_code in [1, 2] {
        let bad = ActionResult {
            exit_code,
            ..ActionResult::default()
        };
        let error = remote
            .prepare_action_result(&bad)
            .await
            .expect_err("bad record");
        assert!(invalid(error));
    }
    assert!(
        remote.stream(5).is_none(),
        "a streamed write over the wire is put's messages"
    );
    let record = ActionRecord {
        result: sha(b"r"),
        closure: kbf_meta::Closure::new(),
    };
    remote
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect("committed");
    assert_eq!(*odd.generations.lock().expect("lock"), [9, 9, 9, 9, 9]);

    let five = Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 5);
    let short = remote
        .read_blob(&five)
        .await
        .expect_err("three bytes of five");
    assert!(matches!(short, StorageError::Internal(_)), "{short:?}");
    let unacknowledged = remote
        .put(Bytes::from_static(b"abc"))
        .await
        .expect_err("0 of 3");
    assert!(
        matches!(unacknowledged, StorageError::Internal(_)),
        "{unacknowledged:?}"
    );
}

/// Catches a refusal the client cannot read becoming a success or a wrong error, and a
/// refusal that lost any of its blobs.
#[test]
fn a_refusal_is_read_back_as_it_was_made() {
    use super::client::refused;
    use super::wire::refusal_of;
    let (blob, other) = (sha(b"b"), sha(b"c"));
    for error in [
        ActionWriteError::NotDaemon,
        ActionWriteError::Absent(vec![blob]),
        ActionWriteError::Absent(vec![blob, other]),
        ActionWriteError::Unreachable(vec![blob]),
        ActionWriteError::Unreachable(vec![other, blob]),
    ] {
        let back = refused(&refusal_of(&error));
        assert!(
            matches!(&back, StorageError::ActionWrite(e) if *e == error),
            "{error:?} read back as {back:?}"
        );
    }
    let unreadable = refused(&kbf_proto::storage::Refusal {
        kind: refusal::Kind::Absent as i32,
        blob: None,
        more: Vec::new(),
    });
    assert!(matches!(unreadable, StorageError::Internal(_)));
    let bad_more = refused(&kbf_proto::storage::Refusal {
        kind: refusal::Kind::Absent as i32,
        blob: Some(super::wire::to_proto(&blob)),
        more: vec![kbf_proto::reapi::Digest {
            hash: "not hex".to_owned(),
            size_bytes: 1,
        }],
    });
    assert!(
        matches!(bad_more, StorageError::Internal(_)),
        "{bad_more:?}"
    );
    let unknown = refused(&kbf_proto::storage::Refusal {
        kind: 99,
        blob: None,
        more: Vec::new(),
    });
    assert!(matches!(unknown, StorageError::Internal(_)));
}

/// Catches the resource-name and digest parsing accepting what REAPI forbids or refusing
/// a spelling it allows.
#[test]
fn resource_names_parse_every_spelling() {
    let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let seven = wire::read_resource(&format!("blobs/{hash}/7"))
        .expect("plain")
        .size_bytes;
    assert_eq!(seven, 7);
    for name in [
        format!("main/ci/blobs/{hash}/7"),
        format!("blobs/sha256/{hash}/7"),
    ] {
        assert_eq!(wire::read_resource(&name).expect(&name).size_bytes, 7);
    }
    for name in [
        format!("uploads/u-1/blobs/{hash}/7"),
        format!("inst/a/uploads/u-1/blobs/sha256/{hash}/7/some/metadata"),
    ] {
        assert_eq!(wire::write_resource(&name).expect(&name).size_bytes, 7);
    }
    for bad in [
        format!("blobs/{hash}"),
        format!("blobs/{hash}/-7"),
        format!("compressed-blobs/zstd/{hash}/7"),
        "nothing".to_owned(),
    ] {
        assert!(wire::read_resource(&bad).is_err(), "{bad}");
        let upload = format!("uploads/u-1/{bad}");
        assert!(wire::write_resource(&upload).is_err(), "{upload}");
    }
    // Segments after the size are upload metadata on a write, but an error on a read.
    assert!(wire::read_resource(&format!("blobs/{hash}/7/extra")).is_err());
    assert!(wire::write_resource(&format!("uploads/blobs/{hash}/7")).is_err());
    assert!(wire::write_resource(&format!("blobs/{hash}/7")).is_err());
}

/// Catches a resource name read at the wrong segment when the instance name holds a word
/// of the grammar. An instance name is any path, so `a/uploads/b` and `blobs` are
/// instances, and `{instance}/uploads/{uuid}/blobs/{hash}/{size}` must be read at the
/// `uploads/{uuid}/blobs/` that is followed by a digest (the first, for a metadata
/// tail that holds another). A parser that takes the first `uploads` or `blobs` word, or
/// the last, fails one of these; each case names the size it must come out with.
#[test]
fn an_instance_name_may_hold_the_words_of_the_grammar() {
    let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let other = "ab".repeat(32);
    let writes = [
        // An empty instance, and one that is a single reserved word.
        (format!("uploads/u/blobs/{hash}/7"), 7),
        (format!("uploads/uploads/blobs/{hash}/7"), 7),
        (format!("blobs/uploads/u/blobs/{hash}/7"), 7),
        // An instance holding `uploads` (the first one is not the anchor).
        (format!("a/uploads/b/uploads/u/blobs/{hash}/7"), 7),
        (format!("uploads/uploads/uploads/blobs/{hash}/7"), 7),
        // An instance that holds a whole `uploads/x/blobs/` before the real one.
        (format!("i/uploads/x/blobs/y/uploads/u/blobs/{hash}/7"), 7),
        (
            format!("uploads/x/blobs/uploads/u/blobs/sha256/{hash}/7"),
            7,
        ),
        // A metadata tail that looks like another upload is metadata.
        (
            format!("uploads/u/blobs/{hash}/7/uploads/v/blobs/{other}/9"),
            7,
        ),
        (format!("a/uploads/u/blobs/sha256/{hash}/7/m"), 7),
    ];
    for (name, size) in writes {
        let digest = wire::write_resource(&name).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(digest.size_bytes, size, "{name}");
        assert_eq!(digest.hash_hex(), hash, "{name}");
    }
    let reads = [
        (format!("blobs/{hash}/7"), 7),
        (format!("blobs/blobs/{hash}/7"), 7),
        (format!("a/blobs/b/blobs/{hash}/7"), 7),
        (format!("blobs/b/blobs/sha256/{hash}/7"), 7),
        (format!("sha256/blobs/{hash}/7"), 7),
        (format!("uploads/blobs/{hash}/7"), 7),
    ];
    for (name, size) in reads {
        let digest = wire::read_resource(&name).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(digest.size_bytes, size, "{name}");
    }
    // The words alone, or with no digest after them, are still refused, each with the
    // reason of the first word.
    for bad in ["blobs", "a/blobs/b", "blobs/blobs/blobs"] {
        let refused = wire::read_resource(bad).expect_err(bad);
        assert_eq!(refused.code(), Code::InvalidArgument, "{bad}");
    }
    for bad in ["uploads", "a/uploads/b/blobs", "uploads/u/uploads/v/blobs"] {
        let refused = wire::write_resource(bad).expect_err(bad);
        assert_eq!(refused.code(), Code::InvalidArgument, "{bad}");
        assert!(refused.message().contains("expected"), "{bad}");
    }
    // A read has nothing after the size: a tail is no read, not a metadata.
    assert!(wire::read_resource(&format!("blobs/{hash}/7/extra/blobs")).is_err());
}

/// Catches a FindMissing that has no limit on the digests of a call, or the wrong one:
/// a call at the limit is answered, one digest over it is INVALID_ARGUMENT (the code
/// that says splitting the call is the fix, and that a client does not retry), and
/// nothing is looked up for it. The limit is under what a full message could hold (the
/// smallest digest encodes to 68 bytes), so it is a bound on work, not on bytes.
#[tokio::test]
async fn find_missing_refuses_a_call_over_the_digest_limit() {
    const { assert!(MAX_FIND_MISSING_DIGESTS * 68 < MAX_MESSAGE_BYTES) };
    let (_mem, _remote, channel) = pair().await;
    let mut cas =
        ContentAddressableStorageClient::new(channel).max_decoding_message_size(MAX_MESSAGE_BYTES);
    let digests = |n| vec![to_proto(&sha(b"a")); n];
    let ask = |blob_digests| FindMissingBlobsRequest {
        instance_name: String::new(),
        blob_digests,
        digest_function: 0,
    };
    let at = cas
        .find_missing_blobs(ask(digests(MAX_FIND_MISSING_DIGESTS)))
        .await
        .expect("at the limit")
        .into_inner();
    assert_eq!(at.missing_blob_digests.len(), MAX_FIND_MISSING_DIGESTS);
    let over = cas
        .find_missing_blobs(ask(digests(MAX_FIND_MISSING_DIGESTS + 1)))
        .await
        .expect_err("over the limit");
    assert_eq!(over.code(), Code::InvalidArgument);
    assert!(
        over.message()
            .contains(&(MAX_FIND_MISSING_DIGESTS + 1).to_string())
            && over
                .message()
                .contains(&MAX_FIND_MISSING_DIGESTS.to_string()),
        "{}",
        over.message()
    );
}

/// Catches a client that claims a streamed put it cannot make: the service has no
/// streamed write to the object store, so `put_streamed` is refused as invalid (the
/// caller then stores the blob with `put`), and nothing is stored.
#[tokio::test]
async fn a_streamed_put_is_refused_over_the_wire() {
    let (_mem, remote, _) = pair().await;
    let digest = sha(b"abc");
    let bytes = stream::iter([Ok(Bytes::from_static(b"abc"))]).boxed();
    let claim = Box::pin(async move { Some(digest) });
    let refused = remote
        .put_streamed(3, bytes, claim)
        .await
        .expect_err("no streamed put");
    assert!(matches!(refused, StorageError::Invalid(_)), "{refused:?}");
    assert_eq!(
        remote.find_missing(&[digest]).await.expect("find"),
        [digest]
    );
}

/// Catches a limit that drifted from the front's (a test in kbf-server pins the front's
/// to these): a full 16 MiB batch has to fit, and a blob is streamed from the same size.
#[test]
fn the_limits_hold_a_full_batch() {
    assert_eq!(MAX_MESSAGE_BYTES, (16 << 20) + (1 << 20));
    assert_eq!(STREAM_FROM_BYTES, 16 << 20);
}

/// `len` bytes of a blob as `Write` messages of at most 1 MiB, the last one finishing
/// the write, with the first named `name`.
fn blob_messages(name: &str, bytes: &[u8]) -> Vec<WriteRequest> {
    let count = bytes.len().div_ceil(READ_CHUNK_BYTES);
    (0..count)
        .map(|i| WriteRequest {
            resource_name: if i == 0 {
                name.to_owned()
            } else {
                String::new()
            },
            write_offset: (i * READ_CHUNK_BYTES) as i64,
            finish_write: i + 1 == count,
            data: bytes[i * READ_CHUNK_BYTES..bytes.len().min((i + 1) * READ_CHUNK_BYTES)].to_vec(),
        })
        .collect()
}

/// Catches a Write of a blob past `STREAM_FROM_BYTES` that is gathered whole (the store's
/// `put` called instead of `put_streamed`, or the blob held in one buffer), and one that
/// is streamed but stored wrong: the blob is stored, byte for byte, by `put_streamed`
/// alone, from pieces of at most one message; one `STREAM_FROM_BYTES` long is still
/// gathered and put.
#[tokio::test]
async fn a_large_write_is_streamed_to_the_store() {
    let (mem, _remote, channel) = pair().await;
    let large = patterned(STREAM_FROM_BYTES as usize + 1);
    let digest = sha(&large);
    let answer = raw_write(&channel, blob_messages(&upload_name(&digest), &large))
        .await
        .expect("written");
    assert_eq!(answer.committed_size, large.len() as i64);
    assert_eq!(mem.streamed.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(mem.puts.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        mem.streamed_most_held
            .load(std::sync::atomic::Ordering::SeqCst)
            <= READ_CHUNK_BYTES
    );
    assert_eq!(
        mem.blobs.lock().expect("lock")[&digest].as_ref(),
        &large[..]
    );
    let edge = patterned(STREAM_FROM_BYTES as usize);
    let edge_digest = sha(&edge);
    raw_write(&channel, blob_messages(&upload_name(&edge_digest), &edge))
        .await
        .expect("written");
    assert_eq!(mem.puts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(mem.streamed.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Catches a streamed Write that stores what it must not or answers the wrong code: other
/// bytes than the digest names, more or fewer bytes than it names, a stream that stops
/// without `finish_write`, are all INVALID_ARGUMENT and store nothing; and a blob is not
/// refused for its size (a digest naming 5 GiB gets as far as its bytes, which fall
/// short).
#[tokio::test]
async fn a_streamed_write_is_checked_against_its_digest() {
    let (mem, _remote, channel) = pair().await;
    let large = patterned(STREAM_FROM_BYTES as usize + 1);
    let digest = sha(&large);
    let name = upload_name(&digest);
    let mut wrong = large.clone();
    wrong[7] ^= 1;
    let mut more = large.clone();
    more.push(1);
    let mut no_finish = blob_messages(&name, &large);
    no_finish.last_mut().expect("messages").finish_write = false;
    let fewer = blob_messages(&name, &large[..large.len() - 1]);
    let huge = Digest::new(kbf_types::DigestFunction::Sha256, [3; 32], 5 << 30);
    let cases = [
        ("other bytes", blob_messages(&name, &wrong)),
        ("more than named", blob_messages(&name, &more)),
        ("no finish", no_finish),
        ("fewer than named", fewer),
        (
            "5 GiB named",
            write_of(&upload_name(&huge), &[(b"short", true)]),
        ),
    ];
    for (what, requests) in cases {
        let status = raw_write(&channel, requests).await.expect_err(what);
        assert_eq!(status.code(), Code::InvalidArgument, "{what}: {status}");
        assert!(!status.message().contains("largest"), "{what}: {status}");
    }
    assert!(mem.blobs.lock().expect("lock").is_empty());
}

/// Catches a streamed Write whose last message carries no bytes: the blob is complete
/// before it, and `finish_write` alone ends the write, as a sender that does not know
/// which message is its last would send it. The blob is stored whole, once.
#[tokio::test]
async fn a_streamed_write_may_finish_with_an_empty_message() {
    let (mem, _remote, channel) = pair().await;
    let large = patterned(STREAM_FROM_BYTES as usize + 1);
    let digest = sha(&large);
    let mut messages = blob_messages(&upload_name(&digest), &large);
    messages.last_mut().expect("messages").finish_write = false;
    messages.push(WriteRequest {
        write_offset: large.len() as i64,
        finish_write: true,
        ..WriteRequest::default()
    });
    let answer = raw_write(&channel, messages).await.expect("written");
    assert_eq!(answer.committed_size, large.len() as i64);
    assert_eq!(mem.streamed.load(Ordering::SeqCst), 1);
    assert_eq!(
        mem.blobs.lock().expect("lock")[&digest].as_ref(),
        &large[..]
    );
}

/// Catches a streamed Write whose sender goes away mid-stream (its connection closed after
/// the store took a piece) that stores a partial blob or leaves the
/// store waiting: the store's pieces end in an error and nothing is stored.
#[tokio::test]
async fn a_streamed_write_whose_sender_goes_away_stores_nothing() {
    let mem = Arc::new(Mem::default());
    let channel = serve(&mem, 1 << 30).await;
    let large = patterned(STREAM_FROM_BYTES as usize + 1);
    let digest = sha(&large);
    let mut messages = blob_messages(&upload_name(&digest), &large);
    messages.pop();
    let mut client = ByteStreamClient::new(channel);
    {
        let call = client.write(stream::iter(messages).chain(stream::pending()));
        tokio::pin!(call);
        tokio::select! {
            answer = &mut call => panic!("answered a write that never finished: {answer:?}"),
            () = mem.piece_seen.notified() => {}
        }
    }
    drop(client);
    mem.pieces_failed.notified().await;
    assert!(mem.blobs.lock().expect("lock").is_empty());
    assert_eq!(mem.streamed.load(Ordering::SeqCst), 0);
}

/// Catches a streamed Write whose second message the server cannot decode (it is larger
/// than the largest message): the broken stream is the sender's fault, answered with the
/// decoder's status, not INTERNAL, and nothing is stored.
#[tokio::test]
async fn a_streamed_write_with_an_oversized_message_stores_nothing() {
    let (mem, _remote, channel) = pair().await;
    let large = patterned(STREAM_FROM_BYTES as usize + 1);
    let digest = sha(&large);
    let mut messages = blob_messages(&upload_name(&digest), &large);
    messages.truncate(1);
    messages[0].finish_write = false;
    messages.push(WriteRequest {
        write_offset: messages[0].data.len() as i64,
        data: vec![0; super::MAX_MESSAGE_BYTES + 1],
        ..WriteRequest::default()
    });
    let status = raw_write(&channel, messages).await.expect_err("refused");
    assert_eq!(status.code(), Code::OutOfRange, "{status}");
    assert!(mem.blobs.lock().expect("lock").is_empty());
    assert_eq!(mem.streamed.load(Ordering::SeqCst), 0);
}

/// Catches a remote storage that gathers a streamed put or answers it as if stored: it
/// takes none (it writes a blob through `put`), and says so as `Invalid`, consuming
/// nothing.
#[tokio::test]
async fn a_remote_storage_takes_no_streamed_put() {
    let (mem, remote, _) = pair().await;
    let bytes = patterned(100);
    let digest = sha(&bytes);
    let body = stream::iter([Ok(Bytes::from(bytes))]).boxed();
    let refused = remote
        .put_streamed(100, body, Box::pin(async move { Some(digest) }))
        .await;
    assert!(
        matches!(refused, Err(StorageError::Invalid(_))),
        "{refused:?}"
    );
    assert!(mem.blobs.lock().expect("lock").is_empty());
}
