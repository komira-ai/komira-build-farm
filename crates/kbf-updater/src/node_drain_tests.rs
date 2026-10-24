//! The real drain of a daemon host's node against a fake operator API, route by route.

use std::time::Duration;

use serde_json::json;

use crate::inventory::{Host, Role};
use crate::node_drain::NodeDrain;
use crate::operator::OperatorApi;
use crate::ports::{Drain, PortErrorKind};
use crate::testutil::{FakeApi, KEY, TOKEN, closed_port, fake_api_by, file, http, scratch};

fn dm(node_id: Option<&str>) -> Host {
    Host {
        alias: "dm".to_owned(),
        address: "secret-host.farm.example".to_owned(),
        role: Role::Daemon,
        host_key: KEY.to_owned(),
        node_id: node_id.map(str::to_owned),
    }
}

fn adapter(server: &FakeApi, name: &str) -> NodeDrain {
    let token = file(&scratch(name), "token", TOKEN, 0o600);
    NodeDrain::with_api(
        OperatorApi::new(server.addr, token)
            .expect("api")
            .with_timeout(Duration::from_millis(500)),
    )
}

fn node(placement: &str, leases: &[&str]) -> String {
    json!({
        "node_id": "dm-node", "connected": true, "placement": {"state": placement},
        "lease_count": leases.len(), "leases": leases,
    })
    .to_string()
}

/// A fake API that answers every request with `status` and `body`.
fn answering(status: u16, body: &str) -> FakeApi {
    let answer = http(status, body);
    fake_api_by(move |_, _| Some(answer.clone()))
}

/// Catches a drain aimed at the wrong route, verb or body: it is `POST /v1/nodes/<id>:drain`
/// with the deadline in seconds as JSON, the operator's bearer token and a JSON content type,
/// and a 200 is success.
#[test]
fn a_drain_posts_the_deadline_to_the_nodes_drain_route() {
    let server = answering(200, &node("draining", &["1.1"]));
    let drain = adapter(&server, "nd-drain");
    drain
        .drain(&dm(Some("dm-node")), Duration::from_secs(600))
        .expect("drained");
    let sent = server.requests();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].starts_with("POST /v1/nodes/dm-node:drain HTTP/1.1\r\n"),
        "{}",
        sent[0]
    );
    assert!(sent[0].contains(&format!("Authorization: Bearer {TOKEN}\r\n")));
    assert!(sent[0].contains("Content-Type: application/json\r\n"));
    assert!(
        sent[0].ends_with("\r\n\r\n{\"deadline_secs\":600}"),
        "{}",
        sent[0]
    );
}

/// Catches an undrain that is not the uncordon route, or that sends a body: it is
/// `POST /v1/nodes/<id>:uncordon` with no body.
#[test]
fn an_undrain_posts_to_the_nodes_uncordon_route() {
    let server = answering(200, &node("serving", &[]));
    adapter(&server, "nd-undrain")
        .undrain(&dm(Some("dm-node")))
        .expect("undrained");
    let sent = server.requests();
    assert!(
        sent[0].starts_with("POST /v1/nodes/dm-node:uncordon HTTP/1.1\r\n"),
        "{}",
        sent[0]
    );
    assert!(sent[0].contains("Content-Length: 0\r\n") && sent[0].ends_with("\r\n\r\n"));
}

/// Catches the lease count read from anywhere but the node's `leases`: the number of entries
/// is what comes back, whatever the state of the drain (draining, drained, paused).
#[test]
fn the_leases_are_the_entries_of_the_nodes_list() {
    for (placement, leases) in [
        ("draining", vec!["1.1", "1.2"]),
        ("drained", vec![]),
        ("drain_paused", vec!["1.3"]),
        ("cordoned", vec![]),
    ] {
        let server = answering(200, &node(placement, &leases));
        let got = adapter(&server, "nd-leases").leases(&dm(Some("dm-node")));
        assert_eq!(got, Ok(leases.len()), "{placement}");
        assert!(server.requests()[0].starts_with("GET /v1/nodes/dm-node HTTP/1.1\r\n"));
    }
}

/// Catches a node that was returned to placement being read as one with no leases: a node that
/// is `serving` is no longer drained, and the read says so instead of letting an install
/// go ahead on a node that takes work.
#[test]
fn a_serving_node_is_not_a_drained_one() {
    let server = answering(200, &node("serving", &[]));
    let e = adapter(&server, "nd-serving")
        .leases(&dm(Some("dm-node")))
        .expect_err("serving");
    assert!(
        e.to_string().contains("node dm-node is serving again"),
        "{e}"
    );
}

