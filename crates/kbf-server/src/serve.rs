//! Serving: the REAPI listener, the worker listener, the operator API listener (if
//! any) and the scheduler tick.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::ListenerExt;
use futures::StreamExt as _;
use futures::future::Either;
use kbf_auth::{AuthenticateLayer, Policy};
use kbf_front::{
    ByteStreamService, Cache, CasService, Connections, MAX_MESSAGE_BYTES, MetaLog,
    RECORDER_CHANNEL, Recorder, RequestMetadataLayer, SegmentsConfig, SegmentsService,
};
use kbf_objstore::ObjectStore;
use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
use kbf_proto::worker::segments_server::SegmentsServer;
use kbf_proto::worker::worker_server::WorkerServer;
use kbf_sched::MemoryLadder;
use kbf_storage::service::Admission;
use kbf_tls::{Incoming, TlsSource};
use tonic::service::Routes;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use tower_layer::{Identity, Stack};

use crate::blobs::{WorkerBlobs, WorkerCas, WorkerSegments};
use crate::drain::Drain;
use crate::farm::{Farm, StateError};
use crate::grpc_health::{HealthService, WATCH_INTERVAL};
use crate::health::Readiness;
use crate::http_blobs::BlobReaders;
use crate::http_invocations::InvocationReaders;
use crate::identity::{DenyList, Peers};
use crate::members::{Members, MembershipError};
use crate::members_rpc::MembershipService;
use crate::state_store::StateStore;
use crate::streams::{StreamLimits, flow_windows};
use crate::token::ApiToken;
use crate::worker::WorkerService;
use crate::worker_roles::WorkerRolesLayer;
use kbf_mapsync::MapSource;
use kbf_proto::kbf::map::v1::membership_server::MembershipServer;

/// How the server listens and paces its daemons.
#[derive(Clone, Debug)]
pub struct Listeners {
    /// The REAPI listener (clients: buck2, Bazel).
    pub reapi: SocketAddr,
    /// TLS for the REAPI listener (a server certificate; clients present none), re-read
    /// from its files as they change. `None` serves it in plain text, on whatever
    /// address `reapi` names (nothing refuses a bind off loopback; the binary's start
    /// line warns about one).
    pub reapi_tls: Option<TlsSource>,
    /// The `kbf.worker.v1` listener: daemons' worker streams and blob calls
    /// ([`crate::blobs`]), and storage nodes' `kbf.map.v1.Membership` calls; who may
    /// call which method is [`crate::worker_roles`]'s table.
    pub worker: SocketAddr,
    /// Mutual TLS for the worker listener; `None` serves it in plain text, where no
    /// node is bound to a certificate and every blob call is refused.
    pub worker_tls: Option<WorkerTls>,
    /// The heartbeat interval `Welcome` names.
    pub heartbeat_interval: Duration,
    /// How long a new worker stream may take to send its `Hello`.
    pub hello_wait: Duration,
    /// How often the scheduler expires silent workers' leases and places queued work
    /// when no input arrives.
    pub tick: Duration,
    /// How long queued work waits while no live worker can run it (none satisfies its
    /// platform, or none that does is large enough) before it is refused
    /// FAILED_PRECONDITION.
    pub unservable_wait: Duration,
    /// The memory rungs of an action: the first is what an action that names no size
    /// books, the others are what it climbs through when it passes its own memory
    /// limit, before a whole host (`--memory-ladder-gib`).
    pub memory_ladder: MemoryLadder,
    /// How long a finished operation is kept after its callers are answered, in which
    /// WaitExecution on it still streams its result; then it is NOT_FOUND.
    pub finished_retention: Duration,
    /// How long a drain that a stop signal began waits for the REAPI listener (see
    /// [`bind_server_with_api`]) before it ends the open streams and stops anyway.
    /// [`Duration::MAX`] waits for as long as an action runs (`--shutdown-timeout-secs
    /// 0`, the flag's default): Buck2 never recovers a stream that breaks mid-action.
    pub shutdown_timeout: Duration,
    /// How long streams stay quiet, and how the connections are kept alive.
    pub streams: StreamTiming,
    /// How long `GET /readyz` on the operator API, and the REAPI listener's
    /// `grpc.health.v1` service, wait for the object store to answer their probe
    /// ([`crate::health`]).
    pub store_probe_timeout: Duration,
    /// The memory bound of the invocation records, in bytes
    /// (`docs/design/invocations.md`, section 3.5).
    pub invocation_memory: usize,
    /// The worker listener's segment uploads (`--segment-upload-*`;
    /// `docs/design/write-path.md`, section 4).
    pub segments: SegmentsConfig,
    /// The most HTTP/2 streams one connection may have open on each listener
    /// (`--reapi-max-streams`, `--worker-max-streams`).
    pub stream_limits: StreamLimits,
}

