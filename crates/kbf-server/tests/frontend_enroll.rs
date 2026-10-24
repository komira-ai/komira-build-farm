//! `kbf frontend --enroll-listen`, started the way the binary starts it: the enroll
//! listener, the CA and the operator API's enrollment routes are wired into one process
//! over a real SQLite state store. A token made through the operator API enrolls a node
//! through the enroll listener, and the node shows in the API's list.
//!
//! Catches: the enroll service built over the wrong store or CA (the token the API made
//! is unknown to the listener, or its pin is not the listener's CA), the listener or the
//! API routes left out of the frontend's start, the listener not closing when the
//! frontend's shutdown begins (the first test connects again after `serve` returns), and
//! the listener's own certificate never being replaced (the second test: the frontend's
//! background tasks must run `keep_fresh`).

use std::future::pending;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use kbf_enroll::client::{Enrollment, connect, enroll};
use kbf_enroll::config::{Config, Host};
use kbf_enroll::pin::Anchor;
use kbf_enroll::store::Store as NodeStore;
use kbf_proto::kbf::enroll::v1::NodeFacts;
use kbf_server::api_client::OperatorClient;
use kbf_server::enroll::api::{NODES, TOKENS};
use kbf_server::frontend::{self, FrontendFlags, start_frontend};
use kbf_types::enroll_api::{NodeList, TokenCreated};
use kbf_types::{Role, RoleSet, Token};

const API_TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

/// A new, empty directory for one test, removed when it drops: bind it before the
/// frontend it holds files for, so it is removed after that frontend has stopped.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("kbf-frontend-enroll-{name}"),
    )
}

fn flags(more: &[String]) -> FrontendFlags {
    let mut argv: Vec<String> = [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
    ]
    .map(str::to_owned)
    .to_vec();
    argv.extend_from_slice(more);
    frontend::parse(
        &frontend::command()
            .try_get_matches_from(argv)
            .expect("argv"),
    )
    .expect("flags")
}

fn unix_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs(),
    )
    .expect("time")
}

/// A token made by the operator API enrolls a node at the frontend's own enroll listener,
/// and the API lists the node afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn a_frontend_with_enroll_listen_enrolls_a_node_with_a_token_from_its_api() {
    let dir = scratch("e2e");
    let token_file = dir.join("api.token");
    std::fs::write(&token_file, format!("{API_TOKEN}\n")).expect("token file");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let f = flags(&[
        format!("--api-token-file={}", token_file.display()),
        format!("--state-store=sqlite://{}", dir.join("state.db").display()),
        "--enroll-listen=127.0.0.1:0".to_owned(),
        format!("--ca-dir={}", dir.join("ca").display()),
        "--ca-dev-file-signer".to_owned(),
        "--enroll-allow-unenforced-deny".to_owned(),
        "--frontend-name=localhost".to_owned(),
        // Stopped at the end of the test: no load balancer to wait for.
        "--drain-lb-wait=0".to_owned(),
    ]);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let handle = start_frontend(&f, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .expect("starts");
    let api = handle.api.expect("the operator API");
    let enroll_at = handle.enroll.expect("the enroll listener");
    let serving = tokio::spawn(handle.serve());

    let created = tokio::task::spawn_blocking(move || {
        let client = OperatorClient::new(api, Some(API_TOKEN.to_owned()));
        let reply = client
            .post(TOKENS, r#"{"roles":["daemon"],"max_uses":1}"#)
            .expect("create");
        assert_eq!(reply.status, 200, "{}", reply.body);
        serde_json::from_str::<TokenCreated>(&reply.body).expect("a created token")
    })
    .await
    .expect("join");
    let token: Token = created.token.parse().expect("a token");

    let root = dir.join("node");
    let text = format!(
        "[frontend]\nurl = \"https://localhost:{}\"\n[node]\nid = \"node-07\"\n[enroll]\ntoken_file = \"{}\"\n[dirs]\nstate = \"{}\"\n",
        enroll_at.port(),
        root.join("enroll.token").display(),
        root.join("state").display(),
    );
    let config = Config::parse(
        &text,
        &Host {
            hostname: "node-07".to_owned(),
            cpus: 4,
        },
    )
    .unwrap_or_else(|e| panic!("config: {e}"));
    std::fs::create_dir_all(&root).expect("node dir");
    std::fs::write(&config.token_file, token.to_line()).expect("token file");
    std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600))
        .expect("mode");
    let store = NodeStore::open(&config.state_dir).expect("store");
    let frontend = connect(&config.frontend, Anchor::Pin(*token.pin()))
        .await
        .expect("the listener's certificate is the API's CA");
    let installed = enroll(
        &frontend,
        &store,
        &Enrollment {
            config: &config,
            token: &token,
            roles: RoleSet::new([Role::Daemon]).expect("roles"),
            facts: NodeFacts {
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                commit: "abc".to_owned(),
                protocol: 1,
                package_version: "0.0.0".to_owned(),
            },
            now: unix_now(),
        },
    )
    .await
    .expect("enrolled");
    assert_eq!(installed.node_id, "node-07");
    assert_eq!(installed.pin().expect("pin"), *token.pin());

    let listed = tokio::task::spawn_blocking(move || {
        let client = OperatorClient::new(api, Some(API_TOKEN.to_owned()));
        let reply = client.get_gated(NODES).expect("list");
        assert_eq!(reply.status, 200, "{}", reply.body);
        serde_json::from_str::<NodeList>(&reply.body).expect("a list")
    })
    .await
    .expect("join");
    let ids: Vec<&str> = listed.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, ["node-07"]);
    assert_eq!(listed.nodes[0].state, "active");

    // The frontend stops: serve returns and the enroll listener's socket is closed.
    stop.send(()).expect("the frontend is still running");
    tokio::time::timeout(Duration::from_secs(30), serving)
        .await
        .expect("the frontend stops within its drain")
        .expect("join")
        .expect("served");
    let again = tokio::net::TcpStream::connect(enroll_at).await;
    assert!(
        again.is_err(),
        "the enroll listener still accepts: {again:?}"
    );
}

