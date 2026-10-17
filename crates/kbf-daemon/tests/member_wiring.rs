//! `kbf storage-and-daemon` on a farm of two, end to end: a real frontend over SQLite, two
//! storage nodes that register with it, and for each a daemon connected to the frontend
//! whose data plane is the one that mode gives its daemon half ([`member_cas`]) over its
//! own node's storage and its own node's map.
//!
//! The nodes are nodes of a chain (R = 2 on two members: every blob is on both). The proof
//! of the fix: a blob the daemon beside node A writes and the map places on node B (as its
//! primary) lands on B and is copied by B's chain to A; a blob whose primary is A is written
//! through A's own service in-process, with no call through the node dialler at all, and A's
//! chain copies it to B. Before this, the daemon half wrote to its own node whatever the
//! map said, and then wrote to its raw storage, which stores one copy and judges no map. The tests wait with deadlines that only
//! end a hang, and assert no duration.

mod support;

use std::future::pending;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::cas::Cas as _;
use kbf_daemon::{
    Daemon, DaemonConfig, FakeRuntime, LocalMember, NodeReport, Runtime, TlsFiles, member_cas,
};
use kbf_map::{Member, MemberId};
use kbf_mapsync::MapHolder;
use kbf_route::Nodes;
use kbf_storage::service::{CatchUp, InProcess};
use kbf_storage::{Storage, StorageError};
use kbf_transfer::{CasError, RoutedCas, TransferConfig, proto_digest};
use support::farm::{
    NodeUnderTest, Pki, blob_owned_by, chain_node, chain_node_with, digest, front, map_of,
    unmapped_in_process,
};
use support::scratch;

/// The node dialler a daemon half calls other members through, counting its calls by
/// member address: a call here is a call over the network.
struct Counted {
    inner: Arc<dyn Nodes>,
    dialled: Mutex<Vec<String>>,
}

impl Counted {
    fn dialled(&self) -> Vec<String> {
        self.dialled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Nodes for Counted {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        self.dialled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(member.addr.clone());
        self.inner.node(member, generation)
    }
}

/// One storage-and-daemon process: its node, its daemon half's map and its data plane.
struct Process {
    node: NodeUnderTest,
    id: MemberId,
    cas: Arc<RoutedCas>,
    dialled: Arc<Counted>,
    tls: TlsFiles,
}

/// A daemon connected to the frontend at `front`, as `name`, holding the map it is sent.
fn daemon(pki: &Pki, front: &support::farm::Front, name: &str) -> (Arc<MapHolder>, TlsFiles) {
    let own = pki.cert(name, "daemon", &[name]);
    let tls = TlsFiles {
        ca_cert: pki.ca(),
        cert: own.cert,
        key: own.key,
        server_name: Some("localhost".to_owned()),
    };
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let report = NodeReport::detect(&[runtime.driver()]).expect("detect this node");
    let mut config = DaemonConfig::new(
        format!("https://localhost:{}", front.worker.port()),
        tls.clone(),
        name.to_owned(),
    );
    config.reconnect_after = Duration::from_millis(100);
    let held = Arc::new(MapHolder::new());
    let daemon = Daemon::new(config, runtime, report)
        .expect("daemon config")
        .with_map(Arc::clone(&held));
    tokio::spawn(daemon.run(pending()));
    (held, tls)
}

/// The process of `node`: its daemon half's plane over `held`, its own member in-process.
fn process(node: NodeUnderTest, id: MemberId, held: Arc<MapHolder>, tls: &TlsFiles) -> Process {
    let local = LocalMember::new(node.addr.to_string(), node.in_process.clone());
    assert_eq!(local.addr(), node.addr.to_string());
    let dialled = Arc::new(Counted {
        inner: kbf_daemon::storage_nodes(tls, kbf_daemon::Liveness::default()).expect("TLS files"),
        dialled: Mutex::default(),
    });
    let cas = member_cas(
        held,
        local,
        Arc::clone(&dialled) as Arc<dyn Nodes>,
        &TransferConfig::default(),
    );
    Process {
        node,
        id,
        cas,
        dialled,
        tls: tls.clone(),
    }
}

/// A farm of two storage-and-daemon processes, A and B, each holding the two-member map,
/// and the frontend, which the farm lasts as long as.
async fn farm(name: &str) -> (Process, Process, Arc<kbf_map::Map>, support::farm::Front) {
    farm_with(name, CatchUp::caught_up()).await
}

/// [`farm`] whose node A is judged by `catch_up_a`.
async fn farm_with(
    name: &str,
    catch_up_a: CatchUp,
) -> (Process, Process, Arc<kbf_map::Map>, support::farm::Front) {
    let dir = scratch(name);
    let pki = Pki::new(&dir);
    let front = front(&pki, &dir);
    let a = chain_node_with(&pki, &front, "node-a", &dir.join("a"), catch_up_a).await;
    let b = chain_node(&pki, &front, "node-b", &dir.join("b")).await;
    let (held_a, tls_a) = daemon(&pki, &front, "daemon-a");
    let (held_b, tls_b) = daemon(&pki, &front, "daemon-b");
    let map = map_of("daemon A", &held_a, 2).await;
    map_of("daemon B", &held_b, 2).await;
    for node in [&a, &b] {
        map_of("a node", &node.maps, 2).await;
    }
    let id = |addr: std::net::SocketAddr| {
        map.members()
            .iter()
            .find(|m| m.addr == addr.to_string())
            .expect("the node is in the map at the address it bound")
            .id
    };
    let (id_a, id_b) = (id(a.addr), id(b.addr));
    assert_ne!(id_a, id_b);
    a.is_member(id_a);
    b.is_member(id_b);
    (
        process(a, id_a, held_a, &tls_a),
        process(b, id_b, held_b, &tls_b),
        map,
        front,
    )
}

/// Catches a daemon half that writes by its own rules instead of the map's, and one that
/// reaches its own node around the node's service. The farm is two chain nodes with R = 2,
/// so every blob is on both.
///
/// A's daemon writes a blob whose primary is B: B stores it and, as the primary, copies it
/// to its replica A; A's daemon dialled B once and no one else. A's daemon writes a blob
/// whose primary is A: the write is in-process (A's daemon dialled no node at all), and A,
/// as the primary, copies it to B, which holds it when the write is answered: only the
/// service's chain does that, so a daemon that called A's storage directly would leave one
/// copy.
///
/// Mutants: the in-process `put` calling the inner storage (the second blob is on A and
/// not on B); a node set that returns the local storage for every member (the dial counts
/// are 0 and the first blob is on A only).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_half_writes_where_the_map_says_and_in_process_for_its_own_node() {
    let (a, b, map, _front) = farm("member-wiring").await;

