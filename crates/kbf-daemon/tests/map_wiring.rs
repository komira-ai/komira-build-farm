//! The storage map, end to end, through the real binaries' start paths: a `kbf frontend`
//! with a SQLite state store, two storage nodes that register with it
//! (`kbf_server::storage_half::start`, the storage half of `kbf storage-and-daemon`, which
//! registers as `kbf storage --frontend` does), and a daemon connected to the frontend's
//! worker listener. Everything is over mutual TLS with certificates that name their roles.
//!
//! The proof of the rehearsal: the frontend hands the members' map to the daemon with no
//! test in the middle, the daemon's data plane ([`FarmCas`]) then writes a blob to its
//! first owner, which copies it to the second (R = 2), and reads it back; each node holds
//! the map its own registration and heartbeats brought it. With the second owner down the
//! write is still answered, the first owner owes the copy in its repair journal, and its
//! repair worker makes it when the owner returns. The tests wait with deadlines that only
//! end a hang, and assert no duration.

mod support;

use std::future::pending;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::cas::Cas as _;
use kbf_daemon::{Daemon, DaemonConfig, FakeRuntime, FarmCas, NodeReport, Runtime, TlsFiles};
use kbf_map::{Map, MemberId};
use kbf_mapsync::MapHolder;
use kbf_transfer::{TransferConfig, Transfers, proto_digest};
use support::farm::{
    Front, NodeUnderTest, Pki, blob_owned_by, digest, eventually, front, map_of, node, try_node,
};
use support::scratch;
use tonic::transport::Endpoint;

/// A frontend, two storage nodes registered with it, and a daemon that holds the map the
/// frontend sent it, with its data plane.
struct Farm {
    pki: Pki,
    front: Front,
    dir: PathBuf,
    a: NodeUnderTest,
    b: NodeUnderTest,
    id_a: MemberId,
    id_b: MemberId,
    map: Arc<Map>,
    cas: Arc<FarmCas>,
}

async fn farm(name: &str) -> Farm {
    let dir = scratch(name);
    let pki = Pki::new(&dir);
    let front = front(&pki, &dir);
    let a = node(&pki, &front, "node-a", &dir.join("a")).await;
    let b = node(&pki, &front, "node-b", &dir.join("b")).await;

    // The daemon connects with its own certificate, and holds what it is sent.
    let own = pki.cert("daemon", "daemon", &["node-1"]);
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
        "node-1".to_owned(),
    );
    config.reconnect_after = Duration::from_millis(100);
    let held = Arc::new(MapHolder::new());
    let daemon = Daemon::new(config, runtime, report)
        .expect("daemon config")
        .with_map(Arc::clone(&held));
    tokio::spawn(daemon.run(pending()));

    // The frontend pushed it: both members, generation 1 or later.
    let map = map_of("the daemon", &held, 2).await;
    assert!(map.generation() >= 1);
    let id = |addr: std::net::SocketAddr| {
        map.members()
            .iter()
            .find(|m| m.addr == addr.to_string())
            .expect("the node is in the map at the address it bound")
            .id
    };
    let (id_a, id_b) = (id(a.addr), id(b.addr));
    assert_ne!(id_a, id_b);

    // The nodes hold the map their heartbeats brought.
    for node in [&a, &b] {
        let at = map_of("a node", &node.maps, 2).await;
        assert!(at.generation() >= 1);
    }

    // The daemon's plane, over the map it was pushed.
    let nodes =
        kbf_daemon::storage_nodes(&tls, kbf_daemon::Liveness::default()).expect("TLS files");
    let channel = Endpoint::from_shared("http://127.0.0.1:1")
        .expect("endpoint")
        .connect_lazy();
    let engine = Arc::new(Transfers::over(channel, TransferConfig::default()));
    let cas = Arc::new(FarmCas::new(
        engine,
        Arc::clone(&held),
        nodes,
        &TransferConfig::default(),
    ));
    Farm {
        pki,
        front,
        dir,
        a,
        b,
        id_a,
        id_b,
        map,
        cas,
    }
}

/// Catches a farm whose map stops at the frontend: both nodes register, the frontend hands
/// the daemon the map of both members (`Welcome.map` or a `MapUpdate`) and each node holds
/// the map its heartbeat brings; the daemon's blobs then go to their first owner, which
/// copies them to the second (R = 2), so both nodes hold each, and read back. Mutants: the
/// frontend started with no map source (the daemon never holds a map: the wait for it never
/// ends); a node that does not keep the map it is sent (its holder stays empty); a plane
/// that writes through the frontend instead of the owner (a blob lands on neither node); a
/// storage service that judges by the map but has no chain (the second owner never
/// holds the blob).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_written_by_a_daemon_lands_on_both_its_owners_through_the_maps_the_frontend_sends() {
    let Farm {
        a,
        b,
        id_a,
        id_b,
        map,
        cas,
        ..
    } = farm("map-wiring").await;
    for (owner, tag) in [(id_a, "a"), (id_b, "b")] {
        let bytes = blob_owned_by(&map, owner, tag);
        let stored = cas.put(bytes.clone()).await.expect("stored");
        let blob = digest(&bytes);
        assert_eq!(stored, proto_digest(&blob), "the digest it answered");
        assert!(a.holds(&blob).await, "{tag}: on the first node");
        assert!(b.holds(&blob).await, "{tag}: and on the second");
        assert!(a.owes().is_empty() && b.owes().is_empty(), "{tag}: no debt");
        assert_eq!(cas.get(&stored).await.expect("read back"), bytes);
    }
}

/// Catches an R = 2 farm that loses the second copy when its owner is away, or never makes
/// it: with the second owner stopped a write is still answered (one copy, not an error),
/// the first owner keeps a record of the copy it owes, and when the second owner returns
/// (the same address, so the same member, with the cache it had) the first owner's repair
/// worker copies the blob and the record is settled. Mutants: no repair worker started (the
/// debt stays for ever); a write that fails when the second owner is away; a worker that
/// settles the debt without making the copy (the returned node does not hold the blob).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_second_owner_away_the_write_is_answered_and_the_copy_is_made_when_it_returns() {
    let Farm {
        pki,
        front,
        dir,
        a,
        b,
        id_a,
        map,
        cas,
        ..
    } = farm("map-wiring-repair").await;
    let bytes = blob_owned_by(&map, id_a, "away");
    let blob = digest(&bytes);

    // The second owner goes away; its address is kept for its return.
    let addr_b = b.addr;
    b.stop().await;

    let stored = cas.put(bytes.clone()).await.expect("stored with one copy");
    assert_eq!(stored, proto_digest(&blob));
    assert!(a.holds(&blob).await, "on the first owner");
    assert_eq!(a.owes(), [blob], "and owed to the second");

    // The second owner returns, with the files it had.
    let b = eventually("the second owner to start again", || async {
        try_node(&pki, &front, "node-b", &dir.join("b"), &addr_b.to_string())
            .await
            .ok()
    })
    .await;
    assert_eq!(b.addr, addr_b);
    eventually("the first owner's worker to make the copy", || async {
        (a.owes().is_empty() && b.holds(&blob).await).then_some(())
    })
    .await;
    assert_eq!(cas.get(&stored).await.expect("read back"), bytes);
}
