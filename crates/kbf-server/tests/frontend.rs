//! `kbf frontend` (`kbf_server::frontend`): its flags, Execute and the scheduler over a
//! storage node (nothing in the frontend's process holds a blob), `/healthz` and
//! `/readyz`, the state-store probe, and the mutual TLS toward a storage node: the
//! frontend presents a certificate that names its role and verifies the node's against
//! its CA and the host in the `--storage` URL (docs/design/architecture-v2.md, section 9),
//! over a real node (`kbf_server::storage_node`), the proxy's raw HTTP/2 path (ByteStream)
//! and its tonic path (FindMissing, the action cache).

mod support;

#[path = "frontend/handover.rs"]
mod handover;

use std::future::pending;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use std::path::{Path, PathBuf};

use clap::CommandFactory as _;
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_proto::grpc::health::v1::HealthCheckRequest;
use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
use kbf_proto::grpc::health::v1::health_client::HealthClient;
use kbf_server::frontend::{self, FrontendFlags, FrontendHandle, start_frontend};
use kbf_server::health::{MapProbe, Probe, StateProbe, StorageProbe, router_of};
use kbf_server::{BoundStorage, Readiness, StorageListener, bind_storage};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Revision, ScanPage, ScanRange, Store, StoreError,
    StoreTime, Txn, Versioned,
};
use kbf_storage::Storage;
use serde_json::Value;
use support::{Client, Job, PROMPT, done, ran, response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tonic::transport::{Channel, Endpoint};

/// The flags of `kbf frontend` for a start on loopback ports, and these more.
pub fn flags(more: &[&str]) -> Result<FrontendFlags, clap::Error> {
    let mut argv = vec!["frontend"];
    for (flag, default) in [
        ("--listen", "127.0.0.1:0"),
        ("--worker-listen", "127.0.0.1:0"),
        ("--api-listen", "127.0.0.1:0"),
    ] {
        if !more
            .iter()
            .any(|m| m.starts_with(flag) && m[flag.len()..].starts_with('='))
        {
            argv.push(Box::leak(format!("{flag}={default}").into_boxed_str()));
        }
    }
    argv.extend_from_slice(more);
    frontend::parse(&frontend::command().try_get_matches_from(argv)?)
}

/// A plain-text storage node over an in-memory cache, stopped by dropping `stop`.
struct Node {
    addr: SocketAddr,
    storage: Arc<dyn Storage>,
    stop: Option<oneshot::Sender<()>>,
}

impl Node {
    async fn start() -> Self {
        let cache = Arc::new(Cache::memory());
        let storage: Arc<dyn Storage> = Arc::new(CacheStorage::new(cache, SegmentsConfig::DEFAULT));
        let (stop, stopped) = oneshot::channel::<()>();
        let BoundStorage { addr, serving } = bind_storage(
            Arc::clone(&storage),
            StorageListener {
                listen: "127.0.0.1:0".parse().unwrap(),
                tls: None,
                service: kbf_storage::service::Config {
                    write_bytes_in_flight: 64 << 20,
                },
            },
            async move {
                let _ = stopped.await;
            },
        )
        .expect("bind the node");
        tokio::spawn(serving);
        Self {
            addr,
            storage,
            stop: Some(stop),
        }
    }

    /// Stops the node and waits until it no longer accepts.
    async fn stop(&mut self) {
        drop(self.stop.take());
        for _ in 0..100 {
            if TcpStream::connect(self.addr).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the node still accepts");
    }
}

/// A started frontend, serving until `stop` is dropped.
struct Front {
    reapi: SocketAddr,
    worker: SocketAddr,
    api: SocketAddr,
    _stop: oneshot::Sender<()>,
}

impl Front {
    async fn start(flags: &FrontendFlags) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let handle: FrontendHandle = start_frontend(flags, || {
            Ok(async move {
                let _ = stopped.await;
            })
        })
        .expect("starts");
        let (reapi, worker, api) = (handle.reapi, handle.worker, handle.api);
        tokio::spawn(handle.serve());
        Self {
            reapi,
            worker,
            api: api.expect("the operator API"),
            _stop: stop,
        }
    }

    async fn client(&self) -> Client {
        Client::connect(self.reapi, self.worker).await
    }
}

async fn get(api: SocketAddr, path: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// The names of the checks `/readyz` says fail.
fn failing(body: &Value) -> Vec<&str> {
    body["failing"]
        .as_array()
        .expect("a failing list")
        .iter()
        .map(|f| f["check"].as_str().expect("a check"))
        .collect()
}

// ---------------------------------------------------------------- the flags

/// Catches a frontend that opens or takes a store of its own, or any flag a storage node
/// owns: every flag of `kbf-server` outside [`frontend::ALLOWED`] and the thread flags is
/// refused by name with exit code 2 (so one added to `kbf-server` later is refused with
/// no edit), the ones that matter most are refused with the reason, `--storage` is
/// required, and every id the allowlist names is a flag of `kbf-server` (a renamed flag
/// would otherwise drop out of the list unnoticed).
#[test]
fn a_frontend_refuses_every_flag_that_is_a_storage_nodes() {
    let server = kbf_server::Args::command();
    for id in frontend::ALLOWED {
        assert!(server.get_arguments().any(|a| a.get_id() == *id), "{id}");
    }
    let mut refused = 0;
    for arg in server.get_arguments() {
        let id = arg.get_id().as_str();
        let taken = frontend::ALLOWED.contains(&id)
            || ["async_threads", "blocking_threads", "cpu_threads"].contains(&id);
        if taken || id == "help" || id == "version" {
            continue;
        }
        let long = arg.get_long().expect("a long flag");
        let e = flags(&["--storage=http://127.0.0.1:1", &format!("--{long}=x")]).expect_err(long);
        assert_eq!(e.exit_code(), 2, "{long}");
        refused += 1;
    }
    assert!(refused > 15, "{refused}");
    for flag in [
        "--cold-store=file:///x",
        "--hot-store=file:///x",
        "--meta-dir=/x",
        "--segment-upload-idle-secs=5",
        "--input-tree-cache-entries=5",
        "--role=all",
        "--invocations-open",
    ] {
        let e = flags(&["--storage=http://127.0.0.1:1", flag]).expect_err(flag);
        let name = flag.split('=').next().unwrap_or(flag);
        assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict, "{flag}");
        assert!(
            e.to_string()
                .contains(&format!("kbf frontend refuses {name}: ")),
            "{e}"
        );
    }
    let missing = flags(&[]).expect_err("--storage is required");
    assert!(missing.to_string().contains("--storage"), "{missing}");
    let help = frontend::command().render_long_help().to_string();
    for shown in [
        "--storage",
        "--storage-client-cert",
        "--storage-ca",
        "--state-store",
        "--worker-tls-cert",
        "--api-listen",
    ] {
        assert!(help.contains(shown), "{shown}");
    }
    for gone in [
        "--cold-store",
        "--hot-store",
        "--write-",
        "--segment-upload",
    ] {
        assert!(!help.contains(gone), "{gone} in:\n{help}");
    }
}

/// Catches a frontend that dials storage in plain text off loopback (the node refuses
/// it, so the start would only find out at the first call), mutual TLS to an `http://`
/// URL or an `https://` one without the three certificate flags, the certificate flags
/// taken one or two at a time, and a loopback or `localhost` plain-text URL refused.
#[test]
fn the_storage_url_and_the_tls_flags_must_agree() {
    let tls = [
        "--storage-client-cert=/c",
        "--storage-client-key=/k",
        "--storage-ca=/ca",
    ];
    let with = |url: &str, tls: &[&str]| {
        let mut more = vec![format!("--storage={url}")];
        more.extend(tls.iter().map(|t| (*t).to_owned()));
        let more: Vec<&str> = more.iter().map(String::as_str).collect();
        flags(&more)
    };
    for ok in [
        "http://127.0.0.1:8982",
        "http://localhost:8982",
        "http://[::1]:8982",
    ] {
        assert!(with(ok, &[]).is_ok(), "{ok}");
    }
    assert!(with("https://storage.example:8982", &tls).is_ok());
    let said = |url: &str, tls: &[&str]| with(url, tls).expect_err(url).to_string();
    let plain = said("http://storage.example:8982", &[]);
    assert!(
        plain.contains("not loopback") && plain.contains("--storage-ca"),
        "{plain}"
    );
    assert!(said("http://192.0.2.10:8982", &[]).contains("not loopback"));
    for (url, flags) in [
        ("https://storage.example:8982", &tls[..0]),
        ("http://127.0.0.1:8982", &tls[..]),
    ] {
        let e = said(url, flags);
        assert!(e.contains("the TLS flags disagree"), "{url}: {e}");
    }
    for partial in [&tls[..1], &tls[1..2], &tls[2..], &tls[..2]] {
        assert!(
            with("https://storage.example:8982", partial).is_err(),
            "{partial:?}"
        );
    }
    let bad = said("not a url", &[]);
    assert!(bad.contains("--storage \"not a url\": "), "{bad}");
}

// ----------------------------------------------- Execute over a storage node

/// Catches Execute and the scheduler still reading a cache of the frontend's own (which
/// holds nothing here): a job whose Action, Command and inputs are on the storage node
/// runs on a daemon, the accepted result's action-cache entry lands on the node (the
/// frontend wrote it with both halves of the write), the same Execute is then answered
/// from the node's action cache without a daemon, and a job whose input is not on the
/// node is refused with the missing digest (the input check read the node's
/// directories and asked it which files are missing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_reads_and_writes_the_storage_node() {
    let node = Node::start().await;
    let url = format!("--storage=http://{}", node.addr);
    let front = Front::start(&flags(&[&url, "--heartbeat-interval-ms=100"]).expect("flags")).await;
    let client = front.client().await;
    let mut daemon = client.daemon("node-a", 4, 8).await;

    let job = Job::new("build", &[]);
    client.upload(&job.blobs()).await;
    assert!(
        node.storage
            .find_missing(&[job.action.digest])
            .await
            .expect("node")
            .is_empty(),
        "the upload went to the node"
    );
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;
    let result = support::output(&client, "built over storage", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result.clone()));
    assert!(!response(&last).cached_result);
    let (_, stored) = node
        .storage
        .action_result(&job.action.digest)
        .await
        .expect("node")
        .expect("the entry is on the node");
    assert_eq!(support::unstamped(stored), result);

    let mut again = client.execute(&job.action).await;
    let answer = response(&done(&mut again).await);
    assert!(
        answer.cached_result,
        "answered from the node's action cache"
    );
    daemon.no_work().await;

    let lonely = Job::new("lonely", &[]);
    client
        .upload(&[&lonely.action, &lonely.command, &lonely.root])
        .await;
    let refused = client
        .execute_with(&lonely.action, false)
        .await
        .map(|_| ())
        .expect_err("the input is not on the node");
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(
        String::from_utf8_lossy(refused.details()).contains(&lonely.input.digest.hash_hex()),
        "{refused:?}"
    );
    drop(front);
}

