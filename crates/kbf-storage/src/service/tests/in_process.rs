//! A node called in its own process ([`InProcess`]): the same checks and the same write
//! body as a call over the wire, with no connection and no certificate.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_meta::{ActionRecord, ActionWriteError, Role};
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{
    BeginRequest, CommitRequest, LocateRequest, PutRangeRequest, ReadRangeRequest,
};
use tonic::{Code, Request, Status};

use super::fake::{Held, Mem, connect, patterned, serve_routes, sha};
use crate::replicate::MapSource;
use crate::service::{
    Admission, CatchUp, Config, InProcess, RemoteStorage, STREAM_FROM_BYTES, StorageService,
};
use crate::{Storage, StorageError};

/// A node over `mem` that judges every call by `held`, called in-process. Certified
/// admission is not asked for: a service in no chain admits whom it is told to, and the
/// in-process path admits no one (there is nobody to admit).
fn node(mem: &Arc<Mem>, held: &Arc<Held>) -> InProcess {
    let storage: Arc<dyn Storage> = Arc::clone(mem) as Arc<dyn Storage>;
    let service = StorageService::new(
        storage,
        Admission::Certified,
        Config {
            write_bytes_in_flight: 1 << 30,
        },
    )
    .with_map(Arc::clone(held) as Arc<dyn MapSource>);
    Arc::new(service).in_process()
}

fn is_stale<T>(answer: &Result<T, StorageError>, current: u64) -> bool {
    matches!(answer, Err(StorageError::StaleMap { current: c }) if *c == current)
}

/// Catches an in-process write that goes around the service: it must be judged by the map
/// generation the caller names, as a call over the wire is. A caller behind the node (an
/// older generation, or 0) gets STALE_MAP naming the node's generation and nothing is
/// stored; one ahead of it is UNAVAILABLE; one at it is stored, with the digest returned.
/// A blob the node holds is answered without a second store, and one of more than a
/// chunk (two messages) is stored whole.
///
/// Mutants: `put` calling the inner storage (no check: the stale write lands); the check
/// removed from the write body; the generation read from the wrong field (the current
/// write is refused).
#[tokio::test]
async fn a_write_is_judged_by_the_generation_the_caller_names_and_stored_once() {
    let mem = Arc::new(Mem::default());
    let held = Held::at(7);
    let node = node(&mem, &held);

    for behind in [5, 0] {
        let answer = node.at(behind).put(Bytes::from_static(b"payload")).await;
        assert!(is_stale(&answer, 7), "generation {behind}: {answer:?}");
    }
    let ahead = node.at(9).put(Bytes::from_static(b"payload")).await;
    assert!(
        matches!(ahead, Err(StorageError::Unavailable(_))),
        "{ahead:?}"
    );
    assert_eq!(
        mem.puts.load(Ordering::SeqCst),
        0,
        "stored under a stale map"
    );

    let current = node.at(7);
    let digest = current
        .put(Bytes::from_static(b"payload"))
        .await
        .expect("under 7");
    assert_eq!(digest, sha(b"payload"));
    assert_eq!(mem.puts.load(Ordering::SeqCst), 1);
    current
        .put(Bytes::from_static(b"payload"))
        .await
        .expect("held already");
    assert_eq!(
        mem.puts.load(Ordering::SeqCst),
        1,
        "a held blob is not stored again"
    );

    let large = patterned((1 << 20) + 5);
    let digest = current
        .put(Bytes::from(large.clone()))
        .await
        .expect("two messages");
    assert_eq!(digest, sha(&large));
    let stored = current.read_blob(&digest).await.expect("read back");
    assert_eq!(stored.as_ref(), large.as_slice());
}

