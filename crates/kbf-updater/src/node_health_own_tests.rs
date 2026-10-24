//! A storage host's health as its own updater reads it: one route, the node's own view
//! (`GET /v1/nodes/{id}/storage-health`), against a fake operator API.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::fakes::{FakeInstaller, Log};
use crate::inventory::{Host, Role};
use crate::node_health::NodeHealth;
use crate::operator::OperatorApi;
use crate::ports::{Health, HealthCheck, PortError, PortErrorKind};
use crate::testutil::{FakeApi, KEY, TOKEN, closed_port, fake_api_by, file, http, scratch};
use crate::version::Version;

const TARGET: &str = "0.1.7+g0123456789ab-3";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";
const PATH: &str = "/v1/nodes/st1/storage-health";

fn host(node_id: Option<&str>) -> Host {
    Host {
        alias: "st1".to_owned(),
        address: "secret-host.farm.example".to_owned(),
        role: Role::Storage,
        host_key: KEY.to_owned(),
        node_id: node_id.map(str::to_owned),
    }
}

fn target() -> Version {
    Version::parse(TARGET).expect("version")
}

/// The node's view: its member (`state`, build `commit`) and the storage numbers.
fn view(state: &str, commit: Option<&str>, active: usize, owed: Option<u64>) -> Value {
    json!({
        "node": "st1", "half": "member",
        "member": {"id": 4, "state": state, "build_commit": commit},
        "storage": {"members_active": active, "under_replicated_blobs": owed},
    })
}

/// A fake operator API that answers `status` and `body` to the node's view and 404 to anything
/// else.
fn answering(status: u16, body: &Value) -> FakeApi {
    let answer = http(status, &body.to_string());
    fake_api_by(move |_, head| {
        let path = head.split(' ').nth(1).unwrap_or_default();
        Some(if path == PATH {
            answer.clone()
        } else {
            http(404, "{}")
        })
    })
}

fn health(server: &FakeApi, name: &str) -> NodeHealth {
    let token = file(&scratch(name), "token", TOKEN, 0o600);
    let api = OperatorApi::new(server.addr, token)
        .expect("api")
        .with_timeout(Duration::from_millis(500));
    NodeHealth::for_updater(api, Arc::new(FakeInstaller::new(&Log::default())))
}

fn check(server: &FakeApi, name: &str) -> Result<Health, PortError> {
    health(server, name).check(&host(Some("st1")), &target())
}

fn waiting(result: &Result<Health, PortError>) -> String {
    match result {
        Ok(Health::Waiting(why)) => why.clone(),
        other => panic!("expected waiting, got {other:?}"),
    }
}

/// The host reads exactly its own node's view, once, and nothing of the enrollment registry or
/// the member list; a member that is active on the target's commit with nothing owed is
/// healthy, with either spelling of the commit (40 digits or the 12 of the version).
#[test]
fn a_storage_host_reads_only_its_own_view() {
    for built in [COMMIT, "0123456789ab"] {
        let server = answering(200, &view("active", Some(built), 3, Some(0)));
        assert_eq!(check(&server, "nho-ok"), Ok(Health::Healthy), "{built}");
        let sent = server.requests();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].starts_with(&format!("GET {PATH} HTTP/1.1\r\n")),
            "{}",
            sent[0]
        );
    }
}

/// The member's own facts decide: not active (each state), no build reported, another build.
#[test]
fn the_members_state_and_build_decide() {
    for state in ["joining", "catching_up", "draining", "down", "unknown"] {
        let server = answering(200, &view(state, Some(COMMIT), 3, Some(0)));
        let why = waiting(&check(&server, "nho-state"));
        assert!(why.contains(&format!("is {state}, not active")), "{why}");
    }
    let server = answering(200, &view("active", None, 3, Some(0)));
    let why = waiting(&check(&server, "nho-none"));
    assert!(why.contains("has not reported its build commit"), "{why}");
    let server = answering(200, &view("active", Some(OTHER), 3, Some(0)));
    let why = waiting(&check(&server, "nho-other"));
    assert!(
        why.contains("runs build") && why.contains("not the target's"),
        "{why}"
    );
}

/// The storage as a whole decides too: blobs known to be owed a copy keep the host waiting
/// (their number is in the line); a number the storage cannot say does not, since it may be
/// another member's gap; and no active member at all is a view in flux, not health.
#[test]
fn the_storage_numbers_decide() {
    let server = answering(200, &view("active", Some(COMMIT), 3, Some(7)));
    let why = waiting(&check(&server, "nho-owed"));
    assert!(why.contains("7 blobs are still owed"), "{why}");
    let server = answering(200, &view("active", Some(COMMIT), 3, None));
    assert_eq!(check(&server, "nho-unknown"), Ok(Health::Healthy));
    let server = answering(200, &view("active", Some(COMMIT), 0, Some(0)));
    let why = waiting(&check(&server, "nho-noactive"));
    assert!(why.contains("no storage member is active"), "{why}");
}

