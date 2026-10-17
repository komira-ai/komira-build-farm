//! The daemon's data plane over the storage map: which side a blob goes to, how each
//! storage node is verified, and what a node that finds the daemon's map old does to a
//! fetch in progress. Storage nodes are real caches served over loopback; the daemon's
//! side is `FarmCas` over a `MapHolder` the test fills as the frontend's pushes would.

mod support;

use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use kbf_daemon::cas::{Cas, CasError, Chunks};
use kbf_daemon::{FarmCas, HeldMaps, MapHolder, storage_nodes};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_proto::reapi::Digest;
use kbf_route::{Liveness, MapSource as _, Nodes as _};
use kbf_transfer::{Record, RecordHasher, TransferConfig, Transfers, proto_digest};
use kbf_types::LeaseId;
use support::storage_node::{Node, serve_plain, serve_tls};
use support::{PROMPT, Pki, pki};
use tokio::time::timeout;
use tonic::transport::Endpoint;

fn member(id: u64, addr: String) -> Member {
    Member {
        id: MemberId(id),
        addr,
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    }
}

fn map(generation: u64, replicas: u8, members: Vec<Member>) -> Map {
    Map::new(
        generation,
        members,
        Rule::Rendezvous,
        replicas,
        1,
        Vec::new(),
    )
    .expect("a map")
}

/// The digest of `bytes` as the daemon names it.
fn digest(bytes: &[u8]) -> Digest {
    let mut hasher = RecordHasher::new();
    hasher.update(bytes);
    proto_digest(&hasher.finish().0)
}

/// A plane over a frontend CAS at `server`, nodes verified as the daemon's TLS files
/// say, and the holder the test fills.
fn plane(pki: &Pki, server: &Node) -> (Arc<FarmCas>, Arc<MapHolder>) {
    plane_with(
        pki,
        server,
        TransferConfig::default(),
        &TransferConfig::default(),
    )
}

/// [`plane`], the frontend's engine under `engine` and the storage nodes' under `routed`.
fn plane_with(
    pki: &Pki,
    server: &Node,
    engine: TransferConfig,
    routed: &TransferConfig,
) -> (Arc<FarmCas>, Arc<MapHolder>) {
    let tls = pki.client.clone();
    let channel = Endpoint::from_shared(format!("http://127.0.0.1:{}", server.port))
        .expect("endpoint")
        .connect_lazy();
    let engine = Arc::new(Transfers::over(channel, engine));
    let holder = Arc::new(MapHolder::new());
    let nodes = storage_nodes(&tls, Liveness::default()).expect("TLS files");
    let cas = FarmCas::new(engine, Arc::clone(&holder), nodes, routed);
    (Arc::new(cas), holder)
}

/// Catches a plane stuck on one side: with no map a blob is read through the frontend's
/// CAS and one only the owner holds is missing; once a map is held the same plane reads
/// from the owner, and the frontend is not asked again, not even for a blob only it holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blobs_go_to_the_owners_once_a_map_is_held_and_through_the_frontend_before() {
    let pki_ = pki("farm-cas-sides");
    let (frontend, owner) = (serve_plain(), serve_tls(&pki_));
    let at_frontend = frontend.store.hold(b"only the frontend has this").await;
    let at_owner = owner.store.hold(b"only the owner has that").await;
    let (cas, holder) = plane(&pki_, &frontend);

    let bytes = cas.get(&at_frontend).await.expect("through the frontend");
    assert_eq!(bytes, b"only the frontend has this");
    assert!(matches!(
        cas.get(&at_owner).await,
        Err(CasError::Missing(_))
    ));
    assert_eq!(frontend.store.reads(), 2);

    assert!(holder.offer(map(2, 1, vec![member(1, owner.name())])));
    let bytes = cas.get(&at_owner).await.expect("from the owner");
    assert_eq!(bytes, b"only the owner has that");
    assert!(matches!(
        cas.get(&at_frontend).await,
        Err(CasError::Missing(_))
    ));
    assert_eq!(owner.store.reads(), 2, "both reads went to the owner");
    assert_eq!(
        frontend.store.reads(),
        2,
        "the frontend was not asked again"
    );
}

