//! A write under a map the node does not hold: STALE_MAP for a caller behind it, UNAVAILABLE
//! for one ahead of it, and nothing stored either way.

use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::stream;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};

use super::fake::{Held, Mem, serve_mapped, sha};
use super::{upload_name, write_of};
use crate::service::client::lift;
use crate::service::{MAP_GENERATION_HEADER, RemoteStorage};
use crate::{Storage, StorageError};

async fn node(generation: u64) -> (std::sync::Arc<Mem>, std::sync::Arc<Held>, Channel) {
    let mem = std::sync::Arc::new(Mem::default());
    let held = Held::at(generation);
    let channel = serve_mapped(&mem, &held).await;
    (mem, held, channel)
}

fn stale(current: u64) -> impl Fn(&Result<crate::Digest, StorageError>) -> bool {
    move |r| matches!(r, Err(StorageError::StaleMap { current: c }) if *c == current)
}

/// Catches a node that serves a write routed by a map it does not hold: a caller behind
/// the node (an older generation, or 0 for "no map seen") must get STALE_MAP naming the
/// node's generation, and the blob must not be stored; the same blob under the node's
/// generation is stored. Mutants: the check removed; `<` made `<=` (the current
/// generation refused); the check moved after the store (the blob lands before the
/// refusal).
#[tokio::test]
async fn a_caller_behind_the_node_gets_stale_map_and_nothing_is_stored() {
    let (mem, _, channel) = node(7).await;
    for behind in [5, 0] {
        let client = RemoteStorage::new(channel.clone(), behind);
        let answer = client.put(Bytes::from_static(b"payload")).await;
        assert!(stale(7)(&answer), "generation {behind}: {answer:?}");
    }
    assert_eq!(
        mem.puts.load(Ordering::SeqCst),
        0,
        "stored under a stale map"
    );
    assert!(mem.blobs.lock().expect("lock").is_empty());
    let current = RemoteStorage::new(channel, 7);
    let digest = current
        .put(Bytes::from_static(b"payload"))
        .await
        .expect("put");
    assert_eq!(digest, sha(b"payload"));
    assert_eq!(mem.puts.load(Ordering::SeqCst), 1);
}

/// Catches a node that tells a caller that is ahead of it that it is stale (the caller's
/// map may be the right one; the node has only not been told) or serves the write under a
/// map it cannot compare. UNAVAILABLE, naming both generations, and nothing stored.
/// Mutant: `>` made `>=` or the arm returning STALE_MAP.
#[tokio::test]
async fn a_caller_ahead_of_the_node_is_unavailable_not_stale() {
    let (mem, _, channel) = node(7).await;
    let client = RemoteStorage::new(channel, 9);
    let answer = client.put(Bytes::from_static(b"payload")).await;
    let Err(StorageError::Unavailable(why)) = answer else {
        panic!("{answer:?}");
    };
    let said = why.to_string();
    assert!(
        said.contains("generation 7") && said.contains("routed by 9"),
        "{said}"
    );
    assert_eq!(mem.puts.load(Ordering::SeqCst), 0);
}

/// Catches a node that reads its map once (at construction) instead of per call, and a
/// client that keeps using a generation it was told is stale: after the node learns of
/// generation 9 the same client is stale, and once it adopts 9 its write lands.
#[tokio::test]
async fn the_node_judges_by_the_map_it_holds_now() {
    let (mem, held, channel) = node(7).await;
    let client = RemoteStorage::new(channel, 7);
    client
        .put(Bytes::from_static(b"one"))
        .await
        .expect("under 7");
    held.set(9);
    let answer = client.put(Bytes::from_static(b"two")).await;
    assert!(stale(9)(&answer), "{answer:?}");
    client.set_map_generation(9);
    client
        .put(Bytes::from_static(b"two"))
        .await
        .expect("under 9");
    assert_eq!(mem.puts.load(Ordering::SeqCst), 2);
}

/// Catches the held-blob shortcut answering a stale caller: a write the node would
/// acknowledge unread ("already stored") is refused all the same, because the node no
/// longer holds the placement the caller wrote under. Mutant: the check after the
/// `find_missing` shortcut.
#[tokio::test]
async fn a_stale_caller_is_refused_even_for_a_blob_the_node_holds() {
    let (_, held, channel) = node(7).await;
    let client = RemoteStorage::new(channel, 7);
    client
        .put(Bytes::from_static(b"held"))
        .await
        .expect("stored");
    held.set(8);
    let answer = client.put(Bytes::from_static(b"held")).await;
    assert!(stale(8)(&answer), "{answer:?}");
}

