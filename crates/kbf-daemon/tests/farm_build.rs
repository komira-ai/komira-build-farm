//! A build through a real server and a real daemon over mutual TLS, with the storage map
//! pushed: the daemon fetches its inputs from and stores its outputs at the storage
//! owner, and the frontend carries the work and no blob bytes. The same build with no
//! map is the control: its blobs cross the frontend, and the counters show it.
//!
//! The counters are TCP relays in front of the frontend's worker listener, one for the
//! daemon's session (`--server`) and one for its CAS connection (`--cas`). They count
//! the bytes of the encrypted stream, which are at least the bytes of the blobs inside
//! it.

mod support;

use std::future::pending;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_daemon::cas::{Cas, CasError, fetch};
use kbf_daemon::{
    Daemon, DaemonConfig, FarmCas, MapHolder, NodeReport, Runtime, RuntimeError, Work,
    storage_nodes,
};
use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_mapsync::{FakeMapSource, MapSource};
use kbf_objstore::MemoryStore;
use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    Action, ActionResult, Command, Digest, Directory, ExecuteRequest, ExecuteResponse, FileNode,
};
use kbf_route::Liveness;
use kbf_server::{Listeners, WorkerTls, bind_server_with_map};
use kbf_transfer::{TransferConfig, Transfers, digest_of};
use prost::Message as _;
use support::storage_node::{Node, serve_tls};
use support::{Pki, pki};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tonic::transport::Endpoint;

/// How long the test waits for a hang to end: no assertion depends on it.
const HANG: Duration = Duration::from_secs(120);

/// The size of the input file and of the output: far more than a session's messages.
const INPUT_BYTES: usize = 1 << 20;
const OUTPUT_BYTES: usize = 512 << 10;

/// `len` bytes that do not repeat, so that nothing could squeeze them.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// A relay to `target` that counts the bytes both ways and the connections made.
struct Relay {
    addr: SocketAddr,
    bytes: Arc<AtomicU64>,
    connections: Arc<AtomicU64>,
}

impl Relay {
    async fn to(target: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let (bytes, connections) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (counted, made) = (Arc::clone(&bytes), Arc::clone(&connections));
        tokio::spawn(async move {
            loop {
                let (down, _) = listener.accept().await.expect("accept");
                made.fetch_add(1, Ordering::SeqCst);
                let up = TcpStream::connect(target).await.expect("connect");
                tokio::spawn(pipe(down, up, Arc::clone(&counted)));
            }
        });
        Self {
            addr,
            bytes,
            connections,
        }
    }

    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::SeqCst)
    }

    fn connections(&self) -> u64 {
        self.connections.load(Ordering::SeqCst)
    }
}

async fn pipe(a: TcpStream, b: TcpStream, bytes: Arc<AtomicU64>) {
    let (ar, aw) = a.into_split();
    let (br, bw) = b.into_split();
    tokio::join!(copy(ar, bw, &bytes), copy(br, aw, &bytes));
}

async fn copy(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, bytes: &AtomicU64) {
    let mut buffer = vec![0_u8; 64 << 10];
    while let Ok(n) = from.read(&mut buffer).await {
        if n == 0 || to.write_all(&buffer[..n]).await.is_err() {
            break;
        }
        bytes.fetch_add(n as u64, Ordering::SeqCst);
    }
    let _ = to.shutdown().await;
}

/// What the daemon's runtime does with a lease: reads the action, its command and its
/// input root, streams the input file into a scratch file, and stores an output, all
/// through the plane's lease.
struct Mover {
    cas: Arc<FarmCas>,
    scratch: std::path::PathBuf,
}

