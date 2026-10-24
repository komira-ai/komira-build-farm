//! The safe-to-stop adapter against a fake operator API on loopback.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::time::Duration;

use crate::inventory::{Host, Role};
use crate::ports::{PortErrorKind, SafeToStop, Stop, Verdict};
use crate::safe_http::HttpSafeToStop;
use crate::secret::{file_rule, read_token};
use crate::testutil::{
    FakeApi, KEY, TOKEN, closed_port, eventually, fake_api, file, http, scratch,
};

fn host(alias: &str) -> Host {
    Host {
        alias: alias.to_owned(),
        address: "secret-host.farm.example".to_owned(),
        role: Role::Storage,
        host_key: KEY.to_owned(),
        node_id: None,
    }
}

fn adapter(addr: SocketAddr) -> (HttpSafeToStop, PathBuf) {
    let token_file = file(&scratch("tok"), "token", &format!("{TOKEN}\n"), 0o600);
    let a = HttpSafeToStop::new(addr, token_file.clone()).expect("adapter");
    (a.with_timeout(Duration::from_millis(400)), token_file)
}

fn ask(api: &FakeApi, script_host: &str) -> Result<Stop, crate::ports::PortError> {
    adapter(api.addr).0.check(&host(script_host))
}

const SAFE: &str = "{\"safe\":true,\"reasons\":[],\"planned_down\":false}";

