//! `grpc.health.v1.Health` on the REAPI listener, for a proxy's gRPC health check
//! (`docs/api.md`).
//!
//! The answer is `/readyz`'s ([`crate::health::failing`]): `SERVING` when every check
//! passes, `NOT_SERVING` when one fails, a stop signal included. Every service the
//! listener serves shares that one answer; [`SERVICES`] lists the names it knows.
//!
//! - `Check`: evaluates the checks once (one store probe) and answers; a service name
//!   it does not know is NOT_FOUND.
//! - `List`: one evaluation, answered for every name of [`SERVICES`].
//! - `Watch`: sends the status at once, then again each time it changes. It evaluates
//!   again whenever [`Readiness`] changes (a stop signal, the scheduler role) and every
//!   `watch_interval` for the store. Once the server is stopping it sends
//!   `NOT_SERVING` (unless it last sent that) and ends the stream UNAVAILABLE, so an
//!   open Watch does not hold the REAPI listener's drain. A name it does not know is
//!   sent `SERVICE_UNKNOWN`, and the stream stays open until the server stops.
//!
//! The service is added to the REAPI listener outside its authentication layer: the
//! REAPI policy (`--reapi-auth-policy`) does not run on health calls ([`crate::serve`]).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use kbf_front::{Cache, MetaLog};
use kbf_objstore::ObjectStore;
use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
use kbf_proto::grpc::health::v1::health_server::Health;
use kbf_proto::grpc::health::v1::{
    HealthCheckRequest, HealthCheckResponse, HealthListRequest, HealthListResponse,
};
use kbf_proto::reapi::action_cache_server::ActionCacheServer;
use kbf_proto::reapi::capabilities_server::CapabilitiesServer;
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
use kbf_proto::reapi::execution_server::ExecutionServer;
use tonic::server::NamedService;
use tonic::{Request, Response, Status};

use crate::health::{Readiness, failing};

/// The service names the health service answers for: `""` (the server as a whole)
/// and each service of the REAPI listener.
pub const SERVICES: [&str; 6] = [
    "",
    <CapabilitiesServer<()> as NamedService>::NAME,
    <ContentAddressableStorageServer<()> as NamedService>::NAME,
    <ByteStreamServer<()> as NamedService>::NAME,
    <ActionCacheServer<()> as NamedService>::NAME,
    <ExecutionServer<()> as NamedService>::NAME,
];

/// How often an open `Watch` probes the store when nothing else changes.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// The `grpc.health.v1.Health` service over a cache's store and a server's
/// [`Readiness`].
pub struct HealthService<M, O> {
    inner: Arc<Inner<M, O>>,
}

struct Inner<M, O> {
    cache: Arc<Cache<M, O>>,
    readiness: Arc<Readiness>,
    probe_timeout: Duration,
    watch_interval: Duration,
}

impl<M, O> HealthService<M, O>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    /// The service: each evaluation probes `cache`'s store, waiting at most
    /// `probe_timeout`; an open `Watch` probes it again every `watch_interval`.
    #[must_use]
    pub fn new(
        cache: Arc<Cache<M, O>>,
        readiness: Arc<Readiness>,
        probe_timeout: Duration,
        watch_interval: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                cache,
                readiness,
                probe_timeout,
                watch_interval,
            }),
        }
    }
}

impl<M, O> Inner<M, O>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    async fn status(&self) -> ServingStatus {
        let failing = failing(&self.cache, &self.readiness, self.probe_timeout).await;
        if failing.is_empty() {
            ServingStatus::Serving
        } else {
            ServingStatus::NotServing
        }
    }
}

fn known(service: &str) -> bool {
    SERVICES.contains(&service)
}

fn response(status: ServingStatus) -> HealthCheckResponse {
    HealthCheckResponse {
        status: status.into(),
    }
}

fn stopping() -> Status {
    Status::unavailable("the server is shutting down")
}

/// Where a `Watch` stream is.
enum Watching<M, O> {
    /// Sending: `last` is the status it sent last, if any.
    Open {
        inner: Arc<Inner<M, O>>,
        known: bool,
        changed: tokio::sync::watch::Receiver<()>,
        last: Option<ServingStatus>,
    },
    /// The server is stopping and the last status is sent: end UNAVAILABLE next.
    Ending,
    /// Ended.
    Done,
}

impl<M, O> Watching<M, O>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    async fn next(self) -> Option<(Result<HealthCheckResponse, Status>, Self)> {
        let (inner, known, mut changed, last) = match self {
            Self::Open {
                inner,
                known,
                changed,
                last,
            } => (inner, known, changed, last),
            Self::Ending => return Some((Err(stopping()), Self::Done)),
            Self::Done => return None,
        };
        loop {
            // Read before the evaluation: a stop signal that comes during it is
            // answered by the next turn, which `changed` wakes at once.
            let is_stopping = inner.readiness.is_stopping();
            let status = if known {
                inner.status().await
            } else {
                ServingStatus::ServiceUnknown
            };
            if last != Some(status) {
                let next = if is_stopping {
                    Self::Ending
                } else {
                    Self::Open {
                        inner,
                        known,
                        changed,
                        last: Some(status),
                    }
                };
                return Some((Ok(response(status)), next));
            }
            if is_stopping {
                return Some((Err(stopping()), Self::Done));
            }
            // `changed` fails only once its sender is dropped, and the sender lives in
            // `inner.readiness`, which this stream holds.
            tokio::select! {
                _ = changed.changed() => {}
                () = tokio::time::sleep(inner.watch_interval) => {}
            }
        }
    }
}

type WatchStream = Pin<Box<dyn Stream<Item = Result<HealthCheckResponse, Status>> + Send>>;

#[tonic::async_trait]
impl<M, O> Health for HealthService<M, O>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    async fn check(
        &self,
        request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        let service = request.into_inner().service;
        if !known(&service) {
            return Err(Status::not_found(format!("unknown service {service:?}")));
        }
        Ok(Response::new(response(self.inner.status().await)))
    }

    async fn list(
        &self,
        _request: Request<HealthListRequest>,
    ) -> Result<Response<HealthListResponse>, Status> {
        let status = self.inner.status().await;
        let statuses = SERVICES
            .iter()
            .map(|name| ((*name).to_owned(), response(status)))
            .collect();
        Ok(Response::new(HealthListResponse { statuses }))
    }

    type WatchStream = WatchStream;

    async fn watch(
        &self,
        request: Request<HealthCheckRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let known = known(&request.into_inner().service);
        // Subscribed before the first evaluation, so no change after it is missed.
        let changed = self.inner.readiness.subscribe();
        let start = Watching::Open {
            inner: Arc::clone(&self.inner),
            known,
            changed,
            last: None,
        };
        let stream = futures::stream::unfold(start, Watching::next);
        Ok(Response::new(Box::pin(stream)))
    }
}
