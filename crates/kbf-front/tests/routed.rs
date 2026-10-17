//! The frontend's storage calls over three storage nodes, routed by a map it holds
//! (`kbf_front::Routed`): the proxy's CAS and action-cache calls and the backend Execute
//! and the scheduler use, each over real storage services on loopback ports, so that where
//! a blob lands and where it is looked for can be read off the nodes themselves.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_auth::Authorizers;
use kbf_front::{
    Backend, Cache, CacheStorage, Channels, Owner, Owners, Proxy, Routed, SegmentsConfig,
};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_meta::ActionWriteError;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    self, ActionResult, BatchReadBlobsRequest, BatchUpdateBlobsRequest, FindMissingBlobsRequest,
    GetActionResultRequest, OutputFile, batch_update_blobs_request,
};
use kbf_route::{MapSource, MapView, Nodes, RefetchError};
use kbf_storage::service::{Admission, Config, RemoteStorage, StorageService};
use kbf_storage::{Storage, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use tonic::Code;
use tonic::service::Routes;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};

/// The map the frontend holds, set by the test; a fetch answers what the test queued.
#[derive(Default)]
struct Cell {
    view: Mutex<Option<MapView>>,
    fetches: Mutex<VecDeque<MapView>>,
    asked: Mutex<Vec<u64>>,
}

impl Cell {
    fn set(&self, map: Arc<Map>) {
        self.set_after(map, None);
    }

    /// `map`, which a change has just made, and the map it replaced.
    fn set_after(&self, map: Arc<Map>, previous: Option<Arc<Map>>) {
        *self.view.lock().unwrap_or_else(PoisonError::into_inner) = Some(MapView { map, previous });
    }
}

impl MapSource for Cell {
    fn current(&self) -> Option<MapView> {
        self.view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            self.asked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(at_least);
            let next = self
                .fetches
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            let view = next.ok_or_else(|| RefetchError {
                at_least,
                reason: "no newer map".to_owned(),
            })?;
            *self.view.lock().unwrap_or_else(PoisonError::into_inner) = Some(view.clone());
            Ok(view)
        })
    }
}

/// Plain-text connections to the nodes, by member id; a node can be made unreachable.
struct Plain {
    channels: BTreeMap<MemberId, Channel>,
    /// Where nobody listens.
    nowhere: Channel,
    down: Mutex<BTreeSet<MemberId>>,
}

impl Plain {
    fn to(&self, member: &Member) -> Result<Channel, StorageError> {
        if self
            .down
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&member.id)
        {
            return Ok(self.nowhere.clone());
        }
        self.channels
            .get(&member.id)
            .cloned()
            .ok_or_else(|| StorageError::Internal(format!("no node {}", member.id)))
    }

    fn take_down(&self, id: u64) {
        self.down
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(MemberId(id));
    }
}

impl Nodes for Plain {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        Ok(Arc::new(RemoteStorage::new(self.to(member)?, generation)))
    }
}

impl Channels for Plain {
    fn channel(&self, member: &Member) -> Result<Channel, StorageError> {
        self.to(member)
    }
}

/// What a node holds of the map the test gives it, when it judges calls by one.
struct NodeMap(Arc<Map>);

impl kbf_storage::replicate::MapSource for NodeMap {
    fn current(&self) -> Option<Arc<Map>> {
        Some(Arc::clone(&self.0))
    }

    fn me(&self) -> Option<MemberId> {
        None
    }
}

fn serve(routes: Routes) -> SocketAddr {
    let incoming = TcpIncoming::bind(([127, 0, 0, 1], 0).into()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming),
    );
    addr
}

fn lazy(addr: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy()
}

/// Three storage nodes (members 1 to 3, over empty in-memory caches), the map cell, and a
/// frontend routed by it: `routed`, whose bootstrap node is member 1.
struct Farm {
    nodes: BTreeMap<u64, Arc<dyn Storage>>,
    cell: Arc<Cell>,
    plain: Arc<Plain>,
    routed: Arc<Routed>,
    bootstrap: Channel,
}