/// What keeps a long-lived stream from looking idle to a load balancer, a proxy or the
/// peer: Buck2 sends one Execute per action and never reconnects a broken one, so an
/// idle-timeout cut fails the action (komira-build-farm#791). Zero turns each off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamTiming {
    /// How often every open Execute and WaitExecution stream is sent its operation
    /// again (`--progress-interval-secs`).
    pub progress_interval: Duration,
    /// How often the REAPI and worker listeners send an HTTP/2 PING on a connection
    /// that has been quiet (`--http2-keepalive-interval-secs`).
    pub keepalive_interval: Duration,
    /// How long a PING may go unanswered before the connection is closed
    /// (`--http2-keepalive-timeout-secs`).
    pub keepalive_timeout: Duration,
}

impl Default for StreamTiming {
    fn default() -> Self {
        Self {
            progress_interval: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(30),
            keepalive_timeout: Duration::from_secs(20),
        }
    }
}

/// How long, after the shutdown timeout ends the open streams UNAVAILABLE, the server
/// waits for the clients to take it before it stops.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// The worker listener's mutual TLS: every daemon's certificate must name its node
/// (see [`crate::identity`]).
#[derive(Clone, Debug)]
pub struct WorkerTls {
    /// The listener's identity and the client CA daemons' certificates chain to, both
    /// re-read from their files as they change.
    pub server: TlsSource,
    /// The certificates and nodes refused, read again at every check.
    pub deny_list: Option<DenyList>,
}

/// The operator API ([`crate::api`]): where it listens, and the token its writes need.
#[derive(Clone, Debug)]
pub struct Api {
    /// The listener.
    pub listen: SocketAddr,
    /// The token writes must present; `None` turns writes off (reads still answer).
    pub token: Option<ApiToken>,
    /// The node's one cache, whose fill `/metrics` reports, when the process runs a
    /// daemon whose input cache shares the disk with this server's store
    /// (`kbf-all-in-one`).
    pub local_cache: Option<Arc<kbf_storage::local_cache::LocalCache>>,
}

