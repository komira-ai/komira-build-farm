//! What a host's updater may ask of its frontend, on the worker listener: four calls about its
//! own node, and no others (`docs/design/updates.md`).
//!
//! The worker listener is where the frontend already authenticates node certificates (mutual
//! TLS against `--worker-client-ca`), and every host that runs a daemon or a storage node
//! reaches it. A host's updater presents the certificate the farm CA made for it
//! (`urn:kbf:role:updater`, one DNS name: the node id), and no token is involved:
//!
//! | call | what it does |
//! |---|---|
//! | `POST /v1/nodes/{id}:drain` | cordons the node and waits for its leases (body `{"deadline_secs": N}`, optional) |
//! | `POST /v1/nodes/{id}:uncordon` | returns it to placement |
//! | `GET /v1/nodes/{id}` | the node as `GET /v1/nodes/{id}` of the operator API has it |
//! | `GET /v1/nodes/{id}/storage-health` | the node's own storage member and two storage-wide numbers ([`storage`]) |
//!
//! **Who may.** [`admit`] decides, in [`crate::worker_roles::WorkerRoles`] before anything is
//! routed: the certificate TLS verified must name the role `updater` and exactly one node, and
//! that node must be the `{id}` of the call. A daemon, storage, frontend or coordinator
//! certificate, a call with no certificate, and an updater of another node are refused (401
//! without a certificate, 403 otherwise). The worker listener's deny list then applies to the
//! node, as it does to a daemon's calls ([`crate::identity::Peers::admit`]). Every other call
//! of an updater certificate is refused: the shapes of [`node_call`] are exact, and the
//! router below serves nothing else, so no operator route is reachable from here whatever the
//! certificate.
//!
//! **Which shapes.** `{id}` is a node id as the farm spells it (1 to 63 of `a-z`, `0-9` and
//! `-`, not starting or ending with `-`), so a `.`, a `/`, a `%` or any other byte makes the
//! call none of the four: nothing is percent-decoded or normalized, and what is compared with
//! the certificate is what the handler reads. A query string makes it none of them, as does
//! any other verb (`:cordon` included) or method.
//!
//! A request over HTTP/1.1 reaches these routes through the same listener as the gRPC calls
//! (`--worker-tls-*` offers `h2` and `http/1.1`).

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::get;
use kbf_state::Store;
use kbf_storage::service::{Admission, PeerRole, role_of};
use kbf_types::WorkerId;
use tonic::Code;
use tonic::service::Routes;

use crate::api::{drain_action, error, json};
use crate::farm::{Farm, NodeAction};
use crate::identity::{PeerCert, Peers, Presented};
use crate::members::Members;

pub(crate) mod storage;

/// The most bytes a call of an updater may carry: a drain's body is a few.
const MAX_BODY: usize = 1024;

/// The four calls, by what they do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `POST /v1/nodes/{id}:drain`.
    Drain,
    /// `POST /v1/nodes/{id}:uncordon`.
    Uncordon,
    /// `GET /v1/nodes/{id}`.
    Status,
    /// `GET /v1/nodes/{id}/storage-health`.
    StorageHealth,
}

/// A call that has the shape of one of the four, about `node`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeCall<'a> {
    /// The node the path names.
    pub node: &'a str,
    /// Which of the four.
    pub kind: Kind,
}

/// The call `method` and `uri` make, if it has the shape of one of the four (see the module
/// docs for how exact the shapes are).
#[must_use]
pub fn node_call<'a>(method: &Method, uri: &'a Uri) -> Option<NodeCall<'a>> {
    if uri.query().is_some() {
        return None;
    }
    let rest = uri.path().strip_prefix("/v1/nodes/")?;
    let call = |node: &'a str, kind| is_node_label(node).then_some(NodeCall { node, kind });
    if let Some((node, verb)) = rest.split_once(':') {
        let kind = match verb {
            "drain" => Kind::Drain,
            "uncordon" => Kind::Uncordon,
            _ => return None,
        };
        if method != Method::POST {
            return None;
        }
        return call(node, kind);
    }
    if method != Method::GET {
        return None;
    }
    match rest.split_once('/') {
        None => call(rest, Kind::Status),
        Some((node, "storage-health")) => call(node, Kind::StorageHealth),
        Some(_) => None,
    }
}

