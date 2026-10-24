//! The accept loop, the per-connection limits and the per-request dispatch.
//!
//! A connection is accepted, passes the limits, and then (in its own task, so a slow client
//! never holds up the accept loop) does its TLS handshake ([`Acceptor`]) and is served over
//! HTTP/2 or HTTP/1.1 by what ALPN picked. Every request is classified in one place,
//! [`Service::handle`]: the route, then who may call it, then where it goes.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use http::{HeaderValue, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::task::JoinHandle;

use crate::body::ProxyBody;
use crate::control::{MAX_UPDATE_BODY, UPDATE_DEADLINE, UpdateHandler, UpdateReply, UpdateRequest};
use crate::core::{Core, now};
use crate::limits::h2_server_builder;
use crate::metrics::Metrics;
use crate::pool::BoxIo;
use crate::reply::{from_update, is_grpc, method_not_allowed, refused, text, unknown};
use crate::route::{Route, Target, auth_class, route_of, ui_method_allowed};
use crate::stream::proxy_child;
use crate::tls::{Acceptor, Peer, Proto};

/// Where connections come from: the public socket, or an in-memory queue in the tests.
pub(crate) trait Listen: Send + 'static {
    /// The next connection and its peer's address.
    fn accept(&mut self) -> impl Future<Output = io::Result<(BoxIo, IpAddr)>> + Send;
}

impl Listen for TcpListener {
    async fn accept(&mut self) -> io::Result<(BoxIo, IpAddr)> {
        let (tcp, peer) = Self::accept(self).await?;
        tcp.set_nodelay(true)?;
        Ok((Box::new(tcp), peer.ip()))
    }
}

/// How long a shutdown waits for open streams to finish before the connections are dropped.
pub(crate) const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// How long the accept loop waits after an accept failed.
pub(crate) const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// A stream that panicked: hyper resets that one stream (`INTERNAL_ERROR`), nothing else.
#[derive(Debug, thiserror::Error)]
#[error("the stream panicked")]
pub(crate) struct StreamPanicked;

/// Stops a task when its handle drops. A plain `JoinHandle` detaches instead, and a client
/// that reset its stream would leave a permit behind.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct Service<U> {
    core: Arc<Core>,
    updates: Arc<U>,
}

