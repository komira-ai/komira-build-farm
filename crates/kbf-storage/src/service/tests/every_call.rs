//! The map generation on every call: STALE_MAP for a caller behind the node, whichever
//! call it makes, and a node that is behind a caller fetches the map instead of refusing
//! or serving blind.

use std::future::pending;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_map::Map;
use kbf_meta::{ActionRecord, Role};
use kbf_proto::google::bytestream::QueryWriteStatusRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{
    BeginRequest, CommitRequest, LocateRequest, PutRangeRequest, ReadRangeRequest,
};
use tonic::transport::Channel;
use tonic::{Request, Status};

use super::fake::{Held, Mem, connect, serve_routes, sha};
use super::upload_name;
use crate::replicate::MapSource;
use crate::service::client::lift;
use crate::service::{Admission, Config, MAP_GENERATION_HEADER, RemoteStorage, StorageService};
use crate::{Storage, StorageError};

/// What a node's source does when asked for a map it has not been told of.
#[derive(Clone, Copy)]
enum Fetch {
    /// It cannot get one.
    Nothing,
    /// The map of this generation arrives.
    Arrives(u64),
}

/// A source that holds a map and answers a fetch as the test says, remembering what it
/// was asked for.
struct Frontend {
    held: Arc<Held>,
    fetch: Fetch,
    asked: Mutex<Vec<u64>>,
}

impl MapSource for Frontend {
    fn current(&self) -> Option<Arc<Map>> {
        self.held.current()
    }