/// Catches outputs stored at the frontend in map mode, and a put that goes to a node that
/// does not own the blob: the blob lands on the owner and nowhere else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outputs_are_stored_at_the_owner() {
    let pki_ = pki("farm-cas-put");
    let (frontend, owner) = (serve_plain(), serve_tls(&pki_));
    let (cas, holder) = plane(&pki_, &frontend);
    holder.offer(map(2, 1, vec![member(1, owner.name())]));

    let stored = cas.put(b"an output".to_vec()).await.expect("stored");
    assert_eq!(stored, digest(b"an output"));
    assert!(owner.store.holds(&stored).await);
    assert!(!frontend.store.holds(&stored).await);
}

/// Catches one server name for every storage node (`--tls-server-name`, which the tests'
/// TLS files set to `localhost`, applied to storage): node-03 is dialled as 127.0.0.1 and
/// answers with the certificate of `localhost` (node-02's), signed by the farm's CA. It is
/// refused before a request is made; the same node by its own name is read. Mutant:
/// storage dialled with `TlsFiles::load()` (the frontend's name) accepts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nodes_certificate_for_another_host_is_refused() {
    let pki_ = pki("farm-cas-names");
    assert_eq!(pki_.client.server_name.as_deref(), Some("localhost"));
    let (frontend, node_03) = (serve_plain(), serve_tls(&pki_));
    let blob = node_03.store.hold(b"node-03's blob").await;
    let (cas, holder) = plane(&pki_, &frontend);

    holder.offer(map(2, 1, vec![member(3, node_03.by_ip())]));
    let refused = cas.get(&blob).await.expect_err("refused");
    assert!(matches!(refused, CasError::Unavailable(..)), "{refused:?}");
    assert_eq!(
        node_03.store.reads(),
        0,
        "the handshake failed before any call"
    );

    holder.offer(map(3, 1, vec![member(3, node_03.name())]));
    assert_eq!(
        cas.get(&blob).await.expect("by its own name"),
        b"node-03's blob"
    );
}

/// Catches a fetch that gives up, or goes on with the old map, when a node answers
/// `STALE_MAP` mid-fetch: node A (the only owner in generation 2) says it is at 3 while
/// the daemon reads; the daemon asks for a map of 3 (`holder.asks`, which its session
/// turns into a `MapRequest`), the frontend's answer arrives (offered here), and the read
/// is made again against generation 3's owner, B. Mutants: a refetch that fails at once
/// (a lost fetch); one that returns the map held without waiting for the newer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_map_mid_fetch_is_refetched_and_the_fetch_goes_to_the_new_owner() {
    let pki_ = pki("farm-cas-stale");
    let (frontend, a, b) = (serve_plain(), serve_tls(&pki_), serve_tls(&pki_));
    let blob = b.store.hold(b"moved to B").await;
    a.store.go_stale(3);
    let (cas, holder) = plane(&pki_, &frontend);
    holder.offer(map(2, 1, vec![member(1, a.name())]));

    let fetch = tokio::spawn({
        let (cas, blob) = (Arc::clone(&cas), blob.clone());
        async move { cas.get(&blob).await }
    });
    let mut asks = holder.asks();
    timeout(PROMPT, asks.wait_for(|wanted| *wanted >= 3))
        .await
        .expect("the daemon asks for generation 3")
        .expect("open");
    assert!(!fetch.is_finished(), "the fetch waits for the map");
    holder.offer(map(3, 1, vec![member(2, b.name())]));

    let bytes = timeout(PROMPT, fetch)
        .await
        .expect("the fetch ends")
        .expect("task")
        .expect("read from the new owner");
    assert_eq!(bytes, b"moved to B");
    assert_eq!((a.store.reads(), b.store.reads()), (1, 1));
}

