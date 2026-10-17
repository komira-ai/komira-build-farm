//! `kbf cluster` end to end: the real binary against a one-shot HTTP server that keeps
//! the request it was sent, so each verb's method, path, headers and body are pinned,
//! and the exit status follows the answer. The same routes are served and called in
//! `kbf-server`'s `tests/frontend_drain.rs`, which runs the client against the real
//! operator API.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::Command;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// How long a test waits for the binary to call the server: a hang deadline.
const HANG: Duration = Duration::from_secs(20);

/// Answers one request with `status` and `body`; the thread returns the request as it
/// was sent (head and body).
fn serve_once(status: &'static str, body: &'static str) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("non-blocking");
    let addr = listener.local_addr().expect("addr");
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + HANG;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("kbf never called the server: {e}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking");
        stream.set_read_timeout(Some(HANG)).expect("timeout");
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        let wanted = loop {
            let n = stream.read(&mut buf).expect("read the request");
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request);
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().expect("a length"))
                    })
                    .unwrap_or(0);
                if rest.len() >= length {
                    break request.len();
                }
            }
            assert!(n > 0, "the request ended early");
        };
        assert_eq!(wanted, request.len());
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).expect("reply");
        String::from_utf8(request).expect("UTF-8")
    });
    (addr, thread)
}

