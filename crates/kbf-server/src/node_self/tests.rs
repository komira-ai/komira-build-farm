//! The four node calls of a host's updater: their shapes, who is admitted to them, and what the
//! handlers do without the listener's layer. The real listener, with a real updater client, is
//! `tests/updater_node_calls.rs`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use kbf_front::{Backend, Cache};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use rcgen::{CertificateParams, KeyPair, SanType};
use tower_service::Service as _;

use super::*;
use crate::DenyList;
use crate::proxy_auth::Identity;

fn call(method: &str, uri: &str) -> Option<(String, Kind)> {
    let method: Method = method.parse().expect("a method");
    let uri: Uri = uri.parse().expect("a uri");
    node_call(&method, &uri).map(|c| (c.node.to_owned(), c.kind))
}

/// The four calls, and only those shapes: the node's id and which call it is are returned, and
/// nothing else (another verb, method, query, spelling or extra segment) names a node.
#[test]
fn the_node_calls_of_an_updater_are_four_exact_shapes() {
    for (method, uri, node, kind) in [
        ("POST", "/v1/nodes/host-1:drain", "host-1", Kind::Drain),
        (
            "POST",
            "/v1/nodes/host-1:uncordon",
            "host-1",
            Kind::Uncordon,
        ),
        ("GET", "/v1/nodes/host-1", "host-1", Kind::Status),
        (
            "GET",
            "/v1/nodes/host-1/storage-health",
            "host-1",
            Kind::StorageHealth,
        ),
        (
            "GET",
            "http://frontend.test/v1/nodes/host-1",
            "host-1",
            Kind::Status,
        ),
        ("GET", "/v1/nodes/9", "9", Kind::Status),
    ] {
        assert_eq!(
            call(method, uri),
            Some((node.to_owned(), kind)),
            "{method} {uri}"
        );
    }
    let longest = format!("/v1/nodes/{}", "a".repeat(63));
    assert_eq!(call("GET", &longest).map(|c| c.0.len()), Some(63));
    let long = format!("/v1/nodes/{}", "a".repeat(64));
    for (method, uri) in [
        ("GET", "/v1/nodes"),
        ("GET", "/v1/nodes/"),
        ("GET", "/v1/nodes/host-1/"),
        ("GET", "/v1/nodes/host-1/leases"),
        ("GET", "/v1/nodes/host-1/storage-health/"),
        ("POST", "/v1/nodes/host-1/storage-health"),
        ("POST", "/v1/nodes/host-1"),
        ("GET", "/v1/nodes/host-1:drain"),
        ("PUT", "/v1/nodes/host-1"),
        ("DELETE", "/v1/nodes/host-1"),
        ("HEAD", "/v1/nodes/host-1"),
        ("POST", "/v1/nodes/host-1:cordon"),
        ("POST", "/v1/nodes/host-1:Drain"),
        ("POST", "/v1/nodes/host-1:drain:drain"),
        ("POST", "/v1/nodes/host-1:"),
        ("POST", "/v1/nodes/:drain"),
        ("GET", "/v1/nodes/host-1?x=1"),
        ("POST", "/v1/nodes/host-1:drain?x=1"),
        ("GET", "/v1/nodes/HOST-1"),
        ("GET", "/v1/nodes/host_1"),
        ("GET", "/v1/nodes/host.1"),
        ("GET", "/v1/nodes/-host"),
        ("GET", "/v1/nodes/host-"),
        ("GET", "/v1/nodes/host%2D1"),
        ("POST", "/v1/nodes/host-1%3Adrain"),
        ("GET", "/v1/nodes/host-1/../host-2"),
        ("GET", "/v1/nodes/../host-2"),
        ("GET", "/v1/nodes/.."),
        ("GET", "//v1/nodes/host-1"),
        ("GET", "/v1/nodesx/host-1"),
        ("GET", "/v1/node/host-1"),
        ("GET", "/v1/members/host-1"),
        ("GET", "/v1/members/host-1/safe-to-stop"),
        ("GET", "/v1/enroll/nodes"),
        ("GET", long.as_str()),
    ] {
        assert_eq!(call(method, uri), None, "{method} {uri}");
    }
}

/// A certificate with these DNS names and, if given, this role, as DER (self-signed: TLS has
/// verified the chain before the listener reads a certificate).
fn der(role: Option<&str>, dns: &[&str]) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = dns
        .iter()
        .map(|name| SanType::DnsName((*name).try_into().expect("an IA5 name")))
        .collect();
    if let Some(role) = role {
        let uri = format!("urn:kbf:role:{role}");
        params
            .subject_alt_names
            .push(SanType::URI(uri.try_into().expect("an IA5 name")));
    }
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("sign").der().to_vec()
}

