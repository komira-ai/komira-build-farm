//! The map generation over the wire: the header and the action store's field carry it
//! on every call, it can be updated after the client is built, and a node's STALE_MAP
//! reaches the caller as `StaleMap` with the node's generation.

use std::sync::Arc;

use bytes::Bytes;
use kbf_meta::{ActionRecord, Role};
use kbf_proto::google::bytestream::QueryWriteStatusRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{
    BeginRequest, CommitRequest as SegmentsCommit, LocateRequest, PutRangeRequest, ReadRangeRequest,
};
use tonic::{Code, Request, Status};

use super::super::{MAP_GENERATION_HEADER, RemoteStorage};
use super::fake::{Mem, pair, serve_odd, serve_spied, sha};
use super::upload_name;
use crate::{Storage, StorageError};

/// Catches a STALE_MAP the client does not recognise (it would be retried as an outage,
/// against the same stale owners), one it recognises from the code alone (any
/// FAILED_PRECONDITION, such as a refused action-cache write, would send the caller to
/// refetch the map), one it reads without checking the code, and a generation it
/// misreads.
#[test]
fn a_stale_map_status_is_lifted_with_the_nodes_generation() {
    use super::super::client::lift;
    use tonic::metadata::MetadataValue;
    let with = |code: Code, generation: &str| {
        let mut status = Status::new(code, "STALE_MAP");
        status.metadata_mut().insert(
            MAP_GENERATION_HEADER,
            MetadataValue::try_from(generation).expect("ASCII"),
        );
        status
    };
    assert!(matches!(
        lift(&with(Code::FailedPrecondition, "9"), None),
        StorageError::StaleMap { current: 9 }
    ));
    // The same words without the entry, with an entry that is not a generation, and on
    // another code, are not a stale map.
    for status in [
        Status::failed_precondition("STALE_MAP"),
        with(Code::FailedPrecondition, "nine"),
        with(Code::Unavailable, "9"),
    ] {
        assert!(
            !matches!(lift(&status, None), StorageError::StaleMap { .. }),
            "{status:?}"
        );
    }
}

/// Catches a STALE_MAP that does not cross the wire as itself: over a real connection a
/// node's answer reaches the caller as `StaleMap` with the generation, on a blob call
/// and on an action-cache call, and is not NOT_FOUND, an outage or a refusal.
#[tokio::test]
async fn a_stale_map_crosses_the_wire() {
    let (mem, remote, _) = pair().await;
    *mem.fail.lock().expect("lock") = Some(StorageError::StaleMap { current: 12 });
    let digest = sha(b"x");
    let errors = [
        remote.find_missing(&[digest]).await.expect_err("stale"),
        remote.read_range(&digest, 0, 0).await.err().expect("stale"),
        remote.action_result(&digest).await.expect_err("stale"),
    ];
    for error in errors {
        assert!(
            matches!(error, StorageError::StaleMap { current: 12 }),
            "{error:?}"
        );
    }
}

/// Catches `Segments` calls dropped, answered locally, or sent without the map
/// generation: all five reach the store's own handlers, each carrying generation 5.
#[tokio::test]
async fn segments_are_forwarded_with_the_map_generation() {
    let (mem, remote, _) = pair().await;
    let segments = remote.segments();
    let begun = segments
        .begin(Request::new(BeginRequest::default()))
        .await
        .expect("begin");
    assert_eq!(begun.into_inner().upload, 7);
    segments
        .put_range(Request::new(PutRangeRequest::default()))
        .await
        .expect("put_range");
    let committed = segments
        .commit(Request::new(SegmentsCommit::default()))
        .await
        .expect("commit");
    assert_eq!(committed.into_inner().committed.len(), 1);
    segments
        .locate(Request::new(LocateRequest::default()))
        .await
        .expect("locate");
    let read = segments
        .read_range(Request::new(ReadRangeRequest::default()))
        .await
        .expect("read");
    assert_eq!(read.into_inner().bytes, [1, 2]);
    let seen = mem.segments.calls.lock().expect("lock").clone();
    let five = Some("5".to_owned());
    let expected: Vec<_> = ["begin", "put_range", "commit", "locate", "read_range"]
        .into_iter()
        .map(|call| (call, five.clone()))
        .collect();
    assert_eq!(seen, expected);
}

