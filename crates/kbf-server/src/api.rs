//! The operator API: HTTP/JSON under `/v1`, on its own listener (`--api-listen`), for
//! the Fleet UI, scripts and clients (`docs/design/fleet-updates.md` section 11.4).
//!
//! - `GET /v1/nodes`: every node registered since the server started, with the newest
//!   software status each sent ([`crate::fleet`]).
//!
//! The API is read-only and unauthenticated today: it is off unless `--api-listen` is
//! given, and it answers anyone who reaches that address. The roles of
//! `fleet-updates-security.md` section S9 (`admin`, `rollout`) are planned.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderName;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;
use kbf_front::MetaLog;
use kbf_objstore::ObjectStore;

use crate::farm::Farm;

/// The media type of every body this API returns.
pub const JSON: &str = "application/json";

/// The API's routes over `farm`.
pub fn router<M, O>(farm: Arc<Farm<M, O>>) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Router::new()
        .route("/v1/nodes", get(nodes::<M, O>))
        .with_state(farm)
}

async fn nodes<M, O>(State(farm): State<Arc<Farm<M, O>>>) -> impl IntoResponse
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    json(&farm.nodes())
}

/// `value` as a JSON body. The views serialize plain strings, numbers and lists, which
/// cannot fail.
fn json(value: &impl serde::Serialize) -> ([(HeaderName, &'static str); 1], Vec<u8>) {
    let body = serde_json::to_vec(value).unwrap_or_default();
    ([(CONTENT_TYPE, JSON)], body)
}