// ------------------------------------------------------------------- health

/// Catches `/readyz` and the health service ignoring the storage node: a frontend whose
/// node is not there answers 503 naming the `storage` check (and the health service
/// NOT_SERVING), one whose node answers answers 200, and it flips to 503 when the node
/// goes away and back when a node is there again; `/healthz` stays 200 throughout (the
/// process is alive).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readyz_follows_the_storage_node() {
    let mut node = Node::start().await;
    let url = format!("--storage=http://{}", node.addr);
    let front =
        Front::start(&flags(&[&url, "--readyz-store-timeout-ms=500"]).expect("flags")).await;
    let mut health = HealthClient::new(
        Channel::from_shared(format!("http://{}", front.reapi))
            .unwrap()
            .connect()
            .await
            .expect("connect"),
    );
    let serving = |status: ServingStatus| status as i32;
    let ask = |health: &mut HealthClient<Channel>| {
        let mut health = health.clone();
        async move {
            health
                .check(HealthCheckRequest::default())
                .await
                .expect("check")
                .into_inner()
                .status
        }
    };

    let (code, body) = get(front.api, "/readyz").await;
    assert_eq!((code, body["ready"].as_bool()), (200, Some(true)), "{body}");
    assert_eq!(ask(&mut health).await, serving(ServingStatus::Serving));

    node.stop().await;
    let (code, body) = get(front.api, "/readyz").await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(failing(&body), ["storage"], "{body}");
    assert!(
        body["failing"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("storage node")
    );
    assert_eq!(ask(&mut health).await, serving(ServingStatus::NotServing));
    let (code, alive) = get(front.api, "/healthz").await;
    assert_eq!((code, alive["status"].as_str()), (200, Some("alive")));
}

/// Catches a storage node that accepts the connection and never answers holding
/// `/readyz` open (the probe has the `--readyz-store-timeout-ms` bound): the answer is
/// 503 with the wait named, within a small multiple of the timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_node_that_never_answers_is_not_ready() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let url = format!("--storage=http://{addr}");
    let front =
        Front::start(&flags(&[&url, "--readyz-store-timeout-ms=200"]).expect("flags")).await;
    let started = std::time::Instant::now();
    let (code, body) = get(front.api, "/readyz").await;
    assert_eq!(code, 503, "{body}");
    assert!(
        body["failing"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("no answer within 200 ms"),
        "{body}"
    );
    assert!(started.elapsed() < PROMPT);
}

