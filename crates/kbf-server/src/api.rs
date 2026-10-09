//! The operator API: HTTP/JSON under `/v1`, on its own listener (`--api-listen`), for
//! the Fleet UI, scripts and clients (`docs/design/fleet-updates.md` section 11.4;
//! `docs/api.md`).
//!
//! - `GET /v1/nodes`: every node registered since the server started, with the newest
//!   software status each sent and where it is in placement ([`crate::fleet`]), and
//!   each node of `--expected-nodes` that has not registered, as `absent`
//!   ([`crate::expected`]).
//! - `POST /v1/nodes/{node}:cordon`, `:drain` and `:uncordon`: take a node out of
//!   placement, drain it, or return it ([`NodeAction`]). A drain's body may be
//!   `{"deadline_secs": N}`; without one the deadline is [`DRAIN_DEADLINE`]. The answer
//!   is the node as it is after the action.
//!
//! Reads answer anyone who reaches the listener. A write must pass every gate of
//! [`write_request`], because a loopback peer is not an operator: every server host is
//! also a worker whose build actions can reach loopback, a reverse proxy on the host
//! makes remote callers loopback, and a browser on the host can POST to it from any
//! page. The gates: a loopback peer, no `Origin` header (no browser page), the bearer
//! token of `--api-token-file` ([`ApiToken`]; without one, writes are off), and a JSON
//! content type. The roles of `fleet-updates-security.md` section S9 are planned.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use kbf_front::MetaLog;
use kbf_objstore::ObjectStore;
use kbf_types::WorkerId;
use serde::Deserialize;

use crate::expected::ExpectedNodes;
use crate::farm::{Farm, NodeAction};
use crate::fleet;
use crate::token::ApiToken;

/// The media type of every body this API returns, and of every write it accepts.
pub const JSON: &str = "application/json";

/// How long a drain waits for leases when its request names no deadline
/// (`drain_deadline` in `fleet-updates.md` section 4.1).
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// What the handlers share: the farm, the token writes must present (`None`: writes
/// are off), and the nodes expected (`None`: none).
struct ApiState<M, O> {
    farm: Arc<Farm<M, O>>,
    token: Option<ApiToken>,
    expected: Option<Arc<ExpectedNodes>>,
}

/// The API's routes over `farm`; writes need `token`, and without one are refused.
/// Each node `expected` lists that has not registered is listed as `absent`. Writes
/// read the peer address from [`ConnectInfo`]: serve them with
/// `into_make_service_with_connect_info`.
pub fn router<M, O>(
    farm: Arc<Farm<M, O>>,
    token: Option<ApiToken>,
    expected: Option<Arc<ExpectedNodes>>,
) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Router::new()
        .route("/v1/nodes", get(nodes::<M, O>))
        .route("/v1/nodes/{target}", post(act::<M, O>))
        .with_state(Arc::new(ApiState {
            farm,
            token,
            expected,
        }))
}

async fn nodes<M, O>(State(api): State<Arc<ApiState<M, O>>>) -> Response
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let view = api.farm.nodes();
    let view = match &api.expected {
        Some(expected) => fleet::with_expected(view, &expected.current()),
        None => view,
    };
    json(StatusCode::OK, &view)
}

async fn act<M, O>(
    State(api): State<Arc<ApiState<M, O>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(target): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let request = Write {
        peer,
        headers: &headers,
        target: &target,
        body: &body,
    };
    match write_request(api.token.as_ref(), &request) {
        Ok((node, action)) => match api.farm.place(&node, action) {
            Ok(mut view) => {
                if let Some(expected) = &api.expected {
                    fleet::mark(&mut view, &expected.current());
                }
                json(StatusCode::OK, &view)
            }
            Err(unknown) => error(StatusCode::NOT_FOUND, &unknown.to_string()),
        },
        Err((code, why)) => error(code, &why),
    }
}

/// A write as it arrived.
#[derive(Clone, Copy, Debug)]
pub struct Write<'a> {
    /// Who sent it.
    pub peer: SocketAddr,
    /// Its headers.
    pub headers: &'a HeaderMap,
    /// The last path segment: `{node}:{verb}`.
    pub target: &'a str,
    /// Its body.
    pub body: &'a [u8],
}

/// The body a drain may carry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DrainBody {
    deadline_secs: u64,
}

/// The node and action `write` names, if it passes every gate and is well formed;
/// else the status and reason to answer with. The gates, in order:
///
/// 1. the peer is loopback, so the token never crosses a network in clear (a remote
///    operator comes through a TLS-terminating proxy on the host): else 403;
/// 2. no `Origin` header: a browser sends one with every cross-origin POST, and a
///    page must not act with an operator's token or from the host: else 403;
/// 3. writes are on (`token` is given): else 403;
/// 4. `Authorization: Bearer <token>`, compared in constant time: else 401;
/// 5. `Content-Type: application/json` (parameters allowed), which a browser cannot
///    send cross-origin without a preflight the API never answers: else 415.
///
/// # Errors
/// The gates above; then 404 for a target without a known verb, and 400 for a drain
/// body that is not `{"deadline_secs": N}`.
pub fn write_request(
    token: Option<&ApiToken>,
    write: &Write<'_>,
) -> Result<(WorkerId, NodeAction), (StatusCode, String)> {
    let Write {
        peer,
        headers,
        target,
        body,
    } = *write;
    if !peer.ip().to_canonical().is_loopback() {
        return Err((
            StatusCode::FORBIDDEN,
            format!("writes are accepted only from the server's own host, not {peer}"),
        ));
    }
    if headers.contains_key(ORIGIN) {
        return Err((
            StatusCode::FORBIDDEN,
            "a write from a browser page (with an Origin header) is refused".to_owned(),
        ));
    }
    let Some(token) = token else {
        return Err((
            StatusCode::FORBIDDEN,
            "writes are off: the server was started without --api-token-file".to_owned(),
        ));
    };
    let presented = headers.get(AUTHORIZATION).map(|v| v.as_bytes());
    if !presented.is_some_and(|p| token.admits(p)) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "a write needs Authorization: Bearer <the --api-token-file token>".to_owned(),
        ));
    }
    let content_type = headers.get(CONTENT_TYPE).map(|v| v.as_bytes());
    if !content_type.is_some_and(is_json) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("a write is sent as Content-Type: {JSON}"),
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

/// Whether a `Content-Type` value is `application/json`, in any case, with or without
/// parameters (`; charset=utf-8`).
fn is_json(value: &[u8]) -> bool {
    let essence = value.split(|&b| b == b';').next().unwrap_or_default();
    essence.trim_ascii().eq_ignore_ascii_case(JSON.as_bytes())
}

/// `value` as a JSON body. The views serialize plain strings, numbers and lists, which
/// cannot fail.
fn json(code: StatusCode, value: &impl serde::Serialize) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_default();
    (code, [(CONTENT_TYPE, JSON)], body).into_response()
}

/// An error answer; a 401 names the scheme it wants (RFC 9110 section 11.6.1).
fn error(code: StatusCode, why: &str) -> Response {
    let mut answer = json(code, &serde_json::json!({ "error": why }));
    if code == StatusCode::UNAUTHORIZED {
        let bearer = axum::http::HeaderValue::from_static("Bearer");
        answer.headers_mut().insert(WWW_AUTHENTICATE, bearer);
    }
    answer
}