/// How the node stands decides before any member: a node with no storage address, or one the
/// registry does not know, fails at once; one no member (or two) has the address of waits, with
/// the frontend's reason; a member named in the answer is required for `member`.
#[test]
fn a_node_without_a_member_fails_or_waits_as_it_should() {
    let own = |half: &str, why: Option<&str>| {
        let mut body = json!({
            "node": "st1", "half": half, "member": null,
            "storage": {"members_active": 3, "under_replicated_blobs": 0},
        });
        if let Some(why) = why {
            body["why"] = json!(why);
        }
        body
    };
    let server = answering(
        200,
        &own("unknown_node", Some("st1 is not in the registry")),
    );
    let Ok(Health::Abandoned(why)) = check(&server, "nho-unknown") else {
        panic!("expected a failure");
    };
    assert!(why.contains("st1 is not an enrolled node"), "{why}");
    let server = answering(200, &own("no_storage", None));
    let Ok(Health::Abandoned(why)) = check(&server, "nho-nostorage") else {
        panic!("expected a failure");
    };
    assert!(why.contains("advertises no storage address"), "{why}");
    let server = answering(200, &own("unlinked", Some("no storage member registered")));
    assert_eq!(
        waiting(&check(&server, "nho-unlinked")),
        "no storage member registered"
    );
    let server = answering(200, &own("unlinked", None));
    assert!(waiting(&check(&server, "nho-unlinked2")).contains("no storage member is registered"));
    let server = answering(200, &own("member", None));
    assert!(waiting(&check(&server, "nho-nomember")).contains("member is not named"));
}

/// A version that names no commit fails at once, before anything is asked; a host without a
/// usable node id is refused without a call (no path is built from free text).
#[test]
fn a_target_without_a_commit_and_a_bad_node_id_are_refused_before_a_call() {
    let server = answering(200, &view("active", Some(COMMIT), 3, Some(0)));
    let bare = Version::parse("0.1.7").expect("version");
    let result = health(&server, "nho-bare").check(&host(Some("st1")), &bare);
    assert!(matches!(result, Ok(Health::Abandoned(_))), "{result:?}");
    for id in [None, Some("../x"), Some("st1/storage-health"), Some("")] {
        let e = health(&server, "nho-badid")
            .check(&host(id), &target())
            .expect_err("refused");
        assert!(
            e.to_string().contains("st1 has no usable node id"),
            "{id:?}: {e}"
        );
    }
    assert!(server.requests().is_empty());
}

/// An answer that is not understood is an error of the right kind and holds neither the token
/// nor an address: a frontend without the route or without members (404), a refusal of the
/// proxy (403, as an older frontend answers), a 5xx, and a body that is not the view.
#[test]
fn errors_are_of_the_right_kind_and_leak_nothing() {
    let cases = [
        (404, "{}", PortErrorKind::Other, "keeps no storage members"),
        (401, "{}", PortErrorKind::Other, "refused the call (401)"),
        (403, "{}", PortErrorKind::Other, "(403)"),
        (503, "{}", PortErrorKind::Unreachable, "answered 503"),
        (
            200,
            "<html>",
            PortErrorKind::Other,
            "is not what it should be",
        ),
        (
            200,
            "{\"half\":\"somewhere\"}",
            PortErrorKind::Other,
            "is not what it should be",
        ),
    ];
    for (status, body, kind, text) in cases {
        let answer = http(status, body);
        let server = fake_api_by(move |_, _| Some(answer.clone()));
        let e = check(&server, "nho-errors").expect_err(text);
        assert_eq!(
            (e.kind(), e.to_string().contains(text)),
            (kind, true),
            "{status}: {e}"
        );
        for secret in [TOKEN, "secret-host", &server.addr.to_string()] {
            assert!(!e.to_string().contains(secret), "{secret} in {e}");
        }
    }
    let token = file(&scratch("nho-dead"), "token", TOKEN, 0o600);
    let dead = NodeHealth::for_updater(
        OperatorApi::new(closed_port(), token).expect("api"),
        Arc::new(FakeInstaller::new(&Log::default())),
    );
    let e = dead.check(&host(Some("st1")), &target()).expect_err("dead");
    assert!(e.is_unreachable(), "{e}");
}

/// Held or not, a storage host is judged the same (it has no serving state to wait for), and
/// the roll's way of reading (`with_api`) is unchanged: it still asks for the registry and the
/// member list, never for the node's view.
#[test]
fn held_is_the_same_and_the_rolls_reading_is_unchanged() {
    let server = answering(200, &view("active", Some(COMMIT), 3, Some(0)));
    let held = health(&server, "nho-held").check_held(&host(Some("st1")), &target());
    assert_eq!(held, Ok(Health::Healthy));

    let token = file(&scratch("nho-roll"), "token", TOKEN, 0o600);
    let roll = NodeHealth::with_api(
        OperatorApi::new(server.addr, token)
            .expect("api")
            .with_timeout(Duration::from_millis(500)),
        Arc::new(FakeInstaller::new(&Log::default())),
    );
    let _ = roll.check(&host(Some("st1")), &target());
    let sent = server.requests();
    assert!(
        sent[1].starts_with("GET /v1/enroll/nodes HTTP/1.1\r\n"),
        "{}",
        sent[1]
    );
    assert_eq!(sent.len(), 2);
}