fn presents(der: Vec<u8>) -> Presented {
    Presented::Leaf(Some(der))
}

fn drain_of(node: &str) -> NodeCall<'_> {
    NodeCall {
        node,
        kind: Kind::Drain,
    }
}

/// An updater certificate is admitted for the node it names and for no other, and the call
/// carries that node and the certificate on to the handlers.
#[test]
fn an_updater_certificate_is_admitted_for_its_own_node_only() {
    let own = presents(der(Some("updater"), &["node-a"]));
    for kind in [
        Kind::Drain,
        Kind::Uncordon,
        Kind::Status,
        Kind::StorageHealth,
    ] {
        let call = NodeCall {
            node: "node-a",
            kind,
        };
        let admitted = admit(Admission::Certified, &call, &own).expect("admitted");
        assert_eq!(admitted.node(), "node-a", "{kind:?}");
        let other = NodeCall {
            node: "node-b",
            kind,
        };
        assert_eq!(
            admit(Admission::Certified, &other, &own).expect_err("another node"),
            StatusCode::FORBIDDEN,
            "{kind:?}"
        );
    }
}

/// Every other certificate is refused, and a call with none is unauthenticated: a node, a
/// frontend, a storage or a daemon certificate for the very node of the call, a certificate
/// with no role, with two nodes or with no node, bytes that are no certificate, no certificate
/// at all, plain text, and the identity a proxy forwarded.
#[test]
fn nothing_but_an_updater_certificate_is_admitted() {
    let call = drain_of("node-a");
    for role in ["daemon", "storage", "frontend", "coordinator", "nonsense"] {
        let presented = presents(der(Some(role), &["node-a"]));
        assert_eq!(
            admit(Admission::Certified, &call, &presented).expect_err(role),
            StatusCode::FORBIDDEN,
            "{role}"
        );
    }
    for (what, presented) in [
        ("no role", presents(der(None, &["node-a"]))),
        (
            "two nodes",
            presents(der(Some("updater"), &["node-a", "node-b"])),
        ),
        ("no node", presents(der(Some("updater"), &[]))),
        ("not a certificate", presents(b"not a certificate".to_vec())),
    ] {
        assert_eq!(
            admit(Admission::Certified, &call, &presented).expect_err(what),
            StatusCode::FORBIDDEN,
            "{what}"
        );
    }
    let own = presents(der(Some("updater"), &["node-a"]));
    let proxied = Presented::Proxy(Identity {
        node_id: Some("node-a".to_owned()),
        role: Some(PeerRole::Updater),
        ips: Vec::new(),
    });
    for (what, admission, presented) in [
        (
            "no certificate",
            Admission::Certified,
            Presented::Leaf(None),
        ),
        ("a proxied identity", Admission::Certified, proxied),
        ("plain text", Admission::Open, own),
    ] {
        assert_eq!(
            admit(admission, &call, &presented).expect_err(what),
            StatusCode::UNAUTHORIZED,
            "{what}"
        );
    }
}

fn farm() -> Arc<Farm> {
    Arc::new(Farm::new(
        Backend::from(Arc::new(Cache::memory())),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ))
}

fn updater_of(node: &str) -> UpdaterCall {
    UpdaterCall {
        node: node.to_owned(),
        cert: PeerCert::from_der(&der(Some("updater"), &[node])).expect("a certificate"),
    }
}

/// The router's answer to `method uri` with `body`, from the updater `who` if given (the
/// listener's layer puts it there), over a farm that knows no node.
async fn ask(
    peers: Peers,
    who: Option<UpdaterCall>,
    method: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let mut router = routes(farm(), None, None, peers).into_axum_router();
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(body.to_owned()))
        .expect("a request");
    if let Some(who) = who {
        request.extensions_mut().insert(who);
    }
    let answer = router.call(request).await.expect("an answer");
    let code = answer.status();
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .expect("a body");
    (code, serde_json::from_slice(&bytes).unwrap_or_default())
}

fn open() -> Peers {
    Peers::Certified { deny_list: None }
}