/// A storage node; one that judges calls by `map` if it is given one.
fn node(map: Option<Arc<Map>>) -> (Arc<dyn Storage>, SocketAddr) {
    let store: Arc<dyn Storage> = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let config = Config {
        write_bytes_in_flight: 64 << 20,
    };
    let service = StorageService::new(Arc::clone(&store), Admission::Open, config);
    let service = match map {
        Some(map) => service.with_map(Arc::new(NodeMap(map))),
        None => service,
    };
    (store, serve(service.routes()))
}

impl Farm {
    fn new(node_map: Option<Arc<Map>>) -> Self {
        let mut nodes = BTreeMap::new();
        let mut channels = BTreeMap::new();
        for id in 1..=3 {
            let (store, addr) = node(node_map.clone());
            nodes.insert(id, store);
            channels.insert(MemberId(id), lazy(addr));
        }
        let bootstrap = channels[&MemberId(1)].clone();
        let plain = Arc::new(Plain {
            channels,
            nowhere: lazy(([127, 0, 0, 1], 1).into()),
            down: Mutex::default(),
        });
        let cell = Arc::new(Cell::default());
        let routed = Arc::new(Routed::new(cell.clone(), plain.clone(), plain.clone()));
        Self {
            nodes,
            cell,
            plain,
            routed,
            bootstrap,
        }
    }

    /// The members `states` (ids 1 up), replicated `r` times.
    fn map(generation: u64, states: &[MemberState], r: u8) -> Arc<Map> {
        let members = states
            .iter()
            .zip(1..)
            .map(|(state, id)| Member {
                id: MemberId(id),
                addr: format!("127.0.0.1:{id}"),
                zone: String::new(),
                weight: 1,
                state: *state,
            })
            .collect();
        Arc::new(Map::new(generation, members, Rule::Rendezvous, r, 1, Vec::new()).expect("a map"))
    }

    fn set(&self, generation: u64, states: &[MemberState], r: u8) -> Arc<Map> {
        let map = Self::map(generation, states, r);
        self.cell.set(Arc::clone(&map));
        map
    }

    /// Which nodes hold `digest`.
    async fn holders(&self, digest: &Digest) -> BTreeSet<u64> {
        let mut held = BTreeSet::new();
        for (id, node) in &self.nodes {
            if node
                .find_missing(&[*digest])
                .await
                .expect("a node answers")
                .is_empty()
            {
                held.insert(*id);
            }
        }
        held
    }

    fn owners(&self) -> Owners {
        Owners::routed(
            Arc::clone(&self.routed),
            Owner::new("bootstrap", self.bootstrap.clone()),
        )
    }

    /// The frontend's CAS and action-cache services, as a client sees them.
    fn front(&self) -> Channel {
        let proxy = Proxy::new(self.owners(), Arc::new(Authorizers::allow_all()));
        lazy(serve(proxy.routes()))
    }

    fn backend(&self) -> Backend {
        let bootstrap = Arc::new(RemoteStorage::new(self.bootstrap.clone(), 0));
        Backend::routed(Arc::clone(&self.routed), bootstrap)
    }

    /// The backend as a frontend without members has it: the one node `--storage` names.
    fn single_node_backend(&self) -> Backend {
        Backend::remote(Arc::new(RemoteStorage::new(self.bootstrap.clone(), 0)))
    }
}

const ALL_ACTIVE: [MemberState; 3] = [MemberState::Active; 3];

fn blob(i: usize) -> (Vec<u8>, Digest) {
    let bytes = format!("routed blob {i}").into_bytes();
    let digest = kbf_segments::sha256(&bytes);
    (bytes, digest)
}

fn proto(digest: &Digest) -> reapi::Digest {
    reapi::Digest {
        hash: digest.hash_hex(),
        size_bytes: i64::try_from(digest.size_bytes).expect("a size"),
    }
}

/// The status code of each item of a BatchUpdateBlobs of `blobs`.
async fn update(front: &Channel, blobs: &[(Vec<u8>, Digest)]) -> Vec<i32> {
    let response = ContentAddressableStorageClient::new(front.clone())
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: blobs
                .iter()
                .map(|(bytes, digest)| batch_update_blobs_request::Request {
                    digest: Some(proto(digest)),
                    data: bytes.clone(),
                    compressor: reapi::compressor::Value::Identity as i32,
                })
                .collect(),
            ..BatchUpdateBlobsRequest::default()
        })
        .await
        .expect("the batch is answered")
        .into_inner();
    response
        .responses
        .iter()
        .map(|r| r.status.as_ref().expect("a status").code)
        .collect()
}

