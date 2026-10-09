//! Serving: the REAPI listener, the worker listener, the operator API listener (if
//! any) and the scheduler tick.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::Either;
use kbf_front::{Cache, MAX_MESSAGE_BYTES, MetaLog};
use kbf_objstore::ObjectStore;
use kbf_proto::worker::worker_server::WorkerServer;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Server, ServerTlsConfig};

use crate::farm::Farm;
use crate::identity::{DenyList, Peers};
use crate::token::ApiToken;
use crate::worker::WorkerService;

/// How the server listens and paces its daemons.
#[derive(Clone, Debug)]
pub struct Listeners {
    /// The REAPI listener (clients: buck2, Bazel).
    pub reapi: SocketAddr,
    /// The `kbf.worker.v1` listener (daemons).
    pub worker: SocketAddr,
    /// Mutual TLS for the worker listener; `None` serves it in plain text, where no
    /// node is bound to a certificate.
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
    /// How long a finished operation is kept after its callers are answered, in which
    /// WaitExecution on it still streams its result; then it is NOT_FOUND.
    pub finished_retention: Duration,
    /// How long shutdown waits for the REAPI listener to drain (see
    /// [`bind_server_with_api`]) before it stops anyway.
    pub shutdown_timeout: Duration,
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
    /// Serves every listener and the tick until the shutdown future completes.
    pub serving: F,
}

fn bind(addr: SocketAddr) -> Result<(TcpIncoming, SocketAddr), ServeError> {
    let bound = TcpIncoming::bind(addr).and_then(|incoming| {
        let local = incoming.local_addr()?;
        Ok((incoming, local))
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
                listener,
                routes.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .into_future(),
        ),
        None => Either::Right(std::future::pending()),
    }
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

/// Binds the REAPI and worker listeners for a farm over `cache`, and no operator API.
/// Nothing is served until the returned future runs; it serves until `shutdown`
/// completes, or a listener fails.
///
/// # Errors
/// A listener cannot be bound, or the worker TLS configuration is refused.
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
/// When `shutdown` completes, the REAPI listener stops accepting, every connection on
/// it is sent GOAWAY, and every open Execute and WaitExecution stream that is not done
/// ends UNAVAILABLE, which clients retry (issue #168). The serving future returns once
/// the REAPI connections have closed, or after `listeners.shutdown_timeout` if one
/// stays open (a client still uploading, say); until then the worker listener, the
/// operator API and the tick go on. Daemon streams never end on their own, so the
/// worker listener is not drained: it stops when the future returns.
///
/// # Errors
/// A listener cannot be bound, or the worker TLS configuration is refused.
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
    let farm = Arc::new(Farm::new(
        Arc::clone(&cache),
        listeners.unservable_wait,
        listeners.finished_retention,
    ));
    let (reapi_incoming, reapi) = bind(listeners.reapi)?;
    let (worker_incoming, worker) = bind(listeners.worker)?;
    let (api_listen, token) = api.map_or((None, None), |api| (Some(api.listen), api.token));
    let api_listener = api_listen.map(bind_api).transpose()?;
    let api = api_listener.as_ref().map(|(_, local)| *local);
    let api_routes = crate::api::router(Arc::clone(&farm), token);

    let mut worker_server = Server::builder();
    let peers = match listeners.worker_tls {
        Some(tls) => {
            worker_server = worker_server.tls_config(tls.server)?;
            Peers::Certified {
                deny_list: tls.deny_list,
            }
        }
        None => Peers::Unauthenticated,
    };
    let worker_service = WorkerServer::new(WorkerService::new(
        Arc::clone(&farm),
        peers,
        listeners.heartbeat_interval,
        listeners.hello_wait,
    ))
    .max_decoding_message_size(MAX_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let (closer, closing) = kbf_front::closing();
    let reapi_routes = kbf_front::routes_with_execution(cache, Arc::clone(&farm), closing);

    let serving = async move {
        let (drain, draining) = tokio::sync::oneshot::channel::<()>();
        let reapi_serve = Server::builder()
            .add_routes(reapi_routes)
            .serve_with_incoming_shutdown(reapi_incoming, async {
                let _ = draining.await;
            });
        let worker_serve = worker_server
            .add_service(worker_service)
            .serve_with_incoming(worker_incoming);
        let api_serve = serve_api(api_listener.map(|(listener, _)| listener), api_routes);
        let ticking = async {
            let mut tick = tokio::time::interval(listeners.tick);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                farm.tick();
            }
        };
        // Completes when the drain has run out of time: the REAPI listener, drained
        // in time, completes first.
        let stopping = async {
            shutdown.await;
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
        serving,
    })
}
