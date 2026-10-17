//! A node that is catching up ([`CatchUp`]): a miss is UNAVAILABLE, never NOT_FOUND or a
//! refusal naming the blob; hits and writes are served; [`CatchUp::finish`] lifts it.
//!
//! The store is [`Mem`]; a blob "held in cold storage but not yet indexed" is one the
//! store does not find, which is what a half-built index answers.

use std::sync::Arc;

use bytes::Bytes;
use kbf_meta::{ActionRecord, ActionWriteError, Role};
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::ActionResult;
use kbf_types::Digest;
use tonic::Code;
use tonic::transport::Channel;

use super::collect;
use super::fake::{Mem, connect, serve_routes, sha};
use crate::service::{Admission, CatchUp, Config, RemoteStorage, StorageService};
use crate::{Storage, StorageError};

/// A plain-text service over `mem` that is catching up as `catch_up` says, and a client.
async fn node(mem: &Arc<Mem>, catch_up: &CatchUp) -> (RemoteStorage, Channel) {
    let storage: Arc<dyn Storage> = Arc::clone(mem) as Arc<dyn Storage>;
    let config = Config {
        write_bytes_in_flight: 1 << 30,
    };
    let service =
        StorageService::new(storage, Admission::Open, config).with_catch_up(catch_up.clone());
    let channel = connect(serve_routes(service.routes(), None).await).await;
    (RemoteStorage::new(channel.clone(), 0), channel)
}

fn unavailable<T: std::fmt::Debug>(answer: &Result<T, StorageError>) -> bool {
    matches!(answer, Err(StorageError::Unavailable(_)))
}

/// A store holding the blob `held`, and the digest of one it does not find.
fn store() -> (Arc<Mem>, Digest, Digest) {
    let mem = Arc::new(Mem::default());
    let held = sha(b"held");
    mem.blobs
        .lock()
        .expect("lock")
        .insert(held, Bytes::from_static(b"held"));
    (mem, held, sha(b"in cold storage, not yet indexed"))
}