/// Catches a stop signal that leaves the health service SERVING (a front's check would
/// keep sending work to a frontend that is shutting down): an open Watch is sent
/// NOT_SERVING and then ended UNAVAILABLE when the shutdown future completes, the
/// serving future then ends, and the start line has what a supervisor reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopping_frontend_is_not_serving() {
    let node = Node::start().await;
    let url = format!("--storage=http://{}", node.addr);
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = start_frontend(&flags(&[&url]).expect("flags"), || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .expect("starts");
    let want = format!(
        "kbf-frontend {} reapi={} worker={} api={} storage=http://{}/",
        kbf_server::SERVER_VERSION,
        handle.reapi,
        handle.worker,
        handle.api.unwrap(),
        node.addr
    );
    assert_eq!(handle.start_line(), want);
    let reapi = handle.reapi;
    let served = tokio::spawn(handle.serve());
    let mut health = HealthClient::new(
        Channel::from_shared(format!("http://{reapi}"))
            .unwrap()
            .connect()
            .await
            .expect("connect"),
    );
    let mut watch = health
        .watch(HealthCheckRequest::default())
        .await
        .expect("watch")
        .into_inner();
    async fn next(
        watch: &mut tonic::Streaming<kbf_proto::grpc::health::v1::HealthCheckResponse>,
    ) -> Result<Option<kbf_proto::grpc::health::v1::HealthCheckResponse>, tonic::Status> {
        tokio::time::timeout(PROMPT, watch.message())
            .await
            .expect("in time")
    }
    let first = next(&mut watch).await.unwrap().unwrap();
    assert_eq!(first.status(), ServingStatus::Serving);
    stop.send(()).unwrap();
    let second = next(&mut watch).await.unwrap().unwrap();
    assert_eq!(second.status(), ServingStatus::NotServing);
    assert_eq!(
        next(&mut watch).await.unwrap_err().code(),
        tonic::Code::Unavailable
    );
    tokio::time::timeout(Duration::from_secs(30), served)
        .await
        .expect("serves to the end")
        .unwrap()
        .expect("served");
}

/// How the planted state store answers a read.
enum Answers {
    Absent,
    Fails,
    /// Never answers until the test drops the sender: a read that does not come back,
    /// whatever the machine's load.
    Hangs(std::sync::Mutex<std::sync::mpsc::Receiver<()>>),
    Panics,
}

/// A state store whose only working call is `get`.
struct PlantedStore(Answers);

impl Store for PlantedStore {
    fn apply_batch(&self, _: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Err(StoreError::Failed("planted".to_owned()))
    }

    fn get(&self, _: &str) -> Result<Option<Versioned>, StoreError> {
        match self.0 {
            Answers::Absent => Ok(None),
            Answers::Fails => Err(StoreError::Failed("disk gone".to_owned())),
            Answers::Hangs(ref release) => {
                // Err once the test drops the sender; the probe has given up by then.
                let _ = release.lock().unwrap().recv();
                Ok(None)
            }
            Answers::Panics => panic!("planted panic in a state store read"),
        }
    }

    fn scan_prefix(&self, _: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        Err(StoreError::Failed("planted".to_owned()))
    }

    fn scan_range(&self, _: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        Err(StoreError::Failed("planted".to_owned()))
    }

    fn changes(&self, _: Revision, _: usize) -> Result<Changes, StoreError> {
        Err(StoreError::Failed("planted".to_owned()))
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        Ok(0)
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats::default()
    }

    fn now(&self) -> StoreTime {
        StoreTime::ZERO
    }
}

/// Catches `/readyz` that does not ask a configured state store: an absent key is an
/// answer, a failing store fails the `state_store` check with the store's reason, and
/// one that does not answer within the timeout fails it naming the wait, and one that
/// panics in the read fails it too; the checks come
/// in the order the route lists them, and the map hook is true.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readyz_asks_the_state_store() {
    // Only the hanging store meets the probe's timeout: it never answers, so the check
    // fails at the timeout however loaded the machine is. Every other case gets a
    // deadline that only a hang reaches (no assertion depends on speed).
    let hang_only = Duration::from_secs(60);
    let probe = |answers, timeout| StateProbe::new(Arc::new(PlantedStore(answers)), timeout);
    assert_eq!(probe(Answers::Absent, hang_only).probe().await, Ok(()));
    let failed = probe(Answers::Fails, hang_only)
        .probe()
        .await
        .expect_err("fails");
    assert!(failed.contains("disk gone"), "{failed}");
    let (release, held) = std::sync::mpsc::channel::<()>();
    let hung = Answers::Hangs(std::sync::Mutex::new(held));
    let slow = probe(hung, Duration::from_millis(100))
        .probe()
        .await
        .expect_err("times out");
    assert!(slow.contains("no answer within 100 ms"), "{slow}");
    drop(release);
    let crashed = probe(Answers::Panics, hang_only)
        .probe()
        .await
        .expect_err("panics");
    assert!(crashed.contains("did not finish"), "{crashed}");
    assert_eq!(MapProbe.probe().await, Ok(()));

    let readiness = Arc::new(Readiness::default());
    let node = Node::start().await;
    let checks = frontend::checks(
        Arc::clone(&readiness),
        Arc::clone(&node.storage),
        None,
        Some(Arc::new(PlantedStore(Answers::Fails))),
        hang_only,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let api = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router_of(Arc::new(checks)))
            .await
            .unwrap();
    });
    let (code, body) = get(api, "/readyz").await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(failing(&body), ["state_store"], "{body}");
    readiness.stop();
    let (_, body) = get(api, "/readyz").await;
    assert_eq!(failing(&body), ["stopping", "state_store"], "{body}");
}

/// A map source with one fixed map, and the connections to the members of it.
struct Dialled {
    map: Arc<kbf_map::Map>,
    channels: std::collections::BTreeMap<kbf_map::MemberId, Channel>,
}

impl kbf_route::MapSource for Dialled {
    fn current(&self) -> Option<kbf_route::MapView> {
        Some(kbf_route::MapView {
            map: Arc::clone(&self.map),
            previous: None,
        })
    }

    fn refetch(
        &self,
        at_least: u64,
    ) -> futures::future::BoxFuture<'_, Result<kbf_route::MapView, kbf_route::RefetchError>> {
        Box::pin(async move {
            Err(kbf_route::RefetchError {
                at_least,
                reason: "nothing newer".to_owned(),
            })
        })
    }
}

impl kbf_route::Nodes for Dialled {
    fn node(
        &self,
        member: &kbf_map::Member,
        generation: u64,
    ) -> Result<Arc<dyn Storage>, kbf_storage::StorageError> {
        Ok(Arc::new(kbf_storage::service::RemoteStorage::new(
            kbf_front::Channels::channel(self, member)?,
            generation,
        )))
    }
}

impl kbf_front::Channels for Dialled {
    fn channel(&self, member: &kbf_map::Member) -> Result<Channel, kbf_storage::StorageError> {
        Ok(self.channels[&member.id].clone())
    }
}

fn lazy(addr: SocketAddr) -> Channel {
    tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_lazy()
}

