//! `GET /healthz` and `GET /readyz` on the operator API listener, for a front's or a
//! proxy's health check (`docs/api.md`).
//!
//! - `/healthz`: the process is alive and its API answers. It reads nothing and
//!   always answers 200 while the server runs, while it stops too.
//! - `/readyz`: the server is ready to serve. 200 only when every check passes; else
//!   503 with the failing checks and why. The checks:
//!   - `stopping`: no stop signal has been received ([`Readiness::stop`]);
//!   - `leader`: this server holds the scheduler role ([`Readiness::set_leader`]);
//!   - `store`: the object store answers a read of one fixed key within the probe
//!     timeout. The probe only reads (`get_range` of one byte), so a poll never writes
//!     to the bucket; an absent key is an answer, and so passes.
//!
//! The routes are served by the same future as the REAPI and worker listeners, which
//! are bound before it runs and stop when it returns, so an answer from either route
//! means both listeners are bound.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kbf_front::{Cache, MetaLog};
use kbf_objstore::{ByteRange, ObjectKey, ObjectStore, ObjectStoreError};
use serde::Serialize;

use crate::api::JSON;
use crate::{BUILD_COMMIT, SERVER_VERSION};

/// The name, under the cache's key prefix, of the key the store probe reads. Nothing
/// writes it.
pub const PROBE_KEY: &str = "readyz-probe";

/// How long the store probe waits for the store unless `--readyz-store-timeout-ms`
/// says otherwise.
pub const STORE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What `/readyz` reads besides the store: whether a stop signal has come, and
/// whether this server holds the scheduler role. Shared by the serving future, which
/// sets it, and the route, which reads it.
#[derive(Debug)]
pub struct Readiness {
    stopping: AtomicBool,
    leader: AtomicBool,
}

impl Readiness {
    /// Not stopping, and the leader: a single server runs every role, and its control
    /// log commits in this process, so it holds the scheduler role from its start.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stopping: AtomicBool::new(false),
            leader: AtomicBool::new(true),
        }
    }

    /// A stop signal has been received: `/readyz` answers 503 from now on.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// Whether a stop signal has been received.
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Whether this server holds the scheduler role. Nothing in the server clears it
    /// yet; it is the hook for a replicated control log, whose followers are not ready.
    pub fn set_leader(&self, leader: bool) {
        self.leader.store(leader, Ordering::SeqCst);
    }

    /// Whether this server holds the scheduler role.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.leader.load(Ordering::SeqCst)
    }
}

impl Default for Readiness {
    fn default() -> Self {
        Self::new()
    }
}

/// One failing check of `/readyz`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Failing {
    /// `stopping`, `leader` or `store`.
    pub check: &'static str,
    /// Why it fails.
    pub reason: String,
}

/// The body of `/readyz`.
#[derive(Debug, Serialize)]
struct ReadyBody {
    ready: bool,
    version: &'static str,
    commit: &'static str,
    failing: Vec<Failing>,
}

/// The body of `/healthz`.
#[derive(Debug, Serialize)]
struct AliveBody {
    status: &'static str,
    version: &'static str,
    commit: &'static str,
}

struct HealthState<M, O> {
    cache: Arc<Cache<M, O>>,
    readiness: Arc<Readiness>,
    timeout: Duration,
}

/// The `/healthz` and `/readyz` routes; `/readyz` probes `cache`'s store, waiting at
/// most `timeout` for it.
pub fn router<M, O>(cache: Arc<Cache<M, O>>, readiness: Arc<Readiness>, timeout: Duration) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz::<M, O>))
        .with_state(Arc::new(HealthState {
            cache,
            readiness,
            timeout,
        }))
}

async fn healthz() -> Response {
    let body = AliveBody {
        status: "alive",
        version: SERVER_VERSION,
        commit: BUILD_COMMIT,
    };
    json(StatusCode::OK, &body)
}

async fn readyz<M, O>(State(health): State<Arc<HealthState<M, O>>>) -> Response
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let mut failing = Vec::new();
    if health.readiness.is_stopping() {
        failing.push(Failing {
            check: "stopping",
            reason: "a stop signal was received; the server is shutting down".to_owned(),
        });
    }
    if !health.readiness.is_leader() {
        failing.push(Failing {
            check: "leader",
            reason: "this server does not hold the scheduler role".to_owned(),
        });
    }
    if let Err(reason) = probe_store(&health.cache, health.timeout).await {
        failing.push(Failing {
            check: "store",
            reason,
        });
    }
    let ready = failing.is_empty();
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = ReadyBody {
        ready,
        version: SERVER_VERSION,
        commit: BUILD_COMMIT,
        failing,
    };
    json(code, &body)
}

/// Reads the first byte of [`PROBE_KEY`] under `cache`'s prefix, waiting at most
/// `timeout`. The store is reachable if it answers with the byte, that the key is
/// absent, or that the range is past its end; any other answer, or none in time, is
/// the reason it is not.
///
/// # Errors
/// The store's error, or that it did not answer within `timeout`.
pub async fn probe_store<M, O>(cache: &Cache<M, O>, timeout: Duration) -> Result<(), String>
where
    M: MetaLog,
    O: ObjectStore,
{
    let key: ObjectKey = cache
        .prefix()
        .key(PROBE_KEY)
        .map_err(|e| format!("the probe key: {e}"))?;
    let first = ByteRange::new(0, 1).ok_or_else(|| "the probe range".to_owned())?;
    match tokio::time::timeout(timeout, cache.objects().get_range(&key, first)).await {
        Ok(Ok(_) | Err(ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. })) => {
            Ok(())
        }
        Ok(Err(e)) => Err(format!("a read of {key} failed: {e}")),
        Err(_) => Err(format!(
            "a read of {key} had no answer within {} ms",
            timeout.as_millis()
        )),
    }
}

fn json(code: StatusCode, value: &impl Serialize) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_default();
    (code, [(CONTENT_TYPE, JSON)], body).into_response()
}
