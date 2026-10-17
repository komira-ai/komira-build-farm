//! A farm for the chain tests: nodes that are real storage services over in-memory stores,
//! serving mutual TLS under certificates of role `storage` (a chain admits no other), each
//! behind a switch the test can cut, holding maps the test publishes.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_store::FaultFs;
use kbf_types::Digest;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{AbortHandle, JoinHandle};
use tonic::transport::Channel;

use crate::Storage;
use crate::repair::{Options, RepairLedger, RepairRecord, Repairs};
use crate::replicate::{Chain, ChainConfig, MapSource, Peers};
use crate::service::tests::fake::{Mem, patterned, serve_routes, sha};
use crate::service::tests::roles::Pki;
use crate::service::{Admission, Config, RemoteStorage, StorageService};

/// A TCP forwarder in front of a node: `cut` closes every connection through it and
/// drops new ones until `restore`, which is how a node dies and comes back.
pub(crate) struct Switch {
    pub addr: SocketAddr,
    up: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<AbortHandle>>>,
    _accept: JoinHandle<()>,
}

impl Switch {
    async fn start(target: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let up = Arc::new(AtomicBool::new(true));
        let connections: Arc<Mutex<Vec<AbortHandle>>> = Arc::default();
        let accept = {
            let (up, connections) = (Arc::clone(&up), Arc::clone(&connections));
            tokio::spawn(async move {
                loop {
                    let Ok((mut inbound, _)) = listener.accept().await else {
                        continue;
                    };
                    if !up.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Ok(mut outbound) = TcpStream::connect(target).await else {
                        continue;
                    };
                    let copy = tokio::spawn(async move {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                    connections.lock().expect("lock").push(copy.abort_handle());
                }
            })
        };
        Self {
            addr,
            up,
            connections,
            _accept: accept,
        }
    }

    /// The node is gone: its connections close, new ones are dropped.
    pub(crate) fn cut(&self) {
        self.up.store(false, Ordering::SeqCst);
        for connection in self.connections.lock().expect("lock").drain(..) {
            connection.abort();
        }
    }

    /// The node is back.
    pub(crate) fn restore(&self) {
        self.up.store(true, Ordering::SeqCst);
    }
}

/// What a node holds of the map: its member id and the map the test last gave it.
pub(crate) struct NodeMap {
    me: MemberId,
    map: Mutex<Option<Arc<Map>>>,
}

impl NodeMap {
    pub(crate) fn new(me: MemberId) -> Arc<Self> {
        Arc::new(Self {
            me,
            map: Mutex::new(None),
        })
    }

    pub(crate) fn set(&self, map: &Arc<Map>) {
        *self.map.lock().expect("lock") = Some(Arc::clone(map));
    }
}

impl MapSource for NodeMap {
    fn current(&self) -> Option<Arc<Map>> {
        self.map.lock().expect("lock").clone()
    }

    fn me(&self) -> Option<MemberId> {
        Some(self.me)
    }
}

/// What the frontend would be told: every call, and whether `owed` stays pending.
#[derive(Default)]
pub(crate) struct TestLedger {
    pub owed: Mutex<Vec<RepairRecord>>,
    pub settled: Mutex<Vec<Digest>>,
    /// `owed` records the call and then never returns.
    pub block: AtomicBool,
}

impl RepairLedger for TestLedger {
    fn owed<'a>(&'a self, record: &'a RepairRecord) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.owed.lock().expect("lock").push(*record);
            if self.block.load(Ordering::SeqCst) {
                futures::future::pending::<()>().await;
            }
        })
    }

    fn settled<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.settled.lock().expect("lock").push(*digest);
        })
    }
}

pub(crate) struct Node {
    pub id: MemberId,
    pub mem: Arc<Mem>,
    pub switch: Switch,
    pub held: Arc<NodeMap>,
    pub chain: Arc<Chain>,
    /// The disk of the node's repair journal.
    pub fs: FaultFs,
    pub repairs: Arc<Repairs>,
    pub ledger: Arc<TestLedger>,
}

/// Nodes numbered from 0; member ids are their numbers plus 1.
pub(crate) struct Farm {
    pub nodes: Vec<Node>,
    pki: Pki,
}

impl Drop for Farm {
    /// A test that fails while it holds a node's appends or commits must not leave the
    /// writer threads parked, which would hang the runtime's shutdown.
    fn drop(&mut self) {
        for node in &self.nodes {
            node.mem.knobs.hold_appends(false);
            node.mem.knobs.release_commits();
        }
    }
}

