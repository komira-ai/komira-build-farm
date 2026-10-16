//! Serving: the REAPI listener, the worker listener, the operator API listener (if
//! any) and the scheduler tick.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::ListenerExt;
use futures::StreamExt as _;
use futures::future::Either;
use kbf_auth::{AuthenticateLayer, Policy};
use kbf_front::{
    ByteStreamService, CONNECTION_WINDOW_BYTES, Cache, CasService, Connections,
    MAX_CONCURRENT_STREAMS, MAX_MESSAGE_BYTES, MetaLog, RECORDER_CHANNEL, Recorder,
    RequestMetadataLayer, STREAM_WINDOW_BYTES, SegmentsConfig, SegmentsService,
};
use kbf_objstore::ObjectStore;
use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
use kbf_proto::worker::segments_server::SegmentsServer;
use kbf_proto::worker::worker_server::WorkerServer;
use kbf_sched::MemoryLadder;
use tonic::service::Routes;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Server, ServerTlsConfig};

use crate::blobs::{WorkerBlobs, WorkerCas, WorkerSegments};
use crate::farm::Farm;
use crate::grpc_health::{HealthService, WATCH_INTERVAL};
use crate::health::Readiness;
use crate::http_blobs::BlobReaders;
use crate::http_invocations::InvocationReaders;
use crate::identity::{DenyList, Peers};
use crate::token::ApiToken;
use crate::worker::{LocalWorker, WorkerService};

/// How the server listens and paces its daemons.
#[derive(Clone, Debug)]
pub struct Listeners {
    /// The REAPI listener (clients: buck2, Bazel).
    pub reapi: SocketAddr,
    /// TLS for the REAPI listener (a server certificate; clients present none). `None`
    /// serves it in plain text, on whatever address `reapi` names (nothing refuses a
    /// bind off loopback; the binary's start line warns about one).
    pub reapi_tls: Option<ServerTlsConfig>,
    /// The `kbf.worker.v1` listener (daemons): their worker streams, and `ByteStream`
    /// for their blobs ([`crate::blobs`]).
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
    /// How long shutdown waits for the REAPI listener to drain (see
    /// [`bind_server_with_api`]) before it stops anyway.
    pub shutdown_timeout: Duration,
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
}

/// The worker listener's mutual TLS: every daemon's certificate must name its node
/// (see [`crate::identity`]).
#[derive(Clone, Debug)]
pub struct WorkerTls {
    /// The listener's identity and the client CA daemons' certificates chain to.
    pub server: ServerTlsConfig,
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
    /// stopping when `shutdown` completes.
    pub readiness: Arc<Readiness>,
    /// Serves every listener and the tick, and runs the invocation recorder's task,
    /// until the shutdown future completes.
    pub serving: F,
    /// The invocation records the serving future builds, which the build pages read
    /// ([`crate::http_invocations`]); tests feed it events.
    pub recorder: Recorder,
    /// The worker service as calls in this process, for a daemon run in it, beside the
    /// worker listener's remote daemons.
    pub local: LocalWorker,
}

/// Aborts its task when dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

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
fn bind(addr: SocketAddr) -> Result<(TcpIncoming, SocketAddr), ServeError> {
    let bound = TcpIncoming::bind(addr).and_then(|incoming| {
        let local = incoming.local_addr()?;
        Ok((incoming.with_nodelay(Some(true)), local))
    });
    bound.map_err(|source| ServeError::Bind { addr, source })
}

