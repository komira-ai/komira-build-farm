//! Serving: the REAPI listener, the worker listener and the scheduler tick.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MAX_MESSAGE_BYTES, MetaLog};
use kbf_objstore::ObjectStore;
use kbf_proto::worker::worker_server::WorkerServer;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Server, ServerTlsConfig};

use crate::farm::Farm;
use crate::worker::WorkerService;

/// How the server listens and paces its daemons.
#[derive(Clone, Debug)]
pub struct Listeners {
    /// The REAPI listener (clients: buck2, Bazel).
    pub reapi: SocketAddr,
    /// The `kbf.worker.v1` listener (daemons).
    pub worker: SocketAddr,
    /// Mutual TLS for the worker listener; `None` serves it in plain text.
    pub worker_tls: Option<ServerTlsConfig>,
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
}

/// A server that is listening: the addresses it bound, and the future that serves
/// until shutdown.
pub struct Bound<F> {
    /// Where the REAPI listener is.
    pub reapi: SocketAddr,
    /// Where the worker listener is.
    pub worker: SocketAddr,
    /// Serves both listeners and the tick until the shutdown future completes.
    pub serving: F,
}

fn bind(addr: SocketAddr) -> Result<(TcpIncoming, SocketAddr), ServeError> {
    let bound = TcpIncoming::bind(addr).and_then(|incoming| {
        let local = incoming.local_addr()?;
        Ok((incoming, local))
    });
    bound.map_err(|source| ServeError::Bind { addr, source })
}

/// Binds both listeners for a farm over `cache`. Nothing is served until the returned
/// future runs; it serves until `shutdown` completes, or a listener fails.
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
    let farm = Arc::new(Farm::new(Arc::clone(&cache), listeners.unservable_wait));
    let (reapi_incoming, reapi) = bind(listeners.reapi)?;
    let (worker_incoming, worker) = bind(listeners.worker)?;

    let mut worker_server = Server::builder();
    if let Some(tls) = listeners.worker_tls {
        worker_server = worker_server.tls_config(tls)?;
    }
    let worker_service = WorkerServer::new(WorkerService::new(
        Arc::clone(&farm),
        listeners.heartbeat_interval,
        listeners.hello_wait,
    ))
    .max_decoding_message_size(MAX_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let reapi_routes = kbf_front::routes_with_execution(cache, Arc::clone(&farm));

    // Shutdown stops accepting and returns at once: daemon streams never end on their
    // own, so a graceful drain would wait forever. The process exits after it.
    let serving = async move {
        let reapi_serve = Server::builder()
            .add_routes(reapi_routes)
            .serve_with_incoming(reapi_incoming);
        let worker_serve = worker_server
            .add_service(worker_service)
            .serve_with_incoming(worker_incoming);
        let ticking = async {
            let mut tick = tokio::time::interval(listeners.tick);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                farm.tick();
            }
        };
        tokio::select! {
            served = reapi_serve => served.map_err(ServeError::from),
            served = worker_serve => served.map_err(ServeError::from),
            () = ticking => Ok(()),
            () = shutdown => Ok(()),
        }
    };
    Ok(Bound {
        reapi,
        worker,
        serving,
    })
}
