//! The `kbf-server` binary: flags, both stores, the start line, and a clean stop.

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, Output, Stdio};

use kbf_proto::reapi::GetCapabilitiesRequest;
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-server");
const ANY_PORT: [&str; 4] = ["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"];

fn server(args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.args(args)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY");
    c
}

fn with_keys(mut c: Command) -> Command {
    c.env("AWS_ACCESS_KEY_ID", "kbf-test-access")
        .env("AWS_SECRET_ACCESS_KEY", "kbf-test-secret");
    c
}

/// Runs to exit and returns the exit code and stderr.
fn fails(mut c: Command) -> (Option<i32>, String) {
    let Output { status, stderr, .. } = c.output().expect("spawn kbf-server");
    (status.code(), String::from_utf8(stderr).expect("UTF-8"))
}

/// Starts the server, reads its start line, and returns it with the REAPI address.
fn started(mut c: Command) -> (Child, String, SocketAddr) {
    let mut child = c
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kbf-server");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("read the start line");
    let reapi = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix("reapi="))
        .unwrap_or_else(|| panic!("no reapi= in {line:?}"))
        .parse()
        .expect("an address");
    (child, line, reapi)
}

/// Stops the server with SIGINT and checks it exits 0.
fn interrupt(mut child: Child) {
    let sent = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success());
    let status = child.wait().expect("wait");
    assert!(status.success(), "kbf-server exited with {status}");
}

/// Catches: a binary that reports another name or version.
#[test]
fn prints_name_and_version() {
    let out = server(&["--version"]).output().expect("spawn kbf-server");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).expect("UTF-8"),
        format!("kbf-server {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// Catches: a server that does not serve REAPI with execution enabled in memory mode,
/// that prints a start line without the addresses it bound, or that does not stop
/// cleanly on SIGINT.
#[cfg(unix)]
#[test]
fn memory_mode_serves_execution_and_stops_on_interrupt() {
    let mut c = server(&["--role", "all", "--store", "memory"]);
    c.args(ANY_PORT);
    let (child, line, reapi) = started(c);
    assert!(
        line.starts_with(&format!("kbf-server {} reapi=", env!("CARGO_PKG_VERSION"))),
        "{line}"
    );
    assert!(line.contains(" worker=127.0.0.1:"), "{line}");
    let caps = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            CapabilitiesClient::connect(format!("http://{reapi}"))
                .await
                .expect("connect")
                .get_capabilities(GetCapabilitiesRequest::default())
                .await
                .expect("GetCapabilities")
                .into_inner()
        });
    assert!(caps.execution_capabilities.expect("execution").exec_enabled);
    interrupt(child);
}

/// Catches: `--store=s3` that does not build an S3 store from its flags and the key
/// pair (the store does no I/O until used, so the server starts).
#[cfg(unix)]
#[test]
fn s3_mode_starts_with_flags_and_the_key_pair() {
    let mut c = with_keys(server(&[
        "--store",
        "s3",
        "--s3-endpoint",
        "http://127.0.0.1:9",
        "--s3-bucket",
        "kbf-test",
        "--s3-conditional-put",
    ]));
    c.args(ANY_PORT);
    let (child, _, _) = started(c);
    interrupt(child);
}

/// Catches: an S3 server that starts without its bucket flags or its key pair, or
/// with a configuration the store refuses, instead of exiting 2 and saying why.
#[test]
fn s3_mode_refuses_what_is_missing_or_wrong() {
    let (code, stderr) = fails(server(&["--store", "s3"]));
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("--s3-endpoint") && stderr.contains("--s3-bucket"),
        "{stderr}"
    );

    let s3_bucket = |bucket: &str, extra: &[&str]| {
        let mut c = server(&[
            "--store",
            "s3",
            "--s3-endpoint",
            "http://127.0.0.1:9",
            "--s3-bucket",
            bucket,
        ]);
        c.args(extra).args(ANY_PORT);
        c
    };
    let s3 = |extra: &[&str]| s3_bucket("kbf-test", extra);
    let (code, stderr) = fails(s3(&[]));
    assert_eq!(code, Some(2));
    assert!(stderr.contains("AWS_ACCESS_KEY_ID must be set"), "{stderr}");

    let mut no_secret = s3(&[]);
    no_secret.env("AWS_ACCESS_KEY_ID", "kbf-test-access");
    let (code, stderr) = fails(no_secret);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("AWS_SECRET_ACCESS_KEY must be set"),
        "{stderr}"
    );

    let (code, stderr) = fails(with_keys(s3_bucket("B", &[])));
    assert_eq!(code, Some(2));
    assert!(stderr.contains("bucket name"), "{stderr}");

    let (code, stderr) = fails(with_keys(s3(&["--s3-prefix", "no spaces/"])));
    assert_eq!(code, Some(2));
    assert!(stderr.contains("--s3-prefix"), "{stderr}");
}

/// Catches: a server that keeps running (or exits 0) when it cannot bind its
/// listeners or read its TLS files, or that accepts part of a TLS configuration.
#[test]
fn startup_failures_exit_2() {
    let held = TcpListener::bind("127.0.0.1:0").expect("bind");
    let taken = held.local_addr().expect("address").to_string();
    for (reapi, worker) in [(&*taken, "127.0.0.1:0"), ("127.0.0.1:0", &*taken)] {
        let (code, stderr) = fails(server(&["--listen", reapi, "--worker-listen", worker]));
        assert_eq!(code, Some(2), "{reapi} {worker}");
        assert!(stderr.contains(&format!("bind {taken}")), "{stderr}");
    }

    let missing = "/nonexistent/kbf-server-test.pem";
    let mut c = server(&ANY_PORT);
    c.args([
        "--worker-tls-cert",
        missing,
        "--worker-tls-key",
        missing,
        "--worker-client-ca",
        missing,
    ]);
    let (code, stderr) = fails(c);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("read /nonexistent"), "{stderr}");

    let mut c = server(&ANY_PORT);
    c.args(["--worker-tls-cert", missing]);
    let (code, stderr) = fails(c);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("--worker-tls-key"), "{stderr}");

    let (code, _) = fails(server(&["--heartbeat-interval-ms", "0"]));
    assert_eq!(code, Some(2), "a zero heartbeat interval accepted");
}

/// Catches (issue #23): a heartbeat interval longer than half the window in which a
/// daemon may act on a `Start` accepted (each `Start` names the newest heartbeat, so
/// a Start sent just before the next one would be refused), or the longest one that
/// fits refused.
#[test]
fn the_heartbeat_interval_fits_the_start_window() {
    use clap::Parser;
    let parse =
        |ms: &str| kbf_server::Args::try_parse_from(["kbf-server", "--heartbeat-interval-ms", ms]);
    let longest = parse("7000").expect("7 s, half of the 14 s window");
    assert_eq!(longest.heartbeat_interval_ms, 7_000);
    assert_eq!(kbf_server::config::MAX_HEARTBEAT_INTERVAL_MS, 7_000);
    let err = parse("7001").expect_err("over half the window accepted");
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
}