impl Runtime for Mover {
    fn driver(&self) -> &'static str {
        "fake"
    }

    fn serves(&self, kind: &str) -> bool {
        kind == "action"
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let failed = |e: CasError| RuntimeError::Failed(e.to_string());
        let lease = self.cas.lease(&work.lease_id);
        let action = fetch(&lease, &work.action_digest).await.map_err(failed)?;
        let action = Action::decode(action.as_slice()).expect("an action");
        let command = action.command_digest.expect("a command");
        fetch(&lease, &command).await.map_err(failed)?;
        let root = action.input_root_digest.expect("an input root");
        let root = fetch(&lease, &root).await.map_err(failed)?;
        let root = Directory::decode(root.as_slice()).expect("a directory");
        let input = root.files[0].digest.clone().expect("an input file");
        let mut file = tokio::fs::File::create(&self.scratch)
            .await
            .expect("create");
        lease.read_to(&input, &mut file).await.map_err(failed)?;
        let read = std::fs::read(&self.scratch).expect("read back");
        assert_eq!(read, noise(1, INPUT_BYTES), "the input arrived whole");
        let output = lease.put(noise(2, OUTPUT_BYTES)).await.map_err(failed)?;
        Ok(ActionResult {
            stdout_digest: Some(output),
            ..ActionResult::default()
        })
    }

    async fn kill(&self, _: kbf_types::LeaseId) {}
}