/// Catches a node that, while it rebuilds its index, tells a caller a blob it may hold is
/// missing: a Read of it must be UNAVAILABLE (the gRPC code, and `Unavailable` through the
/// client a router uses), never NOT_FOUND; a FindMissingBlobs naming it beside a held blob
/// must fail as a whole, UNAVAILABLE; an action-cache miss is UNAVAILABLE too. Mutants:
/// the gate removed (NOT_FOUND, a missing list, a `None` entry come back); a gate that
/// drops only the missed digests from the FindMissing answer (an empty list comes back,
/// which tells the caller the blob is held); the gate read after the lookup is not caught
/// here (no rebuild finishes mid-call) but by the module docs' argument.
#[tokio::test]
async fn no_false_miss_during_catch_up() {
    let (mem, held, unindexed) = store();
    let (client, channel) = node(&mem, &CatchUp::rebuilding()).await;

    let read = client.read_blob(&unindexed).await;
    assert!(unavailable(&read), "a read of an unindexed blob: {read:?}");
    let ranged = client.read_range(&unindexed, 0, 0).await.map(drop);
    assert!(unavailable(&ranged), "a ranged read: {ranged:?}");
    let status = ByteStreamClient::new(channel)
        .read(ReadRequest {
            resource_name: format!("blobs/{}/{}", unindexed.hash_hex(), unindexed.size_bytes),
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .map(drop)
        .expect_err("a read of an unindexed blob is refused");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert!(status.message().contains("catching up"), "{status:?}");

    let found = client.find_missing(&[held, unindexed]).await;
    assert!(unavailable(&found), "FindMissing with a miss: {found:?}");

    let action = client.action_result(&sha(b"an action")).await;
    assert!(unavailable(&action), "an action-cache miss: {action:?}");
}

/// Catches an action-cache write, while the node rebuilds its index, refused for a
/// closure blob the node did not find as if the blob were missing: Prepare and Commit
/// must answer UNAVAILABLE (a caller retries) and never the Absent refusal (which a caller
/// acts on by uploading, or gives up with FAILED_PRECONDITION). Refusals that are not
/// about a blob's absence stand: Unreachable and NotDaemon are still refusals. Mutant:
/// the refusal passed through unchanged (FAILED_PRECONDITION / Absent leaks).
#[tokio::test]
async fn a_closure_write_during_catch_up_is_retryable() {
    let (mem, _, _) = store();
    let (client, _) = node(&mem, &CatchUp::rebuilding()).await;

    let absent = ActionResult {
        exit_code: 99,
        ..ActionResult::default()
    };
    let prepared = client.prepare_action_result(&absent).await;
    assert!(unavailable(&prepared), "Prepare: {prepared:?}");

    let missing = sha(b"a closure blob");
    *mem.fail.lock().expect("lock") = Some(ActionWriteError::Absent(vec![missing]).into());
    let record = ActionRecord {
        result: sha(b"result"),
        closure: [missing].into_iter().collect(),
    };
    let committed = client
        .commit_action_record(Role::Daemon, sha(b"an action"), record.clone())
        .await;
    assert!(unavailable(&committed), "Commit: {committed:?}");

    *mem.fail.lock().expect("lock") = None;
    let unreadable = ActionResult {
        exit_code: 98,
        ..ActionResult::default()
    };
    let refused = client.prepare_action_result(&unreadable).await;
    assert!(
        matches!(
            refused,
            Err(StorageError::ActionWrite(ActionWriteError::Unreachable(_)))
        ),
        "an unreadable blob is still a refusal: {refused:?}"
    );
    let by_client = client
        .commit_action_record(Role::Client, sha(b"an action"), record)
        .await;
    assert!(
        matches!(
            by_client,
            Err(StorageError::ActionWrite(ActionWriteError::NotDaemon))
        ),
        "a client's write is still refused: {by_client:?}"
    );
}

/// Catches a gate that refuses more than misses: while the node rebuilds, a held blob
/// reads back whole, a FindMissing of held blobs answers none missing, a new blob is
/// written and reads back, and an action-cache entry is written (Prepare, Commit) and
/// read. Mutant: every call refused while catching up.
#[tokio::test]
async fn hits_and_writes_are_served_while_catching_up() {
    let (mem, held, _) = store();
    let (client, _) = node(&mem, &CatchUp::rebuilding()).await;

    let (_, bytes) = collect(client.read_range(&held, 0, 0).await.expect("read")).await;
    assert_eq!(bytes, b"held");
    assert_eq!(client.find_missing(&[held]).await.expect("find"), []);

    let written = client
        .put(Bytes::from_static(b"written while catching up"))
        .await
        .expect("a write is served");
    assert_eq!(written, sha(b"written while catching up"));
    let back = client.read_blob(&written).await.expect("read back");
    assert_eq!(&back[..], b"written while catching up");

    let result = ActionResult {
        exit_code: 2,
        stdout_raw: b"out".to_vec(),
        ..ActionResult::default()
    };
    let record = client
        .prepare_action_result(&result)
        .await
        .expect("prepared");
    let action = sha(b"an action");
    client
        .commit_action_record(Role::Daemon, action, record.clone())
        .await
        .expect("committed");
    let (digest, _) = client
        .action_result(&action)
        .await
        .expect("read")
        .expect("a hit");
    assert_eq!(digest, record.result);
}

/// Catches a gate that outlives the rebuild: after [`CatchUp::finish`] a true miss is the
/// store's answer again: NOT_FOUND for a read, the digest in the FindMissing answer, no
/// entry for an action-cache miss, and the Absent refusal for a closure blob. The same
/// calls are refused UNAVAILABLE first, so the test sees the gate lift. Mutant: misses
/// answered UNAVAILABLE whatever the state.
#[tokio::test]
async fn catch_up_done_lifts_the_gate() {
    let (mem, held, unindexed) = store();
    let catch_up = CatchUp::rebuilding();
    let (client, _) = node(&mem, &catch_up).await;
    assert!(unavailable(&client.read_blob(&unindexed).await));

    catch_up.finish();
    assert!(!catch_up.is_catching_up());
    let read = client.read_blob(&unindexed).await;
    assert!(
        matches!(read, Err(StorageError::NotFound(d)) if d == unindexed),
        "{read:?}"
    );
    assert_eq!(
        client.find_missing(&[held, unindexed]).await.expect("find"),
        [unindexed]
    );
    assert_eq!(
        client.action_result(&sha(b"an action")).await.expect("get"),
        None
    );
    let absent = ActionResult {
        exit_code: 99,
        ..ActionResult::default()
    };
    let refused = client.prepare_action_result(&absent).await;
    assert!(
        matches!(
            refused,
            Err(StorageError::ActionWrite(ActionWriteError::Absent(_)))
        ),
        "{refused:?}"
    );
}

/// Catches a service given no catch-up state, or one caught up from the start, that
/// gates anyway: a miss is NOT_FOUND at once.
#[tokio::test]
async fn a_node_caught_up_from_the_start_answers_misses() {
    let (mem, _, unindexed) = store();
    let (client, _) = node(&mem, &CatchUp::caught_up()).await;
    assert!(matches!(
        client.read_blob(&unindexed).await,
        Err(StorageError::NotFound(_))
    ));
}

/// Catches a `finished` that never returns once the rebuild is done (it waits for a
/// notification sent before it registered: mutant, the flag read before the waiter is
/// registered), or that returns while the rebuild is still going: a waiter that began
/// before `finish` and one that began after both complete; a caught-up state completes
/// at once. The hang guard is generous and asserts nothing about speed.
#[tokio::test]
async fn finished_completes_once_and_only_once_the_rebuild_is_done() {
    const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(60);
    let rebuilding = CatchUp::rebuilding();
    let before = {
        let rebuilding = rebuilding.clone();
        tokio::spawn(async move { rebuilding.finished().await })
    };
    // Let the waiter run to its wait.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(!before.is_finished(), "still rebuilding");
    rebuilding.finish();
    tokio::time::timeout(HANG_GUARD, before)
        .await
        .expect("the waiter wakes")
        .expect("joins");
    tokio::time::timeout(HANG_GUARD, rebuilding.finished())
        .await
        .expect("a waiter after finish completes");
    tokio::time::timeout(HANG_GUARD, CatchUp::caught_up().finished())
        .await
        .expect("a caught-up state completes");
}