/// Whether `id` is a node id: a lower-case DNS label.
fn is_node_label(id: &str) -> bool {
    (1..=63).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

/// A call [`admit`] let through: the node of the certificate, which is the node of the call,
/// and the certificate. The listener's layer puts it in the request's extensions, and nothing
/// else does.
#[derive(Clone, Debug)]
pub struct UpdaterCall {
    node: String,
    cert: PeerCert,
}

impl UpdaterCall {
    /// The node, as the certificate names it.
    #[must_use]
    pub fn node(&self) -> &str {
        &self.node
    }
}

/// Whether the call `call`, presenting `presented`, may be made, as the module docs say.
///
/// # Errors
/// 401: no certificate (plain text, or a proxied identity: the listener's certificate is the
/// only credential here). 403: a certificate that does not parse, names no role or another
/// than `updater`, does not name exactly one node, or names another node than the call.
pub fn admit(
    admission: Admission,
    call: &NodeCall<'_>,
    presented: &Presented,
) -> Result<UpdaterCall, StatusCode> {
    let leaf = match (admission, presented.leaf()) {
        (Admission::Certified, Some(leaf)) => leaf,
        _ => return Err(StatusCode::UNAUTHORIZED),
    };
    match role_of(leaf) {
        Ok(PeerRole::Updater) => {}
        _ => return Err(StatusCode::FORBIDDEN),
    }
    let cert = PeerCert::from_der(leaf).map_err(|_| StatusCode::FORBIDDEN)?;
    match cert.node() {
        Ok(node) if node == call.node => Ok(UpdaterCall {
            node: node.to_owned(),
            cert,
        }),
        _ => Err(StatusCode::FORBIDDEN),
    }
}

/// What the handlers share.
struct Calls {
    farm: Arc<Farm>,
    members: Option<Arc<Members>>,
    store: Option<Arc<dyn Store>>,
    peers: Peers,
}

/// The routes of the four calls, to be served on the worker listener behind
/// [`crate::worker_roles::WorkerRolesLayer`], which lets only [`admit`]ted calls reach them.
/// `members` and `store` are the storage members and the state store the storage health reads
/// (`None`: this server keeps none, and that call answers 404); `peers` applies the listener's
/// deny list to the node.
#[must_use]
pub fn routes(
    farm: Arc<Farm>,
    members: Option<Arc<Members>>,
    store: Option<Arc<dyn Store>>,
    peers: Peers,
) -> Routes {
    let router = Router::new()
        .route("/v1/nodes/{target}", get(status).post(act))
        .route("/v1/nodes/{id}/storage-health", get(storage_health))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(Arc::new(Calls {
            farm,
            members,
            store,
            peers,
        }));
    // The unimplemented fallback for gRPC paths is the base `Routes` carries.
    let mut routes = Routes::default();
    let base = std::mem::take(routes.axum_router_mut());
    *routes.axum_router_mut() = base.merge(router);
    routes
}

/// The node `who` is, if it is `node`, and the listener's deny list lets it through.
async fn own(
    calls: &Calls,
    who: Option<Extension<UpdaterCall>>,
    node: &str,
) -> Result<WorkerId, Response> {
    let Some(Extension(who)) = who.filter(|Extension(who)| who.node == node) else {
        return Err(error(
            StatusCode::FORBIDDEN,
            "a host's updater may call only its own node",
        ));
    };
    match calls.peers.admit(Some(&who.cert), node).await {
        Ok(()) => Ok(WorkerId::new(node)),
        Err(refused) => {
            let code = match refused.code() {
                Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::FORBIDDEN,
            };
            Err(error(code, refused.message()))
        }
    }
}

async fn status(
    State(calls): State<Arc<Calls>>,
    who: Option<Extension<UpdaterCall>>,
    Path(node): Path<String>,
) -> Response {
    let node = match own(&calls, who, &node).await {
        Ok(node) => node,
        Err(refused) => return refused,
    };
    match calls.farm.node_detail(&node) {
        Some(detail) => json(StatusCode::OK, &detail),
        None => error(StatusCode::NOT_FOUND, "no_node"),
    }
}

async fn act(
    State(calls): State<Arc<Calls>>,
    who: Option<Extension<UpdaterCall>>,
    Path(target): Path<String>,
    body: Bytes,
) -> Response {
    let (node, verb) = target.rsplit_once(':').unwrap_or((&target, ""));
    let action = match verb {
        "uncordon" => Some(Ok(NodeAction::Uncordon)),
        "drain" => Some(drain_action(&body)),
        _ => None,
    };
    let node = match own(&calls, who, node).await {
        Ok(node) => node,
        Err(refused) => return refused,
    };
    let action = match action {
        Some(Ok(action)) => action,
        Some(Err((code, why))) => return error(code, &why),
        None => return error(StatusCode::NOT_FOUND, "no_action"),
    };
    match calls.farm.place(&node, action) {
        Ok(view) => json(StatusCode::OK, &view),
        Err(unknown) => error(StatusCode::NOT_FOUND, &unknown.to_string()),
    }
}

async fn storage_health(
    State(calls): State<Arc<Calls>>,
    who: Option<Extension<UpdaterCall>>,
    Path(node): Path<String>,
) -> Response {
    let node = match own(&calls, who, &node).await {
        Ok(node) => node,
        Err(refused) => return refused,
    };
    storage::one(node, calls.members.clone(), calls.store.clone()).await
}

#[cfg(test)]
mod tests;