/// A call that did not pass the layer (no admitted updater with it), or that names another
/// node than the one admitted, is refused by the handlers too, whichever of the four it is:
/// by the node they were told, and (with a certificate check on the listener) by the
/// certificate as well.
#[tokio::test]
async fn a_handler_serves_only_the_node_the_layer_admitted() {
    for (method, uri) in [
        ("POST", "/v1/nodes/node-a:drain"),
        ("POST", "/v1/nodes/node-a:uncordon"),
        ("GET", "/v1/nodes/node-a"),
        ("GET", "/v1/nodes/node-a/storage-health"),
    ] {
        let (code, _) = ask(open(), None, method, uri, "").await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{method} {uri} unadmitted");
        let (code, _) = ask(open(), Some(updater_of("node-b")), method, uri, "").await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{method} {uri} another node");
        // Where the listener checks no certificate against the node, the node the layer
        // admitted is all that stands between an updater and another node's call.
        let unchecked = Peers::Unauthenticated;
        let (code, _) = ask(unchecked, Some(updater_of("node-b")), method, uri, "").await;
        assert_eq!(
            code,
            StatusCode::FORBIDDEN,
            "{method} {uri} another node, unchecked"
        );
    }
}

/// A node the farm does not know is a 404 for its status, its drain and its uncordon, a verb
/// that is not one of the two is a 404 whoever asks, a bad drain body is a 400, and the
/// storage health of a server with no members is a 404.
#[tokio::test]
async fn the_handlers_answer_what_the_farm_says() {
    let who = || Some(updater_of("node-a"));
    for (method, uri, body, want) in [
        ("GET", "/v1/nodes/node-a", "", StatusCode::NOT_FOUND),
        ("POST", "/v1/nodes/node-a:drain", "", StatusCode::NOT_FOUND),
        (
            "POST",
            "/v1/nodes/node-a:drain",
            r#"{"deadline_secs": 5}"#,
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            "/v1/nodes/node-a:uncordon",
            "",
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            "/v1/nodes/node-a:drain",
            "not json",
            StatusCode::BAD_REQUEST,
        ),
        ("POST", "/v1/nodes/node-a:cordon", "", StatusCode::NOT_FOUND),
        (
            "GET",
            "/v1/nodes/node-a/storage-health",
            "",
            StatusCode::NOT_FOUND,
        ),
    ] {
        let (code, said) = ask(open(), who(), method, uri, body).await;
        assert_eq!(code, want, "{method} {uri} {body}: {said}");
    }
    let (_, said) = ask(open(), who(), "POST", "/v1/nodes/node-a:cordon", "").await;
    assert_eq!(said["error"], "no_action");
}

/// A call with no `:verb` at all reaches the handler only through the route of the node (the
/// layer refuses it first); the handler refuses it as an unknown action.
#[tokio::test]
async fn a_post_to_a_bare_node_is_an_unknown_action() {
    let (code, said) = ask(
        open(),
        Some(updater_of("node-a")),
        "POST",
        "/v1/nodes/node-a",
        "",
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(said["error"], "no_action");
}

/// A body of more than a drain's few bytes is refused before it is read.
#[tokio::test]
async fn a_large_body_is_refused() {
    let body = "x".repeat(MAX_BODY + 1);
    let (code, _) = ask(
        open(),
        Some(updater_of("node-a")),
        "POST",
        "/v1/nodes/node-a:drain",
        &body,
    )
    .await;
    assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE);
}

/// The listener's deny list applies to the node of an updater as it does to a daemon's calls:
/// a listed node is refused (403), and a list that cannot be read refuses the call as
/// unavailable (503).
#[tokio::test]
async fn the_deny_list_applies_to_the_node() {
    let dir = crate::test_dir::scratch_dir("node-self-deny");
    let path = dir.join("denied");
    std::fs::write(&path, "node node-a\n").expect("write the list");
    let list = DenyList::open(&path).expect("a list");
    let peers = Peers::Certified {
        deny_list: Some(list),
    };
    let (code, said) = ask(
        peers.clone(),
        Some(updater_of("node-a")),
        "GET",
        "/v1/nodes/node-a",
        "",
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{said}");
    assert!(
        said["error"]
            .as_str()
            .unwrap_or_default()
            .contains("denied")
    );
    let (code, _) = ask(
        peers.clone(),
        Some(updater_of("node-b")),
        "GET",
        "/v1/nodes/node-b",
        "",
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND, "an unlisted node is served");

    std::fs::remove_file(&path).expect("remove the list");
    let (code, _) = ask(
        peers,
        Some(updater_of("node-b")),
        "GET",
        "/v1/nodes/node-b",
        "",
    )
    .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
}

/// The storage health of a server with a state store but no members is a 404 too: it needs
/// both.
#[tokio::test]
async fn storage_health_without_members_is_a_404_beside_a_store() {
    let clock = Arc::new(ManualClock::default());
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let mut router = routes(farm(), None, Some(store), open()).into_axum_router();
    let mut request = Request::builder()
        .uri("/v1/nodes/node-a/storage-health")
        .body(Body::empty())
        .expect("a request");
    request.extensions_mut().insert(updater_of("node-a"));
    let answer = router.call(request).await.expect("an answer");
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
}
