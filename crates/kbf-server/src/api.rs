//! The operator API: HTTP/JSON under `/v1`, on its own listener (`--api-listen`), for
//! the Fleet UI, scripts and clients (`docs/design/fleet-updates.md` section 11.4;
//! `docs/api.md`).
//!
//! - `GET /v1/nodes`: every node registered since the server started, with the newest
//!   software status each sent and where it is in placement ([`crate::fleet`]).
//! - `POST /v1/nodes/{node}:cordon`, `:drain` and `:uncordon`: take a node out of
//!   placement, drain it, or return it ([`NodeAction`]). A drain's body may be
//!   `{"deadline_secs": N}`; without one the deadline is [`DRAIN_DEADLINE`]. The answer
//!   is the node as it is after the action.
//!
//! **Authentication is planned** (the `admin` role of `fleet-updates-security.md`
//! section S9). Until it lands, reads answer anyone who reaches the listener, and a
//! write is accepted only from a loopback peer: an operator on the server's own host.
//! Everyone else gets 403.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderName, StatusCode};
use axum::routing::{get, post};
use kbf_front::MetaLog;
use kbf_objstore::ObjectStore;
use kbf_types::WorkerId;
use serde::Deserialize;

use crate::farm::{Farm, NodeAction};

/// The media type of every body this API returns.
pub const JSON: &str = "application/json";

/// How long a drain waits for leases when its request names no deadline
/// (`drain_deadline` in `fleet-updates.md` section 4.1).
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(30 * 60);

type Answer = (StatusCode, [(HeaderName, &'static str); 1], Vec<u8>);

/// The API's routes over `farm`. Writes read the peer address from
/// [`ConnectInfo`]: serve them with `into_make_service_with_connect_info`.
pub fn router<M, O>(farm: Arc<Farm<M, O>>) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Router::new()
        .route("/v1/nodes", get(nodes::<M, O>))
        .route("/v1/nodes/{target}", post(act::<M, O>))
        .with_state(farm)
}

async fn nodes<M, O>(State(farm): State<Arc<Farm<M, O>>>) -> Answer
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    json(StatusCode::OK, &farm.nodes())
}

async fn act<M, O>(
    State(farm): State<Arc<Farm<M, O>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(target): Path<String>,
    body: Bytes,
) -> Answer
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    match node_action(peer, &target, &body) {
        Ok((node, action)) => match farm.place(&node, action) {
            Ok(view) => json(StatusCode::OK, &view),
            Err(unknown) => error(StatusCode::NOT_FOUND, &unknown.to_string()),
        },
        Err((code, why)) => error(code, &why),
    }
}

/// The body a drain may carry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DrainBody {
    deadline_secs: u64,
}

/// The node and action a write names, if `peer` may write and the request is
/// well formed; else the status and reason to answer with.
///
/// # Errors
/// 403 for a peer that is not loopback; 404 for a target without a known verb; 400 for
/// a drain body that is not `{"deadline_secs": N}`.
pub fn node_action(
    peer: SocketAddr,
    target: &str,
    body: &[u8],
) -> Result<(WorkerId, NodeAction), (StatusCode, String)> {
    if !peer.ip().to_canonical().is_loopback() {
        return Err((
            StatusCode::FORBIDDEN,
            format!("writes are accepted only from the server's own host, not {peer}"),
        ));
    }
    let unknown = || (StatusCode::NOT_FOUND, format!("no action {target:?}"));
    let (node, verb) = target.rsplit_once(':').ok_or_else(unknown)?;
    let action = match verb {
        "cordon" => NodeAction::Cordon,
        "uncordon" => NodeAction::Uncordon,
        "drain" if body.is_empty() => NodeAction::Drain(DRAIN_DEADLINE),
        "drain" => {
            let drain: DrainBody = serde_json::from_slice(body).map_err(|e| {
                let why = format!("a drain body is {{\"deadline_secs\": N}}: {e}");
                (StatusCode::BAD_REQUEST, why)
            })?;
            NodeAction::Drain(Duration::from_secs(drain.deadline_secs))
        }
        _ => return Err(unknown()),
    };
    Ok((WorkerId::new(node), action))
}

/// `value` as a JSON body. The views serialize plain strings, numbers and lists, which
/// cannot fail.
fn json(code: StatusCode, value: &impl serde::Serialize) -> Answer {
    let body = serde_json::to_vec(value).unwrap_or_default();
    (code, [(CONTENT_TYPE, JSON)], body)
}

fn error(code: StatusCode, why: &str) -> Answer {
    json(code, &serde_json::json!({ "error": why }))
}