/// Catches answers read wrongly: a 404 says the frontend does not know the node (an answer
/// that says no), the gates' refusals are named with what to check, a 5xx is no answer, a
/// body that is not a node is an error, and none of it holds the token or an address. This
/// holds for all three calls.
#[test]
fn errors_are_of_the_right_kind_and_leak_nothing() {
    let cases = [
        (
            404,
            "{\"error\":\"no_node\"}",
            PortErrorKind::Other,
            "does not know node dm-node",
        ),
        (401, "{}", PortErrorKind::Other, "refused the call (401)"),
        (403, "{}", PortErrorKind::Other, "(403)"),
        (503, "{}", PortErrorKind::Unreachable, "answered 503"),
        (302, "{}", PortErrorKind::Other, "answered 302"),
    ];
    for (status, body, kind, text) in cases {
        let server = answering(status, body);
        let drain = adapter(&server, "nd-errors");
        let host = dm(Some("dm-node"));
        let errors = [
            drain
                .drain(&host, Duration::from_secs(5))
                .expect_err("drain"),
            drain.leases(&host).expect_err("leases"),
            drain.undrain(&host).expect_err("undrain"),
        ];
        for e in errors {
            assert_eq!(
                (e.kind(), e.to_string().contains(text)),
                (kind, true),
                "{status}: {e}"
            );
            for secret in [TOKEN, "secret-host", &server.addr.to_string()] {
                assert!(!e.to_string().contains(secret), "{secret} in {e}");
            }
        }
    }
    let server = answering(200, "<html>");
    let e = adapter(&server, "nd-html")
        .leases(&dm(Some("dm-node")))
        .expect_err("not a node");
    assert!(e.to_string().contains("is not what it should be"), "{e}");
    let dead = NodeDrain::with_api(
        OperatorApi::new(
            closed_port(),
            file(&scratch("nd-dead"), "token", TOKEN, 0o600),
        )
        .expect("api"),
    );
    assert!(
        dead.undrain(&dm(Some("dm-node")))
            .expect_err("dead")
            .is_unreachable()
    );
}

/// Catches a path built from text that is not a node id: a host without a node id, or with
/// one that is not a DNS label (a path, a verb, a space), is refused for all three calls and
/// the operator API is not called.
#[test]
fn a_host_without_a_usable_node_id_is_refused_without_a_call() {
    let server = answering(200, "{}");
    let drain = adapter(&server, "nd-noid");
    for id in [None, Some("../x"), Some("dm:drain"), Some("a b"), Some("")] {
        let host = dm(id);
        for e in [
            drain
                .drain(&host, Duration::from_secs(5))
                .expect_err("drain"),
            drain.leases(&host).expect_err("leases"),
            drain.undrain(&host).expect_err("undrain"),
        ] {
            assert!(
                e.to_string().contains("dm has no usable node id"),
                "{id:?}: {e}"
            );
        }
    }
    assert!(server.requests().is_empty());
}

fn adapter_if_present(server: &FakeApi, name: &str) -> NodeDrain {
    let token = file(&scratch(name), "token", TOKEN, 0o600);
    NodeDrain::if_present(
        OperatorApi::new(server.addr, token)
            .expect("api")
            .with_timeout(Duration::from_millis(500)),
    )
}

/// A storage host that runs a daemon has it drained, counted and given back like any daemon's:
/// the same three routes.
#[test]
fn a_daemon_the_frontend_knows_is_drained_counted_and_given_back() {
    let server = answering(200, &node("draining", &["1.1", "1.2"]));
    let drain = adapter_if_present(&server, "nd-present");
    let host = dm(Some("dm-node"));
    drain.drain(&host, Duration::from_secs(60)).expect("drain");
    assert_eq!(drain.leases(&host), Ok(2));
    drain.undrain(&host).expect("undrain");
    let sent = server.requests();
    let lines: Vec<&str> = sent
        .iter()
        .map(|r| r.lines().next().unwrap_or(""))
        .collect();
    assert_eq!(
        lines,
        [
            "POST /v1/nodes/dm-node:drain HTTP/1.1",
            "GET /v1/nodes/dm-node HTTP/1.1",
            "POST /v1/nodes/dm-node:uncordon HTTP/1.1",
        ]
    );
}

/// A storage host with no daemon is not a failure: the frontend does not know the node (404),
/// so there is nothing to drain, no lease and nothing to give back. Any other answer is still
/// the failure it is.
#[test]
fn a_storage_host_with_no_daemon_has_nothing_to_drain() {
    let server = answering(404, "{\"error\":\"no_node\"}");
    let drain = adapter_if_present(&server, "nd-absent");
    let host = dm(Some("dm-node"));
    assert_eq!(drain.drain(&host, Duration::from_secs(60)), Ok(()));
    assert_eq!(drain.leases(&host), Ok(0));
    assert_eq!(drain.undrain(&host), Ok(()));
    assert_eq!(server.requests().len(), 3);

    for status in [401, 403, 503] {
        let server = answering(status, "{}");
        let drain = adapter_if_present(&server, "nd-absent-errors");
        assert!(
            drain.drain(&host, Duration::from_secs(1)).is_err(),
            "{status}"
        );
        assert!(drain.leases(&host).is_err(), "{status}");
        assert!(drain.undrain(&host).is_err(), "{status}");
    }
}