    fn me(&self) -> Option<kbf_map::MemberId> {
        self.held.me()
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Option<Arc<Map>>> {
        self.asked.lock().expect("lock").push(at_least);
        if let Fetch::Arrives(generation) = self.fetch {
            self.held.set(generation);
        }
        Box::pin(async move { self.held.current() })
    }
}

/// A node at map generation `generation` whose store fails every call it is given
/// ("reached the store"), so a call the generation check lets through is told from one it
/// stops.
async fn node(generation: u64, fetch: Fetch) -> (Arc<Mem>, Arc<Frontend>, Channel) {
    node_holding(Held::at(generation), fetch).await
}

/// [`node`] over a source that holds `held` (which may be no map yet).
async fn node_holding(held: Arc<Held>, fetch: Fetch) -> (Arc<Mem>, Arc<Frontend>, Channel) {
    let mem = Arc::new(Mem::default());
    *mem.fail.lock().expect("lock") = Some(StorageError::Internal("reached the store".into()));
    let source = Arc::new(Frontend {
        held,
        fetch,
        asked: Mutex::new(Vec::new()),
    });
    let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
    let service = StorageService::new(
        storage,
        Admission::Open,
        Config {
            write_bytes_in_flight: 1 << 30,
        },
    )
    .with_map(Arc::clone(&source) as Arc<dyn MapSource>);
    (
        mem,
        source,
        connect(serve_routes(service.routes(), None).await).await,
    )
}

/// How a call went: refused as stale (naming the node's generation), UNAVAILABLE, or let
/// through to the store (the fake's failure) or the segments (they answer).
#[derive(Debug, PartialEq, Eq)]
enum Went {
    Stale(u64),
    Unavailable,
    Reached,
}

fn went_status<T>(answer: Result<T, Status>) -> Went {
    answer.map_or_else(|status| went_error(lift(&status, None)), |_| Went::Reached)
}

fn went<T>(answer: Result<T, StorageError>) -> Went {
    answer.map_or_else(went_error, |_| Went::Reached)
}

fn went_error(error: StorageError) -> Went {
    match error {
        StorageError::StaleMap { current } => Went::Stale(current),
        StorageError::Unavailable(_) => Went::Unavailable,
        // The store this node's fake fails every call with.
        StorageError::Internal(ref why) if why.contains("reached the store") => Went::Reached,
        other => panic!("another answer: {other:?}"),
    }
}

/// Every call a node serves, made through `channel` by a caller at generation
/// `generation`, with how each went.
async fn every_call(channel: &Channel, generation: u64) -> Vec<(&'static str, Went)> {
    let remote = RemoteStorage::new(channel.clone(), generation);
    let digest = sha(b"blob");
    let segments = remote.segments();
    let mut query = Request::new(QueryWriteStatusRequest {
        resource_name: upload_name(&digest),
    });
    query
        .metadata_mut()
        .insert(MAP_GENERATION_HEADER, generation.into());
    let mut bytestream = ByteStreamClient::new(channel.clone());
    vec![
        (
            "FindMissingBlobs",
            went(remote.find_missing(&[digest]).await),
        ),
        (
            "ByteStream.Read",
            went(remote.read_range(&digest, 0, 0).await.map(|_| ())),
        ),
        (
            "ByteStream.Write",
            went(remote.put(Bytes::from_static(b"blob")).await),
        ),
        (
            "ByteStream.QueryWriteStatus",
            went_status(bytestream.query_write_status(query).await),
        ),
        ("ActionStore.Get", went(remote.action_result(&digest).await)),
        (
            "ActionStore.Prepare",
            went(remote.prepare_action_result(&ActionResult::default()).await),
        ),
        (
            "ActionStore.Commit",
            went(
                remote
                    .commit_action_record(
                        Role::Daemon,
                        digest,
                        ActionRecord {
                            result: digest,
                            closure: Default::default(),
                        },
                    )
                    .await,
            ),
        ),
        (
            "Segments.Begin",
            went_status(segments.begin(Request::new(BeginRequest::default())).await),
        ),
        (
            "Segments.PutRange",
            went_status(
                segments
                    .put_range(Request::new(PutRangeRequest::default()))
                    .await,
            ),
        ),
        (
            "Segments.Commit",
            went_status(
                segments
                    .commit(Request::new(CommitRequest::default()))
                    .await,
            ),
        ),
        (
            "Segments.Locate",
            went_status(
                segments
                    .locate(Request::new(LocateRequest::default()))
                    .await,
            ),
        ),
        (
            "Segments.ReadRange",
            went_status(
                segments
                    .read_range(Request::new(ReadRangeRequest::default()))
                    .await,
            ),
        ),
    ]
}

/// Catches a call that a node serves whatever map its caller routed by: each of the
/// twelve calls (every method of the four services) from a caller behind the node, or
/// with no map seen (0), is STALE_MAP naming the node's generation, and none reaches the
/// store or the segments; the same calls under the node's generation all do. Mutants:
/// the check removed from any one call (the row for that call reads `Reached`).
#[tokio::test]
async fn every_call_of_a_caller_behind_the_node_is_stale_map() {
    let (mem, _, channel) = node(7, Fetch::Nothing).await;
    for behind in [5, 0] {
        for (call, went) in every_call(&channel, behind).await {
            assert_eq!(went, Went::Stale(7), "{call} at generation {behind}");
        }
    }
    assert!(
        mem.segments.calls.lock().expect("lock").is_empty(),
        "a segment call was forwarded under a stale map"
    );
    for (call, went) in every_call(&channel, 7).await {
        assert_eq!(went, Went::Reached, "{call} at the node's generation");
    }
    assert_eq!(
        mem.segments.calls.lock().expect("lock").len(),
        5,
        "the current caller's segment calls"
    );
}

/// Catches a node that refuses a caller that is ahead of it without trying to learn the
/// map (a lost push would then fail every call to it until the next push), and one that
/// serves it blind: a node at 7 asked under 9 fetches generation 9 once, and serves the
/// call; a call that finds the node already there fetches nothing.
#[tokio::test]
async fn a_caller_ahead_of_the_node_makes_it_fetch_the_map() {
    let (mem, source, channel) = node(7, Fetch::Arrives(9)).await;
    let ahead = RemoteStorage::new(channel.clone(), 9);
    assert_eq!(went(ahead.find_missing(&[sha(b"x")]).await), Went::Reached);
    assert_eq!(*source.asked.lock().expect("lock"), [9]);
    assert_eq!(went(ahead.find_missing(&[sha(b"x")]).await), Went::Reached);
    assert_eq!(*source.asked.lock().expect("lock"), [9], "fetched again");
    // The fetch moved the node to 9: a caller still at 7 is now behind it.
    let behind = RemoteStorage::new(channel, 7);
    assert_eq!(
        went(behind.find_missing(&[sha(b"x")]).await),
        Went::Stale(9)
    );
    assert_eq!(mem.puts.load(Ordering::SeqCst), 0);
}

/// Catches a node that serves a caller under a map that is not the caller's: the fetch
/// brings generation 10 for a caller at 8, so the caller is now the stale one.
#[tokio::test]
async fn a_fetch_that_passes_the_caller_makes_it_stale() {
    let (_, source, channel) = node(7, Fetch::Arrives(10)).await;
    let caller = RemoteStorage::new(channel, 8);
    assert_eq!(
        went(caller.find_missing(&[sha(b"x")]).await),
        Went::Stale(10)
    );
    assert_eq!(*source.asked.lock().expect("lock"), [8]);
}

/// Catches a node that serves, or calls stale, a caller ahead of it when it could not get
/// the map: UNAVAILABLE (the caller's map may be the right one), naming both generations,
/// and nothing reaches the store.
#[tokio::test]
async fn a_fetch_that_brings_nothing_is_unavailable() {
    let (_, source, channel) = node(7, Fetch::Nothing).await;
    let caller = RemoteStorage::new(channel, 9);
    let Err(StorageError::Unavailable(why)) = caller.find_missing(&[sha(b"x")]).await else {
        panic!("not UNAVAILABLE");
    };
    let said = why.to_string();
    assert!(
        said.contains("generation 7") && said.contains("by 9"),
        "{said}"
    );
    assert_eq!(*source.asked.lock().expect("lock"), [9]);
}

/// Catches a node with a map source and no map that serves whatever generation it is
/// asked under (a multi-member node must not serve until it has learned its first map):
/// each of the twelve calls, from a caller at any generation (0 included), is UNAVAILABLE
/// and none reaches the store or the segments; the node tried to fetch a map for each.
/// Mutant: `check` returning `Ok(())` when the source has no map.
#[tokio::test]
async fn a_node_with_a_source_and_no_map_refuses_every_call() {
    let (mem, source, channel) = node_holding(Held::none(), Fetch::Nothing).await;
    for generation in [0, 7] {
        for (call, went) in every_call(&channel, generation).await {
            assert_eq!(went, Went::Unavailable, "{call} at generation {generation}");
        }
    }
    assert!(
        mem.segments.calls.lock().expect("lock").is_empty(),
        "a segment call was forwarded by a node with no map"
    );
    assert_eq!(
        source.asked.lock().expect("lock").len(),
        24,
        "one fetch per call"
    );
}

/// Catches a node that, having no map, never goes to get one (every call would fail until
/// the next push): the fetch brings generation 7, the caller at 7 is served and the one
/// at 5 is stale, and the node asked once, not again.
#[tokio::test]
async fn a_node_with_no_map_fetches_its_first_one() {
    let (_, source, channel) = node_holding(Held::none(), Fetch::Arrives(7)).await;
    let caller = RemoteStorage::new(channel.clone(), 7);
    assert_eq!(went(caller.find_missing(&[sha(b"x")]).await), Went::Reached);
    let behind = RemoteStorage::new(channel, 5);
    assert_eq!(
        went(behind.find_missing(&[sha(b"x")]).await),
        Went::Stale(7)
    );
    assert_eq!(*source.asked.lock().expect("lock"), [7]);
}

/// A source whose fetch never comes.
struct Hung(Arc<Held>);

impl MapSource for Hung {
    fn current(&self) -> Option<Arc<Map>> {
        self.0.current()
    }