    // Primary B, written by A's daemon half: on B, and on its replica A through B's chain.
    let bytes = blob_owned_by(&map, b.id, "b");
    let blob = digest(&bytes);
    let stored = a.cas.put(bytes.clone()).await.expect("stored");
    assert_eq!(stored, proto_digest(&blob));
    assert!(b.node.holds(&blob).await, "the blob is on its primary, B");
    assert!(
        a.node.holds(&blob).await,
        "and on its replica, A, through B's chain"
    );
    assert_eq!(
        a.dialled.dialled(),
        [b.node.addr.to_string()],
        "A dialled B once and no one else"
    );
    assert_eq!(b.cas.get(&stored).await.expect("B reads it"), bytes);
    assert_eq!(
        b.dialled.dialled(),
        Vec::<String>::new(),
        "B dialled no one"
    );
    assert_eq!(a.cas.get(&stored).await.expect("A reads it"), bytes);

    // Primary A, written by A's daemon half: in-process, and copied to B by A's chain.
    let before = a.dialled.dialled().len();
    let bytes = blob_owned_by(&map, a.id, "a");
    let blob = digest(&bytes);
    let stored = a.cas.put(bytes.clone()).await.expect("stored");
    assert!(a.node.holds(&blob).await, "the blob is on its primary, A");
    assert!(
        b.node.holds(&blob).await,
        "and on its replica, B, through A's chain"
    );
    assert_eq!(a.cas.get(&stored).await.expect("read back"), bytes);
    assert_eq!(
        a.dialled.dialled().len(),
        before,
        "a blob A is the primary of crossed no network from A's daemon"
    );
    assert_eq!(b.cas.get(&stored).await.expect("B reads it"), bytes);
}