/// Serves `routes` on `listener` until it fails; without a listener, never completes.
fn serve_api(
    listener: Option<tokio::net::TcpListener>,
    routes: axum::Router,
) -> impl Future<Output = std::io::Result<()>> {
    match listener {
        Some(listener) => Either::Left(
            axum::serve(
                listener.tap_io(set_nodelay),
                routes.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .into_future(),
        ),
        None => Either::Right(std::future::pending()),
    }
}

/// Turns Nagle's algorithm off on a connection the operator API accepted, as [`bind`]
/// does for the gRPC listeners. The call fails only on a socket that is already
/// unusable, whose connection then fails on its own, so its error is not kept.
fn set_nodelay(tcp: &mut tokio::net::TcpStream) {
    let _ = tcp.set_nodelay(true);
}

fn bind_api(addr: SocketAddr) -> Result<(tokio::net::TcpListener, SocketAddr), ServeError> {
    let bound = std::net::TcpListener::bind(addr).and_then(|listener| {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let local = listener.local_addr()?;
        Ok((listener, local))
    });
    bound.map_err(|source| ServeError::Bind { addr, source })
}

/// `server` with the HTTP/2 limits both listeners state rather than leave to the
/// transport's defaults: they bound what is held outside `--write-bytes-in-flight`, and
/// the connection window is what lets a Write admitted after others finish at all
/// (`kbf_front::CONNECTION_WINDOW_BYTES`).
fn flow_control(server: Server) -> Server {
    server
        .max_concurrent_streams(Some(MAX_CONCURRENT_STREAMS))
        .initial_stream_window_size(Some(STREAM_WINDOW_BYTES))
        .initial_connection_window_size(Some(CONNECTION_WINDOW_BYTES))
}

/// The warning every start logs ([`bind_server_with_api`]).
const STATE_IN_MEMORY: &str = "the scheduler's state is in memory: no cordon, drain, lease or \
    operation of an earlier server process is restored, and the server keeps no rollout record";

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
/// When `shutdown` completes, `GET /readyz` answers 503 (`stopping`) from then on and
/// the health service `NOT_SERVING` (an open Watch is sent it, then ends UNAVAILABLE);
/// then the REAPI listener stops accepting, every connection on
/// it is sent GOAWAY, and every open Execute and WaitExecution stream that is not done
/// ends UNAVAILABLE, which clients retry (issue #168). The serving future returns once
/// the REAPI connections have closed, or after `listeners.shutdown_timeout` if one
/// stays open (a client still uploading, say); until then the worker listener, the
/// operator API and the tick go on. Daemon streams never end on their own, so the
/// worker listener is not drained: it stops when the future returns.
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
    let (recorder, recording) = Recorder::new(listeners.invocation_memory, RECORDER_CHANNEL);
    let default_memory = listeners.memory_ladder.first();
    let farm = Arc::new(
        Farm::new(
            Arc::clone(&cache),
            listeners.unservable_wait,
            listeners.finished_retention,
        )
        .with_memory_ladder(listeners.memory_ladder.clone())
        .recording(recorder.clone()),
    );
    let (reapi_incoming, reapi) = bind(listeners.reapi)?;
    let (worker_incoming, worker) = bind(listeners.worker)?;
    let probe_timeout = listeners.store_probe_timeout;
    let (api_listen, token) = api.map_or((None, None), |api| (Some(api.listen), api.token));
    let api_listener = api_listen.map(bind_api).transpose()?;
    let api = api_listener.as_ref().map(|(_, local)| *local);
    let readiness = Arc::new(Readiness::default());
    let health_routes =
        crate::health::router(Arc::clone(&cache), Arc::clone(&readiness), probe_timeout);
    let blob_routes =
        crate::http_blobs::router(Arc::clone(&cache), BlobReaders::from_policy(&policy));
    let page_routes = crate::http_invocations::router(
        recorder.clone(),
        Arc::clone(&cache),
        InvocationReaders::from_policy(&policy),
    );
    let api_routes = crate::api::router(Arc::clone(&farm), token)
        .merge(health_routes)
        .merge(crate::metrics::router(Arc::clone(&cache)))
        .merge(blob_routes)
        .merge(page_routes);
    // Logged at every start, the first one too (issue #156).
    let (term, epoch) = (farm.term(), farm.lease_epoch());
    tracing::warn!(term, epoch, "{STATE_IN_MEMORY}");

    let mut reapi_server = flow_control(Server::builder());
    if let Some(tls) = listeners.reapi_tls {
        reapi_server = reapi_server.tls_config(tls)?;
    }
    let mut worker_server = flow_control(Server::builder());
    let peers = match listeners.worker_tls {
        Some(tls) => {
            worker_server = worker_server.tls_config(tls.server)?;
            Peers::Certified {
                deny_list: tls.deny_list,
            }
        }
        None => Peers::Unauthenticated,
    };
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
    let worker_incoming =
        worker_incoming.map(move |accepted| accepted.map(|stream| tracking.track(stream)));
    let worker_segments = SegmentsServer::new(WorkerSegments::new(
        SegmentsService::new(Arc::clone(&cache), listeners.segments, connections),
        peers.clone(),
    ))
    .max_decoding_message_size(MAX_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_MESSAGE_BYTES);
    // Sessions opened in this process carry no certificate: admitted as on a
    // plain-text listener, with no deny list.
    let local = LocalWorker::new(WorkerService::new(
        Arc::clone(&farm),
        Peers::Unauthenticated,
        listeners.heartbeat_interval,
        listeners.hello_wait,
    ));
    let worker_service = WorkerServer::new(WorkerService::new(
        Arc::clone(&farm),
        peers,
        listeners.heartbeat_interval,
        listeners.hello_wait,
    ))
    .max_decoding_message_size(MAX_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let (closer, closing) = kbf_front::closing();
    let authorizers = Arc::new(policy.authorizers);
    let cache_for_health = Arc::clone(&cache);
    let reapi_routes = kbf_front::routes_recorded(
        cache,
        Arc::clone(&farm),
        closing,
        authorizers,
        &recorder,
        default_memory,
    );
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

    let stop_readiness = Arc::clone(&readiness);
    let serving = async move {
        // A task of its own: applying events never delays the tick or an accept loop,
        // and they never delay it. It stops when the serving future does.
        let _recording = AbortOnDrop(tokio::spawn(recording.run()).abort_handle());
        let (drain, draining) = tokio::sync::oneshot::channel::<()>();
        // `reapi_server` carries the REAPI TLS configuration when one is given, so
        // the health service is served over the same TLS as the REAPI routes; the
        // authenticator is already applied to those routes alone (above).
        let reapi_serve = reapi_server
            .add_routes(reapi_routes)
            .serve_with_incoming_shutdown(reapi_incoming, async {
                let _ = draining.await;
            });
        let worker_serve = worker_server
            .add_service(worker_service)
            .add_service(worker_blobs)
            .add_service(worker_cas)
            .add_service(worker_segments)
            .serve_with_incoming(worker_incoming);
        let api_serve = serve_api(api_listener.map(|(listener, _)| listener), api_routes);
        let ticking = async {
            let mut tick = tokio::time::interval(listeners.tick);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                farm.tick().await;
            }
        };
        // Completes when the drain has run out of time: the REAPI listener, drained
        // in time, completes first.
        let stopping = async {
            shutdown.await;
            // Before anything stops, so a front's health check (`/readyz`, or the
            // health service) sends no new work here.
            stop_readiness.stop();
            closer.close();
            let _ = drain.send(());
            tokio::time::sleep(listeners.shutdown_timeout).await;
            tracing::warn!(
                timeout = ?listeners.shutdown_timeout,
                "REAPI connections still open at the shutdown timeout; stopping anyway"
            );
        };
        tokio::select! {
            served = reapi_serve => served.map_err(ServeError::from),
            served = worker_serve => served.map_err(ServeError::from),
            served = api_serve => served.map_err(ServeError::Api),
            () = ticking => Ok(()),
            () = stopping => Ok(()),
        }
    };
    Ok(Bound {
        reapi,
        worker,
        api,
        readiness,
        serving,
        recorder,
        local,
    })
}