impl<U: UpdateHandler + 'static> Service<U> {
    pub(crate) fn new(core: Arc<Core>, updates: U) -> Arc<Self> {
        Arc::new(Self {
            core,
            updates: Arc::new(updates),
        })
    }

    /// Runs one request in its own task, so a panic is that stream's `JoinError` and not a
    /// connection's, and the task stops with the request if the client goes away.
    pub(crate) async fn call(
        self: Arc<Self>,
        request: Request<Incoming>,
        peer: Arc<Peer>,
    ) -> Result<Response<ProxyBody>, StreamPanicked> {
        let core = Arc::clone(&self.core);
        let mut task = AbortOnDrop(tokio::spawn(
            async move { self.handle(request, &peer).await },
        ));
        (&mut task.0).await.map_err(|_| {
            Metrics::inc(&core.metrics.panics);
            StreamPanicked
        })
    }

    async fn handle(
        self: Arc<Self>,
        request: Request<Incoming>,
        peer: &Peer,
    ) -> Response<ProxyBody> {
        // HTTP/1.1 is checked, and made to look like HTTP/2, before anything reads the request.
        let request = if peer.h1 {
            match crate::h1::admit(request) {
                Ok(request) => request,
                Err(answer) => return *answer,
            }
        } else {
            request
        };
        let grpc = is_grpc(request.headers());
        let route = match route_of(request.uri().path()) {
            // Switched off: as unknown as a path outside the table.
            Route::Ui if !self.core.ui.load(Ordering::Acquire) => Route::Unknown,
            route => route,
        };
        // The UI is read-only: the proxy turns a write away before the child is asked.
        if route == Route::Ui && !ui_method_allowed(request.method()) {
            return method_not_allowed();
        }
        // Who may call it, before the call is counted or forwarded.
        if let Err(refused) = admit(&self.core, route, &request, peer) {
            return *refused;
        }
        // Counted from accept: a stream the proxy answers itself is one for its whole life, a
        // backend stream until its place on a member exists.
        let pending = self.core.begin_stream();
        match route {
            Route::Child(target) => {
                let core = Arc::clone(&self.core);
                let identity = peer.identity.clone();
                proxy_child(core, pending, target, request, identity, peer.ip).await
            }
            Route::Ui => {
                let core = Arc::clone(&self.core);
                let target = Target::Operator;
                let identity = peer.identity.clone();
                proxy_child(core, pending, target, request, identity, peer.ip).await
            }
            Route::Update => self.update(request).await,
            Route::Health => text(StatusCode::OK, "ok"),
            // The writers are the polls of the members (`pool_probe`), each of which writes its
            // own member.
            Route::Ready if self.core.is_ready() => text(StatusCode::OK, "ok"),
            Route::Ready => text(StatusCode::SERVICE_UNAVAILABLE, "not ready"),
            Route::Metrics => {
                // The proxy's own series, then a series for each member.
                let mut body = self.core.metrics.render().into_bytes();
                body.extend_from_slice(self.core.upstreams.render().as_bytes());
                let mut response = text(StatusCode::OK, body);
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; version=0.0.4"),
                );
                response
            }
            Route::Unknown => unknown(grpc),
        }
    }

    /// The update API: the proxy reads the body (at most [`MAX_UPDATE_BODY`]) and bounds the
    /// whole call by [`UPDATE_DEADLINE`]; the handler is the only thing it calls.
    async fn update(&self, request: Request<Incoming>) -> Response<ProxyBody> {
        let reply = tokio::time::timeout(UPDATE_DEADLINE, self.run_update(request))
            .await
            .unwrap_or_else(|_| UpdateReply::timed_out());
        from_update(reply)
    }

    async fn run_update(&self, request: Request<Incoming>) -> UpdateReply {
        let (parts, body) = request.into_parts();
        let Ok(body) = Limited::new(body, MAX_UPDATE_BODY).collect().await else {
            return UpdateReply::payload_too_large();
        };
        let request = UpdateRequest {
            method: parts.method.to_string(),
            path: parts.uri.path().to_owned(),
            body: body.to_bytes().to_vec(),
        };
        // Its own task: a 504 drops this future, and the handler (commands are idempotent)
        // still finishes. A panic goes on as the stream's panic.
        let updates = Arc::clone(&self.updates);
        let task = tokio::spawn(async move { updates.handle(request).await });
        match task.await {
            Ok(reply) => reply,
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }
}

/// Decides whether the caller of `request` may make it ([`crate::security::Security`]). `Err`
/// is the refusal to answer with. Not generic, so that every instantiation of the service
/// shares one copy.
fn admit(
    core: &Core,
    route: Route,
    request: &Request<Incoming>,
    peer: &Peer,
) -> Result<(), Box<Response<ProxyBody>>> {
    let Some(class) = auth_class(route) else {
        return Ok(());
    };
    let verdict = core.security.authorize(
        class,
        request.headers(),
        peer.identity.as_ref(),
        peer.ip,
        now(),
    );
    verdict.map_err(|refusal| {
        Metrics::inc(&core.metrics.auth_refused);
        Box::new(refused(route.grpc(), refusal))
    })
}

/// The per-connection limits: total, and per client address.
pub(crate) struct ConnLimits {
    total: Arc<Semaphore>,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    max_per_ip: usize,
}

/// One accepted connection's place under the limits.
pub(crate) struct ConnPermit {
    _total: OwnedSemaphorePermit,
    limits: Arc<ConnLimits>,
    ip: IpAddr,
}

impl ConnLimits {
    pub(crate) fn new(max_connections: usize, max_per_ip: usize) -> Arc<Self> {
        Arc::new(Self {
            total: Arc::new(Semaphore::new(max_connections)),
            per_ip: Mutex::new(HashMap::new()),
            max_per_ip,
        })
    }

    /// A place for one more connection from `ip`, or nothing: the connection is closed.
    pub(crate) fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<ConnPermit> {
        let total = Arc::clone(&self.total).try_acquire_owned().ok()?;
        let mut per_ip = self.per_ip.lock().unwrap_or_else(PoisonError::into_inner);
        let held = per_ip.entry(ip).or_insert(0);
        if *held >= self.max_per_ip {
            return None;
        }
        *held += 1;
        Some(ConnPermit {
            _total: total,
            limits: Arc::clone(self),
            ip,
        })
    }
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        let mut per_ip = self
            .limits
            .per_ip
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // The entry exists: `admit` made it for this permit.
        let held = per_ip.entry(self.ip).or_insert(1);
        *held -= 1;
        if *held == 0 {
            per_ip.remove(&self.ip);
        }
    }
}