/// Catches a local arm that judges no map: the daemon half's calls to its own node carry
/// the generation the daemon routed by, so a daemon behind its node is told so (STALE_MAP),
/// as it is by any other node, and the blob is stored nowhere. The node learns a newer map
/// than the one the frontend (and so the daemon, which can fetch nothing newer) holds; the
/// write the daemon half makes to the blob's primary, its own node, is refused and the
/// router asks the frontend for the node's newer map, which the frontend does not have (the
/// call ends when the router's bound on that ask does, a deadline that only ends a hang),
/// and answers UNAVAILABLE.
///
/// Mutants: `InProcess::at` ignoring its generation (the write lands on A and on B);
/// `check_map` removed from the in-process write body (the same).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_half_behind_its_node_is_told_so_by_its_own_node() {
    let (a, b, map, _front) = farm("member-stale").await;
    let newer = kbf_map::Map::new(
        map.generation() + 1,
        map.members().to_vec(),
        kbf_map::Rule::Rendezvous,
        map.r(),
        1,
        Vec::new(),
    )
    .expect("a newer map");
    assert!(a.node.maps.offer(newer), "the node learns a newer map");

    let bytes = blob_owned_by(&map, a.id, "stale");
    let blob = digest(&bytes);
    let refused = a
        .cas
        .put(bytes)
        .await
        .expect_err("the daemon is behind its node");
    assert!(matches!(refused, CasError::Unavailable(..)), "{refused:?}");
    assert!(
        refused
            .to_string()
            .contains("could not fetch map generation"),
        "{refused}"
    );
    assert!(!a.node.holds(&blob).await, "nothing reached A");
    assert!(!b.node.holds(&blob).await, "and nothing B");
}

/// A plane with the map of `source`'s node held in a holder of its own, and the local
/// member `local` served in-process by `node`; and the dialler it calls other members
/// through, which counts.
fn plane_over(
    daemon_map: Option<&kbf_map::Map>,
    local: &Process,
    node: InProcess,
    tls: &TlsFiles,
) -> (Arc<RoutedCas>, Arc<Counted>) {
    let held = Arc::new(MapHolder::new());
    if let Some(map) = daemon_map {
        assert!(held.offer(map.clone()));
    }
    let member = LocalMember::new(local.node.addr.to_string(), node);
    let dialled = Arc::new(Counted {
        inner: kbf_daemon::storage_nodes(tls, kbf_daemon::Liveness::default()).expect("TLS files"),
        dialled: Mutex::default(),
    });
    let cas = member_cas(
        held,
        member,
        Arc::clone(&dialled) as Arc<dyn Nodes>,
        &TransferConfig::default(),
    );
    (cas, dialled)
}

/// Catches the no-map rule not holding. Until the daemon half holds a map, a write fails
/// UNAVAILABLE and reaches no node, its own included (the defect: a daemon with no map
/// wrote to its own node as if it were the owner). Until the storage half holds a map,
/// its own member is refused by the storage service's own check, as its listener refuses
/// every call (the node here is a service that holds none): a blob it owns is taken by the
/// blob's other owner (the router goes on to the next owner when one cannot answer) and
/// never reaches it, and a blob owned by another member goes on to its owner.
///
/// Mutants: the service's map check skipped on the in-process path (the second write is
/// taken in-process by the node with an empty map and B is never dialled); the router asked
/// for a local write when no map is held (the first write lands on A).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_half_writes_nowhere_until_it_and_its_node_hold_a_map() {
    let (a, b, map, _front) = farm("member-no-map").await;
    let tls = a.tls.clone();
    let own = blob_owned_by(&map, a.id, "own");
    let own_digest = digest(&own);

    // No map for the daemon half: nothing is written, not even to its own node.
    let (cas, dialled) = plane_over(None, &a, a.node.in_process.clone(), &tls);
    let refused = cas.put(own.clone()).await.expect_err("no map");
    assert!(matches!(refused, CasError::Unavailable(..)), "{refused:?}");
    assert!(!a.node.holds(&own_digest).await, "nothing reached A");
    assert!(dialled.dialled().is_empty());

    // A map for the daemon half, none for its node: its own member is refused by the
    // service's own check, as its listener refuses every call, so the write goes on to the
    // blob's other owner, B, over the network, and B takes it as the primary (and copies it
    // to A, whose listener does hold a map) ...
    let (cas, dialled) = plane_over(Some(&map), &a, unmapped_in_process(), &tls);
    cas.put(own.clone())
        .await
        .expect("B takes it as the next owner");
    assert_eq!(
        dialled.dialled(),
        [b.node.addr.to_string()],
        "the daemon's own member was refused and B was dialled"
    );
    assert!(b.node.holds(&own_digest).await, "B took it");
    // ... and a blob another member owns is not affected.
    let theirs = blob_owned_by(&map, b.id, "theirs");
    cas.put(theirs.clone()).await.expect("B takes it");
    assert!(b.node.holds(&digest(&theirs)).await);

    // The node holds a map: its own member is used, in-process.
    let (cas, dialled) = plane_over(Some(&map), &a, a.node.in_process.clone(), &tls);
    let again = blob_owned_by(&map, a.id, "own again");
    cas.put(again.clone()).await.expect("A takes it");
    assert!(a.node.holds(&digest(&again)).await);
    assert!(dialled.dialled().is_empty(), "in-process, no network");
}

