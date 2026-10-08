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

/// Catches (issue #86): a server whose SIGINT handler is installed after it prints the
/// start line, so a SIGINT sent as soon as the line is read kills it by the default
/// action instead of stopping it with exit 0. In both store modes the test holds the
/// server at that line (its stdout is a full pipe, so the print blocks), waits until
/// `/proc` shows SIGINT caught, sends SIGINT, then lets the line through. A late handler
/// never shows as caught while the print is blocked, so it fails here every time rather
/// than now and then.
#[cfg(target_os = "linux")]
#[test]
fn sigint_is_caught_before_the_start_line_is_printed() {
    let memory = server(&["--store", "memory"]);
    let s3 = with_keys(server(&[
        "--store",
        "s3",
        "--s3-endpoint",
        "http://127.0.0.1:9",
        "--s3-bucket",
        "kbf-test",
    ]));
    for (mode, mut c) in [("memory", memory), ("s3", s3)] {
        c.args(ANY_PORT);
        held_at_the_start_line_then_interrupted(mode, c);
    }
}

#[cfg(target_os = "linux")]
fn held_at_the_start_line_then_interrupted(mode: &str, mut c: Command) {
    use std::time::{Duration, Instant};
    let (stdout, full) = full_pipe();
    let mut child = c
        .stdout(full)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kbf-server");
    drop(c); // the server now holds the only write end
    let pid = i32::try_from(child.id()).expect("pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !catches_sigint(pid) {
        if Instant::now() > deadline {
            child.kill().expect("kill");
            child.wait().expect("wait");
            panic!("{mode}: SIGINT not caught while the start line is being printed");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: kill(2) on the child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let line = BufReader::new(stdout)
        .lines()
        .map(|l| l.expect("read stdout"))
        .find(|l| !l.is_empty())
        .expect("a start line");
    assert!(line.starts_with("kbf-server "), "{mode}: {line}");
    let status = child.wait().expect("wait");
    assert!(status.success(), "{mode}: kbf-server exited with {status}");
}

/// A pipe whose write end is full (of newlines), so the next write to it blocks until
/// the read end is read.
#[cfg(target_os = "linux")]
fn full_pipe() -> (std::io::PipeReader, std::io::PipeWriter) {
    use std::io::{ErrorKind, Write};
    use std::os::fd::AsRawFd;
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let fd = writer.as_raw_fd();
    // SAFETY: fcntl(2) on a descriptor this function owns; it changes only status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL");
    // SAFETY: as above.
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    loop {
        match writer.write(b"\n") {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => panic!("fill the pipe: {e}"),
        }
    }
    // The server inherits this open file and must block on it, not fail.
    // SAFETY: as above.
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) }, 0);
    (reader, writer)
}

/// Whether process `pid` has a handler installed for SIGINT (`SigCgt` in its status).
#[cfg(target_os = "linux")]
fn catches_sigint(pid: i32) -> bool {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("read status");
    let caught = status
        .lines()
        .find_map(|l| l.strip_prefix("SigCgt:"))
        .expect("a SigCgt line");
    let caught = u64::from_str_radix(caught.trim(), 16).expect("a hex mask");
    caught & (1 << (libc::SIGINT - 1)) != 0
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

/// Catches: `--unservable-wait-secs` parsed but not handed to the scheduler (work no
/// daemon can run would then wait the default 300 s whatever the operator set, or
/// some other fixed time), and a default other than the scheduler's.
#[test]
fn the_unservable_wait_comes_from_its_flag() {
    use clap::Parser;
    use std::time::Duration;
    let wait = |args: &[&str]| {
        kbf_server::Args::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied()))
            .expect("flags parse")
            .listeners()
            .expect("listeners")
            .unservable_wait
    };
    assert_eq!(wait(&[]), kbf_sched::UNSERVABLE_WAIT);
    assert_eq!(
        wait(&["--unservable-wait-secs", "7"]),
        Duration::from_secs(7)
    );
    assert_eq!(
        wait(&["--unservable-wait-secs", "3600"]),
        Duration::from_secs(3_600)
    );
}