/// The status code and bytes of each item of a BatchReadBlobs of `digests`.
async fn read(front: &Channel, digests: &[Digest]) -> Vec<(i32, Vec<u8>)> {
    let response = ContentAddressableStorageClient::new(front.clone())
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: digests.iter().map(proto).collect(),
            ..BatchReadBlobsRequest::default()
        })
        .await
        .expect("the batch is answered")
        .into_inner();
    response
        .responses
        .into_iter()
        .map(|r| (r.status.expect("a status").code, r.data))
        .collect()
}

const OK: i32 = Code::Ok as i32;

/// Catches a CAS write that does not follow the map (everything on the `--storage` node,
/// where the daemons do not look): each blob of a batch lands on the first owner of it the
/// map says is not down, and on no other node. Blobs are searched for that spread over all
/// three nodes, so a frontend that sent them to one node cannot pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_update_lands_on_each_blobs_writer() {
    let farm = Farm::new(None);
    let map = farm.set(1, &ALL_ACTIVE, 1);
    let blobs: Vec<_> = (0..24).map(blob).collect();
    let front = farm.front();

    assert_eq!(update(&front, &blobs).await, vec![OK; blobs.len()]);

    let mut writers = BTreeSet::new();
    for (_, digest) in &blobs {
        let writer = map.write_owners(digest)[0].0;
        writers.insert(writer);
        assert_eq!(farm.holders(digest).await, BTreeSet::from([writer]));
    }
    assert_eq!(writers.len(), 3, "the blobs are not spread over the nodes");
}

/// Catches a read that asks only the `--storage` node, and a failover that stops at a node
/// that does not hold the blob: a blob held only by the owner a read asks second is read,
/// a blob held nowhere is NOT_FOUND, and a read answered by the owners is the blob's own
/// bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_read_goes_to_the_owners_and_on_to_the_next_that_has_the_blob() {
    let farm = Farm::new(None);
    let map = farm.set(1, &ALL_ACTIVE, 2);
    let blobs: Vec<_> = (0..12).map(blob).collect();
    for (bytes, digest) in &blobs[..11] {
        // The owner a read asks second is the only one that holds it.
        let second = map.read_order(digest)[1].0;
        farm.nodes[&second]
            .put(Bytes::from(bytes.clone()))
            .await
            .expect("stored");
    }
    let digests: Vec<_> = blobs.iter().map(|(_, d)| *d).collect();
    let answers = read(&farm.front(), &digests).await;

    for ((bytes, _), (code, data)) in blobs.iter().zip(&answers).take(11) {
        assert_eq!((*code, data), (OK, bytes));
    }
    assert_eq!(answers[11].0, Code::NotFound as i32, "{:?}", answers[11]);
}

/// Catches a FindMissing that asks one node about every digest: the blobs stored at their
/// owners are reported held, the rest missing, in the order asked, whichever nodes they
/// are on.
#[tokio::test(flavor = "multi_thread")]
async fn find_missing_asks_each_digests_primary() {
    let farm = Farm::new(None);
    let map = farm.set(1, &ALL_ACTIVE, 1);
    let blobs: Vec<_> = (0..24).map(blob).collect();
    let (held, absent) = blobs.split_at(12);
    let mut where_held = BTreeSet::new();
    for (bytes, digest) in held {
        let primary = map.write_owners(digest)[0].0;
        where_held.insert(primary);
        farm.nodes[&primary]
            .put(Bytes::from(bytes.clone()))
            .await
            .expect("stored");
    }
    assert!(where_held.len() > 1, "the held blobs are on one node");

    let asked: Vec<_> = blobs.iter().map(|(_, d)| proto(d)).collect();
    let missing = ContentAddressableStorageClient::new(farm.front())
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: asked,
            ..FindMissingBlobsRequest::default()
        })
        .await
        .expect("answered")
        .into_inner()
        .missing_blob_digests;
    let want: Vec<_> = absent.iter().map(|(_, d)| proto(d)).collect();
    assert_eq!(missing, want);
}

/// An action whose entry is owned by a different node than the output it names, and the
/// result that names the output.
struct Case {
    action: Digest,
    output: (Vec<u8>, Digest),
    result: ActionResult,
}