/// Catches a readiness check that asks the `--storage` node when the frontend routes by
/// the map: a farm whose `--storage` node is gone but whose other member answers is ready;
/// one whose members all fail is not; and while the map has had only one member, the
/// `--storage` node is the check, whatever the member is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_check_asks_the_owners_once_the_frontend_routes_by_the_map() {
    use kbf_map::{Map, Member, MemberId, MemberState, Rule};
    let live = Node::start().await;
    let nowhere: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let dead_storage: Arc<dyn Storage> =
        Arc::new(kbf_storage::service::RemoteStorage::new(lazy(nowhere), 0));
    let member = |id| Member {
        id: MemberId(id),
        addr: format!("127.0.0.1:{id}"),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let routed_over = |ids: &[u64], addrs: &[SocketAddr]| {
        let members = ids.iter().map(|id| member(*id)).collect();
        let map = Map::new(
            1,
            members,
            Rule::Rendezvous,
            u8::try_from(ids.len().min(2)).unwrap(),
            1,
            Vec::new(),
        )
        .expect("a map");
        let dialled = Arc::new(Dialled {
            map: Arc::new(map),
            channels: ids
                .iter()
                .zip(addrs)
                .map(|(id, a)| (MemberId(*id), lazy(*a)))
                .collect(),
        });
        Arc::new(kbf_front::Routed::new(
            dialled.clone(),
            dialled.clone(),
            dialled,
        ))
    };
    let probe = |routed| {
        StorageProbe::new(Arc::clone(&dead_storage), Duration::from_secs(5)).routed(routed)
    };

    // Two members, one of them gone: the other answers, and `--storage` is not asked.
    let one_gone = routed_over(&[1, 2], &[nowhere, live.addr]);
    assert_eq!(probe(one_gone).probe().await, Ok(()));
    // Two members, both gone.
    let all_gone = routed_over(&[1, 2], &[nowhere, nowhere]);
    let failed = probe(all_gone).probe().await.expect_err("nobody answers");
    assert!(failed.contains("did not answer"), "{failed}");
    // One member: the `--storage` node is the check, and it is gone.
    let alone = routed_over(&[1], &[live.addr]);
    let failed = probe(alone).probe().await.expect_err("the --storage node");
    assert!(failed.contains("did not answer"), "{failed}");
}

/// Catches a `--state-store` that is not SQLite reaching a start (or echoing its value, a
/// `postgres://` URL holds a password): it is refused when the flags are read, with the
/// fixed text, before anything is bound.
#[test]
fn a_state_store_that_is_not_sqlite_is_refused() {
    for value in [
        "postgres://user:hunter2@db.example/kbf",
        "file:///var/lib/kbf",
        "sqlite://relative.db",
    ] {
        let refused = flags(&[
            "--storage=http://127.0.0.1:1",
            &format!("--state-store={value}"),
        ])
        .expect_err("refused");
        let said = refused.to_string();
        assert!(said.contains("--state-store: "), "{said}");
        assert!(
            said.contains("only sqlite:///") || said.contains("needs an absolute file path"),
            "{said}"
        );
        assert!(!said.contains("hunter2"), "{said}");
    }
}

/// A fresh directory under the test target directory, for one test.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("frontend-state")
        .join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("directory");
    dir
}

/// Catches `--state-store` accepted and ignored by a frontend (the scheduler would run
/// in memory while the operator believes its state is kept): a start over a SQLite path
/// makes the file and serves, `/readyz` is ready with the store probed, a second start
/// over the file the first holds is refused before anything is bound, and a path that
/// cannot be a database stops the start too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sqlite_state_store_starts_and_is_held() {
    let dir = scratch_dir("sqlite");
    let state = dir.join("state").join("state.db");
    let flag = format!("--state-store=sqlite://{}", state.display());
    let f = flags(&["--storage=http://127.0.0.1:1", &flag]).expect("parses");
    let handle = start_frontend(&f, || Ok(pending::<()>())).expect("starts over SQLite");
    assert!(state.exists(), "the state file is made");
    let api = handle.api.expect("the operator API");
    let served = tokio::spawn(handle.serve());

    // The storage node is not there, so only the storage check may fail: the state
    // store, reached through a real store, answers.
    let (_, body) = get(api, "/readyz").await;
    assert_eq!(failing(&body), ["storage"], "{body}");

    let refused = start_frontend(&f, || Ok(pending::<()>()))
        .err()
        .expect("the file is held");
    let said = refused.to_string();
    assert!(
        said.starts_with("the state store cannot be opened: "),
        "{said}"
    );
    assert!(said.contains("another kbf process"), "{said}");

    // A directory is not a database file.
    let flag = format!("--state-store=sqlite://{}", dir.display());
    let f = flags(&["--storage=http://127.0.0.1:1", &flag]).expect("parses");
    assert!(start_frontend(&f, || Ok(pending::<()>())).is_err());
    served.abort();
}

/// Catches a plain-text listener off loopback starting without the warning the other
/// modes print (the supervisor's only sign).
#[tokio::test]
async fn the_start_line_warns_of_plain_text_off_loopback() {
    let f = flags(&["--storage=http://127.0.0.1:1", "--listen=0.0.0.0:0"]).expect("parses");
    let handle = start_frontend(&f, || Ok(pending::<()>())).expect("starts");
    assert!(
        handle
            .start_line()
            .ends_with(" warning=plaintext-off-loopback:reapi"),
        "{}",
        handle.start_line()
    );
}

// ---------------------------------------------------------------------- TLS

/// A CA and the certificates issued from it, written under a directory.
struct Pki {
    ca: rcgen::CertifiedIssuer<'static, rcgen::KeyPair>,
    dir: PathBuf,
}

impl Pki {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("frontend-tls")
            .join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("directory");
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let key = rcgen::KeyPair::generate().expect("key");
        let ca = rcgen::CertifiedIssuer::self_signed(ca, key).expect("CA");
        Self { ca, dir }
    }

    fn write(&self, name: &str, text: String) -> String {
        let path = self.dir.join(name);
        std::fs::write(&path, text).expect("write");
        path.display().to_string()
    }

    fn ca_file(&self) -> String {
        self.write("ca.pem", self.ca.pem())
    }

    /// The storage node's flags for a server certificate valid for the DNS `names`.
    fn node_flags(&self, names: &[&str]) -> Vec<String> {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let mut params = rcgen::CertificateParams::new(names).expect("params");
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        let tag = names_tag(&params);
        vec![
            format!(
                "--storage-tls-cert={}",
                self.write(&format!("server-{tag}.pem"), cert.pem())
            ),
            format!(
                "--storage-tls-key={}",
                self.write(&format!("server-{tag}.key"), key.serialize_pem())
            ),
            format!("--storage-client-ca={}", self.ca_file()),
        ]
    }

    /// The frontend's flags for a client certificate that names `role`, and trusts this
    /// CA for the node.
    fn frontend_flags(&self, role: &str) -> Vec<String> {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = vec![rcgen::SanType::URI(
            format!("{}{role}", kbf_storage::service::ROLE_URI_PREFIX)
                .try_into()
                .expect("IA5"),
        )];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        vec![
            format!(
                "--storage-client-cert={}",
                self.write(&format!("{role}.pem"), cert.pem())
            ),
            format!(
                "--storage-client-key={}",
                self.write(&format!("{role}.key"), key.serialize_pem())
            ),
            format!("--storage-ca={}", self.ca_file()),
        ]
    }
}

