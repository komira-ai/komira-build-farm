//! The gate's HTTP API: the complete set of verbs `kbf-server` may call (M2.2), and
//! nothing else. It is served only over the mutual-TLS listener of [`crate::tls`].
//!
//! | Method and path | Verb | Body |
//! |---|---|---|
//! | `GET /v1/macs` | the inventory, the gate's view and its erase budget | |
//! | `GET /v1/macs/{serial}` | `status` | |
//! | `POST /v1/macs/{serial}/enforce` | `enforce` | `{"key_statement", "set", "deadline"}` |
//! | `POST /v1/macs/{serial}/withdraw` | `withdraw` | |
//! | `POST /v1/macs/{serial}/profile` | `profile` | `{"digest"}` |
//! | `POST /v1/macs/{serial}/grant-admin` | `grant-admin` | `{"lease"}` |
//! | `POST /v1/macs/{serial}/erase` | the signed-erase relay | `{"message", "signature"}` |
//! | `POST /v1/macs/{serial}/bring-forward` | `bring-forward` | `{"lease"}` |
//!
//! Answers are JSON; a refusal is `{"error": "..."}` with the status of
//! [`Refusal::status`]. Request bodies are refused above 64 KiB, and unknown fields are
//! refused, so the server cannot smuggle profile bytes or an erase target past a verb.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::backend::MdmBackend;
use crate::gate::{Gate, Refusal};
use crate::request::valid_lease;

/// The largest request body the gate reads.
pub const MAX_BODY: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lease {
    lease: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Digest {
    digest: String,
}

fn respond(status: u16, body: &impl serde::Serialize) -> Response {
    let mut response = Response::new(Body::from(serde_json::to_vec(body).unwrap_or_default()));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

fn answer<T: serde::Serialize>(result: Result<T, Refusal>) -> Response {
    match result {
        Ok(body) => respond(200, &body),
        Err(refusal) => respond(
            refusal.status(),
            &serde_json::json!({"error": refusal.to_string()}),
        ),
    }
}

fn body<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Refusal> {
    serde_json::from_slice(bytes).map_err(|e| Refusal::BadRequest(e.to_string()))
}

fn lease(bytes: &[u8]) -> Result<String, Refusal> {
    let Lease { lease } = body(bytes)?;
    if valid_lease(&lease) {
        Ok(lease)
    } else {
        Err(Refusal::BadRequest("bad lease id".into()))
    }
}

async fn fleet<B: MdmBackend>(State(gate): State<Arc<Gate<B>>>) -> Response {
    answer(Ok(gate.fleet().await))
}

async fn status<B: MdmBackend>(
    State(gate): State<Arc<Gate<B>>>,
    Path(serial): Path<String>,
) -> Response {
    answer(gate.status(&serial).await)
}

async fn enforce<B: MdmBackend>(
    State(gate): State<Arc<Gate<B>>>,
    Path(serial): Path<String>,
    bytes: Bytes,
) -> Response {
    answer(match body(&bytes) {
        Ok(request) => gate.enforce(&serial, &request).await,
        Err(e) => Err(e),
    })
}

async fn withdraw<B: MdmBackend>(
    State(gate): State<Arc<Gate<B>>>,
    Path(serial): Path<String>,
) -> Response {
    answer(
        gate.withdraw(&serial)
            .await
            .map(|()| serde_json::json!({"outcome": "withdrawn"})),
    )
}

async fn profile<B: MdmBackend>(
    State(gate): State<Arc<Gate<B>>>,
    Path(serial): Path<String>,
    bytes: Bytes,
) -> Response {
    answer(match body::<Digest>(&bytes) {
        Ok(d) => gate
            .profile(&serial, &d.digest)
            .await
            .map(|()| serde_json::json!({"outcome": "installed"})),
        Err(e) => Err(e),
    })
}

async fn grant_admin<B: MdmBackend>(
    State(gate): State<Arc<Gate<B>>>,
    Path(serial): Path<String>,
    bytes: Bytes,
) -> Response {
    answer(match lease(&bytes) {
        Ok(lease) => gate.grant_admin(&serial, &lease).await,
        Err(e) => Err(e),
    })
}

async fn erase<B: MdmBackend>(
    State(gate): State<Arc<Gate<B>>>,
    Path(serial): Path<String>,
    bytes: Bytes,
) -> Response {
    answer(match body(&bytes) {
        Ok(request) => gate.erase(&serial, &request).await,
        Err(e) => Err(e),
    })
}

async fn bring_forward<B: MdmBackend>(
    State(gate): State<Arc<Gate<B>>>,
    Path(serial): Path<String>,
    bytes: Bytes,
) -> Response {
    answer(match lease(&bytes) {
        Ok(lease) => gate.bring_forward(&serial, &lease).await,
        Err(e) => Err(e),
    })
}

async fn unknown() -> Response {
    respond(404, &serde_json::json!({"error": "no such verb"}))
}

/// The API's routes over `gate`.
pub fn router<B: MdmBackend>(gate: Arc<Gate<B>>) -> Router {
    Router::new()
        .route("/v1/macs", get(fleet::<B>))
        .route("/v1/macs/{serial}", get(status::<B>))
        .route("/v1/macs/{serial}/enforce", post(enforce::<B>))
        .route("/v1/macs/{serial}/withdraw", post(withdraw::<B>))
        .route("/v1/macs/{serial}/profile", post(profile::<B>))
        .route("/v1/macs/{serial}/grant-admin", post(grant_admin::<B>))
        .route("/v1/macs/{serial}/erase", post(erase::<B>))
        .route("/v1/macs/{serial}/bring-forward", post(bring_forward::<B>))
        .fallback(unknown)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(gate)
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
