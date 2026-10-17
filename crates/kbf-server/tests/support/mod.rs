//! An in-process single-node server on loopback ports, REAPI clients for it, and a
//! scripted fake daemon speaking `kbf.worker.v1` in plain text.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_front::{Cache, Leadership, MAX_MESSAGE_BYTES, MemoryMetaLog, MetaLog, MetaLogError};
use kbf_meta::{Applied, Command as MetaCommand, MetaState, Retention};
use kbf_objstore::KeyPrefix;
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    self, Action, ActionResult, BatchUpdateBlobsRequest, Command, Directory, ExecuteRequest,
    ExecuteResponse, FileNode, FindMissingBlobsRequest, GetActionResultRequest, OutputFile,
    Platform, batch_update_blobs_request, platform,
};
use kbf_proto::worker::{
    Capability, DaemonMessage, Heartbeat, Hello, LeaseId, LeaseOffer, NodeStatus, ResultAck,
    ServerMessage, Start, daemon_message, server_message, worker_client::WorkerClient,
};
use kbf_server::token::ApiToken;
use kbf_server::{
    Api, Bound, Listeners, MemoryLadder, RaftMetaLog, ServeError, StreamLimits,
    bind_server_with_map,
};
use kbf_store::FaultFs;
use prost::Message;
use tokio::sync::{Notify, oneshot};
use tokio::time::timeout;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Streaming};

mod store;
pub use store::SharedStore;

/// How long a test waits for something that should happen promptly.
pub const PROMPT: Duration = Duration::from_secs(5);

/// How long a test watches for something that must not happen. Several ticks and
/// heartbeat intervals.
pub const QUIET: Duration = Duration::from_millis(400);

/// How long the test server waits for a stream's `Hello`.
pub const HELLO_WAIT: Duration = Duration::from_millis(300);

/// The heartbeat interval the test server names in `Welcome`.
pub const INTERVAL: Duration = Duration::from_millis(100);