/// A file-name tag for a server certificate's names.
fn names_tag(params: &rcgen::CertificateParams) -> String {
    params
        .subject_alt_names
        .iter()
        .filter_map(|s| match s {
            rcgen::SanType::DnsName(n) => Some(n.as_str().to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// A started storage node over mutual TLS, with the server certificate for `names`.
async fn tls_node(pki: &Pki, names: &[&str], dir: &str) -> SocketAddr {
    use kbf_server::storage_node;
    let work = pki.dir.join(dir);
    std::fs::create_dir_all(&work).expect("directory");
    let mut argv = vec![
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", work.join("cold").display()),
        format!("--meta-dir={}", work.join("meta").display()),
    ];
    argv.extend(pki.node_flags(names));
    let flags = storage_node::parse(&storage_node::command().try_get_matches_from(argv).unwrap())
        .expect("flags");
    let node = storage_node::start_storage(&flags, || Ok(pending::<()>()))
        .await
        .expect("starts");
    let addr = node.addr;
    tokio::spawn(node.serve());
    addr
}

/// A frontend dialing `https://localhost:<port>` with these client flags.
async fn tls_front(port: u16, client: &[String]) -> Front {
    let url = format!("--storage=https://localhost:{port}");
    let mut more = vec![url.as_str(), "--readyz-store-timeout-ms=2000"];
    more.extend(client.iter().map(String::as_str));
    Front::start(&flags(&more).expect("flags")).await
}

/// Writes `blob` through the frontend's ByteStream, reads it back and asks FindMissing,
/// and asks for an action result: the raw HTTP/2 path twice and the tonic path twice.
/// Returns what each said, so a test can compare them.
async fn exercise(client: &Client, blob: &[u8]) -> Exercise {
    use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
    use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
    use kbf_proto::reapi::FindMissingBlobsRequest;
    use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;

    let digest = kbf_segments::sha256(blob);
    let wire = kbf_front::digest_to_proto(&digest);
    let mut bytestream = ByteStreamClient::new(client.channel());
    let write = WriteRequest {
        resource_name: format!("uploads/u/blobs/{}/{}", digest.hash_hex(), blob.len()),
        data: blob.to_vec(),
        finish_write: true,
        ..Default::default()
    };
    let wrote = bytestream
        .write(futures::stream::iter([write]))
        .await
        .map(|r| r.into_inner().committed_size)
        .map_err(|s| s.code());
    let read = ReadRequest {
        resource_name: format!("blobs/{}/{}", digest.hash_hex(), blob.len()),
        ..Default::default()
    };
    let read = match bytestream.read(read).await {
        Ok(stream) => {
            let mut stream = stream.into_inner();
            let mut got = Vec::new();
            loop {
                match stream.message().await {
                    Ok(Some(m)) => got.extend(m.data),
                    Ok(None) => break Ok(got),
                    Err(s) => break Err(s.code()),
                }
            }
        }
        Err(s) => Err(s.code()),
    };
    let missing = ContentAddressableStorageClient::new(client.channel())
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![wire],
            ..Default::default()
        })
        .await
        .map(|r| r.into_inner().missing_blob_digests.len())
        .map_err(|s| s.code());
    let action = Job::new("probe", &[]).action;
    let cached = client.cached(&action).await.map(|_| ());
    Exercise {
        wrote,
        read,
        missing,
        cached,
    }
}

#[derive(Debug, PartialEq)]
struct Exercise {
    wrote: Result<i64, tonic::Code>,
    read: Result<Vec<u8>, tonic::Code>,
    missing: Result<usize, tonic::Code>,
    cached: Result<(), tonic::Code>,
}

/// Catches a frontend that cannot reach a TLS storage node, or reaches it on one path
/// only: with a certificate that names the role `frontend`, ByteStream Write and Read
/// (the raw HTTP/2 path) and FindMissing and GetActionResult (the tonic path) all work
/// end to end through the frontend over one mutual-TLS connection, the blob is on the
/// node, and `/readyz` is 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_a_frontend_certificate_reaches_a_tls_node_on_both_paths() {
    let pki = Pki::new("ok");
    let addr = tls_node(&pki, &["localhost"], "node").await;
    let front = tls_front(addr.port(), &pki.frontend_flags("frontend")).await;
    let blob = b"through the frontend, over mutual TLS";
    let client = front.client().await;
    let got = exercise(&client, blob).await;
    assert_eq!(
        got,
        Exercise {
            wrote: Ok(blob.len() as i64),
            read: Ok(blob.to_vec()),
            missing: Ok(0),
            cached: Err(tonic::Code::NotFound),
        }
    );
    let (code, body) = get(front.api, "/readyz").await;
    assert_eq!((code, body["ready"].as_bool()), (200, Some(true)), "{body}");
}

/// Catches a frontend whose certificate names another role being served: a daemon may
/// read and write blobs on a node but never read the action cache, so through a
/// frontend holding a daemon certificate the blob calls work and the action-cache call is
/// refused by the node naming the role, and `/readyz` is 503 on the `storage` check
/// (the probe reads the action cache, the call only a frontend makes); a certificate
/// that names no role fails everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_a_daemon_certificate_is_refused_by_storage() {
    let pki = Pki::new("roles");
    let addr = tls_node(&pki, &["localhost"], "node").await;
    let daemon = tls_front(addr.port(), &pki.frontend_flags("daemon")).await;
    let client = daemon.client().await;
    let got = exercise(&client, b"a daemon-role frontend").await;
    assert_eq!(got.wrote, Ok(22));
    assert_eq!(got.missing, Ok(0));
    let refused = got
        .cached
        .expect_err("a daemon never reads the action cache");
    assert_ne!(refused, tonic::Code::NotFound);
    let (code, body) = get(daemon.api, "/readyz").await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(failing(&body), ["storage"], "{body}");
    let reason = body["failing"][0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("role daemon may not call ActionGet"),
        "{reason}"
    );

    let stranger = tls_front(addr.port(), &pki.frontend_flags("admin")).await;
    let got = exercise(&stranger.client().await, b"a stranger").await;
    assert!(
        got.wrote.is_err() && got.missing.is_err() && got.cached.is_err(),
        "{got:?}"
    );
}

/// Catches a frontend that does not check the node's certificate against the name in
/// the `--storage` URL (or against its CA): a node presenting another node's
/// certificate (valid, from the same CA, for `node-b.test`) at the address the frontend
/// dials as `localhost` is refused on both paths and `/readyz` is 503, as is a node whose
/// certificate is for `localhost` but from another CA; with the right certificate the
/// same frontend and the same client certificate work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_dialing_a_node_with_another_nodes_certificate_is_refused() {
    let pki = Pki::new("names");
    let client_flags = pki.frontend_flags("frontend");
    let wrong_name = tls_node(&pki, &["node-b.test"], "b").await;
    let other_ca = Pki::new("other-ca");
    let wrong_ca = tls_node(&other_ca, &["localhost"], "c").await;
    let right = tls_node(&pki, &["localhost"], "a").await;

    for (what, port) in [("name", wrong_name.port()), ("CA", wrong_ca.port())] {
        let front = tls_front(port, &client_flags).await;
        let got = exercise(&front.client().await, b"must not reach it").await;
        assert_eq!(got.wrote, Err(tonic::Code::Unavailable), "{what}");
        assert_eq!(got.missing, Err(tonic::Code::Unavailable), "{what}");
        assert_eq!(got.cached, Err(tonic::Code::Unavailable), "{what}");
        let (code, body) = get(front.api, "/readyz").await;
        assert_eq!(
            (code, failing(&body)),
            (503, vec!["storage"]),
            "{what}: {body}"
        );
    }
    let front = tls_front(right.port(), &client_flags).await;
    let got = exercise(&front.client().await, b"reaches the right node").await;
    assert_eq!(got.wrote, Ok(22));
    assert_eq!(get(front.api, "/readyz").await.0, 200);
}

