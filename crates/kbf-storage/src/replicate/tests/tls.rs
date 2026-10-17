//! A chain over mutual TLS: the replica admits a copy only from a certificate whose role is
//! `storage`.

use std::sync::Arc;

use bytes::Bytes;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_store::FaultFs;

use super::farm::NodeMap;
use crate::repair::{NoLedger, Options, Repairs};
use crate::replicate::{Chain, ChainConfig, MapSource, Peers};
use crate::service::tests::fake::{Mem, serve_routes, sha};
use crate::service::tests::roles::Pki;
use crate::service::{Admission, Config, RemoteStorage, StorageService};
use crate::{Storage, StorageError};

struct Pair {
    mems: [Arc<Mem>; 2],
    owing: Vec<Arc<Repairs>>,
    addrs: [std::net::SocketAddr; 2],
}

/// Two nodes serving under certificates of role `storage`; the first calls its peer with a
/// certificate of role `caller_role`.
async fn pair(pki: &Pki, caller_role: &str) -> Pair {
    let mems = [Arc::new(Mem::default()), Arc::new(Mem::default())];
    let maps = [NodeMap::new(MemberId(1)), NodeMap::new(MemberId(2))];
    let mut addrs = Vec::new();
    let mut owing = Vec::new();
    for i in 0..2 {
        mems[i]
            .knobs
            .enabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (server, _) = pki.node("storage");
        let (_, calls_as) = pki.node(if i == 0 { caller_role } else { "storage" });
        let repairs = Arc::new(
            Repairs::open(FaultFs::new(), Options::default(), Arc::new(NoLedger))
                .expect("a journal"),
        );
        owing.push(Arc::clone(&repairs));
        let chain = Chain::new(
            Arc::clone(&maps[i]) as Arc<dyn MapSource>,
            Peers::tls(calls_as),
            repairs,
            ChainConfig::default(),
        );
        let storage: Arc<dyn Storage> = Arc::clone(&mems[i]) as Arc<dyn Storage>;
        let service = StorageService::new(
            storage,
            Admission::Certified,
            Config {
                write_bytes_in_flight: 1 << 30,
            },
        )
        .with_chain(Arc::new(chain))
        .expect("a certified service");
        addrs.push(serve_routes(service.routes(), Some(server)).await);
    }
    let members = addrs
        .iter()
        .enumerate()
        .map(|(i, addr)| Member {
            id: MemberId(i as u64 + 1),
            addr: addr.to_string(),
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
        })
        .collect();
    let map = Arc::new(Map::new(1, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("map"));
    for held in &maps {
        held.set(&map);
    }
    Pair {
        mems,
        owing,
        addrs: [addrs[0], addrs[1]],
    }
}

/// A payload whose primary is the first node.
fn payload(pair: &Pair) -> Vec<u8> {
    let members = [MemberId(1), MemberId(2)];
    let member = |i: usize| Member {
        id: members[i],
        addr: pair.addrs[i].to_string(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let map = Map::new(
        1,
        vec![member(0), member(1)],
        Rule::Rendezvous,
        2,
        1,
        Vec::new(),
    )
    .expect("map");
    (0u32..)
        .map(|n| n.to_le_bytes().repeat(100))
        .find(|bytes| map.write_owners(&sha(bytes))[0] == members[0])
        .expect("a payload")
}

/// Catches a storage-to-storage call that does not authenticate as a storage node: the
/// primary's copy reaches the replica with the primary's certificate (role `storage`,
/// clientAuth and serverAuth on the one certificate) and is admitted, so both nodes hold the
/// blob; the writer is a frontend. With the primary calling its peer under a `daemon`
/// certificate, the replica refuses the copy (a daemon may not send one), the write fails
/// and neither node holds the blob. Mutants: `WriteLeg` open to every role; the leg sent
/// without the primary's certificate.
#[tokio::test]
async fn a_copy_is_admitted_only_from_a_storage_certificate() {
    let pki = Pki::new();
    let storage = pair(&pki, "storage").await;
    let bytes = payload(&storage);
    let channel = pki.client(storage.addrs[0], "frontend").await;
    let digest = RemoteStorage::new(channel, 1)
        .put(Bytes::from(bytes.clone()))
        .await
        .expect("a frontend writes through the chain");
    assert_eq!(digest, sha(&bytes));
    for mem in &storage.mems {
        assert!(mem.blobs.lock().expect("lock").contains_key(&digest));
    }

    let daemon = pair(&pki, "daemon").await;
    let channel = pki.client(daemon.addrs[0], "frontend").await;
    let answer = RemoteStorage::new(channel, 1).put(Bytes::from(bytes)).await;
    assert!(
        matches!(answer, Err(StorageError::Invalid(_))),
        "{answer:?}"
    );
    for mem in &daemon.mems {
        assert!(mem.blobs.lock().expect("lock").is_empty());
    }
    // A refusal is not a replica that cannot be reached: nothing is owed.
    assert!(daemon.owing[0].open_records().is_empty());
}