/// Catches an in-process unowned write that skips the generation check or stores nothing:
/// it is judged by the caller's generation like any write (STALE_MAP behind the node), and
/// at the node's generation the blob is stored. A store that does not distinguish the two
/// (the default of `Storage::put_unowned`) takes it as a plain write.
#[tokio::test]
async fn an_unowned_write_is_judged_by_the_generation_and_stored() {
    let mem = Arc::new(Mem::default());
    let node = node(&mem, &Held::at(7));
    let stale = node
        .at(3)
        .put_unowned(Bytes::from_static(b"copy"))
        .await
        .expect_err("behind the node");
    assert!(
        matches!(stale, StorageError::StaleMap { current: 7 }),
        "{stale:?}"
    );
    let digest = node
        .at(7)
        .put_unowned(Bytes::from_static(b"copy"))
        .await
        .expect("stored");
    assert!(mem.blobs.lock().expect("lock").contains_key(&digest));
    let plain = mem
        .put_unowned(Bytes::from_static(b"plain"))
        .await
        .expect("the default is a plain put");
    assert!(mem.blobs.lock().expect("lock").contains_key(&plain));
}

/// Catches a store failure taken for a stale map or swallowed: the store's failure is the
/// write's, and reaches the caller as the store's error.
#[tokio::test]
async fn a_store_that_fails_fails_the_write() {
    let mem = Arc::new(Mem::default());
    let node = node(&mem, &Held::at(7));
    *mem.fail.lock().expect("lock") = Some(StorageError::Internal("disk".to_owned()));
    let answer = node.at(7).put(Bytes::from_static(b"payload")).await;
    assert!(
        matches!(&answer, Err(StorageError::Internal(why)) if why.contains("disk")),
        "{answer:?}"
    );
}

/// Catches the write body (the one a `Write` call and an in-process write both run) that
/// answers a stream ending early with anything but INVALID_ARGUMENT naming how it ended:
/// no message at all, or messages that stop before `finish_write`. Nothing is stored.
#[tokio::test]
async fn a_write_stream_that_ends_early_is_invalid_and_stores_nothing() {
    let mem = Arc::new(Mem::default());
    let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
    let service = StorageService::new(
        storage,
        Admission::Certified,
        Config {
            write_bytes_in_flight: 1 << 30,
        },
    );
    let blob = b"twelve bytes";
    let name = format!("uploads/u-1/blobs/{}/{}", sha(blob).hash_hex(), blob.len());
    let message = |name: &str, data: &[u8]| WriteRequest {
        resource_name: name.to_owned(),
        write_offset: 0,
        finish_write: false,
        data: data.to_vec(),
    };
    let cases = [
        ("empty", Vec::new(), "write stream sent no message"),
        (
            "one message",
            vec![message(&name, &blob[..5])],
            "write stream ended without finish_write",
        ),
        (
            "two messages",
            vec![message(&name, &blob[..5]), message("", &blob[5..8])],
            "write stream ended without finish_write",
        ),
    ];
    for (what, requests, text) in cases {
        let stream = futures::stream::iter(requests.into_iter().map(Ok)).boxed();
        let status = service
            .write_in(0, None, false, stream)
            .await
            .expect_err(what);
        assert_eq!(status.code(), Code::InvalidArgument, "{what}: {status}");
        assert_eq!(status.message(), text, "{what}");
    }
    assert_eq!(mem.puts.load(Ordering::SeqCst), 0, "something was stored");
    assert!(mem.blobs.lock().expect("lock").is_empty());
}

/// Catches a write that swallows or rewrites the error its own stream carries (a client
/// that resets mid-upload): the status the stream yields, before the first message and
/// after it, is the call's answer, and nothing is stored.
#[tokio::test]
async fn a_write_stream_that_fails_answers_with_its_own_status() {
    let mem = Arc::new(Mem::default());
    let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
    let service = StorageService::new(
        storage,
        Admission::Certified,
        Config {
            write_bytes_in_flight: 1 << 30,
        },
    );
    let blob = b"twelve bytes";
    let name = format!("uploads/u-1/blobs/{}/{}", sha(blob).hash_hex(), blob.len());
    let first = WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: false,
        data: blob[..5].to_vec(),
    };
    let cases = [
        ("before any message", vec![Err(Status::aborted("reset"))]),
        (
            "after a message",
            vec![Ok(first), Err(Status::aborted("reset"))],
        ),
    ];
    for (what, items) in cases {
        let stream = futures::stream::iter(items).boxed();
        let status = service
            .write_in(0, None, false, stream)
            .await
            .expect_err(what);
        assert_eq!(status.code(), Code::Aborted, "{what}: {status}");
        assert_eq!(status.message(), "reset", "{what}");
    }
    assert_eq!(mem.puts.load(Ordering::SeqCst), 0, "something was stored");
}