/// Catches certificate files that are not read at start (a path that is not there would
/// then fail at the first call, long after the supervisor saw a start line): the start is
/// refused naming the file, before anything is bound.
#[tokio::test]
async fn a_certificate_file_that_is_not_there_is_a_refused_start() {
    let f = flags(&[
        "--storage=https://storage.example:8982",
        "--storage-client-cert=/no/such/cert.pem",
        "--storage-client-key=/no/such/cert.key",
        "--storage-ca=/no/such/ca.pem",
    ])
    .expect("parses");
    let refused = start_frontend(&f, || Ok(pending::<()>()))
        .err()
        .expect("refused");
    assert!(refused.to_string().contains("/no/such/ca.pem"), "{refused}");
}

/// Catches a client identity the transport refuses being swallowed or reported without
/// the flag that caused it: a key file that holds no key is a refused start whose message
/// names `--storage` and the URL (the files are read fine, the failure is the transport's).
#[test]
fn an_identity_the_transport_refuses_is_a_refused_start_naming_the_url() {
    let pki = Pki::new("bad-identity");
    let ca = pki.ca_file();
    let cert = pki.write("not-a-cert.pem", "not a certificate\n".to_owned());
    let key = pki.write("not-a-key.key", "not a key\n".to_owned());
    let f = flags(&[
        "--storage=https://localhost:8982",
        &format!("--storage-client-cert={cert}"),
        &format!("--storage-client-key={key}"),
        &format!("--storage-ca={ca}"),
    ])
    .expect("parses");
    let refused = start_frontend(&f, || Ok(pending::<()>()))
        .err()
        .expect("refused");
    let text = refused.to_string();
    assert!(
        text.contains("--storage") && text.contains("https://localhost:8982"),
        "{text}"
    );
}

/// Catches a frontend whose `/metrics` route is not mounted or answers in another
/// format: the API listener answers 200 in the Prometheus text format with a metric the
/// backend renders.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_api_listener_serves_metrics_in_the_prometheus_text_format() {
    let node = Node::start().await;
    let front =
        Front::start(&flags(&[&format!("--storage=http://{}", node.addr)]).expect("parses")).await;
    let mut stream = TcpStream::connect(front.api)
        .await
        .expect("connect to the API");
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n")
        .await
        .expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let lower = head.to_ascii_lowercase();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        lower.contains(&format!(
            "content-type: {}",
            kbf_server::metrics::TEXT_FORMAT.to_ascii_lowercase()
        )),
        "{head}"
    );
    assert!(body.contains("# TYPE "), "{body}");
}

/// Catches a `--reapi-tls-cert` and `--reapi-tls-key` that a frontend takes and does not
/// serve (clients would find plain text where the flags promised TLS): the REAPI listener
/// answers a health check over TLS for the certificate's name and does not answer in
/// plain text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reapi_listener_serves_tls_when_given_a_certificate() {
    let pki = Pki::new("reapi");
    let [cert, key, _] = pki.node_flags(&["localhost"]).try_into().unwrap();
    let node = Node::start().await;
    let (storage, cert, key) = (
        format!("--storage=http://{}", node.addr),
        cert.replace("--storage-tls-cert", "--reapi-tls-cert"),
        key.replace("--storage-tls-key", "--reapi-tls-key"),
    );
    let front = Front::start(&flags(&[&storage, &cert, &key]).expect("flags")).await;
    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(pki.ca.pem()))
        .domain_name("localhost");
    let secure = Channel::from_shared(format!("https://localhost:{}", front.reapi.port()))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .expect("TLS connect");
    let status = HealthClient::new(secure)
        .check(HealthCheckRequest::default())
        .await
        .expect("check over TLS")
        .into_inner()
        .status();
    assert_eq!(status, ServingStatus::Serving);
    let plain = Channel::from_shared(format!("http://{}", front.reapi))
        .unwrap()
        .connect()
        .await;
    let answered = match plain {
        Ok(channel) => HealthClient::new(channel)
            .check(HealthCheckRequest::default())
            .await
            .is_ok(),
        Err(_) => false,
    };
    assert!(!answered, "plain text was answered on a TLS listener");
}

/// Catches a frontend whose stop ends an open Execute (the copy of the shutdown path from
/// before the drain: `closer.close()` at stop, UNAVAILABLE to the client, which Buck2 does
/// not recover and so fails the action): with an action still running on a daemon, stop
/// is signalled, a new Execute is refused, the open stream stays open, and
/// when the daemon finishes the client gets the result and no error; the serving future
/// ends once the client is gone. Planted mutant: `kbf_server::serve::drain_and_stop` calling
/// `closer.close()` before the sleep: the serving future ends at stop and the "holds the
/// stop" assertion goes red.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_execute_survives_stop_until_its_action_is_done() {
    let node = Node::start().await;
    let url = format!("--storage=http://{}", node.addr);
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = start_frontend(
        &flags(&[&url, "--heartbeat-interval-ms=100"]).expect("flags"),
        || {
            Ok(async move {
                let _ = stopped.await;
            })
        },
    )
    .expect("starts");
    let (reapi, worker) = (handle.reapi, handle.worker);
    let served = tokio::spawn(handle.serve());
    let client = Client::connect(reapi, worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;

    let job = Job::new("build", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;

    let late = Job::new("late", &[]);
    client.upload(&late.blobs()).await;
    // Uploaded now: the connection is going away once stop is signalled.
    let result = support::output(&client, "built across the stop", 0).await;
    stop.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let refused = client
        .execute_with(&late.action, false)
        .await
        .map(|_| ())
        .expect_err("a stopping frontend takes no new Execute");
    // UNAVAILABLE from the service, or CANCELLED when the listener's GOAWAY got there first.
    assert!(
        matches!(
            refused.code(),
            tonic::Code::Unavailable | tonic::Code::Cancelled
        ),
        "{refused:?}"
    );
    assert!(!served.is_finished(), "the open stream holds the stop");

    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));
    drop(ops);
    drop(client);
    drop(daemon);
    tokio::time::timeout(Duration::from_secs(30), served)
        .await
        .expect("the stop ends when the client is gone")
        .unwrap()
        .expect("served");
}

/// Catches a shutdown that waits for a client that never finishes: with a REAPI client
/// stuck in an upload that never ends, the serving future ends when
/// `--shutdown-timeout-secs` runs out, not when the client gives up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_never_finishes_holds_the_shutdown_only_until_its_timeout() {
    use kbf_proto::google::bytestream::WriteRequest;
    use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
    let node = Node::start().await;
    let url = format!("--storage=http://{}", node.addr);
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = start_frontend(
        &flags(&[&url, "--shutdown-timeout-secs=1"]).expect("flags"),
        || {
            Ok(async move {
                let _ = stopped.await;
            })
        },
    )
    .expect("starts");
    let reapi = handle.reapi;
    let served = tokio::spawn(handle.serve());
    let channel = Channel::from_shared(format!("http://{reapi}"))
        .unwrap()
        .connect()
        .await
        .expect("connect");
    let (hold, held) = futures::channel::mpsc::unbounded::<WriteRequest>();
    let blob = kbf_segments::sha256(b"never finished");
    hold.unbounded_send(WriteRequest {
        resource_name: format!("uploads/u/blobs/{}/14", blob.hash_hex()),
        data: b"never".to_vec(),
        ..Default::default()
    })
    .unwrap();
    let mut bytestream = ByteStreamClient::new(channel);
    let upload = tokio::spawn(async move { bytestream.write(held).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let began = std::time::Instant::now();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(20), served)
        .await
        .expect("the shutdown timeout ends the serving future")
        .unwrap()
        .expect("served");
    assert!(
        began.elapsed() >= Duration::from_millis(900),
        "{:?}",
        began.elapsed()
    );
    upload.abort();
    drop(hold);
}