/// A point a test can close: the next caller to reach it waits there until the test
/// opens it, so the test can act while the server is in the middle of that step.
#[derive(Debug, Default)]
pub struct Gate {
    armed: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl Gate {
    /// Holds the next caller at the gate.
    pub fn close_next(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Waits until a caller is held at the gate.
    pub async fn held(&self) {
        timeout(PROMPT, self.entered.notified())
            .await
            .expect("a caller reaches the gate");
    }

    /// Lets the held caller through.
    pub fn open(&self) {
        self.release.notify_one();
    }

    async fn pass(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

/// The metadata log, in memory or a Raft log, with gates a test can close on the next
/// query and on the next action-cache write, and a switch that makes the next
/// action-cache write fail as an unreachable leader would.
#[derive(Debug)]
pub struct GateLog {
    inner: Inner,
    pub query: Gate,
    pub action_write: Gate,
    fail_action_write: AtomicBool,
}

#[derive(Debug)]
enum Inner {
    Memory(Box<MemoryMetaLog>),
    Raft(RaftMetaLog<FaultFs>),
}

impl GateLog {
    fn new(inner: Inner) -> Self {
        Self {
            inner,
            query: Gate::default(),
            action_write: Gate::default(),
            fail_action_write: AtomicBool::new(false),
        }
    }

    /// The Raft log, for a cell started with one ([`Cell::start_raft`]).
    pub fn raft(&self) -> Option<&RaftMetaLog<FaultFs>> {
        match &self.inner {
            Inner::Memory(_) => None,
            Inner::Raft(log) => Some(log),
        }
    }

    /// Fails the next action-cache write with `MetaLogError::Unavailable`.
    pub fn fail_next_action_write(&self) {
        self.fail_action_write.store(true, Ordering::SeqCst);
    }
}

impl MetaLog for GateLog {
    async fn commit(&self, command: MetaCommand) -> Result<Applied, MetaLogError> {
        if matches!(command, MetaCommand::PutAction { .. }) {
            self.action_write.pass().await;
            if self.fail_action_write.swap(false, Ordering::SeqCst) {
                return Err(MetaLogError::Unavailable("failed by the test".to_owned()));
            }
        }
        match &self.inner {
            Inner::Memory(log) => log.commit(command).await,
            Inner::Raft(log) => log.commit(command).await,
        }
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.query.pass().await;
        match &self.inner {
            Inner::Memory(log) => log.query(f).await,
            Inner::Raft(log) => log.query(f).await,
        }
    }

    fn leadership(&self) -> Option<Leadership> {
        match &self.inner {
            Inner::Memory(log) => log.leadership(),
            Inner::Raft(log) => log.leadership(),
        }
    }
}

/// A running server in this process, and a [`Client`] of it.
pub struct Cell {
    pub cache: Arc<Cache<GateLog, SharedStore>>,
    /// The bucket, shared with every restart of this cell.
    pub store: SharedStore,
    /// The operator API's address, for a cell started with one.
    pub api: Option<SocketAddr>,
    /// The operator API's configuration, kept for a restart.
    api_config: Option<Api>,
    /// The raft directory of a cell started with [`Cell::start_raft`], as the old
    /// process left it.
    raft_dir: Option<FaultFs>,
    /// Stops the server; taken by [`Cell::cold_restart`], sent on drop otherwise.
    stop: Option<oneshot::Sender<()>>,
    /// Answers once the server's runtime, and every task on it, is gone.
    stopped: Option<oneshot::Receiver<()>>,
    /// The server's invocation records.
    pub recorder: kbf_front::Recorder,
    /// The server's frontend drain.
    pub drain: Arc<kbf_server::Drain>,
    client: Client,
}

impl Drop for Cell {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Deref for Cell {
    type Target = Client;

    fn deref(&self) -> &Client {
        &self.client
    }
}

/// Channels to a server's two listeners, and REAPI helpers.
pub struct Client {
    pub reapi_addr: SocketAddr,
    pub worker_addr: SocketAddr,
    channel: Channel,
}

/// The memory ladder of every cell but [`Cell::start_with_ladder`]'s: 1, 2 and 4 GiB,
/// so an action that names no size books 1 GiB and a test node of 1 to 4 GiB runs it.
/// (The server's default is 4, 16 and 32 GiB.)
pub fn small_ladder() -> MemoryLadder {
    MemoryLadder::from_gib(&[1, 2, 4]).expect("a ladder")
}

/// The bearer token of a cell's operator API ([`Cell::start_with_api`]).
pub const API_TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

impl Cell {
    pub async fn start() -> Self {
        Self::start_with(kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION).await
    }

    /// A cell whose scheduler refuses queued work no live worker can run after `wait`.
    pub async fn start_with_unservable_wait(wait: Duration) -> Self {
        Self::start_with(wait, kbf_sched::FINISHED_RETENTION).await
    }

    /// A cell that books and climbs `ladder`, as `--memory-ladder-gib` sets it. The
    /// other cells book 1 GiB first ([`small_ladder`]), so the small nodes of these
    /// tests can run an action that names no size.
    pub async fn start_with_ladder(ladder: MemoryLadder) -> Self {
        let store = SharedStore::new();
        let cache = Self::cold_cache(&store, None).await;
        let (wait, retention) = (kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION);
        Self::serve(
            cache,
            store,
            None,
            None,
            wait,
            retention,
            ladder,
            StreamLimits::DEFAULT,
        )
        .await
    }

    /// A cell whose listeners allow `streams` HTTP/2 streams per connection
    /// (`--reapi-max-streams`, `--worker-max-streams`).
    pub async fn start_with_streams(streams: StreamLimits) -> Self {
        let store = SharedStore::new();
        let cache = Self::cold_cache(&store, None).await;
        let (wait, retention) = (kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION);
        Self::serve(
            cache,
            store,
            None,
            None,
            wait,
            retention,
            small_ladder(),
            streams,
        )
        .await
    }

    /// A cell that sends its daemons the storage map `map` holds.
    pub async fn start_with_map(map: Arc<dyn kbf_mapsync::MapSource>) -> Self {
        let store = SharedStore::new();
        let cache = Self::cold_cache(&store, None).await;
        let (wait, retention) = (kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION);
        Self::serve_with_map(
            cache,
            store,
            None,
            None,
            wait,
            retention,
            small_ladder(),
            StreamLimits::DEFAULT,
            Some(map),
            None,
        )
        .await
    }

    /// A cell whose scheduler keeps its state in `state` (and writes the build history it
    /// names, if any), and that serves the operator API without a token: its `/metrics`
    /// is at [`Cell::api`].
    pub async fn start_with_state(state: kbf_server::StateStore) -> Self {
        let store = SharedStore::new();
        let cache = Self::cold_cache(&store, None).await;
        let api = Api {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            token: None,
            local_cache: None,
            releases: None,
        };
        let (wait, retention) = (kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION);
        Self::serve_with_map(
            cache,
            store,
            Some(api),
            None,
            wait,
            retention,
            small_ladder(),
            StreamLimits::DEFAULT,
            None,
            Some(state),
        )
        .await
    }

    /// A cell whose scheduler refuses queued work no live worker can run after `wait`,
    /// and keeps a finished operation for `retention`.
    pub async fn start_with(wait: Duration, retention: Duration) -> Self {
        let store = SharedStore::new();
        let cache = Self::cold_cache(&store, None).await;
        Self::serve(
            cache,
            store,
            None,
            None,
            wait,
            retention,
            small_ladder(),
            StreamLimits::DEFAULT,
        )
        .await
    }

    /// A cell whose metadata is a Raft log ([`RaftMetaLog`], as `kbf-server` runs it) in
    /// an in-memory raft directory, which [`Cell::cold_restart`] carries over as a
    /// process restart leaves it. Objects go under `kbf/<log id>/`.
    pub async fn start_raft() -> Self {
        let store = SharedStore::new();
        let dir = FaultFs::new();
        let cache = Self::cold_cache(&store, Some(&dir)).await;
        let (wait, retention) = (kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION);
        Self::serve(
            cache,
            store,
            None,
            Some(dir),
            wait,
            retention,
            small_ladder(),
            StreamLimits::DEFAULT,
        )
        .await
    }

    /// The Raft log of a cell started with [`Cell::start_raft`].
    pub fn raft_log(&self) -> &RaftMetaLog<FaultFs> {
        self.cache.meta().raft().expect("a cell with a Raft log")
    }

    /// A cell that also serves the operator API, whose writes need [`API_TOKEN`].
    pub async fn start_with_api() -> Self {
        let store = SharedStore::new();
        let api = Api {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            token: Some(api_token()),
            local_cache: None,
            releases: None,
        };
        let (wait, retention) = (kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION);
        let cache = Self::cold_cache(&store, None).await;
        Self::serve(
            cache,
            store,
            Some(api),
            None,
            wait,
            retention,
            small_ladder(),
            StreamLimits::DEFAULT,
        )
        .await
    }

    /// A new server over this cell's store and its in-memory action cache and CAS
    /// index, with a fresh scheduler, no daemon registered and nothing queued. This one
    /// is left running; its daemons simply never reach the new one's state.
    ///
    /// Sharing the cache models a durable cache, which `kbf-server` does not have
    /// today: its metadata is in memory. [`Cell::cold_restart`] is a restart as the
    /// binary has it. On a cell started with [`Cell::start_raft`] both servers share
    /// one open Raft log, and so its term: use [`Cell::cold_restart`] there.
    pub async fn restart(&self) -> Self {
        let (wait, retention) = (kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION);
        let (cache, store) = (Arc::clone(&self.cache), self.store.clone());
        let dir = self.raft_dir.clone();
        Self::serve(
            cache,
            store,
            self.api_config.clone(),
            dir,
            wait,
            retention,
            small_ladder(),
            StreamLimits::DEFAULT,
        )
        .await
    }

    /// Stops this server, waits until its runtime and every task on it are gone (each
    /// connection and stream of it ends, as when the process exits), and starts a new
    /// one over the same bucket, as a restart of the `kbf-server` binary does: a new
    /// `Cache` with a new, empty metadata log (so an empty CAS index and action
    /// cache), a fresh scheduler (no daemon, no cordon, no drain, nothing queued), and
    /// its objects written under a key prefix no earlier start used, as an S3 cold store
    /// does. The objects earlier starts wrote stay in the bucket. The operator API, if
    /// this cell has one, listens again on a new port.
    ///
    /// A cell started with [`Cell::start_raft`] reopens its Raft log from the raft
    /// directory as the old process left it (the Raft metadata log), so the CAS index and
    /// the action cache are kept, and writes under the log's prefix in a new epoch.
    pub async fn cold_restart(mut self) -> Self {
        let stop = self.stop.take().expect("the server is running");
        let stopped = self.stopped.take().expect("the server is running");
        let (store, api_config) = (self.store.clone(), self.api_config.clone());
        let dir = self.raft_dir.take();
        drop(self);
        stop.send(()).expect("the server is running");
        timeout(PROMPT * 4, stopped)
            .await
            .expect("the old server stops in time")
            .expect("the old server stops cleanly");
        let dir = dir.map(|d| d.restart());
        let cache = Self::cold_cache(&store, dir.as_ref()).await;
        let (wait, retention) = (kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION);
        Self::serve(
            cache,
            store,
            api_config,
            dir,
            wait,
            retention,
            small_ladder(),
            StreamLimits::DEFAULT,
        )
        .await
    }

    /// A new cache over `store` and a writer epoch it allocates. Without `raft_dir`:
    /// an empty metadata log, and objects under `start-<n>/`, where `n` is new to this
    /// test process. With it: the Raft log there (a new one, with id `n + 1`, if the
    /// directory is empty), and objects under the log's prefix.
    async fn cold_cache(
        store: &SharedStore,
        raft_dir: Option<&FaultFs>,
    ) -> Arc<Cache<GateLog, SharedStore>> {
        static STARTS: AtomicU32 = AtomicU32::new(0);
        let n = STARTS.fetch_add(1, Ordering::Relaxed);
        let (log, prefix) = match raft_dir {
            None => {
                let prefix = KeyPrefix::new(format!("start-{n}/")).expect("a key prefix");
                (
                    Inner::Memory(Box::new(MemoryMetaLog::new(Retention::default()))),
                    prefix,
                )
            }
            Some(dir) => {
                let id = || {
                    Ok(kbf_log::FarmLogId::new(
                        (u64::from(n) + 1).try_into().unwrap(),
                    ))
                };
                let log = RaftMetaLog::open(dir.clone(), Retention::default(), "kbf/", id).await;
                let log = log.expect("open the Raft log");
                let prefix = log.store_prefix();
                (Inner::Raft(log), prefix)
            }
        };
        let cache = Cache::open(GateLog::new(log), store.clone(), prefix).await;
        Arc::new(cache.expect("open the cache"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn serve(
        cache: Arc<Cache<GateLog, SharedStore>>,
        store: SharedStore,
        api_config: Option<Api>,
        raft_dir: Option<FaultFs>,
        wait: Duration,
        retention: Duration,
        ladder: MemoryLadder,
        streams: StreamLimits,
    ) -> Self {
        Self::serve_with_map(
            cache, store, api_config, raft_dir, wait, retention, ladder, streams, None, None,
        )
        .await
    }

    /// [`Cell::serve`], sending its daemons the storage map `map` holds if given.
    #[allow(clippy::too_many_arguments)]
    async fn serve_with_map(
        cache: Arc<Cache<GateLog, SharedStore>>,
        store: SharedStore,
        api_config: Option<Api>,
        raft_dir: Option<FaultFs>,
        wait: Duration,
        retention: Duration,
        ladder: MemoryLadder,
        streams: StreamLimits,
        map: Option<Arc<dyn kbf_mapsync::MapSource>>,
        state: Option<kbf_server::StateStore>,
    ) -> Self {
        let listeners = Listeners {
            reapi: SocketAddr::from(([127, 0, 0, 1], 0)),
            worker: SocketAddr::from(([127, 0, 0, 1], 0)),
            reapi_tls: None,
            worker_tls: None,
            heartbeat_interval: INTERVAL,
            hello_wait: HELLO_WAIT,
            tick: Duration::from_millis(50),
            unservable_wait: wait,
            memory_ladder: ladder,
            finished_retention: retention,
            shutdown_timeout: Duration::from_secs(10),
            streams: kbf_server::StreamTiming::default(),
            store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
            invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
            segments: kbf_front::SegmentsConfig::DEFAULT,
            stream_limits: streams,
        };
        let (stop, stop_rx) = oneshot::channel::<()>();
        let (stopped_tx, stopped) = oneshot::channel::<()>();
        let (bound_tx, bound_rx) = oneshot::channel();
        let (server_cache, server_api) = (Arc::clone(&cache), api_config.clone());
        // The server runs on a runtime of its own, as a process apart: once it stops,
        // the runtime is dropped and every task of it (each connection, each stream)
        // goes with it, as when the process exits.
        std::thread::Builder::new()
            .name("kbf-server-cell".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime");
                runtime.block_on(async move {
                    let shutdown = async move {
                        let _ = stop_rx.await;
                    };
                    let bound = bind_server_with_map(
                        server_cache,
                        listeners,
                        server_api,
                        kbf_auth::Policy::allow_all(),
                        state,
                        map,
                        shutdown,
                    );
                    run(bound.expect("bind"), bound_tx).await;
                });
                drop(runtime);
                let _ = stopped_tx.send(());
            })
            .expect("a server thread");
        let (reapi, worker, api, recorder, drain) = bound_rx.await.expect("the server binds");
        Self {
            cache,
            store,
            api,
            api_config,
            raft_dir,
            stop: Some(stop),
            stopped: Some(stopped),
            recorder,
            drain,
            client: Client::connect(reapi, worker).await,
        }
    }
}

/// What a started server tells the test: its REAPI, worker and operator API addresses,
/// its invocation records and its frontend drain.
type Started = (
    SocketAddr,
    SocketAddr,
    Option<SocketAddr>,
    kbf_front::Recorder,
    Arc<kbf_server::Drain>,
);

/// Says where `bound` listens on `addresses`, then serves it until it stops.
async fn run(
    bound: Bound<impl Future<Output = Result<(), ServeError>>>,
    addresses: oneshot::Sender<Started>,
) {
    let recorder = bound.recorder.clone();
    let _ = addresses.send((
        bound.reapi,
        bound.worker,
        bound.api,
        recorder,
        Arc::clone(&bound.drain),
    ));
    bound.serving.await.expect("serve");
}

/// An operator API token holding [`API_TOKEN`], from a file of mode 0600 unique to
/// this call.
fn api_token() -> ApiToken {
    use std::os::unix::fs::PermissionsExt;
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-cell");
    std::fs::create_dir_all(&dir).expect("a token directory");
    let path = dir.join(format!("token-{}-{n}", std::process::id()));
    std::fs::write(&path, format!("{API_TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
}

impl Client {
    /// A client of the server listening at `reapi` and `worker`.
    pub async fn connect(reapi: SocketAddr, worker: SocketAddr) -> Self {
        let channel = Endpoint::from_shared(format!("http://{reapi}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        Self {
            reapi_addr: reapi,
            worker_addr: worker,
            channel,
        }
    }

    /// The channel to the REAPI listener.
    pub fn channel(&self) -> Channel {
        self.channel.clone()
    }

    pub fn exec(&self) -> ExecutionClient<Channel> {
        ExecutionClient::new(self.channel.clone()).max_decoding_message_size(MAX_MESSAGE_BYTES)
    }

    pub fn ac(&self) -> ActionCacheClient<Channel> {
        ActionCacheClient::new(self.channel.clone())
    }

    pub async fn upload(&self, blobs: &[&Blob]) {
        let response = ContentAddressableStorageClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
            .batch_update_blobs(BatchUpdateBlobsRequest {
                requests: blobs
                    .iter()
                    .map(|b| batch_update_blobs_request::Request {
                        digest: Some(b.proto.clone()),
                        data: b.data.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .await
            .expect("BatchUpdateBlobs")
            .into_inner();
        for r in response.responses {
            assert_eq!(
                r.status.expect("status").code,
                0,
                "upload of {:?}",
                r.digest
            );
        }
    }

    /// Whether the CAS holds `blob`.
    pub async fn holds(&self, blob: &Blob) -> bool {
        ContentAddressableStorageClient::new(self.channel.clone())
            .find_missing_blobs(FindMissingBlobsRequest {
                blob_digests: vec![blob.proto.clone()],
                ..Default::default()
            })
            .await
            .expect("FindMissingBlobs")
            .into_inner()
            .missing_blob_digests
            .is_empty()
    }

    /// Starts an Execute of `action`.
    pub async fn execute(&self, action: &Blob) -> Streaming<Operation> {
        self.execute_with(action, false).await.expect("Execute")
    }

    pub async fn execute_with(
        &self,
        action: &Blob,
        skip_cache_lookup: bool,
    ) -> Result<Streaming<Operation>, tonic::Status> {
        self.exec()
            .execute(ExecuteRequest {
                instance_name: "main".to_owned(),
                action_digest: Some(action.proto.clone()),
                skip_cache_lookup,
                ..Default::default()
            })
            .await
            .map(tonic::Response::into_inner)
    }

    /// GetActionResult: the result as the daemon reported it ([`unstamped`]), or the
    /// error code.
    pub async fn cached(&self, action: &Blob) -> Result<ActionResult, Code> {
        self.cached_stamped(action).await.map(unstamped)
    }

    /// GetActionResult: the result as the cache holds it, with the server's metadata.
    pub async fn cached_stamped(&self, action: &Blob) -> Result<ActionResult, Code> {
        self.ac()
            .get_action_result(GetActionResultRequest {
                action_digest: Some(action.proto.clone()),
                ..Default::default()
            })
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| s.code())
    }

    /// A daemon for `node` with `cpus` CPUs and `mem_gib` GiB, registered. Every daemon
    /// a test opens for one node this way is the same daemon process (a new stream of
    /// it); see [`Client::daemon_process`].
    pub async fn daemon(&self, node: &str, cpus: u32, mem_gib: u32) -> FakeDaemon {
        FakeDaemon::connect(self.worker_addr, hello(node, cpus, mem_gib))
            .await
            .expect("registered")
    }

    /// A daemon for `node`, registered by the daemon process `instance` (its
    /// `Hello.instance_id`).
    pub async fn daemon_process(
        &self,
        node: &str,
        instance: &str,
        cpus: u32,
        mem_gib: u32,
    ) -> FakeDaemon {
        let hello = Hello {
            instance_id: instance.to_owned(),
            ..hello(node, cpus, mem_gib)
        };
        FakeDaemon::connect(self.worker_addr, hello)
            .await
            .expect("registered")
    }
}

/// A Hello for a Linux x86-64 `node` reporting `cpus` and `mem_gib`.
pub fn hello(node: &str, cpus: u32, mem_gib: u32) -> Hello {
    hello_on(node, cpus, mem_gib, &[("arch", "x86_64"), ("os", "linux")])
}

/// A Hello for `node` reporting `cpus`, `mem_gib` and the `platform` entries, and
/// `drivers=fake` (which serves `action` leases) unless `platform` names its drivers.
pub fn hello_on(node: &str, cpus: u32, mem_gib: u32, platform: &[(&str, &str)]) -> Hello {
    let drivers: &[(&str, &str)] = if platform.iter().any(|(key, _)| *key == "drivers") {
        &[]
    } else {
        &[("drivers", "fake")]
    };
    let platform = platform
        .iter()
        .chain(drivers)
        .map(|(key, value)| Capability {
            key: (*key).to_owned(),
            value: (*value).to_owned(),
        });
    Hello {
        protocol_version: 1,
        node_id: node.to_owned(),
        daemon_version: "test".to_owned(),
        capabilities: vec![
            Capability {
                key: "cpus".to_owned(),
                value: cpus.to_string(),
            },
            Capability {
                key: "mem_gib".to_owned(),
                value: mem_gib.to_string(),
            },
        ]
        .into_iter()
        .chain(platform)
        .collect(),
        report_hash: Vec::new(),
        // One daemon process per node unless a test says otherwise.
        instance_id: format!("{node}-daemon"),
        phase: None,
        predecessor_instance_id: String::new(),
    }
}

/// The server's side of a session, as a daemon sees it. Messages a test is not waiting
/// for are kept, in order, for a later `expect`.
pub struct FakeDaemon {
    tx: UnboundedSender<DaemonMessage>,
    inbound: Streaming<ServerMessage>,
    kept: VecDeque<server_message::Message>,
    seq: u64,
    /// The lease epoch the Welcome named (0 before it).
    pub epoch: u64,
    /// The storage map the Welcome carried.
    pub map: Option<kbf_proto::kbf::map::v1::Map>,
}

impl FakeDaemon {
    /// Opens a stream, sends `hello`, waits for Welcome and sends a `NodeStatus`, as a
    /// daemon does.
    pub async fn connect(addr: SocketAddr, hello: Hello) -> Result<Self, tonic::Status> {
        let d = Self::connect_without_status(addr, hello).await?;
        d.send(daemon_message::Message::NodeStatus(NodeStatus {
            os_name: "Ubuntu".to_owned(),
            daemon_version: "test".to_owned(),
            ..NodeStatus::default()
        }));
        Ok(d)
    }

    /// Opens a stream, sends `hello` and waits for Welcome: a daemon that predates
    /// `NodeStatus`.
    pub async fn connect_without_status(
        addr: SocketAddr,
        hello: Hello,
    ) -> Result<Self, tonic::Status> {
        let mut d = Self::open(addr, daemon_message::Message::Hello(hello)).await?;
        match d.next().await {
            Some(server_message::Message::Welcome(w)) => {
                assert_eq!(w.protocol_version, 1);
                assert_eq!(w.heartbeat_interval_ms, INTERVAL.as_millis() as u64);
                d.epoch = w.epoch;
                d.map = w.map;
                Ok(d)
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    /// Opens a stream whose first message is `first`, and returns once the server has
    /// answered with headers (or refused).
    pub async fn open(
        addr: SocketAddr,
        first: daemon_message::Message,
    ) -> Result<Self, tonic::Status> {
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let (tx, rx) = unbounded();
        tx.unbounded_send(DaemonMessage {
            message: Some(first),
        })
        .expect("queue the first message");
        let inbound = WorkerClient::new(channel).session(rx).await?.into_inner();
        Ok(Self {
            tx,
            inbound,
            kept: VecDeque::new(),
            seq: 0,
            epoch: 0,
            map: None,
        })
    }

    pub fn send(&self, message: daemon_message::Message) {
        self.tx
            .unbounded_send(DaemonMessage {
                message: Some(message),
            })
            .expect("the stream is open");
    }

    pub fn send_empty(&self) {
        self.tx
            .unbounded_send(DaemonMessage { message: None })
            .expect("the stream is open");
    }

    /// Ends the stream from the daemon side.
    pub fn close(self) {}

    async fn next(&mut self) -> Option<server_message::Message> {
        if let Some(m) = self.kept.pop_front() {
            return Some(m);
        }
        let msg = timeout(PROMPT, self.inbound.message())
            .await
            .expect("a server message in time")
            .expect("the stream is healthy")?;
        Some(msg.message.expect("a non-empty server message"))
    }

    /// The first message `pick` accepts, within `within`, keeping the others.
    pub async fn expect_within<T>(
        &mut self,
        within: Duration,
        mut pick: impl FnMut(&server_message::Message) -> Option<T>,
    ) -> Option<T> {
        if let Some(at) = self.kept.iter().position(|m| pick(m).is_some()) {
            let m = self.kept.remove(at).expect("present");
            return pick(&m);
        }
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let msg = timeout(
                deadline - tokio::time::Instant::now(),
                self.inbound.message(),
            )
            .await
            .ok()?
            .expect("the stream is healthy")?;
            let m = msg.message.expect("a non-empty server message");
            if let Some(t) = pick(&m) {
                return Some(t);
            }
            self.kept.push_back(m);
        }
    }

    /// The generation of the next `MapUpdate`, within `within`.
    pub async fn map_update_within(&mut self, within: Duration) -> Option<u64> {
        self.expect_within(within, |m| match m {
            server_message::Message::MapUpdate(u) => {
                Some(u.map.as_ref().map_or(0, |m| m.generation))
            }
            _ => None,
        })
        .await
    }

    pub async fn expect<T>(
        &mut self,
        what: &str,
        pick: impl FnMut(&server_message::Message) -> Option<T>,
    ) -> T {
        self.expect_within(PROMPT, pick)
            .await
            .unwrap_or_else(|| panic!("no {what} within {PROMPT:?}"))
    }

    pub async fn offer(&mut self) -> LeaseOffer {
        self.expect("LeaseOffer", |m| match m {
            server_message::Message::LeaseOffer(o) => Some(o.clone()),
            _ => None,
        })
        .await
    }

    /// The next Start. The offer of the same lease, if kept, is dropped with it.
    pub async fn start(&mut self) -> Start {
        let start = self
            .expect("Start", |m| match m {
                server_message::Message::Start(s) => Some(s.clone()),
                _ => None,
            })
            .await;
        self.kept.retain(|m| {
            !matches!(m, server_message::Message::LeaseOffer(o) if o.lease_id == start.lease_id)
        });
        start
    }

    /// The status the server ends the stream with, within [`PROMPT`]; messages before
    /// it are dropped. Panics if the stream stays open or ends without an error.
    pub async fn ended(&mut self) -> tonic::Status {
        let ending = async {
            loop {
                match self.inbound.message().await {
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("the stream ended without an error"),
                    Err(status) => return status,
                }
            }
        };
        timeout(PROMPT, ending)
            .await
            .expect("the server ended the stream in time")
    }

    /// Waits, within [`PROMPT`], until the stream ends, by the server's status or by
    /// the connection closing; messages before that are dropped.
    pub async fn closed(&mut self) {
        let ending = async { while let Ok(Some(_)) = self.inbound.message().await {} };
        timeout(PROMPT, ending)
            .await
            .expect("the stream ended in time");
    }

    /// The lease of the next `Cancel` within [`QUIET`], if one arrives.
    pub async fn cancelled(&mut self) -> Option<LeaseId> {
        self.expect_within(QUIET, |m| match m {
            server_message::Message::Cancel(c) => Some(c.lease_id.expect("a lease id")),
            _ => None,
        })
        .await
    }

    /// Asserts no Offer or Start arrives within [`QUIET`].
    pub async fn no_work(&mut self) {
        let work = self
            .expect_within(QUIET, |m| match m {
                server_message::Message::LeaseOffer(_) | server_message::Message::Start(_) => {
                    Some(m.clone())
                }
                _ => None,
            })
            .await;
        assert_eq!(work, None, "work arrived");
    }

    /// Sends a heartbeat listing `running` and returns whether it was acknowledged
    /// within [`QUIET`].
    pub async fn heartbeat(&mut self, running: &[LeaseId]) -> bool {
        self.seq += 1;
        let seq = self.seq;
        self.send(daemon_message::Message::Heartbeat(Heartbeat {
            seq,
            report_hash: Vec::new(),
            running: running.to_vec(),
        }));
        self.expect_within(QUIET, |m| match m {
            server_message::Message::HeartbeatAck(a) if a.seq == seq => Some(()),
            _ => None,
        })
        .await
        .is_some()
    }

    /// Sends `result` and returns the server's acknowledgement.
    pub async fn report(&mut self, result: kbf_proto::worker::Result) -> ResultAck {
        let lease = result.lease_id;
        self.send(daemon_message::Message::Result(result));
        self.expect("ResultAck", |m| match m {
            server_message::Message::ResultAck(a) if a.lease_id == lease => Some(*a),
            _ => None,
        })
        .await
    }
}

/// An OK Result for `lease` carrying `result`.
pub fn ran(lease: Option<LeaseId>, result: &ActionResult) -> kbf_proto::worker::Result {
    kbf_proto::worker::Result {
        lease_id: lease,
        status: Some(rpc::Status::default()),
        action_result: Some(result.clone()),
        // As a daemon that predates the field sends it: the server checks no action.
        action_digest: None,
        memory_kill: 0,
        lost: 0,
    }
}

/// Reports `result(lease)` for `first` and for each rerun the farm's budget allows
/// (`1 + FARM_RERUNS` runs in all, each `Start` taken from `daemon`): a run the farm
/// could not do is run again, so only the last report finishes the operation.
pub async fn report_every_run(
    daemon: &mut FakeDaemon,
    first: Start,
    result: impl Fn(Option<LeaseId>) -> kbf_proto::worker::Result,
) {
    let mut start = first;
    for run in 0..=kbf_sched::FARM_RERUNS {
        assert!(
            daemon.report(result(start.lease_id)).await.accepted,
            "run {run}"
        );
        if run < kbf_sched::FARM_RERUNS {
            start = daemon.start().await;
        }
    }
}

/// A failed Result for `lease`.
pub fn failed(lease: Option<LeaseId>, code: Code) -> kbf_proto::worker::Result {
    kbf_proto::worker::Result {
        lease_id: lease,
        status: Some(rpc::Status {
            code: code as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        action_result: None,
        action_digest: None,
        memory_kill: 0,
        lost: 0,
    }
}

/// A blob: its bytes and its digest in both forms.
#[derive(Clone, Debug)]
pub struct Blob {
    pub data: Vec<u8>,
    pub digest: kbf_types::Digest,
    pub proto: reapi::Digest,
}

impl Blob {
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        let data = data.into();
        let digest = kbf_segments::sha256(&data);
        let proto = kbf_front::digest_to_proto(&digest);
        Self {
            data,
            digest,
            proto,
        }
    }

    pub fn of(message: &impl Message) -> Self {
        Self::new(message.encode_to_vec())
    }
}

/// An action and every blob it needs.
pub struct Job {
    pub action: Blob,
    pub command: Blob,
    pub root: Blob,
    pub input: Blob,
}

impl Job {
    /// A job running `argv`, with one input file, on `platform` properties.
    pub fn new(argv: &str, platform: &[(&str, &str)]) -> Self {
        let input = Blob::new(format!("input of {argv}"));
        let root = Blob::of(&Directory {
            files: vec![FileNode {
                name: "in".to_owned(),
                digest: Some(input.proto.clone()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let command = Blob::of(&Command {
            arguments: vec![argv.to_owned()],
            ..Default::default()
        });
        let action = Blob::of(&Action {
            command_digest: Some(command.proto.clone()),
            input_root_digest: Some(root.proto.clone()),
            platform: Some(Platform {
                properties: platform
                    .iter()
                    .map(|(name, value)| platform::Property {
                        name: (*name).to_owned(),
                        value: (*value).to_owned(),
                    })
                    .collect(),
            }),
            ..Default::default()
        });
        Self {
            action,
            command,
            root,
            input,
        }
    }

    pub fn blobs(&self) -> [&Blob; 4] {
        [&self.action, &self.command, &self.root, &self.input]
    }
}

/// Uploads an output and returns an ActionResult naming it, with `exit_code`.
pub async fn output(cell: &Client, text: &str, exit_code: i32) -> ActionResult {
    let out = Blob::new(text);
    cell.upload(&[&out]).await;
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(out.proto.clone()),
            ..Default::default()
        }],
        exit_code,
        ..Default::default()
    }
}

/// The ExecuteResponse of a done operation, its result as the daemon reported it
/// ([`unstamped`]).
pub fn response(op: &Operation) -> ExecuteResponse {
    let mut response = stamped_response(op);
    response.result = response.result.map(unstamped);
    response
}

/// `result` without the metadata the server adds to every accepted result (issue
/// #166), which must be there: the node id in `worker` and a `queued_timestamp`. The
/// fake daemons here report results without metadata, so what is left is what they
/// reported. `tests/execute.rs` checks the metadata's values.
pub fn unstamped(mut result: ActionResult) -> ActionResult {
    let metadata = result
        .execution_metadata
        .take()
        .unwrap_or_else(|| panic!("a result without the server's metadata: {result:?}"));
    assert!(!metadata.worker.is_empty(), "no worker: {metadata:?}");
    assert!(
        metadata.queued_timestamp.is_some(),
        "no queued_timestamp: {metadata:?}"
    );
    result
}

/// The ExecuteResponse of a done operation, as the server sent it.
pub fn stamped_response(op: &Operation) -> ExecuteResponse {
    assert!(op.done, "{op:?} is not done");
    match &op.result {
        Some(operation::Result::Response(any)) => {
            ExecuteResponse::decode(any.value.as_slice()).expect("an ExecuteResponse")
        }
        other => panic!("a done operation without a response: {other:?}"),
    }
}

/// The stage of an operation, from its metadata.
pub fn stage(op: &Operation) -> i32 {
    let any = op.metadata.as_ref().expect("metadata");
    reapi::ExecuteOperationMetadata::decode(any.value.as_slice())
        .expect("ExecuteOperationMetadata")
        .stage
}

/// Reads `ops` until the done operation, which it returns.
pub async fn done(ops: &mut Streaming<Operation>) -> Operation {
    loop {
        let op = timeout(PROMPT, ops.message())
            .await
            .expect("an operation update in time")
            .expect("a healthy stream")
            .expect("the stream ends only after the done operation");
        if op.done {
            return op;
        }
    }
}

/// Whether `ops` produces a done operation within [`QUIET`].
pub async fn done_within_quiet(ops: &mut Streaming<Operation>) -> bool {
    timeout(QUIET, done(ops)).await.is_ok()
}