/// Catches a write answered with one copy and nothing owed. The partner B of A's chain is
/// down (its listener stopped: the leg to it is refused). A's daemon half writes a blob
/// whose primary is A, in-process: the write is answered (one copy, on A) and A's repair
/// journal holds the copy owed to B for that digest and no other.
///
/// Mutant: the in-process `put` calling the inner storage (`Storage::put`) instead of the
/// service's `write_in`: the write is answered, the blob is on A, and nothing is owed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_with_the_partner_down_is_answered_and_the_copy_is_owed() {
    let (a, b, map, _front) = farm("member-partner-down").await;
    b.node.stop_server();

    let bytes = blob_owned_by(&map, a.id, "owed");
    let blob = digest(&bytes);
    let stored = a
        .cas
        .put(bytes.clone())
        .await
        .expect("answered with one copy");
    assert_eq!(stored, proto_digest(&blob));
    assert!(a.node.holds(&blob).await, "the blob is on its primary, A");
    assert!(!b.node.holds(&blob).await, "and not on B, which is down");
    assert_eq!(
        a.node.owed(),
        [(blob, b.id)],
        "A owes B the copy of this blob, durably, and nothing else"
    );
    assert_eq!(a.cas.get(&stored).await.expect("read back"), bytes);
}

/// Catches a local arm that answers a miss on a node that is catching up as NOT_FOUND. A's
/// index is being rebuilt, so A's "not held" is no answer: A's daemon half reads a blob A
/// is the primary of and nobody holds, through the plane (`read_to` and `get`). B, the
/// other owner, is caught up and says it does not hold it. The walk must end on A's "could
/// not say", UNAVAILABLE (retryable), not on a miss ([`CasError::Missing`]), which the
/// daemon takes to mean the farm does not have the blob. Once A has finished catching up,
/// the same read is the miss. A's own wire listener is not asked: the read is A's
/// in-process member, whose miss rule is the one under test.
///
/// Mutant: the in-process `read_range` skipping `read_error` (the catch-up rule): the
/// `read_to` assertion fails with `Missing`. Both reads go through `read_range` at the
/// plane, so this pins the `read_range` rule only; `read_blob`'s rule is pinned by the
/// kbf-storage in-process tests, not here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_half_reading_a_miss_on_its_catching_up_node_is_told_unavailable() {
    let catch_up = CatchUp::rebuilding();
    let (a, b, map, _front) = farm_with("member-catching-up", catch_up.clone()).await;
    let absent = digest(&blob_owned_by(&map, a.id, "absent"));
    let wire = proto_digest(&absent);

    let dir = scratch("member-catching-up-file");
    let mut file = tokio::fs::File::create(dir.join("blob"))
        .await
        .expect("a file");
    let refused = a.cas.read_to(&wire, &mut file).await.expect_err("a miss");
    assert!(matches!(refused, CasError::Unavailable(..)), "{refused:?}");
    assert!(refused.to_string().contains("catching up"), "{refused}");
    let refused = a.cas.get(&wire).await.expect_err("a miss");
    assert!(matches!(refused, CasError::Unavailable(..)), "{refused:?}");
    assert!(refused.to_string().contains("catching up"), "{refused}");
    assert!(!a.node.holds(&absent).await && !b.node.holds(&absent).await);

    catch_up.finish();
    let missing = a.cas.get(&wire).await.expect_err("a miss");
    assert!(matches!(missing, CasError::Missing(..)), "{missing:?}");
}