/// Catches an in-process read, find or action call that skips the generation check: a
/// reader on a stale map must be told so (it refetches the map and reads from the right
/// owner), not served from a node that no longer owns the blob. Every `Storage` call of a
/// caller behind the node is STALE_MAP, and the same calls at the node's generation are
/// the store's own answers.
///
/// Mutants: the check removed from any one call (that call answers under generation 3).
#[tokio::test]
async fn every_call_is_judged_by_the_generation_the_caller_names() {
    let mem = Arc::new(Mem::default());
    let held = Held::at(7);
    let node = node(&mem, &held);
    let digest = node
        .at(7)
        .put(Bytes::from_static(b"payload"))
        .await
        .expect("stored");
    let (stale, current) = (node.at(3), node.at(7));
    let action = sha(b"action");
    let result = ActionResult::default();

    assert!(is_stale(&stale.find_missing(&[digest]).await, 7));
    assert!(is_stale(&stale.read_blob(&digest).await, 7));
    assert!(is_stale(&stale.read_range(&digest, 0, 0).await, 7));
    assert!(is_stale(&stale.action_result(&action).await, 7));
    assert!(is_stale(&stale.prepare_action_result(&result).await, 7));
    let commit = stale
        .commit_action_record(
            Role::Daemon,
            action,
            kbf_meta::ActionRecord {
                result: digest,
                closure: Default::default(),
            },
        )
        .await;
    assert!(is_stale(&commit, 7), "{commit:?}");
    assert!(stale.stream(8).is_none());

    assert!(
        current
            .find_missing(&[digest])
            .await
            .expect("find")
            .is_empty()
    );
    assert_eq!(
        current.read_blob(&digest).await.expect("blob").as_ref(),
        b"payload"
    );
    let chunks: Vec<_> = current
        .read_range(&digest, 3, 0)
        .await
        .expect("range")
        .collect()
        .await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].as_ref().expect("chunk").as_ref(), b"load");
    assert!(current.action_result(&action).await.expect("get").is_none());
    let record = current
        .prepare_action_result(&result)
        .await
        .expect("prepare");
    current
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect("commit");
    assert!(current.action_result(&action).await.expect("get").is_some());
    assert!(current.stream(8).is_none());
}

/// Catches `Segments` handlers that skip the generation check, or that judge by the
/// request's header instead of the caller's generation: a caller behind the node is
/// FAILED_PRECONDITION on each of the five and the store's handler is not reached; at the
/// node's generation each reaches the store's handler.
#[tokio::test]
async fn the_segments_handlers_are_judged_too() {
    let mem = Arc::new(Mem::default());
    let node = node(&mem, &Held::at(7));
    let behind = node.at(3).segments();
    let at = node.at(7).segments();

    let refused = [
        behind
            .begin(Request::new(BeginRequest::default()))
            .await
            .err(),
        behind
            .put_range(Request::new(PutRangeRequest::default()))
            .await
            .err(),
        behind
            .commit(Request::new(CommitRequest::default()))
            .await
            .err(),
        behind
            .locate(Request::new(LocateRequest::default()))
            .await
            .err(),
        behind
            .read_range(Request::new(ReadRangeRequest::default()))
            .await
            .err(),
    ];
    for status in refused {
        assert_eq!(status.expect("refused").code(), Code::FailedPrecondition);
    }
    assert!(mem.segments.calls.lock().expect("lock").is_empty());

    at.begin(Request::new(BeginRequest::default()))
        .await
        .expect("begin");
    at.put_range(Request::new(PutRangeRequest::default()))
        .await
        .expect("put_range");
    at.commit(Request::new(CommitRequest::default()))
        .await
        .expect("commit");
    at.locate(Request::new(LocateRequest::default()))
        .await
        .expect("locate");
    at.read_range(Request::new(ReadRangeRequest::default()))
        .await
        .expect("read_range");
    let seen: Vec<_> = mem
        .segments
        .calls
        .lock()
        .expect("lock")
        .iter()
        .map(|(call, _)| *call)
        .collect();
    assert_eq!(
        seen,
        ["begin", "put_range", "commit", "locate", "read_range"]
    );
}