/// Catches a node with a map source that serves before it has learned its first map: it
/// is one of several, so it cannot tell a caller's placement is the current one. Every
/// generation is refused UNAVAILABLE and nothing is stored. (A node given no source at all
/// is a one-member farm and serves every generation: the plain `StorageService` tests.)
/// Mutant: the no-map case answering `Ok`.
#[tokio::test]
async fn a_node_with_a_source_and_no_map_serves_no_generation() {
    let mem = std::sync::Arc::new(Mem::default());
    let channel = serve_mapped(&mem, &Held::none()).await;
    for generation in [0, 3, 90] {
        let client = RemoteStorage::new(channel.clone(), generation);
        let answer = client.put(Bytes::from_static(b"any")).await;
        assert!(
            matches!(answer, Err(StorageError::Unavailable(_))),
            "generation {generation}: {answer:?}"
        );
    }
    assert_eq!(mem.puts.load(Ordering::SeqCst), 0);
}

async fn raw(channel: &Channel, header: Option<MetadataValue<tonic::metadata::Ascii>>) -> Status {
    let name = upload_name(&sha(b"abc"));
    let mut request = Request::new(stream::iter(write_of(&name, &[(b"abc", true)])));
    if let Some(value) = header {
        request.metadata_mut().insert(MAP_GENERATION_HEADER, value);
    }
    ByteStreamClient::new(channel.clone())
        .write(request)
        .await
        .expect_err("refused")
}

/// Catches the wire spelling of the answers: STALE_MAP is FAILED_PRECONDITION with the
/// node's generation in the trailer (the only way to tell it from the FAILED_PRECONDITION
/// of an action write whose closure is not held); a call with no generation entry is "no
/// map seen", so stale; an entry that is not a decimal number is INVALID_ARGUMENT, not
/// read as 0 or as current. Mutants: the trailer dropped; an absent entry read as
/// current; a malformed entry read as 0.
#[tokio::test]
async fn the_wire_names_the_generation_and_refuses_a_malformed_one() {
    let (mem, _, channel) = node(7).await;
    let behind = raw(&channel, Some(MetadataValue::from(5u64))).await;
    assert_eq!(behind.code(), Code::FailedPrecondition);
    assert_eq!(
        behind
            .metadata()
            .get(MAP_GENERATION_HEADER)
            .map(|v| v.to_str().expect("ASCII")),
        Some("7")
    );
    let none = raw(&channel, None).await;
    assert_eq!(none.code(), Code::FailedPrecondition, "{none:?}");
    let ahead = raw(&channel, Some(MetadataValue::from(8u64))).await;
    assert_eq!(ahead.code(), Code::Unavailable);
    let garbled = raw(&channel, Some("seven".parse().expect("ASCII"))).await;
    assert_eq!(garbled.code(), Code::InvalidArgument, "{garbled:?}");
    assert!(garbled.message().contains(MAP_GENERATION_HEADER));
    let opaque = raw(&channel, Some("-1".parse().expect("ASCII"))).await;
    assert_eq!(opaque.code(), Code::InvalidArgument, "{opaque:?}");
    assert_eq!(mem.puts.load(Ordering::SeqCst), 0);
}

/// Catches `lift` reading STALE_MAP from anything but FAILED_PRECONDITION with a decimal
/// trailer: the same code without one (an action write whose closure is missing), with
/// text that is not a number, or another code carrying the trailer, must keep the error
/// it had.
#[test]
fn only_failed_precondition_with_a_decimal_trailer_is_stale_map() {
    let with = |status: Status, value: &str| {
        let mut status = status;
        status
            .metadata_mut()
            .insert(MAP_GENERATION_HEADER, value.parse().expect("ASCII"));
        status
    };
    assert!(matches!(
        lift(&with(Status::failed_precondition("x"), "12"), None),
        StorageError::StaleMap { current: 12 }
    ));
    let mut unreadable = Status::failed_precondition("x");
    unreadable.metadata_mut().insert(
        MAP_GENERATION_HEADER,
        MetadataValue::try_from(&b"\xff"[..]).expect("a byte HTTP allows"),
    );
    for status in [
        Status::failed_precondition("x"),
        with(Status::failed_precondition("x"), "twelve"),
        with(Status::failed_precondition("x"), "-1"),
        unreadable,
    ] {
        assert!(
            matches!(lift(&status, None), StorageError::Internal(m) if m.contains("FailedPrecondition")),
            "{status:?}"
        );
    }
    assert!(matches!(
        lift(&with(Status::invalid_argument("x"), "12"), None),
        StorageError::Invalid(_)
    ));
}