fn case(map: &Map) -> Case {
    let output = (0..)
        .map(blob)
        .find(|(_, d)| map.write_owners(d)[0] == MemberId(2))
        .expect("an output owned by node 2");
    let action = (0..1000)
        .map(|i| kbf_segments::sha256(format!("action {i}").as_bytes()))
        .find(|d| map.write_owners(d)[0] == MemberId(3))
        .expect("an action owned by node 3");
    let result = ActionResult {
        output_files: vec![OutputFile {
            path: "out/file.txt".to_owned(),
            digest: Some(proto(&output.1)),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    };
    Case {
        action,
        output,
        result,
    }
}

/// Catches the failure that kept three storage nodes from running an action: the backend
/// asked the `--storage` node whether an output is held, and was told it is not, because
/// the daemon had put it on its owner. With the output on another node than the
/// `--storage` node, prepare finds it, the result blob is stored where the map puts it,
/// and the same call over the one node is refused as the output being absent.
#[tokio::test(flavor = "multi_thread")]
async fn prepare_finds_an_output_at_its_owner() {
    let farm = Farm::new(None);
    let map = farm.set(1, &ALL_ACTIVE, 1);
    let case = case(&map);
    farm.nodes[&2]
        .put(Bytes::from(case.output.0.clone()))
        .await
        .expect("stored");

    let record = farm
        .backend()
        .prepare_action_result(&case.result)
        .await
        .expect("the output is held, at its owner");
    let result_blob = kbf_segments::sha256(&case.result.encode_to_vec());
    assert_eq!(record.result, result_blob);
    assert_eq!(
        record.closure.iter().copied().collect::<Vec<_>>(),
        [case.output.1]
    );
    assert_eq!(
        farm.holders(&result_blob).await,
        BTreeSet::from([map.write_owners(&result_blob)[0].0])
    );

    let alone = farm
        .single_node_backend()
        .prepare_action_result(&case.result)
        .await
        .expect_err("not held at the --storage node");
    assert!(
        matches!(alone, StorageError::ActionWrite(ActionWriteError::Absent(ref d)) if *d == [case.output.1]),
        "{alone:?}"
    );
}

/// Catches a prepare that refuses a result during a map change: the daemon put the output
/// on its owner under generation 1, and generation 2 (node 3 gone) puts it elsewhere, where
/// it has not been copied yet. The output is held, at the previous generation's owner, so
/// prepare accepts the result, and the commit copies the output to the action's owner from
/// there. An output held by no owner of either generation is still refused.
#[tokio::test(flavor = "multi_thread")]
async fn prepare_finds_an_output_at_the_previous_generations_owner() {
    let farm = Farm::new(None);
    let old = Farm::map(1, &ALL_ACTIVE, 1);
    let new = Farm::map(2, &ALL_ACTIVE[..2], 1);
    farm.cell
        .set_after(Arc::clone(&new), Some(Arc::clone(&old)));
    let output = (0..)
        .map(blob)
        .find(|(_, d)| old.write_owners(d)[0] == MemberId(3))
        .expect("an output owned by node 3 before the change");
    let action = (0..1000)
        .map(|i| kbf_segments::sha256(format!("action {i}").as_bytes()))
        .find(|d| new.write_owners(d)[0] == MemberId(1))
        .expect("an action owned by node 1 after it");
    let result = ActionResult {
        output_files: vec![OutputFile {
            path: "out/file.txt".to_owned(),
            digest: Some(proto(&output.1)),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    };
    let backend = farm.backend();

    let refused = backend
        .prepare_action_result(&result)
        .await
        .expect_err("held nowhere");
    assert!(
        matches!(refused, StorageError::ActionWrite(ActionWriteError::Absent(ref d)) if *d == [output.1]),
        "{refused:?}"
    );

    farm.nodes[&3]
        .put(Bytes::from(output.0.clone()))
        .await
        .expect("stored");
    let record = backend
        .prepare_action_result(&result)
        .await
        .expect("held, at the owner of the generation before");
    backend
        .commit_action_record(action, record)
        .await
        .expect("committed, with the output copied from there");
    assert!(farm.holders(&output.1).await.contains(&1));
}

/// Catches a prepare that accepts a result whose output is held nowhere, or names the
/// wrong blob: the error names the blob of the closure no node holds.
#[tokio::test(flavor = "multi_thread")]
async fn prepare_names_the_output_no_node_holds() {
    let farm = Farm::new(None);
    let map = farm.set(1, &ALL_ACTIVE, 1);
    let case = case(&map);

    let error = farm
        .backend()
        .prepare_action_result(&case.result)
        .await
        .expect_err("held nowhere");
    assert!(
        matches!(error, StorageError::ActionWrite(ActionWriteError::Absent(ref d)) if *d == [case.output.1]),
        "{error:?}"
    );
}

/// Catches a prepare that names only the first of several outputs held nowhere: the
/// refusal carries every missing blob of the closure, as the refusal of a write at a
/// storage node does.
#[tokio::test(flavor = "multi_thread")]
async fn prepare_names_every_output_no_node_holds() {
    let farm = Farm::new(None);
    let map = farm.set(1, &ALL_ACTIVE, 1);
    let (first, second) = (blob(0).1, blob(1).1);
    let result = ActionResult {
        output_files: [("out/a.txt", &first), ("out/b.txt", &second)]
            .into_iter()
            .map(|(path, d)| OutputFile {
                path: path.to_owned(),
                digest: Some(proto(d)),
                ..OutputFile::default()
            })
            .collect(),
        ..ActionResult::default()
    };
    drop(map);

    let error = farm
        .backend()
        .prepare_action_result(&result)
        .await
        .expect_err("held nowhere");
    let StorageError::ActionWrite(ActionWriteError::Absent(mut named)) = error else {
        panic!("not an absent refusal: {error:?}");
    };
    named.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(named, expected);
}

/// Catches a prepare that checks an output directory's tree but not what the tree names:
/// the tree is read from its owner, and a file in it that is held nowhere is the first
/// absent blob, while with the file stored at its owner (not the tree's) the closure holds
/// the tree and the file.
#[tokio::test(flavor = "multi_thread")]
async fn prepare_follows_an_output_directory_to_the_files_in_it() {
    let farm = Farm::new(None);
    let map = farm.set(1, &ALL_ACTIVE, 1);
    let file = (0..)
        .map(blob)
        .find(|(_, d)| map.write_owners(d)[0] == MemberId(2))
        .expect("a file owned by node 2");
    let tree = reapi::Tree {
        root: Some(reapi::Directory {
            files: vec![reapi::FileNode {
                name: "f".to_owned(),
                digest: Some(proto(&file.1)),
                ..reapi::FileNode::default()
            }],
            ..reapi::Directory::default()
        }),
        children: Vec::new(),
    };
    let tree_bytes = tree.encode_to_vec();
    let tree_digest = kbf_segments::sha256(&tree_bytes);
    let tree_owner = map.write_owners(&tree_digest)[0].0;
    farm.nodes[&tree_owner]
        .put(Bytes::from(tree_bytes))
        .await
        .expect("stored");
    let result = ActionResult {
        output_directories: vec![reapi::OutputDirectory {
            path: "out".to_owned(),
            tree_digest: Some(proto(&tree_digest)),
            ..reapi::OutputDirectory::default()
        }],
        ..ActionResult::default()
    };
    let backend = farm.backend();

    let error = backend
        .prepare_action_result(&result)
        .await
        .expect_err("the file is held nowhere");
    assert!(
        matches!(error, StorageError::ActionWrite(ActionWriteError::Absent(ref d)) if *d == [file.1]),
        "{error:?}"
    );

    farm.nodes[&2]
        .put(Bytes::from(file.0.clone()))
        .await
        .expect("stored");
    let record = backend
        .prepare_action_result(&result)
        .await
        .expect("the file is at its owner");
    let closure: BTreeSet<_> = record.closure.iter().copied().collect();
    assert_eq!(closure, BTreeSet::from([tree_digest, file.1]));
}

/// Catches an action-cache entry that is committed where the output is rather than where
/// the action is (a lookup would then miss), or committed without its blobs (the owner
/// refuses it): the whole cycle of an Execute over three nodes, an entry whose action, result
/// blob and output are on three different nodes, committed through the backend and then
/// answered to a client's GetActionResult through the proxy.
#[tokio::test(flavor = "multi_thread")]
async fn an_entry_is_committed_and_found_through_the_owners() {
    let farm = Farm::new(None);
    let map = farm.set(1, &ALL_ACTIVE, 1);
    let case = case(&map);
    farm.nodes[&2]
        .put(Bytes::from(case.output.0.clone()))
        .await
        .expect("stored");
    let backend = farm.backend();

    let record = backend
        .prepare_action_result(&case.result)
        .await
        .expect("prepared");
    backend
        .commit_action_record(case.action, record)
        .await
        .expect("committed at the action's owner, with its blobs");

    let through_the_proxy = ActionCacheClient::new(farm.front())
        .get_action_result(GetActionResultRequest {
            action_digest: Some(proto(&case.action)),
            ..GetActionResultRequest::default()
        })
        .await
        .expect("GetActionResult")
        .into_inner();
    assert_eq!(through_the_proxy, case.result);
    assert!(
        farm.holders(&case.output.1).await.contains(&3),
        "the output is on the action's owner"
    );
}

/// Catches routing that starts too early or never starts: with no map, and with a map of
/// one member (whose address is not a node the frontend can reach), the frontend writes
/// to the node `--storage` names; with two members it writes to the map's owners.
#[tokio::test(flavor = "multi_thread")]
async fn the_storage_node_is_the_bootstrap_until_the_map_has_two_members() {
    let farm = Farm::new(None);
    let front = farm.front();
    let blobs: Vec<_> = (0..24).map(blob).collect();

    assert_eq!(update(&front, &blobs[..4]).await, vec![OK; 4]);
    farm.cell.set(Farm::map(1, &[MemberState::Active], 1));
    assert_eq!(update(&front, &blobs[4..8]).await, vec![OK; 4]);
    for (_, digest) in &blobs[..8] {
        assert_eq!(farm.holders(digest).await, BTreeSet::from([1]));
    }

    let map = farm.set(2, &ALL_ACTIVE[..2], 1);
    let elsewhere: Vec<_> = blobs[8..]
        .iter()
        .filter(|(_, d)| map.write_owners(d)[0] == MemberId(2))
        .cloned()
        .collect();
    assert!(!elsewhere.is_empty());
    assert_eq!(update(&front, &elsewhere).await, vec![OK; elsewhere.len()]);
    for (_, digest) in &elsewhere {
        assert_eq!(farm.holders(digest).await, BTreeSet::from([2]));
    }
    assert!(farm.routed.active());
    assert_eq!(format!("{:?}", farm.routed), "Routed(..)");
}

/// Catches a frontend that calls the `--storage` node at generation 0 while the map has one
/// member: a node that judges calls by its map refuses 0 as behind (STALE_MAP), so a farm of
/// one would answer nothing. The node holds the map of generation 5 with one member, and the
/// frontend learns that map only after the backend and the proxy are made, so the tag cannot
/// have been taken at construction: the backend's and the proxy's calls to the bootstrap node
/// are answered, and tagged 5. Mutants: the backend not tagging before it uses the
/// bootstrap node; the proxy's fixed list not tagged.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_judges_its_map_is_called_at_the_generation_the_frontend_holds() {
    let map = Farm::map(5, &[MemberState::Active], 1);
    let farm = Farm::new(Some(Arc::clone(&map)));
    let handle = RemoteStorage::new(farm.bootstrap.clone(), 0);
    let routed = Arc::new(
        Routed::new(farm.cell.clone(), farm.plain.clone(), farm.plain.clone())
            .bootstrapped(handle.clone()),
    );
    let backend = Backend::routed(Arc::clone(&routed), Arc::new(handle));
    let owners = Owners::routed(
        Arc::clone(&routed),
        Owner::new("bootstrap", farm.bootstrap.clone()),
    );
    let front = lazy(serve(
        Proxy::new(owners, Arc::new(Authorizers::allow_all())).routes(),
    ));
    farm.cell.set(map);
    assert!(!routed.active(), "one member: the bootstrap node is called");

    let blobs: Vec<_> = (0..4).map(blob).collect();
    let digests: Vec<_> = blobs.iter().map(|(_, d)| *d).collect();
    assert_eq!(
        backend.find_missing(&digests).await.expect("answered"),
        digests
    );
    assert_eq!(update(&front, &blobs).await, vec![OK; 4]);
    assert!(
        backend
            .find_missing(&digests)
            .await
            .expect("answered")
            .is_empty()
    );
}

/// What a node holds of a map the test moves.
struct Moving(Mutex<Arc<Map>>);

impl kbf_storage::replicate::MapSource for Moving {
    fn current(&self) -> Option<Arc<Map>> {
        Some(Arc::clone(
            &self.0.lock().unwrap_or_else(PoisonError::into_inner),
        ))
    }

    fn me(&self) -> Option<MemberId> {
        None
    }
}

/// Catches a frontend that takes the generation for its `--storage` calls once: the map moves
/// from 5 to 6 (the node's and the frontend's), and the next call, made by the same backend
/// and the same proxy, is tagged 6 and answered, not refused as behind (STALE_MAP naming 6).
/// Mutant: the generation tagged on the first call only.
#[tokio::test(flavor = "multi_thread")]
async fn the_bootstrap_calls_follow_a_map_bump() {
    let five = Farm::map(5, &[MemberState::Active], 1);
    let held = Arc::new(Moving(Mutex::new(Arc::clone(&five))));
    let store: Arc<dyn Storage> = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let config = Config {
        write_bytes_in_flight: 64 << 20,
    };
    let service = StorageService::new(Arc::clone(&store), Admission::Open, config)
        .with_map(Arc::clone(&held) as Arc<dyn kbf_storage::replicate::MapSource>);
    let channel = lazy(serve(service.routes()));

    let farm = Farm::new(None);
    let handle = RemoteStorage::new(channel.clone(), 0);
    let routed = Arc::new(
        Routed::new(farm.cell.clone(), farm.plain.clone(), farm.plain.clone())
            .bootstrapped(handle.clone()),
    );
    let backend = Backend::routed(Arc::clone(&routed), Arc::new(handle));
    let owners = Owners::routed(Arc::clone(&routed), Owner::new("bootstrap", channel));
    let front = lazy(serve(
        Proxy::new(owners, Arc::new(Authorizers::allow_all())).routes(),
    ));
    let blobs: Vec<_> = (0..4).map(blob).collect();
    let digests: Vec<_> = blobs.iter().map(|(_, d)| *d).collect();

    farm.cell.set(five);
    assert_eq!(update(&front, &blobs[..2]).await, vec![OK; 2]);
    assert_eq!(
        backend.find_missing(&digests).await.expect("at 5"),
        digests[2..]
    );

    let six = Farm::map(6, &[MemberState::Active], 1);
    *held.0.lock().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&six);
    farm.cell.set(six);
    assert_eq!(update(&front, &blobs[2..]).await, vec![OK; 2]);
    assert!(
        backend
            .find_missing(&digests)
            .await
            .expect("at 6")
            .is_empty()
    );
}

/// Catches a farm that shrinks to one member being sent back to the `--storage` node,
/// which may be one that left: with members 1 to 3 and `--storage` naming node 1, the map
/// becomes node 2 alone, and a write lands on node 2 (as the map says), not node 1. The
/// backend and the proxy decide alike, so the halves of an action-cache write cannot go
/// to different nodes.
#[tokio::test(flavor = "multi_thread")]
async fn a_farm_that_shrinks_to_one_member_is_still_routed_by_its_map() {
    let farm = Farm::new(None);
    let front = farm.front();
    let blobs: Vec<_> = (0..16).map(blob).collect();
    farm.set(1, &ALL_ACTIVE, 1);
    assert_eq!(update(&front, &blobs[..4]).await, vec![OK; 4]);
    assert!(farm.routed.active());

    let survivor = Member {
        id: MemberId(2),
        addr: "127.0.0.1:2".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let alone = Map::new(2, vec![survivor], Rule::Rendezvous, 1, 1, Vec::new()).expect("a map");
    farm.cell.set(Arc::new(alone));
    assert!(farm.routed.active(), "a one-member map after a larger one");
    assert_eq!(update(&front, &blobs[4..]).await, vec![OK; 12]);
    for (_, digest) in &blobs[4..] {
        assert_eq!(farm.holders(digest).await, BTreeSet::from([2]));
    }
    let result = ActionResult {
        output_files: vec![OutputFile {
            path: "out/file.txt".to_owned(),
            digest: Some(proto(&blobs[4].1)),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    };
    farm.backend()
        .prepare_action_result(&result)
        .await
        .expect("the output is found on the survivor, where the map put it");
}

/// Catches a frontend that does not follow a map change, on any of its calls: a node that
/// holds a newer map answers STALE_MAP, the frontend fetches the newer map (once) and the
/// call is run again. A write lands where the newer map puts it, and a read, a FindMissing
/// and an action-cache lookup are answered rather than failed.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_a_newer_map_makes_the_frontend_fetch_it() {
    let newer = Farm::map(2, &ALL_ACTIVE, 1);
    let farm = Farm::new(Some(Arc::clone(&newer)));
    let older = farm.set(1, &ALL_ACTIVE, 1);
    let front = farm.front();
    let blobs: Vec<_> = (0..6).map(blob).collect();
    // The frontend holds the older map again, with the newer one to be fetched.
    let behind = |farm: &Farm| {
        farm.cell.set(Arc::clone(&older));
        farm.cell.fetches.lock().expect("lock").push_back(MapView {
            map: Arc::clone(&newer),
            previous: None,
        });
        farm.cell.asked.lock().expect("lock").clear();
    };
    let asked = |farm: &Farm| farm.cell.asked.lock().expect("lock").clone();

    behind(&farm);
    assert_eq!(update(&front, &blobs[..1]).await, vec![OK]);
    assert_eq!(asked(&farm), [2]);
    let digest = blobs[0].1;
    assert_eq!(
        farm.holders(&digest).await,
        BTreeSet::from([newer.write_owners(&digest)[0].0])
    );

    behind(&farm);
    assert_eq!(
        read(&front, &[digest]).await,
        vec![(OK, blobs[0].0.clone())]
    );
    assert_eq!(asked(&farm), [2]);

    behind(&farm);
    let missing = ContentAddressableStorageClient::new(front.clone())
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![proto(&digest), proto(&blobs[1].1)],
            ..FindMissingBlobsRequest::default()
        })
        .await
        .expect("answered")
        .into_inner()
        .missing_blob_digests;
    assert_eq!(missing, [proto(&blobs[1].1)]);
    assert_eq!(asked(&farm), [2]);

    behind(&farm);
    let miss = ActionCacheClient::new(front.clone())
        .get_action_result(GetActionResultRequest {
            action_digest: Some(proto(&blobs[2].1)),
            ..GetActionResultRequest::default()
        })
        .await
        .expect_err("no entry");
    assert_eq!(
        miss.code(),
        Code::NotFound,
        "{miss:?} asked {:?}",
        asked(&farm)
    );
    assert_eq!(asked(&farm), [2]);
}

/// Catches a FindMissing that fails when a digest's primary is down: the answer comes
/// from the next owner, which holds the blob, and a blob it lacks is missing. And a
/// read of a blob whose first owner is down is served by the next.
#[tokio::test(flavor = "multi_thread")]
async fn a_down_primary_is_gone_around() {
    let farm = Farm::new(None);
    let map = farm.set(1, &ALL_ACTIVE, 2);
    let blobs: Vec<_> = (0..40)
        .map(blob)
        .filter(|(_, d)| map.write_owners(d)[0] == MemberId(3))
        .take(4)
        .collect();
    assert_eq!(blobs.len(), 4);
    for (bytes, digest) in &blobs[..2] {
        for owner in map.write_owners(digest) {
            farm.nodes[&owner.0]
                .put(Bytes::from(bytes.clone()))
                .await
                .expect("stored");
        }
    }
    farm.plain.take_down(3);
    let front = farm.front();

    let digests: Vec<_> = blobs.iter().map(|(_, d)| proto(d)).collect();
    let missing = ContentAddressableStorageClient::new(front.clone())
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: digests,
            ..FindMissingBlobsRequest::default()
        })
        .await
        .expect("answered by the next owner")
        .into_inner()
        .missing_blob_digests;
    assert_eq!(missing, [proto(&blobs[2].1), proto(&blobs[3].1)]);

    let read = read(&front, &[blobs[0].1]).await;
    assert_eq!(read, vec![(OK, blobs[0].0.clone())]);
}