/// Catches a map generation that cannot change after the client is built, or that does
/// not reach every call: the header arrives at the server on every service the client
/// calls (FindMissing, ByteStream Read and Write, the action store, all five Segments
/// calls), as 5 for the first of each and as 6 for each after `set_map_generation`,
/// whichever clone made the update. (`RemoteStorage` makes no `QueryWriteStatus` call;
/// the last call here is a raw one that carries the header, to see that service's
/// handler is behind the same stack.)
#[tokio::test]
async fn the_map_generation_reaches_every_service_and_can_be_updated() {
    let mem = Arc::new(Mem::default());
    let (seen, channel) = serve_spied(&mem).await;
    let remote = RemoteStorage::new(channel.clone(), 5);
    let held = remote.put(Bytes::from_static(b"abc")).await.expect("write");
    remote.find_missing(&[held]).await.expect("find_missing");
    remote.read_blob(&held).await.expect("read");
    let action = sha(b"action");
    remote.action_result(&action).await.expect("get");
    remote
        .prepare_action_result(&ActionResult::default())
        .await
        .expect("prepare");
    let record = ActionRecord {
        result: held,
        closure: kbf_meta::Closure::new(),
    };
    remote
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect("commit");
    let segments = remote.segments();
    segments
        .begin(Request::new(BeginRequest::default()))
        .await
        .expect("begin");
    segments
        .put_range(Request::new(PutRangeRequest::default()))
        .await
        .expect("put_range");
    segments
        .commit(Request::new(SegmentsCommit::default()))
        .await
        .expect("commit");
    segments
        .locate(Request::new(LocateRequest::default()))
        .await
        .expect("locate");
    segments
        .read_range(Request::new(ReadRangeRequest::default()))
        .await
        .expect("read_range");
    let mut raw = Request::new(QueryWriteStatusRequest {
        resource_name: upload_name(&held),
    });
    raw.metadata_mut()
        .insert(MAP_GENERATION_HEADER, 5u64.into());
    ByteStreamClient::new(channel)
        .query_write_status(raw)
        .await
        .expect("query_write_status");

    let services = [
        "bytestream",
        "cas",
        "bytestream",
        "actions",
        "actions",
        "actions",
        "segments",
        "segments",
        "segments",
        "segments",
        "segments",
        "bytestream",
    ];
    let first: Vec<_> = std::mem::take(&mut *seen.lock().expect("lock"));
    let want: Vec<_> = services
        .into_iter()
        .map(|service| (service, Some("5".to_owned())))
        .collect();
    assert_eq!(first, want);

    // An update through a clone, and the original (and its Segments handle) call again.
    remote.clone().set_map_generation(6);
    remote.find_missing(&[held]).await.expect("find_missing");
    remote.read_blob(&held).await.expect("read");
    remote.action_result(&action).await.expect("get");
    segments
        .begin(Request::new(BeginRequest::default()))
        .await
        .expect("begin");
    let second = seen.lock().expect("lock").clone();
    let want: Vec<_> = ["cas", "bytestream", "actions", "segments"]
        .into_iter()
        .map(|service| (service, Some("6".to_owned())))
        .collect();
    assert_eq!(second, want);
}

/// Catches an action-store call that keeps the generation the client was built with:
/// the message's field, which section 6's STALE_MAP reads, is the updated one on `Get`,
/// `Prepare` and `Commit`, and is the one the header carries (see above).
#[tokio::test]
async fn an_updated_generation_is_in_the_action_store_field() {
    let (odd, channel) = serve_odd().await;
    let remote = RemoteStorage::new(channel, 9);
    let record = || ActionRecord {
        result: sha(b"r"),
        closure: kbf_meta::Closure::new(),
    };
    let calls = |remote: &RemoteStorage| {
        let remote = remote.clone();
        async move {
            let _ = remote.action_result(&sha(b"a")).await;
            let _ = remote.prepare_action_result(&ActionResult::default()).await;
            remote
                .commit_action_record(Role::Daemon, sha(b"a"), record())
                .await
                .expect("commit");
        }
    };
    calls(&remote).await;
    remote.set_map_generation(10);
    calls(&remote).await;
    assert_eq!(
        *odd.generations.lock().expect("lock"),
        [9, 9, 9, 10, 10, 10]
    );
}