/// Catches a request the operator API's gates would refuse or that names the wrong thing: the
/// path carries the alias, the bearer is the file's token, the content type is JSON (the gate
/// wants one on every call), and the request has exactly the headers of `kbf cluster`.
#[test]
fn the_request_is_what_the_operator_gate_wants() {
    let api = fake_api(vec![Some(http(200, SAFE))]);
    assert_eq!(
        ask(&api, "st1").expect("answer"),
        Stop::Answer(Verdict {
            safe: true,
            reasons: vec![],
            planned_down: false
        })
    );
    let seen = api.requests();
    assert_eq!(seen.len(), 1);
    let head = &seen[0];
    assert!(
        head.starts_with("GET /v1/members/st1/safe-to-stop HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(
        head.contains(&format!("Authorization: Bearer {TOKEN}\r\n")),
        "{head}"
    );
    assert!(
        head.contains("Content-Type: application/json\r\n"),
        "{head}"
    );
    assert!(head.contains("Connection: close\r\n"), "{head}");
}

/// Catches the #1489 body read wrongly: an unsafe verdict with its reasons and planned_down
/// comes through whole (a flipped `safe` would stop a host with a blob on one copy), and a
/// 200 that is no verdict is an error, never "safe".
#[test]
fn a_200_is_the_verdict_and_nothing_else_is() {
    let body = "{\"safe\":false,\"reasons\":[\"member m2 owes 5 blobs\"],\"planned_down\":true}";
    let api = fake_api(vec![Some(http(200, body))]);
    assert_eq!(
        ask(&api, "st1").expect("answer"),
        Stop::Answer(Verdict {
            safe: false,
            reasons: vec!["member m2 owes 5 blobs".into()],
            planned_down: true
        })
    );
    for bad in ["{\"safe\":true}", "", "<html>ok</html>", "null"] {
        let api = fake_api(vec![Some(http(200, bad))]);
        let e = ask(&api, "st1").expect_err(bad);
        assert_eq!(e.kind(), PortErrorKind::Other, "{bad}");
        assert!(e.to_string().contains("not a safe-to-stop verdict"));
    }
}

/// Catches a 404 taken for anything but "the cluster does not know the host".
#[test]
fn a_404_is_unknown() {
    let api = fake_api(vec![Some(http(404, "{\"error\":\"no member or node\"}"))]);
    assert_eq!(ask(&api, "ghost").expect("answer"), Stop::Unknown);
}

/// Catches each non-answer mapped to the wrong kind: a refusal of the call is an error that
/// is an answer (`Other`), a 5xx or a short body is no answer (`Unreachable`), and a surprise
/// status is `Other`. None of the texts holds the token or the host's address.
#[test]
fn refusals_and_failures_are_errors_of_the_right_kind_and_leak_nothing() {
    let cases: Vec<(Vec<u8>, PortErrorKind, &str)> = vec![
        (
            http(401, "{}"),
            PortErrorKind::Other,
            "refused the call (401)",
        ),
        (http(403, "{}"), PortErrorKind::Other, "(403)"),
        (http(415, "{}"), PortErrorKind::Other, "(415)"),
        (
            http(409, "{}"),
            PortErrorKind::Other,
            "storage member (409)",
        ),
        (http(500, "{}"), PortErrorKind::Unreachable, "answered 500"),
        (http(503, "{}"), PortErrorKind::Unreachable, "answered 503"),
        (http(302, "{}"), PortErrorKind::Other, "answered 302"),
        (
            b"garbage, not http".to_vec(),
            PortErrorKind::Other,
            "did not answer with HTTP",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n0\r\n\r\n".to_vec(),
            PortErrorKind::Other,
            "encoding",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n{\"safe\":true".to_vec(),
            PortErrorKind::Unreachable,
            "closed the answer short",
        ),
        // A length header that is not a number is ignored, the body read to the end.
        (
            format!("HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n{SAFE}").into_bytes(),
            PortErrorKind::Other,
            "never an error",
        ),
    ];
    for (answer, kind, text) in cases {
        let api = fake_api(vec![Some(answer)]);
        match ask(&api, "st1") {
            Err(e) => {
                assert_eq!(
                    (e.kind(), e.to_string().contains(text)),
                    (kind, true),
                    "{text}: {e}"
                );
                for secret in [TOKEN, "secret-host", &api.addr.to_string()] {
                    assert!(!e.to_string().contains(secret), "{secret} in {e}");
                }
            }
            Ok(stop) => assert_eq!(text, "never an error", "{stop:?}"),
        }
    }
}

/// Catches a refused connection or a server that never answers being anything but "no
/// answer" (`Unreachable`), which the roll retries and fails closed at; and a hang with no
/// timeout.
#[test]
fn a_refused_connection_and_a_silent_server_are_unreachable() {
    let (a, _) = adapter(closed_port());
    let e = a.check(&host("st1")).expect_err("refused");
    assert!(e.is_unreachable(), "{e}");
    assert!(!e.to_string().contains("127.0.0.1"));
    let silent = fake_api(vec![None]);
    let started = std::time::Instant::now();
    let e = ask(&silent, "st1").expect_err("timeout");
    assert!(e.is_unreachable(), "{e}");
    assert!(started.elapsed() < Duration::from_secs(10));
    // A client that goes away before it finishes its request is no problem for the fake.
    drop(std::net::TcpStream::connect(silent.addr).expect("connect"));
    eventually(|| silent.requests().len() == 2);
}

/// Catches a token file accepted that the server itself would refuse: wrong mode, a symlink,
/// a directory, too short, a byte that could split a header, empty, or too long; and an error
/// text that holds the secret or the file's path (it reaches `GET /rolls/{id}`; it names the
/// flag).
#[test]
fn the_token_file_is_held_to_the_servers_rule() {
    let dir = scratch("tokrule");
    let good = file(&dir, "good", &format!("  {TOKEN}\n\n"), 0o400);
    assert_eq!(read_token(&good).as_deref(), Ok(TOKEN));
    let readable = file(&dir, "r", TOKEN, 0o644);
    let short = file(&dir, "short", "tok-short", 0o600);
    let split = file(&dir, "split", &format!("{TOKEN}\r\nX-Evil: 1"), 0o600);
    let inner = file(&dir, "inner", &format!("{TOKEN} {TOKEN}"), 0o600);
    let empty = file(&dir, "empty", "  \n", 0o600);
    let long = file(&dir, "long", &"t".repeat(4097), 0o600);
    let exact = file(&dir, "exact", &"t".repeat(4096), 0o600);
    // Bytes that are not text are an error, not a token.
    let binary = dir.join("binary");
    std::fs::write(&binary, [0xff_u8; 40]).expect("write");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let link = dir.join("link");
    symlink(&good, &link).expect("symlink");
    for (path, why) in [
        (&readable, "mode must be 0600 or 0400"),
        (&short, "32 to 4096"),
        (&split, "32 to 4096"),
        (&inner, "32 to 4096"),
        (&empty, "32 to 4096"),
        (&long, "32 to 4096"),
        (&dir.join("missing"), "not found"),
        (&link, "symbolic"),
        (&dir, "not a regular file"),
        (&binary, "32 to 4096"),
    ] {
        let e = read_token(path).expect_err(why);
        assert!(e.starts_with("--token-file: "), "{e}");
        assert!(!e.contains(&path.display().to_string()), "path in {e}");
        assert!(!e.contains("X-Evil") && !e.contains(TOKEN), "{e}");
        let ok = e.to_lowercase().contains(&why.to_lowercase()) || why == "symbolic";
        assert!(ok, "{why} not in {e}");
    }
    assert_eq!(read_token(&exact).map(|t| t.len()), Ok(4096));
    // The adapter checks the file when it is made, so a bad file fails the start-up.
    assert!(HttpSafeToStop::new(closed_port(), short).is_err());
}

/// Catches a token cached for ever (a rotation would need a restart) or a token file that
/// goes bad after start-up being sent anyway: each call reads the file, and a call with the
/// file gone is an error, with no connection made.
#[test]
fn the_token_is_read_on_every_call() {
    let api = fake_api(vec![Some(http(200, SAFE))]);
    let (a, token_file) = adapter(api.addr);
    a.check(&host("st1")).expect("first");
    let rotated = "rotated-0123456789abcdef0123456789abcdef";
    std::fs::write(&token_file, rotated).expect("rewrite");
    a.check(&host("st1")).expect("second");
    let seen = api.requests();
    assert!(seen[0].contains(TOKEN) && !seen[0].contains(rotated));
    assert!(seen[1].contains(rotated));
    std::fs::remove_file(&token_file).expect("remove");
    let e = a.check(&host("st1")).expect_err("gone");
    assert_eq!(e.kind(), PortErrorKind::Other);
    assert_eq!(api.requests().len(), 2, "no call without a token");
}

/// Catches the file rule letting through a file of another user (a root-owned file is not the
/// updater's secret), a device or a directory, or a mode wider than 0600/0400 or with a
/// special bit; a second uid needs root, so the rule is checked with fakes.
#[test]
fn the_file_rule_wants_a_regular_file_of_mine_with_mode_0600_or_0400() {
    assert_eq!(file_rule(true, 7, 0o100_600, 7), Ok(()));
    assert_eq!(file_rule(true, 7, 0o100_400, 7), Ok(()));
    assert!(
        file_rule(false, 7, 0o600, 7)
            .expect_err("dir")
            .contains("regular")
    );
    assert!(
        file_rule(true, 8, 0o600, 7)
            .expect_err("owner")
            .contains("owned")
    );
    assert!(
        file_rule(true, 0, 0o600, 7)
            .expect_err("root's")
            .contains("owned")
    );
    for mode in [
        0o644, 0o660, 0o640, 0o700, 0o4600, 0o1600, 0o200, 0o000, 0o604,
    ] {
        assert!(
            file_rule(true, 7, mode, 7)
                .expect_err("mode")
                .contains("mode"),
            "{mode:o}"
        );
    }
}