    fn me(&self) -> Option<kbf_map::MemberId> {
        self.0.me()
    }

    fn refetch(&self, _: u64) -> BoxFuture<'_, Option<Arc<Map>>> {
        Box::pin(pending())
    }
}

/// Catches a node that ignores the wait it was given (`with_refetch_wait`): a fetch that
/// never comes, with an hour allowed, ends the call after that hour on a paused clock,
/// and not at the default bound.
#[tokio::test(start_paused = true)]
async fn the_wait_for_a_fetch_is_the_one_the_service_was_given() {
    let mem = Arc::new(Mem::default());
    let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
    let wait = std::time::Duration::from_secs(3600);
    let service = StorageService::new(
        storage,
        Admission::Open,
        Config {
            write_bytes_in_flight: 1 << 30,
        },
    )
    .with_map(Arc::new(Hung(Held::at(7))))
    .with_refetch_wait(wait);
    let channel = connect(serve_routes(service.routes(), None).await).await;
    let started = tokio::time::Instant::now();
    let answer = RemoteStorage::new(channel, 9)
        .find_missing(&[sha(b"x")])
        .await;
    assert_eq!(went(answer), Went::Unavailable);
    assert_eq!(started.elapsed(), wait);
}

/// Catches a node whose call waits for a fetch for ever (a frontend that never answers
/// would hold every caller ahead of it): on a paused clock the wait ends when the bound
/// passes, UNAVAILABLE, and not before.
#[tokio::test(start_paused = true)]
async fn a_fetch_that_never_comes_ends_at_the_bound() {
    let source = Hung(Held::at(7));
    let wait = std::time::Duration::from_secs(2);
    let started = tokio::time::Instant::now();
    // Twice the bound: a wait that is not bounded by `wait` ends here instead.
    let answer = tokio::time::timeout(wait * 2, crate::replicate::check(&source, 9, wait))
        .await
        .expect("the fetch is waited for no longer than the bound");
    assert!(
        matches!(answer, Err(StorageError::Unavailable(_))),
        "{answer:?}"
    );
    assert_eq!(started.elapsed(), wait);
}