impl Farm {
    /// `n` nodes whose stores stream, each holding the generation-1 map with all of them
    /// active and R = 2.
    pub(crate) async fn start(n: usize, config: ChainConfig) -> Self {
        let pki = Pki::new();
        let mut nodes = Vec::new();
        for i in 0..n {
            let mem = Arc::new(Mem::default());
            mem.knobs.enabled.store(true, Ordering::SeqCst);
            let id = MemberId(i as u64 + 1);
            let held = NodeMap::new(id);
            let (server, calls_as) = pki.node("storage");
            let fs = FaultFs::new();
            let ledger = Arc::new(TestLedger::default());
            let repairs = Arc::new(
                Repairs::open(
                    fs.clone(),
                    Options::default(),
                    Arc::clone(&ledger) as Arc<dyn RepairLedger>,
                )
                .expect("a journal"),
            );
            let chain = Arc::new(Chain::new(
                Arc::clone(&held) as Arc<dyn MapSource>,
                Peers::tls(calls_as),
                Arc::clone(&repairs),
                config,
            ));
            let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
            let service = StorageService::new(
                storage,
                Admission::Certified,
                Config {
                    write_bytes_in_flight: 1 << 30,
                },
            )
            .with_refetch_wait(Duration::from_secs(5))
            .with_chain(Arc::clone(&chain))
            .expect("a certified service");
            let served = serve_routes(service.routes(), Some(server)).await;
            nodes.push(Node {
                id,
                mem,
                switch: Switch::start(served).await,
                held,
                chain,
                fs,
                repairs,
                ledger,
            });
        }
        let farm = Self { nodes, pki };
        farm.publish(&farm.map(1, 2, &[]));
        farm
    }

    /// A map of generation `generation` and replication `r` over every node, active but
    /// for the ones named in `states` (by node number).
    pub(crate) fn map(&self, generation: u64, r: u8, states: &[(usize, MemberState)]) -> Arc<Map> {
        let members = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| Member {
                id: node.id,
                addr: node.switch.addr.to_string(),
                zone: String::new(),
                weight: 1,
                state: states
                    .iter()
                    .find(|(n, _)| *n == i)
                    .map_or(MemberState::Active, |(_, s)| *s),
            })
            .collect();
        Arc::new(Map::new(generation, members, Rule::Rendezvous, r, 1, Vec::new()).expect("a map"))
    }

    /// Every node learns `map`.
    pub(crate) fn publish(&self, map: &Arc<Map>) {
        for node in 0..self.nodes.len() {
            self.publish_to(node, map);
        }
    }

    /// One node learns `map`.
    pub(crate) fn publish_to(&self, node: usize, map: &Arc<Map>) {
        self.nodes[node].held.set(map);
    }

    /// A client of node `node`, routing by `generation`.
    pub(crate) fn client(&self, node: usize, generation: u64) -> RemoteStorage {
        RemoteStorage::new(self.channel(node), generation)
    }

    /// The way a storage node dials its peers: as a node of role `storage`.
    pub(crate) fn peers(&self) -> Peers {
        Peers::tls(self.pki.node("storage").1)
    }

    /// A channel to node `node` as a frontend (the role that writes).
    pub(crate) fn channel(&self, node: usize) -> Channel {
        self.pki
            .lazy_client(self.nodes[node].switch.addr, "frontend")
    }

    /// A channel to node `node` as a caller of `role`.
    pub(crate) fn channel_role(&self, node: usize, role: &str) -> Channel {
        self.pki.lazy_client(self.nodes[node].switch.addr, role)
    }

    /// A channel to node `node` as another storage node (the role that sends a copy).
    pub(crate) fn storage_channel(&self, node: usize) -> Channel {
        self.pki
            .lazy_client(self.nodes[node].switch.addr, "storage")
    }

    /// [`Farm::channel`], or [`Farm::storage_channel`] for a call that is a copy (`leg`).
    pub(crate) fn channel_as(&self, node: usize, leg: Option<u64>) -> Channel {
        if leg.is_some() {
            self.storage_channel(node)
        } else {
            self.channel(node)
        }
    }

    /// The number of the node that is `digest`'s primary under `map`.
    pub(crate) fn primary_of(&self, map: &Map, digest: &Digest) -> usize {
        let owners = map.write_owners(digest);
        self.nodes
            .iter()
            .position(|n| n.id == owners[0])
            .expect("a node")
    }

    /// A blob of `len` bytes whose primary under `map` is node `primary`, and its
    /// replica is node `replica`; `salt` makes it a different blob.
    pub(crate) fn blob(
        &self,
        map: &Map,
        primary: usize,
        replica: usize,
        len: usize,
        salt: u8,
    ) -> Vec<u8> {
        for round in 0..10_000u32 {
            let mut bytes = patterned(len);
            bytes[..4].copy_from_slice(&round.to_le_bytes());
            bytes[4] = salt;
            let owners = map.write_owners(&sha(&bytes));
            if owners[0] == self.nodes[primary].id && owners[1] == self.nodes[replica].id {
                return bytes;
            }
        }
        panic!("no blob with that placement");
    }
}
