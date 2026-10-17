//! `PUT /v1/releases/{commit}/{platform}/{name}` and `GET /v1/releases`.
//!
//! - The body of a `PUT` is the file: a tarball for `kbf` or `kbf-daemon`, a build
//!   attestation bundle for the same with the suffix `.attestation`. It is written to the
//!   store as it streams in ([`ReleaseStore`]), so the server holds a chunk of it, not the
//!   file. `201` when the key is new, `200` when it already held exactly these bytes, `409`
//!   for other bytes, `413` past the limit, `400` for a segment that is not what it must be,
//!   an empty body or a body cut short.
//! - A `PUT` needs what the other `/v1` writes need ([`crate::api::operator_gate`]): a
//!   loopback peer, no `Origin`, the bearer token. Its body is not JSON, so the JSON
//!   content type rule of the others does not apply. `GET` is open like `GET /v1/nodes`.
//! - Without a store (no `--releases-dir`) every answer is `503` and names the flag.
//!   Serve the router with `into_make_service_with_connect_info`, as the API's.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, put};
use futures::StreamExt;
use serde_json::json;

use super::key::{Key, Part};
use super::store::{CommitListing, PutError, ReleaseStore, Stored};
use crate::api::{error, json, operator_gate};
use crate::token::ApiToken;

/// The list route.
pub const RELEASES: &str = "/v1/releases";

struct Releases {
    store: Option<Arc<ReleaseStore>>,
    token: Option<ApiToken>,
}

/// The routes over `store`, or over nothing: `None` answers `503` to all of them. A `PUT`
/// needs `token`, and is refused without one.
pub fn router(store: Option<Arc<ReleaseStore>>, token: Option<ApiToken>) -> Router {
    Router::new()
        .route(RELEASES, get(list))
        .route("/v1/releases/{commit}/{platform}/{name}", put(put_file))
        .with_state(Arc::new(Releases { store, token }))
}

/// The `503` of a server without a store.
fn off() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "the release store is off: the server was started without --releases-dir",
    )
}

fn disk_failed(what: &str, e: &io::Error) -> Response {
    tracing::error!("release store: {what}: {e}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the release store failed",
    )
}

async fn put_file(
    State(releases): State<Arc<Releases>>,
    Path((commit, platform, name)): Path<(String, String, String)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Err((code, why)) = operator_gate(releases.token.as_ref(), peer, &headers) {
        return error(code, &why);
    }
    let Some(store) = releases.store.as_deref() else {
        return off();
    };
    let (key, part) = match Key::parse(&commit, &platform, &name) {
        Ok(parsed) => parsed,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let mut chunks = body.into_data_stream().map(|c| c.map_err(io::Error::other));
    let put = match part {
        Part::Tarball => store.put_tarball(&key, &mut chunks).await,
        Part::Attestation => store.put_attestation(&key, &mut chunks).await,
    };
    match put {
        Ok(stored) => stored_answer(&stored),
        Err(e) => put_refused(&e),
    }
}

fn stored_answer(stored: &Stored) -> Response {
    let code = if stored.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let answer = json!({ "sha256": stored.sha256.as_str(), "size": stored.size });
    json(code, &answer)
}

fn put_refused(e: &PutError) -> Response {
    let code = match e {
        PutError::Conflict(_) => StatusCode::CONFLICT,
        PutError::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        PutError::Empty | PutError::Body(_) => StatusCode::BAD_REQUEST,
        PutError::Disk(e) => return disk_failed("put", e),
    };
    error(code, &e.to_string())
}

async fn list(State(releases): State<Arc<Releases>>) -> Response {
    let Some(store) = releases.store.as_deref() else {
        return off();
    };
    match store.list().await {
        Ok(listing) => json(StatusCode::OK, &listing_view(&listing)),
        Err(e) => disk_failed("list", &e),
    }
}

/// The `GET /v1/releases` body: commits with, per platform, the components stored and
/// whether the platform is complete.
fn listing_view(listing: &[CommitListing]) -> serde_json::Value {
    let releases: Vec<_> = listing
        .iter()
        .map(|commit| {
            let platforms: Vec<_> = commit
                .platforms
                .iter()
                .map(|p| {
                    let components: Vec<_> = p
                        .components
                        .iter()
                        .map(|c| {
                            json!({
                                "name": c.component.as_str(),
                                "sha256": c.sha256.as_str(),
                                "size": c.size,
                                "attested": c.attested,
                            })
                        })
                        .collect();
                    json!({
                        "platform": p.platform.as_str(),
                        "complete": p.complete,
                        "components": components,
                    })
                })
                .collect();
            json!({ "commit": commit.commit.as_str(), "platforms": platforms })
        })
        .collect();
    json!({ "releases": releases })
}
