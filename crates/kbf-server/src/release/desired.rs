//! The desired release (`docs/design/pull-updates.md` section 6.3, over the `update/desired`
//! row of [`crate::update::Rows`]): `GET /v1/update`, `POST /v1/update/desired`, `POST
//! /v1/update/pause` and `POST /v1/update/resume`.
//!
//! - `GET /v1/update` is open like `GET /v1/nodes`: the desired release (`null` before any),
//!   and the commits the release store holds with whether each platform is complete.
//! - `POST /v1/update/desired {"commit": "<40 hex>"}` offers the commit: the store must hold
//!   it complete (tarball and attestation) for at least one platform, else `409` naming
//!   what is missing. The row's generation moves only when the commit differs from the
//!   desired one (a repeated click keeps it); a rollback is the older commit offered the same
//!   way. It passes the gates of every `/v1` write ([`crate::api::operator_gate`]) and wants a
//!   JSON body.
//! - `POST /v1/update/pause` and `resume` set whether the run may start a host.
//! - Without a release store (`--releases-dir`) or without a state store (`--state-store`:
//!   the row has nowhere to live) every answer is `503` saying which is missing.
//!
//! Serve the router with `into_make_service_with_connect_info`, as the API's.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use super::key::Commit;
use super::store::ReleaseStore;
use crate::api::{error, json, operator_gate};
use crate::farm::Farm;
use crate::token::ApiToken;
use crate::update::{Desired, Release, RowError, Rows};

struct Update {
    store: Option<Arc<ReleaseStore>>,
    rows: Option<Rows>,
    token: Option<ApiToken>,
    farm: Arc<Farm>,
}

/// The routes. `store` and `rows` are each `None` when the server has none: the routes then
/// answer `503`.
pub fn router(
    store: Option<Arc<ReleaseStore>>,
    rows: Option<Rows>,
    token: Option<ApiToken>,
    farm: Arc<Farm>,
) -> Router {
    Router::new()
        .route("/v1/update", get(view))
        .route("/v1/update/desired", post(set_desired))
        .route("/v1/update/pause", post(pause))
        .route("/v1/update/resume", post(resume))
        .with_state(Arc::new(Update {
            store,
            rows,
            token,
            farm,
        }))
}

fn unavailable(what: &str, flag: &str) -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("{what} is off: the server was started without {flag}"),
    )
}

fn row_failed(e: &RowError) -> Response {
    tracing::error!("update rows: {e}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "the update rows failed")
}

fn desired_view(desired: Option<&Desired>) -> serde_json::Value {
    desired.map_or(serde_json::Value::Null, |d| {
        json!({
            "generation": d.generation.0,
            "commit": d.commit.as_str(),
            "set_at_ms": d.set_at.as_millis(),
            "paused": d.paused,
        })
    })
}

/// Runs a blocking call on the store (the rows block on it).
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, RowError> + Send + 'static,
) -> Result<T, RowError> {
    tokio::task::spawn_blocking(call).await.unwrap_or_else(|e| {
        Err(RowError::Store(kbf_state::StoreError::Failed(
            e.to_string(),
        )))
    })
}

async fn view(State(update): State<Arc<Update>>) -> Response {
    let Some(rows) = update.rows.clone() else {
        return unavailable("the update state", "--state-store");
    };
    let Some(store) = update.store.as_deref() else {
        return unavailable("the release store", "--releases-dir");
    };
    let desired = match blocking(move || rows.desired()).await {
        Ok(stored) => stored.value,
        Err(e) => return row_failed(&e),
    };
    let releases = match store.list().await {
        Ok(listing) => super::http::listing_view(&listing),
        Err(e) => {
            tracing::error!("release store: list: {e}");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the release store failed",
            );
        }
    };
    json(
        StatusCode::OK,
        &json!({ "desired": desired_view(desired.as_ref()), "releases": releases }),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    commit: String,
}

/// The gates of a write, then the JSON content type for a body.
fn write_gate(
    update: &Update,
    peer: SocketAddr,
    headers: &HeaderMap,
    wants_json: bool,
) -> Result<(), (StatusCode, String)> {
    operator_gate(update.token.as_ref(), peer, headers)?;
    let json = headers.get(CONTENT_TYPE).map(|v| v.as_bytes());
    if wants_json && json != Some(b"application/json") {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the body must be JSON: Content-Type: application/json".to_owned(),
        ));
    }
    Ok(())
}

async fn set_desired(
    State(update): State<Arc<Update>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((code, why)) = write_gate(&update, peer, &headers, true) {
        return error(code, &why);
    }
    let Some(rows) = update.rows.clone() else {
        return unavailable("the update state", "--state-store");
    };
    let Some(store) = update.store.as_deref() else {
        return unavailable("the release store", "--releases-dir");
    };
    let Ok(Offer { commit }) = serde_json::from_slice::<Offer>(&body) else {
        return error(
            StatusCode::BAD_REQUEST,
            "the body must be {\"commit\": \"<40 hex>\"}",
        );
    };
    let Some(parsed) = Commit::parse(&commit) else {
        return error(
            StatusCode::BAD_REQUEST,
            "commit must be 40 lowercase hex digits",
        );
    };
    let listing = match store.list().await {
        Ok(listing) => listing,
        Err(e) => {
            tracing::error!("release store: list: {e}");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the release store failed",
            );
        }
    };
    let complete = listing
        .iter()
        .filter(|c| c.commit == parsed)
        .flat_map(|c| &c.platforms)
        .any(|p| p.complete);
    if !complete {
        return error(
            StatusCode::CONFLICT,
            "the release store holds no platform of this commit complete (tarball and attestation)",
        );
    }
    let now = update.farm.now();
    match blocking(move || rows.offer(&Release::new(commit), now)).await {
        Ok(desired) => json(StatusCode::OK, &desired_view(Some(&desired))),
        Err(e) => row_failed(&e),
    }
}

async fn pause(
    state: State<Arc<Update>>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    paused(state, peer, headers, true).await
}

async fn resume(
    state: State<Arc<Update>>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    paused(state, peer, headers, false).await
}

async fn paused(
    State(update): State<Arc<Update>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    paused: bool,
) -> Response {
    if let Err((code, why)) = write_gate(&update, peer, &headers, false) {
        return error(code, &why);
    }
    let Some(rows) = update.rows.clone() else {
        return unavailable("the update state", "--state-store");
    };
    match blocking(move || rows.set_paused(paused)).await {
        Ok(true) => json(StatusCode::OK, &json!({ "paused": paused })),
        Ok(false) => error(
            StatusCode::CONFLICT,
            "there is no run to pause: nothing was offered",
        ),
        Err(e) => row_failed(&e),
    }
}