/// The frontend runs the task that replaces the enroll listener's certificate: with the
/// interval cut to 100 ms (the flag's minimum is an hour, so the test sets the field the
/// flags leave `None`), the served configuration is replaced again and again within a
/// bounded time of the frontend serving.
///
/// Catches: the renewal future built but never spawned (`background.extend` replaced by
/// dropping it): the listener's certificate would expire after its lifetime and every node
/// would fail to connect; nothing else in the tests waits that long.
#[tokio::test(flavor = "multi_thread")]
async fn a_serving_frontend_replaces_the_enroll_listeners_certificate() {
    let dir = scratch("renewal");
    let mut f = flags(&[
        format!("--state-store=sqlite://{}", dir.join("state.db").display()),
        "--enroll-listen=127.0.0.1:0".to_owned(),
        format!("--ca-dir={}", dir.join("ca").display()),
        "--ca-dev-file-signer".to_owned(),
        "--enroll-allow-unenforced-deny".to_owned(),
        "--frontend-name=localhost".to_owned(),
    ]);
    f.enroll.as_mut().expect("enroll flags").renew_every = Some(Duration::from_millis(100));
    let handle = start_frontend(&f, || Ok(pending::<()>())).expect("starts");
    let tls = handle.enroll_tls().expect("the enroll listener's TLS");
    assert_eq!(tls.reloads().applied, 0, "nothing replaced before serving");
    let serving = tokio::spawn(handle.serve());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tls.reloads().applied < 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the certificate was replaced {} times in 30 s",
            tls.reloads().applied
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(tls.reloads().rejected, 0);
    serving.abort();
}

/// `--enroll-listen` without `--enroll-allow-unenforced-deny` is refused while deny rows
/// are not enforced; the frontend start returns that refusal (the enroll preparation runs
/// before any listener is bound, so nothing serves).
#[tokio::test(flavor = "multi_thread")]
async fn a_frontend_start_returns_the_enroll_gate_refusal() {
    let dir = scratch("gate");
    let f = flags(&[
        format!("--state-store=sqlite://{}", dir.join("state.db").display()),
        "--enroll-listen=127.0.0.1:0".to_owned(),
        format!("--ca-dir={}", dir.join("ca").display()),
        "--ca-dev-file-signer".to_owned(),
        "--frontend-name=localhost".to_owned(),
    ]);
    let err = start_frontend(&f, || Ok(pending::<()>()))
        .err()
        .expect("the gate refuses")
        .to_string();
    assert!(
        err.contains("--enroll-allow-unenforced-deny"),
        "unexpected error: {err}"
    );
    assert!(
        !dir.join("ca").exists(),
        "nothing is written before the gate passes"
    );
}

/// The enroll registry lives in the state store, so a start with an enroll listener and no
/// state store is refused. The flag parser already requires `--state-store` with
/// `--enroll-listen`, so the test clears it on the parsed flags: this is the start's own
/// check, for callers that build the flags without the parser.
#[tokio::test(flavor = "multi_thread")]
async fn a_frontend_start_refuses_enroll_listen_without_a_state_store() {
    let dir = scratch("nostore");
    let mut f = flags(&[
        format!("--state-store=sqlite://{}", dir.join("state.db").display()),
        "--enroll-listen=127.0.0.1:0".to_owned(),
        format!("--ca-dir={}", dir.join("ca").display()),
        "--ca-dev-file-signer".to_owned(),
        "--enroll-allow-unenforced-deny".to_owned(),
        "--frontend-name=localhost".to_owned(),
    ]);
    f.server.state_store = None;
    let err = start_frontend(&f, || Ok(pending::<()>()))
        .err()
        .expect("refused")
        .to_string();
    assert_eq!(
        err,
        "--enroll-listen needs --state-store: the registry lives in it"
    );
}