/// Catches an in-process call that takes a catching-up node's miss for a real one: while the
/// node rebuilds its index, a read (whole, ranged), a FindMissing with a miss, an
/// action-cache lookup with no entry, and a Prepare or Commit naming a closure blob the store
/// did not find must be UNAVAILABLE, with the very text the same node answers over the wire
/// (the daemon then asks another owner instead of acting on a false miss). Once the rebuild
/// is done the same calls are the store's own answers: NOT_FOUND, the digest listed, no entry,
/// the Absent refusal.
///
/// Mutants: the in-process read of either kind skipping the catch-up rule (NOT_FOUND comes
/// back); FindMissing, the action lookup, or the write refusal skipping it; a rule applied
/// after the rebuild is done.
#[tokio::test]
async fn a_miss_on_a_catching_up_node_is_unavailable_as_over_the_wire() {
    let mem = Arc::new(Mem::default());
    let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
    let catch_up = CatchUp::rebuilding();
    let config = Config {
        write_bytes_in_flight: 1 << 30,
    };
    let service = Arc::new(
        StorageService::new(storage, Admission::Open, config).with_catch_up(catch_up.clone()),
    );
    let local = service.in_process().at(0);
    let wire = RemoteStorage::new(
        connect(serve_routes(service.shared_routes(), None).await).await,
        0,
    );

    let unindexed = sha(b"in cold storage, not yet indexed");
    let action = sha(b"an action");
    let absent = ActionResult {
        exit_code: 99,
        ..ActionResult::default()
    };
    let closure = sha(b"a closure blob");
    *mem.fail.lock().expect("lock") = None;
    let record = ActionRecord {
        result: sha(b"result"),
        closure: [closure].into_iter().collect(),
    };

    // Each call as text: the same rule must give the same words by either road.
    async fn words(
        storage: &Arc<dyn Storage>,
        unindexed: &kbf_types::Digest,
        action: &kbf_types::Digest,
        absent: &ActionResult,
    ) -> Vec<String> {
        vec![
            format!("{:?}", storage.read_blob(unindexed).await.map(drop)),
            format!("{:?}", storage.read_range(unindexed, 0, 0).await.map(drop)),
            format!("{:?}", storage.find_missing(&[*unindexed]).await),
            format!("{:?}", storage.action_result(action).await),
            format!("{:?}", storage.prepare_action_result(absent).await),
        ]
    }
    let wire: Arc<dyn Storage> = Arc::new(wire);
    let (near, far) = (
        words(&local, &unindexed, &action, &absent).await,
        words(&wire, &unindexed, &action, &absent).await,
    );
    for text in &near {
        assert!(
            text.contains("Unavailable") && text.contains("catching up"),
            "{near:?}"
        );
    }
    assert_eq!(near, far, "the same words in-process and over the wire");

    *mem.fail.lock().expect("lock") = Some(ActionWriteError::Absent(vec![closure]).into());
    let committed = local
        .commit_action_record(Role::Daemon, action, record.clone())
        .await;
    assert!(
        matches!(&committed, Err(StorageError::Unavailable(why)) if why.to_string().contains("catching up")),
        "Commit: {committed:?}"
    );
    *mem.fail.lock().expect("lock") = Some(StorageError::Internal("disk".to_owned()));
    let broken = local.prepare_action_result(&absent).await;
    assert!(
        matches!(&broken, Err(StorageError::Internal(why)) if why.contains("disk")),
        "a store failure is not a miss, catching up or not: {broken:?}"
    );
    *mem.fail.lock().expect("lock") = None;

    catch_up.finish();
    let read = local.read_blob(&unindexed).await;
    assert!(
        matches!(read, Err(StorageError::NotFound(d)) if d == unindexed),
        "{read:?}"
    );
    let ranged = local.read_range(&unindexed, 0, 0).await.map(drop);
    assert!(
        matches!(ranged, Err(StorageError::NotFound(d)) if d == unindexed),
        "{ranged:?}"
    );
    assert_eq!(
        local.find_missing(&[unindexed]).await.expect("find"),
        [unindexed]
    );
    assert!(
        local
            .action_result(&action)
            .await
            .expect("lookup")
            .is_none()
    );
    let prepared = local.prepare_action_result(&absent).await;
    assert!(
        matches!(
            prepared,
            Err(StorageError::ActionWrite(ActionWriteError::Absent(_)))
        ),
        "{prepared:?}"
    );
    *mem.fail.lock().expect("lock") = Some(ActionWriteError::Absent(vec![closure]).into());
    let committed = local
        .commit_action_record(Role::Daemon, action, record)
        .await;
    assert!(
        matches!(&committed, Err(StorageError::ActionWrite(ActionWriteError::Absent(b))) if *b == [closure]),
        "{committed:?}"
    );
}