/// Catches callers of one blob each reading it from the owners: eight callers ask at once
/// for a blob whose first owner (member 1, in rank order) lacks it and whose second holds
/// it. The two owners are asked once between them, not once per caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callers_of_one_blob_share_one_read_across_its_owners() {
    let pki_ = pki("farm-cas-flight");
    let (frontend, one, two) = (serve_plain(), serve_tls(&pki_), serve_tls(&pki_));
    let both = map(2, 2, vec![member(1, one.name()), member(2, two.name())]);
    let bytes = (0_u64..10_000)
        .map(|i| format!("blob number {i}").into_bytes())
        .find(|b| {
            let mut h = RecordHasher::new();
            h.update(b);
            both.read_order(&h.finish().0) == [MemberId(1), MemberId(2)]
        })
        .expect("a blob whose first owner is member 1");
    let blob = two.store.hold(&bytes).await;
    let (cas, holder) = plane(&pki_, &frontend);
    holder.offer(both);

    let reads = (0..8).map(|_| cas.get(&blob));
    for read in join_all(reads).await {
        assert_eq!(read.expect("read"), bytes);
    }
    assert_eq!(one.store.reads(), 1, "member 1 was asked once");
    assert_eq!(two.store.reads(), 1, "member 2 was asked once");
}

/// Catches a view that offers a replaced map as the previous one when the holder moved
/// between the two reads, and a refetch with no bound: the clock is paused and the wait
/// for a generation that never comes ends at its deadline.
#[tokio::test(start_paused = true)]
async fn held_maps_view_the_holder_and_give_up_on_a_map_that_never_comes() {
    let holder = Arc::new(MapHolder::new());
    let maps = HeldMaps::new(Arc::clone(&holder), Duration::from_secs(5));
    assert!(maps.current().is_none(), "nothing to route by yet");
    holder.offer(map(2, 1, vec![member(1, "localhost:1".to_owned())]));
    let view = maps.current().expect("a map");
    assert_eq!(view.map.generation(), 2);
    assert!(view.previous.is_none());
    holder.offer(map(3, 1, vec![member(1, "localhost:1".to_owned())]));
    let view = maps.current().expect("a map");
    assert_eq!(view.previous.expect("replaced").generation(), 2);

    let fresh = maps.refetch(3).await.expect("held already");
    assert_eq!(fresh.map.generation(), 3);
    let error = maps.refetch(9).await.expect_err("never comes");
    assert_eq!(error.at_least, 9);
    assert!(error.reason.contains("5s"), "{error}");
    assert_eq!(
        *holder.asks().borrow(),
        9,
        "the wait was recorded for the session"
    );
}

fn config(in_flight: usize) -> TransferConfig {
    TransferConfig {
        in_flight,
        ..TransferConfig::default()
    }
}

/// Runs every operation of the store on `cas` and checks that each one reached `here`
/// and not `elsewhere`: a blob put, streamed in chunks and put as a record is held by
/// `here` alone; one read into a file comes back whole; `missing` names the blob nobody
/// holds and not the one just stored; the store runs `in_flight` requests at once. The
/// bytes carry `tag`, so a node holding a blob of an earlier call proves nothing.
async fn through<C: Cas>(
    cas: &C,
    tag: &str,
    (here, elsewhere): (&Node, &Node),
    in_flight: usize,
    dir: &std::path::Path,
) {
    let put = format!("{tag}: put").into_bytes();
    let stored = cas.put(put.clone()).await.expect("put");
    assert_eq!(stored, digest(&put), "{tag}");

    let whole = format!("{tag}: chunked").into_bytes();
    let (first, second) = whole.split_at(5);
    let chunks: Chunks = Box::pin(futures::stream::iter(vec![
        Ok(first.to_vec()),
        Ok(second.to_vec()),
    ]));
    let chunked = cas
        .put_chunks(digest(&whole), chunks)
        .await
        .expect("put_chunks");
    assert_eq!(chunked, digest(&whole), "{tag}");

    let record = format!("{tag}: record").into_bytes();
    cas.put_records(vec![Record::bytes(record.clone())])
        .await
        .expect("put_records");
    let recorded = digest(&record);

    for blob in [&stored, &chunked, &recorded] {
        assert!(here.store.holds(blob).await, "{tag}: held where it went");
        assert!(
            !elsewhere.store.holds(blob).await,
            "{tag}: and not elsewhere"
        );
    }

    let path = dir.join(format!("{}.blob", tag.replace(' ', "-")));
    let mut file = tokio::fs::File::create(&path).await.expect("create");
    cas.read_to(&chunked, &mut file).await.expect("read_to");
    assert_eq!(std::fs::read(&path).expect("read back"), whole, "{tag}");

    let nobody = digest(format!("{tag}: held by nobody").as_bytes());
    assert_eq!(
        cas.missing(&[stored.clone(), nobody.clone()]).await,
        [nobody],
        "{tag}"
    );
    assert!(
        cas.intake().is_none(),
        "{tag}: neither side takes an intake"
    );
    assert_eq!(cas.in_flight(), in_flight, "{tag}");
}