/// Why the server could not start or stopped.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// A listener could not be bound.
    #[error("bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    /// The transport failed.
    #[error(transparent)]
    Transport(#[from] tonic::transport::Error),
    /// The operator API listener failed.
    #[error("operator API: {0}")]
    Api(#[source] std::io::Error),
    /// The scheduler's state store could not be taken up.
    #[error("--state-store: {0}")]
    State(#[from] StateError),
    /// The storage map could not be read from the state store.
    #[error("storage map: {0}")]
    Map(#[from] MembershipError),
}

/// A server that is listening: the addresses it bound, and the future that serves
/// until shutdown.
pub struct Bound<F> {
    /// Where the REAPI listener is.
    pub reapi: SocketAddr,
    /// Where the worker listener is.
    pub worker: SocketAddr,
    /// Where the operator API listener is, if there is one.
    pub api: Option<SocketAddr>,
    /// What `GET /readyz` reads besides the store; the serving future marks it
    /// stopping when `shutdown` completes, and a drain ([`Bound::drain`]) does.
    pub readiness: Arc<Readiness>,
    /// The frontend's drain: begun when `shutdown` completes or by
    /// `POST /v1/frontend/drain`, and escalated by [`Drain::close`], which a second
    /// stop signal calls (`ServerHandle::serve_escalating`).
    pub drain: Arc<Drain>,
    /// Serves every listener and the tick, and runs the invocation recorder's task,
    /// until the shutdown future completes.
    pub serving: F,
    /// The invocation records the serving future builds, which the build pages read
    /// ([`crate::http_invocations`]); tests feed it events.
    pub recorder: Recorder,
}

/// Aborts its task when dropped.
pub(crate) struct AbortOnDrop(pub(crate) tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Binds a gRPC listener whose accepted connections have Nagle's algorithm off
/// (`TCP_NODELAY`). With it on, a small reply written while an earlier small write is
/// unacknowledged waits for the client's delayed ACK, about 40 ms on Linux, so each
/// sequential small call pays that. tonic's `Server::tcp_nodelay` does not apply here:
/// `serve_with_incoming` takes the options of the incoming stream it is given.
pub(crate) fn bind(addr: SocketAddr) -> Result<(TcpIncoming, SocketAddr), ServeError> {
    let bound = TcpIncoming::bind(addr).and_then(|incoming| {
        let local = incoming.local_addr()?;
        Ok((incoming.with_nodelay(Some(true)), local))
    });
    bound.map_err(|source| ServeError::Bind { addr, source })
}

/// Serves `routes` on `listener` until it fails, or until `stop` completes and the
/// requests in flight are answered; without a listener, it only waits for `stop`.
pub(crate) fn serve_api(
    listener: Option<tokio::net::TcpListener>,
    routes: axum::Router,
    stop: impl Future<Output = ()> + Send + 'static,
) -> impl Future<Output = std::io::Result<()>> {
    match listener {
        Some(listener) => Either::Left(
            axum::serve(
                listener.tap_io(set_nodelay),
                routes.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(stop)
            .into_future(),
        ),
        // Nothing to answer: done as soon as it is told to stop, never before.
        None => Either::Right(async move {
            stop.await;
            Ok(())
        }),
    }
}

/// Turns Nagle's algorithm off on a connection the operator API accepted, as [`bind`]
/// does for the gRPC listeners. The call fails only on a socket that is already
/// unusable, whose connection then fails on its own, so its error is not kept.
fn set_nodelay(tcp: &mut tokio::net::TcpStream) {
    let _ = tcp.set_nodelay(true);
}

pub(crate) fn bind_api(
    addr: SocketAddr,
) -> Result<(tokio::net::TcpListener, SocketAddr), ServeError> {
    let bound = std::net::TcpListener::bind(addr).and_then(|listener| {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let local = listener.local_addr()?;
        Ok((listener, local))
    });
    bound.map_err(|source| ServeError::Bind { addr, source })
}

/// The worker listener's server: every call goes through the per-method role table
/// ([`crate::worker_roles`]) before it is routed.
pub(crate) type WorkerListener = Server<Stack<WorkerRolesLayer, Identity>>;

/// The worker listener's role table, its TLS source, and the way its peers are told
/// apart. Every worker listener is made here, so none serves a call the table does not
/// list. Not generic, so both arms are one set of lines whatever the cache types (a
/// per-instantiation copy that misses one arm would count as missed lines).
pub(crate) fn worker_peers(
    server: Server,
    tls: Option<WorkerTls>,
) -> (WorkerListener, Peers, Option<TlsSource>) {
    match tls {
        Some(tls) => (
            server.layer(WorkerRolesLayer::new(Admission::Certified)),
            Peers::Certified {
                deny_list: tls.deny_list,
            },
            Some(tls.server),
        ),
        None => (
            server.layer(WorkerRolesLayer::new(Admission::Open)),
            Peers::Unauthenticated,
            None,
        ),
    }
}

/// The accepted connections of the worker listener, each registered in `connections`
/// until it closes, so an upload is dropped when the connection that began it closes.
/// Not generic, for the same reason as [`worker_peers`].
pub(crate) fn tracked(
    incoming: Incoming,
    connections: Connections,
) -> impl futures::Stream<Item = std::io::Result<kbf_front::Tracked<kbf_tls::Conn>>> {
    incoming.map(move |accepted| {
        accepted.map(|conn| {
            let peer = conn.remote_addr();
            connections.track_peer(conn, peer)
        })
    })
}

/// `server` with the HTTP/2 limits a listener states rather than leaves to the
/// transport's defaults: at most `max_streams` open streams per connection (each
/// listener its own, [`StreamLimits`]), and the stream and connection windows
/// ([`flow_windows`]), which bound what is held outside `--write-bytes-in-flight`. The
/// connection window holds every stream's window at once, which is what lets a Write
/// admitted after others finish at all.
pub(crate) fn flow_control(server: Server, max_streams: u32) -> Server {
    let windows = flow_windows(max_streams);
    server
        .max_concurrent_streams(Some(windows.streams))
        .initial_stream_window_size(Some(windows.stream))
        .initial_connection_window_size(Some(windows.connection))
}

/// `server` pinging a quiet connection every `timing.keepalive_interval` and closing it
/// when a ping stays unanswered for `timing.keepalive_timeout` (HTTP/2 PING, which a
/// peer must answer even when it never sends data).
pub(crate) fn keepalive(server: Server, timing: StreamTiming) -> Server {
    let on = !timing.keepalive_interval.is_zero();
    server
        .http2_keepalive_interval(on.then_some(timing.keepalive_interval))
        .http2_keepalive_timeout(on.then_some(timing.keepalive_timeout))
}

/// The warning every start logs ([`bind_server_with_api`]).
const STATE_IN_MEMORY: &str = "the scheduler's state is in memory: no cordon, drain, lease or \
    operation of an earlier server process is restored, and the server keeps no rollout record";

/// The members a frontend with a state store keeps. Not generic, for the same reason as
/// [`sweeping`].
pub(crate) fn members_of(state: Option<&crate::StateStore>) -> Option<Arc<Members>> {
    let state = state?;
    Some(Arc::new(Members::new(
        Arc::clone(&state.store),
        state.members,
    )))
}

/// The storage map daemons are sent: `explicit` if a caller gave one, else the members'
/// (every frontend with a state store has members), else none (a farm of one member, whose
/// daemons use the cache they were given). Not generic, for the same reason as
/// [`sweeping`].
///
/// # Errors
/// The state store failing to read the map.
pub(crate) fn map_of(
    explicit: Option<Arc<dyn MapSource>>,
    members: Option<&Arc<Members>>,
) -> Result<Option<Arc<dyn MapSource>>, MembershipError> {
    if explicit.is_some() {
        return Ok(explicit);
    }
    members.map(Members::map_source).transpose()
}

/// The membership service storage nodes register with, when there are members to keep:
/// under worker TLS only the storage role may call it. Not generic, for the same reason
/// as [`sweeping`].
pub(crate) fn membership(
    members: Option<Arc<Members>>,
    certified: bool,
) -> Option<MembershipServer<MembershipService>> {
    let admission = if certified {
        Admission::Certified
    } else {
        Admission::Open
    };
    Some(MembershipService::new(members?, admission).server())
}

/// Expires members while the server runs. Never completes; with no state store there
/// are none, and it waits forever. A function of its own and not generic, so the tests
/// measure one copy of these lines and not the best of the server's instantiations.
///
/// Builds the future and does not await it here: a function that awaits a future that
/// never completes has a closing line no test can reach.
pub(crate) fn sweeping(members: Option<Arc<Members>>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    match members {
        Some(members) => Box::pin(crate::members_rpc::sweep(members)),
        None => Box::pin(std::future::pending()),
    }
}

/// Logs where the scheduler's state is, at every start. Not generic, unlike the binding
/// that calls it, so every test binary's runs count toward the same function's coverage.
pub(crate) fn log_state(durable: bool, term: u64, epoch: u64) {
    if durable {
        tracing::info!(term, epoch, "the scheduler's state is in the state store");
    } else {
        tracing::warn!(term, epoch, "{STATE_IN_MEMORY}");
    }
}

/// Stops the operator API once the REAPI listener is done: `api_serve` (see
/// [`serve_api`]) answers the requests in flight, the one that began a drain on an idle
/// frontend among them (its response would otherwise race the process's exit), and
/// stops. Bounded: a client that holds its connection open must not hold the exit.
pub(crate) async fn stop_api(
    stop: tokio::sync::oneshot::Sender<()>,
    api_serve: impl Future<Output = std::io::Result<()>>,
) {
    let _ = stop.send(());
    let _ = tokio::time::timeout(CLOSE_GRACE, api_serve).await;
}

/// What both `kbf-server` and `kbf frontend` do once their shutdown future completes, so
/// the two cannot drift: [`Drain::begin`] (`/readyz` answers `stopping`, new Execute
/// calls are refused while the open ones run until their action is done, the REAPI
/// listener is sent GOAWAY; a no-op when `POST /v1/frontend/drain` began it), and then
/// only `timeout`, or a [`Drain::close`] from a second stop signal, ends the open streams
/// UNAVAILABLE (Buck2 does not recover a stream that breaks). Returns once the grace after
/// that has passed, which is when the caller stops anyway.
pub(crate) async fn drain_and_stop(drain: &Drain, timeout: Duration) {
    drain.begin();
    tokio::select! {
        () = tokio::time::sleep(timeout) => drain.close(),
        () = drain.closed() => {}
    }
    tokio::time::sleep(CLOSE_GRACE).await;
    tracing::warn!(
        ?timeout,
        "REAPI connections still open after the drain ran out of time or was ended; stopping anyway"
    );
}

/// Binds the REAPI and worker listeners for a farm over `cache`, and no operator API.
/// Nothing is served until the returned future runs; it serves until `shutdown`
/// completes, or a listener fails.
///
/// # Errors
/// A listener cannot be bound, or a TLS configuration is refused (a key that does not
/// match its certificate, say).
pub fn bind_server<M, O>(
    cache: Arc<Cache<M, O>>,
    listeners: Listeners,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Bound<impl Future<Output = Result<(), ServeError>>>, ServeError>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    bind_server_with_api(cache, listeners, None, shutdown)
}

/// [`bind_server`], and the operator API ([`crate::api`]) if `api` is given.
///
/// The REAPI listener accepts and allows every call; [`bind_server_with_policy`] runs a
/// policy instead.
///
/// It logs a warning that the scheduler's state (cordons, drains, leases, operations)
/// starts empty, as it does at every start: nothing of an earlier process is restored.
///
/// The REAPI listener also serves `grpc.health.v1.Health` ([`crate::grpc_health`]),
/// with `/readyz`'s answer.
///
/// When `shutdown` completes (or `POST /v1/frontend/drain` is called: see [`Drain`]),
/// `GET /readyz` answers 503 (`stopping`) from then on and the health service
/// `NOT_SERVING` (an open Watch is sent it, then ends UNAVAILABLE); new Execute calls
/// are refused UNAVAILABLE, which Buck2 retries when it opens the call; and the REAPI
/// listener stops accepting and every connection on it is sent GOAWAY. Open Execute and
/// WaitExecution streams are kept until their action is done: Buck2 does not recover a
/// stream that breaks (komira-build-farm#791), so ending one fails its action. The
/// serving future returns once the REAPI connections have closed. Only
/// `listeners.shutdown_timeout` (when it is not [`Duration::MAX`]) or [`Drain::close`]
/// (a second stop signal) ends the streams that are not done, UNAVAILABLE, after which
/// the future returns within a short grace even if a connection stays open (a client
/// still uploading, say). Until then the worker listener, the operator API and the
/// tick go on; once the REAPI listener is done the operator API finishes the requests
/// it is answering and stops. Daemon streams never end on their own, so the worker
/// listener is not drained: it stops when the future returns.
///
/// # Errors
/// A listener cannot be bound, or a TLS configuration is refused (a key that does not
/// match its certificate, say).
pub fn bind_server_with_api<M, O>(
    cache: Arc<Cache<M, O>>,
    listeners: Listeners,
    api: Option<Api>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Bound<impl Future<Output = Result<(), ServeError>>>, ServeError>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    bind_server_with_policy(cache, listeners, api, Policy::allow_all(), shutdown)
}

/// [`bind_server_with_api`], with the REAPI listener running `policy`
/// (`docs/reapi-auth.md`): its authenticator on every call of that listener, before
/// routing, and its authorizers in the services. The worker listener, the operator
/// API and the REAPI listener's `grpc.health.v1` service do not run it: a proxy's
/// health check carries no credentials. The exceptions are the operator API's build
/// pages ([`crate::http_invocations`]) and blob route ([`crate::http_blobs`]), which
/// run its authenticator and its `invocations.getAuthorizer` and
/// `blobs.getAuthorizer`: the pages are off when the policy names no
/// `invocations.getAuthorizer`, and the blob route when it names either not.
///
/// # Errors
/// A listener cannot be bound, or a TLS configuration is refused (a key that does not
/// match its certificate, say).
pub fn bind_server_with_policy<M, O>(
    cache: Arc<Cache<M, O>>,
    listeners: Listeners,
    api: Option<Api>,
    policy: Policy,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Bound<impl Future<Output = Result<(), ServeError>>>, ServeError>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    bind_server_with_options(cache, listeners, api, policy, None, None, None, shutdown)
}

/// [`bind_server_with_policy`], with the scheduler's state kept in `state` if given
/// (`--state-store`): see [`Farm::with_state`].
///
/// # Errors
/// As [`bind_server_with_policy`], and the state store cannot be read.
pub fn bind_server_with_state<M, O>(
    cache: Arc<Cache<M, O>>,
    listeners: Listeners,
    api: Option<Api>,
    policy: Policy,
    state: Option<StateStore>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Bound<impl Future<Output = Result<(), ServeError>>>, ServeError>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    bind_server_with_options(cache, listeners, api, policy, state, None, None, shutdown)
}

/// [`bind_server_with_state`], sending daemons the storage map `map` holds if given: in
/// `Welcome`, and in a `MapUpdate` each time it changes ([`WorkerService::with_map`]).
///
/// # Errors
/// As [`bind_server_with_state`].
pub fn bind_server_with_map<M, O>(
    cache: Arc<Cache<M, O>>,
    listeners: Listeners,
    api: Option<Api>,
    policy: Policy,
    state: Option<StateStore>,
    map: Option<Arc<dyn MapSource>>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Bound<impl Future<Output = Result<(), ServeError>>>, ServeError>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    bind_server_with_options(cache, listeners, api, policy, state, None, map, shutdown)
}

/// [`bind_server_with_policy`], and with a `storage` node (`--storage`) the REAPI
/// listener proxies the client's CAS, ByteStream and action-cache calls to it
/// ([`kbf_front::Proxy`]) instead of answering them from `cache`.
///
/// # Errors
/// As [`bind_server_with_policy`].
pub fn bind_server_with_storage<M, O>(
    cache: Arc<Cache<M, O>>,
    listeners: Listeners,
    api: Option<Api>,
    policy: Policy,
    storage: Option<tonic::transport::Endpoint>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Bound<impl Future<Output = Result<(), ServeError>>>, ServeError>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    bind_server_with_options(cache, listeners, api, policy, None, storage, None, shutdown)
}

/// [`bind_server_with_policy`] with all of its options: the scheduler's state in
/// `state` ([`bind_server_with_state`]), the client's CAS, ByteStream and
/// action-cache calls proxied to a `storage` node ([`bind_server_with_storage`]), and
/// the storage map `map` sent to daemons ([`bind_server_with_map`]).
///
/// # Errors
/// As [`bind_server_with_state`].
#[expect(
    clippy::too_many_arguments,
    reason = "every option the bind functions delegate"
)]
pub fn bind_server_with_options<M, O>(
    cache: Arc<Cache<M, O>>,
    listeners: Listeners,
    api: Option<Api>,
    policy: Policy,
    state: Option<StateStore>,
    storage: Option<tonic::transport::Endpoint>,
    map: Option<Arc<dyn MapSource>>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Bound<impl Future<Output = Result<(), ServeError>>>, ServeError>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let (recorder, recording) = Recorder::new(listeners.invocation_memory, RECORDER_CHANNEL);
    let (state, history) = crate::history::take(state);
    let recording = crate::history::install(recording, history.as_ref());
    let default_memory = listeners.memory_ladder.first();
    let farm = Farm::new(
        Arc::clone(&cache),
        listeners.unservable_wait,
        listeners.finished_retention,
    )
    .with_memory_ladder(listeners.memory_ladder.clone())
    .recording(recorder.clone());
    let durable = state.is_some();
    // Storage nodes register with a frontend that has a state store to keep them in.
    let members = members_of(state.as_ref());
    let map = map_of(map, members.as_ref())?;
    let farm = Arc::new(farm.with_state(state)?);
    let (reapi_incoming, reapi) = bind(listeners.reapi)?;
    let (worker_incoming, worker) = bind(listeners.worker)?;
    let peers_certified = listeners.worker_tls.is_some();
    let (mut worker_server, peers, worker_tls) = worker_peers(
        keepalive(
            flow_control(Server::builder(), listeners.stream_limits.worker),
            listeners.streams,
        ),
        listeners.worker_tls,
    );
    let tls_sources: Vec<TlsSource> = listeners
        .reapi_tls
        .iter()
        .chain(&worker_tls)
        .cloned()
        .collect();
    let reapi_incoming = Incoming::new(reapi_incoming, listeners.reapi_tls);
    let worker_incoming = Incoming::new(worker_incoming, worker_tls);
    let probe_timeout = listeners.store_probe_timeout;
    let (api_listen, token, local) = api.map_or((None, None, None), |api| {
        (Some(api.listen), api.token, api.local_cache)
    });
    let api_listener = api_listen.map(bind_api).transpose()?;
    let api = api_listener.as_ref().map(|(_, local)| *local);
    let readiness = Arc::new(Readiness::default());
    let (closer, closing) = kbf_front::closing();
    let (drain, goaway) = Drain::new(closer, Arc::clone(&readiness));
    let drain = Arc::new(drain);
    let health_routes =
        crate::health::router(Arc::clone(&cache), Arc::clone(&readiness), probe_timeout);
    let blob_routes =
        crate::http_blobs::router(Arc::clone(&cache), BlobReaders::from_policy(&policy));
    let page_routes = crate::http_invocations::router(
        recorder.clone(),
        Arc::clone(&cache),
        InvocationReaders::from_policy(&policy),
    );
    let api_routes = crate::api::router(
        Arc::clone(&farm),
        token,
        Arc::clone(&drain),
        members.clone(),
    )
    .merge(health_routes)
    .merge(crate::metrics::router(
        Arc::clone(&cache),
        local,
        history.as_ref().map(crate::history::History::stats),
        tls_sources,
    ))
    .merge(blob_routes)
    .merge(page_routes);
    // Logged at every start, the first one too (issue #156).
    let (term, epoch) = (farm.term(), farm.lease_epoch());
    log_state(durable, term, epoch);

    let mut reapi_server = keepalive(
        flow_control(Server::builder(), listeners.stream_limits.reapi),
        listeners.streams,
    );
    let worker_blobs = ByteStreamServer::new(WorkerBlobs::new(
        ByteStreamService::new(Arc::clone(&cache)),
        peers.clone(),
    ))
    .max_decoding_message_size(MAX_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let worker_cas = ContentAddressableStorageServer::new(WorkerCas::new(
        CasService::new(Arc::clone(&cache)),
        peers.clone(),
    ))
    .max_decoding_message_size(MAX_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_MESSAGE_BYTES);
    // Each accepted worker connection is tracked, so an upload is dropped when the
    // connection that began it closes.
    let connections = Connections::new();
    let tracking = connections.clone();
    let worker_incoming = tracked(worker_incoming, tracking);
    let worker_segments = SegmentsServer::new(WorkerSegments::new(
        SegmentsService::new(Arc::clone(&cache), listeners.segments, connections),
        peers.clone(),
    ))
    .max_decoding_message_size(MAX_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let sessions = WorkerService::new(
        Arc::clone(&farm),
        peers,
        listeners.heartbeat_interval,
        listeners.hello_wait,
    );
    // The map, if there is one: no branch in this generic function.
    let sessions = map.into_iter().fold(sessions, WorkerService::with_map);
    let worker_service = WorkerServer::new(sessions)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let membership = membership(members.clone(), peers_certified);
    let (ticker, progress) = kbf_front::progress(listeners.streams.progress_interval);
    let authorizers = Arc::new(policy.authorizers);
    let cache_for_health = Arc::clone(&cache);
    let reapi_routes = match storage {
        Some(node) => kbf_front::routes_proxied(
            &kbf_front::Proxy::to_endpoint(&node, Arc::clone(&authorizers)),
            cache,
            Arc::clone(&farm),
            kbf_front::OpenStreams { closing, progress },
            authorizers,
            &recorder,
            default_memory,
        ),
        None => kbf_front::routes_recorded(
            cache,
            Arc::clone(&farm),
            closing,
            progress,
            authorizers,
            &recorder,
            default_memory,
        ),
    };
    let authenticate = AuthenticateLayer::new(policy.authenticator);
    // The layers wrap the REAPI routes (and the fallback for unknown paths) present
    // when they are applied; the health service, added after, is answered without
    // them. The RequestMetadata layer runs first and never refuses a call.
    let health = HealthServer::new(HealthService::new(
        cache_for_health,
        Arc::clone(&readiness),
        probe_timeout,
        WATCH_INTERVAL,
    ));
    let reapi_routes = Routes::from(
        reapi_routes
            .into_axum_router()
            .layer(authenticate)
            .layer(RequestMetadataLayer::new()),
    )
    .add_service(health);

    let stopper = Arc::clone(&drain);
    let serving = async move {
        // A task of its own: applying events never delays the tick or an accept loop,
        // and they never delay it. It stops when the serving future does.
        let history = crate::history::spawn(history);
        let _recording = AbortOnDrop(tokio::spawn(recording.run()).abort_handle());
        // `reapi_incoming` carries the REAPI TLS when there is any, so the health
        // service is served over the same TLS as the REAPI routes; the authenticator is
        // already applied to those routes alone (above).
        let reapi_serve = reapi_server
            .add_routes(reapi_routes)
            .serve_with_incoming_shutdown(reapi_incoming, async {
                let _ = goaway.await;
            });
        let worker_serve = worker_server
            .add_service(worker_service)
            .add_service(worker_blobs)
            .add_service(worker_cas)
            .add_service(worker_segments)
            .add_optional_service(membership)
            .serve_with_incoming(worker_incoming);
        // Told when the REAPI listener is done, so the operator API answers what it is
        // answering (the request that began a drain on an idle frontend, whose
        // response would otherwise race the process's exit) and then stops.
        let (api_stop, api_stopped) = tokio::sync::oneshot::channel::<()>();
        let api_serve = serve_api(
            api_listener.map(|(listener, _)| listener),
            api_routes,
            async {
                let _ = api_stopped.await;
            },
        );
        tokio::pin!(api_serve);
        let sweeping = sweeping(members);
        let ticking = async {
            let mut tick = tokio::time::interval(listeners.tick);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                farm.tick().await;
            }
        };
        // Completes when the drain has run out of time or was forced: the REAPI
        // listener, drained in time, completes first.
        let stopping = async {
            shutdown.await;
            // A no-op when `POST /v1/frontend/drain` began the drain already.
            drain_and_stop(&stopper, listeners.shutdown_timeout).await;
        };
        let mut reapi_done = false;
        let outcome = tokio::select! {
            served = reapi_serve => {
                reapi_done = true;
                served.map_err(ServeError::from)
            }
            served = worker_serve => served.map_err(ServeError::from),
            // Finished on its own: there is nothing left to answer, so it is not waited
            // for again below (`reapi_done` stays false).
            served = &mut api_serve => served.map_err(ServeError::Api),
            () = ticking => Ok(()),
            () = sweeping => Ok(()),
            // One timer for every open stream's progress operation.
            () = ticker.run() => Ok(()),
            () = stopping => Ok(()),
        };
        if reapi_done {
            stop_api(api_stop, api_serve).await;
        }
        crate::history::finish(history).await;
        outcome
    };
    Ok(Bound {
        reapi,
        worker,
        api,
        readiness,
        drain,
        serving,
        recorder,
    })
}

/// Where a storage node listens (`kbf storage`) and what it keeps.
#[derive(Clone, Debug)]
pub struct StorageListener {
    /// The listener.
    pub listen: SocketAddr,
    /// Mutual TLS (the server's identity and the client CA), re-read from its files as
    /// they change; `None` serves plain text, where no caller has a role and every call
    /// is admitted (`kbf storage` builds this only for a loopback address:
    /// [`crate::storage_node::StorageFlags::listener`]).
    pub tls: Option<TlsSource>,
    /// What the service keeps.
    pub service: kbf_storage::service::Config,
}

impl StorageListener {
    /// The service over `storage` this listener serves: callers admitted by the role in
    /// their certificates with mutual TLS, every caller without it.
    #[must_use]
    pub fn service(
        &self,
        storage: Arc<dyn kbf_storage::Storage>,
    ) -> kbf_storage::service::StorageService {
        let admission = match self.tls {
            Some(_) => kbf_storage::service::Admission::Certified,
            None => kbf_storage::service::Admission::Open,
        };
        kbf_storage::service::StorageService::new(storage, admission, self.service)
    }
}

/// A storage node that is listening.
pub struct BoundStorage<F> {
    /// Where the listener is.
    pub addr: SocketAddr,
    /// Serves until the shutdown future completes, then until the calls in flight end.
    pub serving: F,
}

/// Binds the storage service ([`kbf_storage::service`]) over `storage`: ByteStream, the
/// CAS `FindMissingBlobs`, `Segments` and the action store, on one listener, each call
/// admitted by the role in the caller's certificate when there is mutual TLS.
///
/// With TLS, call it inside a Tokio runtime: the task that looks at the TLS files starts
/// here ([`Incoming::new`]).
///
/// # Errors
/// The listener cannot be bound.
pub fn bind_storage(
    storage: Arc<dyn kbf_storage::Storage>,
    listener: StorageListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<BoundStorage<impl Future<Output = Result<(), ServeError>>>, ServeError> {
    let service = listener.service(storage);
    bind_service(service, listener.listen, listener.tls, shutdown).map(|(bound, _)| bound)
}

/// [`bind_storage`] over a service the caller has set up: a node that is one of several
/// gives it the map it judges calls by and its chain ([`crate::replication`]). `tls` must be
/// what the service's admission was made from ([`StorageListener::service`]). Also returns
/// the [`kbf_storage::service::InProcess`] view of the very service the listener serves: a
/// caller in this process (the daemon half of `kbf storage-and-daemon`) reaches the node
/// through it and takes the path a call over the listener takes, the service's own map
/// check and chain included, not the inner storage.
///
/// # Errors
/// As [`bind_storage`].
pub fn bind_service(
    service: kbf_storage::service::StorageService,
    listen: SocketAddr,
    tls: Option<TlsSource>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<
    (
        BoundStorage<impl Future<Output = Result<(), ServeError>>>,
        kbf_storage::service::InProcess,
    ),
    ServeError,
> {
    serve_service(service, bind_storage_socket(listen)?, tls, shutdown)
}

/// A storage listener's socket: bound, so its address is known and callers queue on it,
/// and not yet serving ([`bind_storage_socket`], then [`serve_service`]).
pub struct StorageSocket {
    incoming: TcpIncoming,
    addr: SocketAddr,
}

impl StorageSocket {
    /// Where the socket is bound.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// Binds `listen`, serving nothing yet: a storage node registers with its frontend from
/// the bound address before it opens the cache it serves.
///
/// # Errors
/// The listener cannot be bound.
pub fn bind_storage_socket(listen: SocketAddr) -> Result<StorageSocket, ServeError> {
    let (incoming, addr) = bind(listen)?;
    Ok(StorageSocket { incoming, addr })
}

/// [`bind_service`] over a socket already bound ([`bind_storage_socket`]).
///
/// # Errors
/// As [`bind_storage`].
pub fn serve_service(
    service: kbf_storage::service::StorageService,
    socket: StorageSocket,
    tls: Option<TlsSource>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<
    (
        BoundStorage<impl Future<Output = Result<(), ServeError>>>,
        kbf_storage::service::InProcess,
    ),
    ServeError,
> {
    let StorageSocket { incoming, addr } = socket;
    let mut server = flow_control(
        Server::builder(),
        crate::streams::StreamLimits::DEFAULT.worker,
    );
    let incoming = Incoming::new(incoming, tls);
    let service = Arc::new(service);
    let (routes, in_process) = (service.shared_routes(), service.in_process());
    let serving = async move {
        server
            .add_routes(routes)
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await
            .map_err(ServeError::from)
    };
    Ok((BoundStorage { addr, serving }, in_process))
}