/// Catches an in-process write of a blob past `STREAM_FROM_BYTES`, on a store that cannot
/// write as bytes arrive, that is gathered whole (the store's `put` called instead of
/// `put_streamed`, or the blob held in one buffer) or stored wrong: it reaches
/// `put_streamed` once, in pieces of at most one write message (1 MiB), and is stored
/// byte for byte.
#[tokio::test]
async fn an_in_process_write_past_the_stream_size_is_streamed_in_pieces() {
    let mem = Arc::new(Mem::default());
    let node = node(&mem, &Held::at(7));
    let large = patterned(STREAM_FROM_BYTES as usize + 1);
    let digest = node
        .at(7)
        .put(Bytes::from(large.clone()))
        .await
        .expect("written");
    assert_eq!(digest, sha(&large));
    assert_eq!(mem.streamed.load(Ordering::SeqCst), 1);
    assert_eq!(mem.puts.load(Ordering::SeqCst), 0);
    assert!(mem.streamed_most_held.load(Ordering::SeqCst) <= 1 << 20);
    assert_eq!(
        mem.blobs.lock().expect("lock")[&digest].as_ref(),
        &large[..]
    );
    let edge = patterned(STREAM_FROM_BYTES as usize);
    node.at(7).put(Bytes::from(edge)).await.expect("written");
    assert_eq!(
        mem.puts.load(Ordering::SeqCst),
        1,
        "a blob of exactly STREAM_FROM_BYTES is put whole"
    );
    assert_eq!(mem.streamed.load(Ordering::SeqCst), 1);
}

/// Catches the messages of a write made all at once (a second whole copy of the blob held
/// before the first message is sent): with one message taken, the rest are still to be
/// made from the caller's bytes, which the messages therefore still share.
#[test]
fn write_messages_are_made_as_they_are_taken() {
    let bytes = Bytes::from(patterned(3 << 20));
    let held = bytes.clone();
    let mut messages =
        super::super::client::write_messages("uploads/u/blobs/x/3".to_owned(), bytes);
    let first = messages.next().expect("a first message");
    assert_eq!(first.data.len(), 1 << 20);
    assert!(
        !held.is_unique(),
        "the rest of the messages were made before they were taken"
    );
    assert_eq!(messages.count(), 2);
    assert!(held.is_unique());
}