fn kbf(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(BIN).args(args).output().expect("run kbf");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A token file holding `token` (the client does not check its mode).
fn token_file(name: &str, token: &str) -> String {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("kbf-cluster");
    std::fs::create_dir_all(&dir).expect("a directory");
    let path = dir.join(format!("{name}-{}", std::process::id()));
    std::fs::write(&path, format!("{token}\n")).expect("write");
    path.display().to_string()
}

/// Runs `kbf cluster --api <server> <args>` against a server answering `status` and
/// `body`; the exit status, stdout, stderr and the request the server saw.
fn cluster(
    status: &'static str,
    body: &'static str,
    args: &[&str],
) -> (Option<i32>, String, String, String) {
    let (addr, request) = serve_once(status, body);
    let api = addr.to_string();
    let mut argv = vec!["cluster", "--api", &api];
    argv.extend_from_slice(args);
    let (code, out, err) = kbf(&argv);
    (code, out, err, request.join().expect("the server thread"))
}

/// Catches each verb sending the wrong call: the method, the path (the node id
/// percent-encoded as one segment), the `Authorization` header of the token file on a
/// write and none on a read, the JSON content type and length on a write, and the
/// deadline in the body. The answer's body is printed as it came, exit 0.
#[test]
fn each_verb_sends_its_call_and_prints_the_answer() {
    let token = token_file("each", "kbf-test-token-0123456789abcdef0123456789");
    let bearer = "authorization: bearer kbf-test-token-0123456789abcdef0123456789";
    let cases: [(&[&str], &str, &str); 9] = [
        (&["status"], "GET /v1/nodes HTTP/1.1", ""),
        (
            &["status", "--frontend"],
            "GET /v1/frontend/drain HTTP/1.1",
            "",
        ),
        (
            &["drain", "--frontend", "--token-file", &token],
            "POST /v1/frontend/drain HTTP/1.1",
            "",
        ),
        (
            &[
                "drain",
                "rack 1/a",
                "--deadline-secs",
                "5",
                "--token-file",
                &token,
            ],
            "POST /v1/nodes/rack%201%2Fa:drain HTTP/1.1",
            r#"{"deadline_secs":5}"#,
        ),
        (
            &["undrain", "n1", "--token-file", &token],
            "POST /v1/nodes/n1:uncordon HTTP/1.1",
            "",
        ),
        (
            &["member", "7", "--state", "active", "--token-file", &token],
            "POST /v1/members/7 HTTP/1.1",
            r#"{"state":"active"}"#,
        ),
        (
            &["member", "7", "--state", "draining", "--token-file", &token],
            "POST /v1/members/7 HTTP/1.1",
            r#"{"state":"draining"}"#,
        ),
        (
            &[
                "member",
                "7",
                "--state",
                "planned-down",
                "--token-file",
                &token,
            ],
            "POST /v1/members/7 HTTP/1.1",
            r#"{"state":"planned_down"}"#,
        ),
        (
            &[
                "member",
                "7",
                "--state",
                "planned-down",
                "--for-secs",
                "600",
                "--token-file",
                &token,
            ],
            "POST /v1/members/7 HTTP/1.1",
            r#"{"state":"planned_down","for_secs":600}"#,
        ),
    ];
    for (args, line, body) in cases {
        let (code, out, err, request) = cluster("200 OK", r#"{"answer":1}"#, args);
        assert_eq!(code, Some(0), "{args:?}: {err}");
        assert_eq!(out, "{\"answer\":1}\n", "{args:?}");
        assert!(
            request.starts_with(&format!("{line}\r\n")),
            "{args:?}: {request}"
        );
        let lower = request.to_ascii_lowercase();
        let writes = line.starts_with("POST");
        assert_eq!(lower.contains(bearer), writes, "{args:?}: {request}");
        assert_eq!(
            lower.contains("content-type: application/json"),
            writes,
            "{args:?}: {request}"
        );
        assert!(
            request.ends_with(&format!("\r\n\r\n{body}")),
            "{args:?}: {request}"
        );
        if writes {
            assert!(
                lower.contains(&format!("content-length: {}\r\n", body.len())),
                "{args:?}: {request}"
            );
        }
    }
}

/// Catches a refused or failed call that exits 0 (a rollout would go on), and one that
/// hides why: a 401 from the API exits 1 naming the status and the API's reason, and an
/// address nothing listens on exits 1 naming the address.
#[test]
fn a_refusal_or_no_answer_exits_1_and_says_why() {
    let token = token_file("refused", "wrong-token-wrong-token-wrong-token-1");
    let (code, out, err, _) = cluster(
        "401 Unauthorized",
        r#"{"error":"a write needs Authorization"}"#,
        &["drain", "--frontend", "--token-file", &token],
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("401") && err.contains("a write needs Authorization"),
        "{err}"
    );

    let gone = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").to_string()
    };
    let (code, out, err) = kbf(&["cluster", "--api", &gone, "status"]);
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains(&gone), "{err}");

    let (code, _, err) = kbf(&[
        "cluster",
        "drain",
        "--frontend",
        "--token-file",
        "/nonexistent/token",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("/nonexistent/token"), "{err}");
}

/// Catches a command line that is accepted and means something else: `drain` with no
/// target or with both, a deadline for the frontend (it has none), `undrain --frontend`
/// (a frontend drain is not undone), a write with no token file, and an unknown verb,
/// all exit 2 with clap's message, and `check-args` does not take `cluster` as a mode.
#[test]
fn a_command_line_that_names_no_single_call_exits_2() {
    let rows: [&[&str]; 7] = [
        &["cluster", "drain", "--token-file", "t"],
        &["cluster", "drain", "n1", "--frontend", "--token-file", "t"],
        &[
            "cluster",
            "drain",
            "--frontend",
            "--deadline-secs",
            "5",
            "--token-file",
            "t",
        ],
        &["cluster", "undrain", "--frontend", "--token-file", "t"],
        &["cluster", "drain", "n1"],
        &["cluster", "undrain", "n1"],
        &["cluster", "restart"],
    ];
    for argv in rows {
        let (code, out, err) = kbf(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
    }
    let (code, _, err) = kbf(&["check-args", "cluster", "status"]);
    assert_eq!(code, Some(2), "{err}");
}

/// Catches `cluster` missing from `kbf --help` (nothing else would tell an operator it
/// exists) while the mode list stays the five modes.
#[test]
fn help_names_cluster_after_the_modes() {
    let (code, out, err) = kbf(&["--help"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("kbf cluster status|drain|undrain"), "{out}");
}