/// Resolves when the shutdown flag is set, or its sender is gone.
async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    // A closed channel is a shutdown too: nobody is left to send one.
    let _ = shutdown.wait_for(|stop| *stop).await;
}

/// Everything one connection's task needs.
struct Conn<U> {
    io: BoxIo,
    ip: IpAddr,
    permit: ConnPermit,
    acceptor: Arc<Acceptor>,
    service: Arc<Service<U>>,
    h2: http2::Builder<TokioExecutor>,
    h1: http1::Builder,
    shutdown: watch::Receiver<bool>,
    /// Dropped when the connection ends: the accept loop waits for every clone to drain.
    drain: mpsc::Sender<()>,
}

/// Drives a connection to its end; on a shutdown asks it to finish its open streams first.
/// A macro and not a generic function: the compiler cannot prove a `Send` future through one.
macro_rules! finish {
    ($conn:expr, $shutdown:expr) => {{
        let mut conn = std::pin::pin!($conn);
        let mut shutdown = $shutdown;
        tokio::select! {
            _ = conn.as_mut() => {}
            () = stopped(&mut shutdown) => {
                conn.as_mut().graceful_shutdown();
                let _ = conn.await;
            }
        }
    }};
}

/// Runs one connection to its end: the handshake, then HTTP/2 or HTTP/1.1 by ALPN. A
/// shutdown makes the connection finish its open streams and close.
async fn connection<U: UpdateHandler + 'static>(conn: Conn<U>) {
    let Conn {
        io,
        ip,
        permit,
        acceptor,
        service,
        h2,
        h1,
        shutdown,
        drain,
    } = conn;
    let _held = (permit, drain);
    let core = &service.core;
    let limit = core.limits.pre_stream_timeout();
    let Some(accepted) = acceptor.accept(io, ip, limit, &core.metrics).await else {
        return;
    };
    let peer = Arc::new(accepted.peer);
    let serve = move |request| Arc::clone(&service).call(request, Arc::clone(&peer));
    let io = TokioIo::new(accepted.io);
    match accepted.proto {
        Proto::H2 => finish!(h2.serve_connection(io, service_fn(serve)), shutdown),
        Proto::H1 => finish!(h1.serve_connection(io, service_fn(serve)), shutdown),
    }
}

/// Accepts connections on `listener` until `shutdown` turns true (or its sender is gone),
/// then sends each connection a graceful GOAWAY, the only GOAWAY the proxy sends on purpose,
/// and gives open streams [`SHUTDOWN_DRAIN`] to finish.
pub(crate) async fn serve<U: UpdateHandler + 'static>(
    core: Arc<Core>,
    updates: U,
    mut listener: impl Listen,
    acceptor: Acceptor,
    mut shutdown: watch::Receiver<bool>,
) {
    let h2 = h2_server_builder(&core.limits);
    let h1 = crate::h1::builder(&core.limits);
    let limits = ConnLimits::new(
        core.limits.max_connections,
        core.limits.max_connections_per_ip,
    );
    let service = Service::new(core, updates);
    let acceptor = Arc::new(acceptor);
    let (drain, mut drained) = mpsc::channel::<()>(1);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    // A connection over a limit is dropped, which closes it.
                    Ok((io, ip)) => if let Some(permit) = limits.admit(ip) {
                        tokio::spawn(connection(Conn {
                            io,
                            ip,
                            permit,
                            acceptor: Arc::clone(&acceptor),
                            service: Arc::clone(&service),
                            h2: h2.clone(),
                            h1: h1.clone(),
                            shutdown: shutdown.clone(),
                            drain: drain.clone(),
                        }));
                    },
                    // Out of descriptors, say: back off instead of spinning on the error.
                    Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
                }
            }
            () = stopped(&mut shutdown) => break,
        }
    }
    drop(listener);
    drop(drain);
    // `recv` ends when the last connection has dropped its sender.
    let _ = tokio::time::timeout(SHUTDOWN_DRAIN, drained.recv()).await;
}