/// Catches an operation that a plane (or a lease of it) does not delegate to the side it
/// is on, or delegates to the wrong one: each method of the store is run on the plane
/// before a map is held (everything goes to the frontend) and after (everything goes to
/// the owner, and the frontend gets none), on a lease begun on each side, and on the
/// plane's own calls. The two sides run different limits, so `in_flight` says which side
/// answered. A lease keeps the side it began on after the map arrives, and leasing from
/// it chooses again. Mutants: any one method of either impl forced to the server side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_operation_follows_the_side_of_the_plane_and_of_its_lease() {
    let pki_ = pki("farm-cas-ops");
    let (frontend, owner) = (serve_plain(), serve_tls(&pki_));
    let (cas, holder) = plane_with(&pki_, &frontend, config(3), &config(7));
    let dir = support::scratch("farm-cas-ops");
    let sides = (&frontend, &owner);
    let (reversed, id) = ((&owner, &frontend), LeaseId::new(1, 1));

    through(&*cas, "plane, no map", sides, 3, &dir).await;
    let early = Arc::new(cas.lease(&id));
    through(&*early, "lease, no map", sides, 3, &dir).await;

    assert!(holder.offer(map(2, 1, vec![member(1, owner.name())])));
    through(&*cas, "plane, map", reversed, 7, &dir).await;
    let late = Arc::new(cas.lease(&id));
    through(&*late, "lease, map", reversed, 7, &dir).await;

    // The lease begun before the map stays on the frontend; one asked of it is new.
    through(&*early, "early lease, map held", sides, 3, &dir).await;
    let renewed = Arc::new(early.lease(&id));
    through(&*renewed, "lease of a lease, map", reversed, 7, &dir).await;
}

/// Catches storage nodes dialled with TLS files that were never read: the daemon with no
/// CA, no certificate or no key refuses to build the plane, naming the file, rather than
/// failing its first blob.
#[test]
fn storage_nodes_need_all_three_tls_files() {
    let pki_ = pki("farm-cas-files");
    for missing in ["ca_cert", "cert", "key"] {
        let mut files = pki_.client.clone();
        let path = std::path::PathBuf::from(format!("/nonexistent/{missing}.pem"));
        match missing {
            "ca_cert" => files.ca_cert = path,
            "cert" => files.cert = path,
            _ => files.key = path,
        }
        let Err(error) = storage_nodes(&files, Liveness::default()) else {
            panic!("a plane was built without its {missing} file");
        };
        assert!(error.to_string().contains(missing), "{error}");
    }
}

/// Catches a `--storage-dial-timeout-ms` that never reaches the nodes the daemon dials: a
/// "node" that takes the TCP connection and never speaks TLS fails a call within the
/// dial timeout given, here 300 ms, not after the default ten seconds the test would
/// otherwise outlive.
#[tokio::test]
async fn a_dial_timeout_given_to_storage_nodes_bounds_a_silent_node() {
    let pki_ = pki("farm-cas-dial");
    let mute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let silent = member(1, mute.local_addr().expect("address").to_string());
    let liveness = Liveness {
        connect: Duration::from_millis(300),
        ..Liveness::default()
    };
    let nodes = storage_nodes(&pki_.client, liveness).expect("TLS files");
    let storage = nodes.node(&silent, 1).expect("a node");
    let asked = [kbf_types::Digest::new(
        kbf_types::DigestFunction::Sha256,
        [0; 32],
        1,
    )];
    let answer = timeout(Duration::from_secs(5), storage.find_missing(&asked))
        .await
        .expect("the dial timeout ends the call within 5 s");
    assert!(answer.is_err(), "{answer:?}");
    drop(mute);
}