/// Catches a shutdown future that cannot be made (the signal handlers are not installed)
/// ignored: the start is refused with the error, before anything is bound.
#[tokio::test]
async fn a_shutdown_that_cannot_be_made_is_a_refused_start() {
    let f = flags(&["--storage=http://127.0.0.1:1"]).expect("parses");
    let refused = start_frontend(&f, || {
        Err::<std::future::Pending<()>, _>(std::io::Error::other("no signal handlers"))
    })
    .err()
    .expect("refused");
    assert_eq!(refused.to_string(), "no signal handlers");
}

/// Catches a `kbf frontend` that lacks what `kbf-server` has for its drain: the operator
/// endpoint (`GET`/`POST /v1/frontend/drain`, one shared `Drain`), its counts of open
/// streams and queued operations, and `Drain::close` (the second stop signal's action).
/// An Execute waits for a worker: the status reads one stream and one queued operation;
/// a `POST` with the token drains (`/readyz` 503); a close ends the stream UNAVAILABLE
/// and the serving future. Mutant: the frontend's API router given a
/// drain that is not the one its serving future drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_drain_endpoint_and_a_close_reach_a_frontend() {
    use std::os::unix::fs::PermissionsExt as _;
    let node = Node::start().await;
    let url = format!("--storage=http://{}", node.addr);
    let token = "kbf-test-token-0123456789abcdef0123456789";
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("front-token-{}", std::process::id()));
    std::fs::write(&path, format!("{token}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let token_flag = format!("--api-token-file={}", path.display());
    let (_stop, stopped) = oneshot::channel::<()>();
    let handle = start_frontend(&flags(&[&url, &token_flag]).expect("flags"), || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .expect("starts");
    let (reapi, worker, api) = (handle.reapi, handle.worker, handle.api.expect("an API"));
    let drain = handle.drain();
    let served = tokio::spawn(handle.serve());
    let client = Client::connect(reapi, worker).await;
    let job = Job::new("drained on a frontend", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    ops.message().await.expect("healthy").expect("QUEUED");

    let call = |token: Option<&'static str>, post: bool| async move {
        tokio::task::spawn_blocking(move || {
            let client = kbf_server::api_client::OperatorClient::new(api, token.map(str::to_owned));
            let path = kbf_server::api::FRONTEND_DRAIN;
            if post {
                client.post(path, "")
            } else {
                client.get(path)
            }
            .expect("a reply")
        })
        .await
        .expect("the call runs")
    };
    let status = call(None, false).await;
    let want = serde_json::json!({"draining": false, "open_executions": 1, "queued_operations": 1});
    assert_eq!(
        serde_json::from_str::<Value>(&status.body).expect("JSON"),
        want
    );
    assert_eq!(call(None, true).await.status, 401);
    let begun = call(Some(token), true).await;
    assert_eq!(begun.status, 200, "{begun:?}");
    assert!(drain.is_draining());
    assert_eq!(get(api, "/readyz").await.0, 503);

    drain.close();
    let ended = tokio::time::timeout(PROMPT, ops.message())
        .await
        .expect("the stream ends after a close")
        .expect_err("with an error");
    assert_eq!(ended.code(), tonic::Code::Unavailable, "{ended:?}");
    drop(ops);
    drop(client);
    tokio::time::timeout(Duration::from_secs(30), served)
        .await
        .expect("the frontend stops")
        .unwrap()
        .expect("served");
}

/// The text of `GET path` on `api`.
async fn get_text(api: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, body.to_owned())
}

/// Catches: a frontend that keeps storage members but does not show their health (its
/// operator API and scrape are assembled apart from the all-in-one's). A node registers
/// with the frontend's membership service; it is CatchingUp, so the number is `null` naming
/// it, `kbf_storage_health_known` is 0, and `kbf_under_replicated_blobs` is absent rather
/// than 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_a_state_store_shows_the_storage_health() {
    use kbf_proto::kbf::map::v1 as pb;
    use kbf_proto::kbf::map::v1::membership_client::MembershipClient;

    let dir = scratch_dir("storage-health");
    let flag = format!("--state-store=sqlite://{}", dir.join("state.db").display());
    let f = flags(&["--storage=http://127.0.0.1:1", &flag]).expect("parses");
    let handle = start_frontend(&f, || Ok(pending::<()>())).expect("starts");
    let (api, worker) = (handle.api.expect("the operator API"), handle.worker);
    let served = tokio::spawn(handle.serve());

    let (code, body) = get(api, "/v1/storage").await;
    assert_eq!(code, 200);
    assert_eq!(body["members_total"], 0, "{body}");
    assert_eq!(body["under_replicated_blobs"], Value::Null, "{body}");

    let channel = Endpoint::from_shared(format!("http://{worker}"))
        .expect("endpoint")
        .connect_lazy();
    MembershipClient::new(channel)
        .register(pb::RegisterRequest {
            role: pb::Role::Storage.into(),
            instance_id: "node-1".to_owned(),
            addr: "192.0.2.1:8982".to_owned(),
            weight: 1,
            zone: String::new(),
            catching_up: true,
            protocol: kbf_server::members::PROTOCOL,
        })
        .await
        .expect("registers");
    let (code, body) = get(api, "/v1/storage").await;
    assert_eq!(code, 200);
    assert_eq!(body["not_active"], serde_json::json!([1]), "{body}");
    assert_eq!(body["under_replicated_blobs"], Value::Null, "{body}");
    assert_eq!(
        body["reasons"],
        serde_json::json!(["member 1 not active"]),
        "{body}"
    );

    let (code, scrape) = get_text(api, "/metrics").await;
    assert_eq!(code, 200);
    assert!(
        scrape.contains("\nkbf_storage_health_known 0\n"),
        "{scrape}"
    );
    assert!(!scrape.contains("kbf_under_replicated_blobs"), "{scrape}");
    served.abort();
}

/// Catches: storage health shown by a frontend that has no state store to keep members in
/// (it would answer a number for a farm it cannot see): `/v1/storage` is not there, and
/// the scrape has neither family.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_without_a_state_store_has_no_storage_health() {
    let node = Node::start().await;
    let front =
        Front::start(&flags(&[&format!("--storage=http://{}", node.addr)]).expect("parses")).await;
    let (code, _) = get_text(front.api, "/v1/storage").await;
    assert_eq!(code, 404);
    let (code, scrape) = get_text(front.api, "/metrics").await;
    assert_eq!(code, 200);
    assert!(!scrape.contains("kbf_storage_health_known"), "{scrape}");
    assert!(!scrape.contains("kbf_under_replicated_blobs"), "{scrape}");
}