fn map(owner: &Node) -> Map {
    let member = Member {
        id: MemberId(1),
        addr: owner.name(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    Map::new(2, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("map")
}

/// What one build counted.
struct Counted {
    session_bytes: u64,
    cas_bytes: u64,
    cas_connections: u64,
    result: ActionResult,
}

/// Runs the build against a server that pushes `owner`'s map (or none), and a daemon
/// that moves blobs as the plane decides.
async fn build(pki: &Pki, owner: &Node, pushes_map: bool, scratch: &std::path::Path) -> Counted {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    let listeners = Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: Some(WorkerTls {
            server: pki.worker_tls(),
            deny_list: None,
        }),
        heartbeat_interval: Duration::from_millis(100),
        hello_wait: Duration::from_secs(2),
        tick: Duration::from_millis(50),
        unservable_wait: Duration::from_secs(300),
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: Duration::from_secs(60),
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    };
    let source: Option<Arc<dyn MapSource>> =
        pushes_map.then(|| Arc::new(FakeMapSource::new(map(owner))) as Arc<dyn MapSource>);
    let cache = Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory());
    let bound = bind_server_with_map(
        cache,
        listeners,
        None,
        kbf_auth::Policy::allow_all(),
        None,
        source,
        pending(),
    )
    .expect("bind");
    let (reapi, worker) = (bound.reapi, bound.worker);
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    let (session, cas_relay) = (Relay::to(worker).await, Relay::to(worker).await);

    // The client uploads the build's blobs to the frontend, and the owner holds them as
    // a farm whose frontend wrote through to its storage nodes would.
    let client = Endpoint::from_shared(format!("http://{reapi}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let upload = Transfers::over(client.clone(), TransferConfig::default());
    let input = noise(1, INPUT_BYTES);
    let root = Directory {
        files: vec![FileNode {
            name: "input".to_owned(),
            digest: Some(digest_of(&input)),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let command = Command {
        arguments: vec!["move".to_owned()],
        ..Command::default()
    };
    let mut blobs = vec![input, root.encode_to_vec(), command.encode_to_vec()];
    let action = |blobs: &[Vec<u8>]| Action {
        command_digest: Some(digest_of(&blobs[2])),
        input_root_digest: Some(digest_of(&blobs[1])),
        ..Action::default()
    };
    blobs.push(action(&blobs).encode_to_vec());
    for blob in &blobs {
        use kbf_daemon::cas::Cas as _;
        upload.put(blob.clone()).await.expect("upload");
        owner.store.hold(blob).await;
    }
    let action_digest: Digest = digest_of(&blobs[3]);
    // This frontend is `kbf-server` over one cache, not `kbf frontend` (which asks the
    // owners of a map, `kbf_front::Routed`): it accepts a result only if its outputs are
    // in its own store. So the output is put there ahead, by the client's connection and
    // not the daemon's, and not at the owner: the owner has it only if the daemon stores
    // it there.
    upload.put(noise(2, OUTPUT_BYTES)).await.expect("upload");

    // The daemon: its session and its CAS connection both through the relays.
    let mut config = DaemonConfig::new(
        format!("https://127.0.0.1:{}", session.addr.port()),
        pki.client.clone(),
        "node-1".to_owned(),
    );
    config.reconnect_after = Duration::from_millis(100);
    let endpoint = Endpoint::from_shared(format!("https://127.0.0.1:{}", cas_relay.addr.port()))
        .expect("endpoint")
        .tls_config(pki.client.load().expect("TLS files"))
        .expect("TLS");
    let holder = Arc::new(MapHolder::new());
    let plane = Arc::new(FarmCas::new(
        Arc::new(Transfers::new(endpoint, TransferConfig::default())),
        Arc::clone(&holder),
        storage_nodes(&pki.client, Liveness::default()).expect("TLS files"),
        &TransferConfig::default(),
    ));
    let runtime = Arc::new(Mover {
        cas: plane,
        scratch: scratch.to_owned(),
    });
    let report = NodeReport::detect(&[runtime.driver()]).expect("detect this node");
    let daemon = Daemon::new(config, runtime, report)
        .expect("daemon config")
        .with_map(Arc::clone(&holder));
    tokio::spawn(daemon.run(pending()));
    if pushes_map {
        let mut changes = holder.changes();
        timeout(HANG, changes.wait_for(|held| *held >= 2))
            .await
            .expect("the map arrives")
            .expect("open");
    }

    let result = timeout(HANG, execute(client, action_digest))
        .await
        .expect("the build ends");
    Counted {
        session_bytes: session.bytes(),
        cas_bytes: cas_relay.bytes(),
        cas_connections: cas_relay.connections(),
        result,
    }
}

/// Executes `action` until its operation is done; returns its result.
async fn execute(reapi: tonic::transport::Channel, action: Digest) -> ActionResult {
    let mut ops = ExecutionClient::new(reapi)
        .execute(ExecuteRequest {
            action_digest: Some(action),
            skip_cache_lookup: true,
            ..ExecuteRequest::default()
        })
        .await
        .expect("Execute")
        .into_inner();
    let done = loop {
        let op = ops.message().await.expect("stream").expect("an update");
        if op.done {
            break op;
        }
    };
    let Some(operation::Result::Response(any)) = done.result else {
        panic!("no response: {done:?}");
    };
    let response = ExecuteResponse::decode(any.value.as_slice()).expect("response");
    let status = response.status.clone();
    response
        .result
        .unwrap_or_else(|| panic!("no result: {status:?}"))
}

/// Catches blob bytes crossing the frontend in map mode. The input (1 MiB) is fetched
/// from the owner and the output (512 KiB) stored at it, so the daemon's CAS connection
/// to the frontend is never made, and the session between them, which carries the lease
/// and its result, carries far less than one blob. Mutant: the plane always on the
/// frontend's side (the map ignored) puts the blobs on the CAS connection and fails here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_a_map_the_frontend_carries_no_blob_bytes() {
    let pki = pki("farm-build-map");
    let owner = serve_tls(&pki);
    let dir = support::scratch("farm-build-map");
    let counted = build(&pki, &owner, true, &dir.join("input")).await;

    assert_eq!(counted.result.exit_code, 0);
    assert_eq!(
        (counted.cas_connections, counted.cas_bytes),
        (0, 0),
        "the daemon never used the frontend's CAS"
    );
    assert!(
        counted.session_bytes < (OUTPUT_BYTES / 4) as u64,
        "the session carried {} bytes: more than control messages",
        counted.session_bytes
    );
    let output = counted.result.stdout_digest.expect("an output");
    assert_eq!(output, digest_of(&noise(2, OUTPUT_BYTES)));
    assert!(
        owner.store.holds(&output).await,
        "the output is at its owner"
    );
    assert!(
        owner.store.reads() >= 3,
        "the inputs were read from the owner"
    );
}

/// The control for the test above, which could otherwise pass because nothing is
/// counted: the same build against a server that pushes no map moves the input over the
/// daemon's CAS connection, and the owner is not read. (The output is already in the
/// frontend, as the build's set-up leaves it, so the daemon's FindMissing skips its
/// upload: the input alone is counted.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_map_the_blobs_cross_the_frontend() {
    let pki = pki("farm-build-nomap");
    let owner = serve_tls(&pki);
    let dir = support::scratch("farm-build-nomap");
    let counted = build(&pki, &owner, false, &dir.join("input")).await;

    assert_eq!(counted.result.exit_code, 0);
    assert!(counted.cas_connections >= 1);
    assert!(
        counted.cas_bytes >= INPUT_BYTES as u64,
        "the CAS connection carried {} bytes",
        counted.cas_bytes
    );
    assert!(counted.session_bytes < (OUTPUT_BYTES / 4) as u64);
    assert_eq!(owner.store.reads(), 0, "no map, so the owner is not asked");
}