/// Catches: `--history-objects` taken by a frontend and not wired (a frontend whose
/// `/metrics` never names history, one that starts over a directory it does not open),
/// and one taken without the state store its index rows go to: refused before anything
/// is bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_history_serves_its_counters_and_needs_a_state_store() {
    let dir = scratch_dir("history");
    let history = dir.join("history");
    let history_flag = format!("--history-objects=file://{}", history.display());
    let without = flags(&["--storage=http://127.0.0.1:1", &history_flag]).expect("parses");
    let refused = start_frontend(&without, || Ok(pending::<()>()))
        .err()
        .expect("no state store");
    assert!(refused.to_string().contains("--state-store"), "{refused}");

    let state = dir.join("state").join("state.db");
    let state_flag = format!("--state-store=sqlite://{}", state.display());
    let f = flags(&["--storage=http://127.0.0.1:1", &state_flag, &history_flag]).expect("parses");
    let front = Front::start(&f).await;
    assert!(
        history.join("objects").is_dir(),
        "the history store is opened"
    );
    let mut stream = TcpStream::connect(front.api)
        .await
        .expect("connect to the API");
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n")
        .await
        .expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (_, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    assert!(body.contains("kbf_history_queue_events 0\n"), "{body}");
    assert!(
        body.contains("kbf_history_dropped_total{stage=\"queue\"} 0\n"),
        "{body}"
    );
}

// -------------------------------------------------------------- routing by the map

/// A connection to a storage node as the frontend's certificate dials it, to read what the
/// node holds.
fn direct(client_flags: &[String], port: u16) -> kbf_storage::service::RemoteStorage {
    let path = |flag: &str| {
        client_flags
            .iter()
            .find_map(|f| f.strip_prefix(flag))
            .map(PathBuf::from)
            .expect("a flag")
    };
    let read = |p: PathBuf| std::fs::read(p).expect("a file");
    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(read(path(
            "--storage-ca=",
        ))))
        .identity(tonic::transport::Identity::from_pem(
            read(path("--storage-client-cert=")),
            read(path("--storage-client-key=")),
        ));
    let channel = tonic::transport::Endpoint::from_shared(format!("https://localhost:{port}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("tls")
        .connect_lazy();
    kbf_storage::service::RemoteStorage::new(channel, 0)
}

/// Writes `blob` through the frontend's ByteStream (the raw path), by an upload name.
async fn upload_through(client: &Client, blob: &[u8]) {
    use kbf_proto::google::bytestream::WriteRequest;
    use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
    let digest = kbf_segments::sha256(blob);
    let write = WriteRequest {
        resource_name: format!("uploads/u/blobs/{}/{}", digest.hash_hex(), blob.len()),
        data: blob.to_vec(),
        finish_write: true,
        ..Default::default()
    };
    ByteStreamClient::new(client.channel())
        .write(futures::stream::iter([write]))
        .await
        .expect("written");
}

/// Which of the nodes (by port) hold `blob`.
async fn holders(nodes: &[(u16, kbf_storage::service::RemoteStorage)], blob: &[u8]) -> Vec<u16> {
    let digest = kbf_segments::sha256(blob);
    let mut held = Vec::new();
    for (port, node) in nodes {
        if node
            .find_missing(&[digest])
            .await
            .expect("asked")
            .is_empty()
        {
            held.push(*port);
        }
    }
    held
}

/// Catches a frontend that has a map and keeps writing to its one `--storage` node (the
/// rehearsal's failure, at a smaller size), and one that routes before there is a map to
/// route by: with a state store and the TLS flags, writes through both of the frontend's
/// paths (the raw ByteStream and the tonic batch) land on the `--storage` node while no
/// member, and then only one, has registered; once two storage nodes have registered, each
/// blob lands on the one node the map writes it to, and the blobs are on both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_members_writes_where_the_map_says() {
    use kbf_proto::kbf::map::v1 as pb;
    use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
    use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
    use kbf_proto::reapi::{BatchUpdateBlobsRequest, batch_update_blobs_request};

    let pki = Pki::new("routed");
    let (a, b) = (
        tls_node(&pki, &["localhost"], "a").await,
        tls_node(&pki, &["localhost"], "b").await,
    );
    let client_flags = pki.frontend_flags("frontend");
    let nodes = [
        (a.port(), direct(&client_flags, a.port())),
        (b.port(), direct(&client_flags, b.port())),
    ];
    let state = scratch_dir("routed").join("state.db");
    let mut more = vec![
        format!("--storage=https://localhost:{}", a.port()),
        "--readyz-store-timeout-ms=2000".to_owned(),
        format!("--state-store=sqlite://{}", state.display()),
    ];
    more.extend(client_flags.iter().cloned());
    let front =
        Front::start(&flags(&more.iter().map(String::as_str).collect::<Vec<_>>()).unwrap()).await;
    let client = front.client().await;
    let blobs: Vec<Vec<u8>> = (0..40)
        .map(|i| format!("routed {i}").into_bytes())
        .collect();

    // No member: the node `--storage` names.
    upload_through(&client, &blobs[0]).await;
    assert_eq!(holders(&nodes, &blobs[0]).await, [a.port()]);

    let mut membership = MembershipClient::new(
        tonic::transport::Endpoint::from_shared(format!("http://{}", front.worker))
            .unwrap()
            .connect()
            .await
            .expect("the worker listener"),
    );
    let register = |port: u16, name: &str| pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: name.to_owned(),
        addr: format!("localhost:{port}"),
        weight: 1,
        zone: String::new(),
        protocol: kbf_server::members::PROTOCOL,
        catching_up: false,
    };
    // One member: still that node, whatever address it advertises.
    membership
        .register(register(a.port(), "node-a"))
        .await
        .expect("a registers");
    upload_through(&client, &blobs[1]).await;
    assert_eq!(holders(&nodes, &blobs[1]).await, [a.port()]);

    // Two members: the map's.
    let both = membership
        .register(register(b.port(), "node-b"))
        .await
        .expect("b registers")
        .into_inner();
    let map = kbf_map::Map::try_from(both.map.expect("a map")).expect("a valid map");
    let port_of = |digest: &kbf_types::Digest| {
        let owner = map.write_owners(digest)[0];
        let member = map.member(owner).expect("a member");
        member
            .addr
            .rsplit(':')
            .next()
            .unwrap()
            .parse::<u16>()
            .unwrap()
    };
    // Half of them through the raw path, half through the batch.
    for blob in &blobs[2..21] {
        upload_through(&client, blob).await;
    }
    let batch = blobs[21..]
        .iter()
        .map(|bytes| batch_update_blobs_request::Request {
            digest: Some(kbf_front::digest_to_proto(&kbf_segments::sha256(bytes))),
            data: bytes.clone(),
            compressor: kbf_proto::reapi::compressor::Value::Identity as i32,
        })
        .collect();
    let answered = ContentAddressableStorageClient::new(client.channel())
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: batch,
            ..Default::default()
        })
        .await
        .expect("the batch")
        .into_inner();
    assert!(
        answered
            .responses
            .iter()
            .all(|r| r.status.as_ref().is_some_and(|s| s.code == 0))
    );

    let mut used = std::collections::BTreeSet::new();
    for blob in &blobs[2..] {
        let want = port_of(&kbf_segments::sha256(blob));
        used.insert(want);
        assert_eq!(holders(&nodes, blob).await, [want]);
    }
    assert_eq!(used.len(), 2, "every blob went to one node");
}
