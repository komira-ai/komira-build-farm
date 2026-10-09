//! The `kbf-server` binary: flags, both stores, the start line, and a clean stop.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kbf_proto::reapi::GetCapabilitiesRequest;
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-server");
const ANY_PORT: [&str; 4] = ["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"];
/// How long a test waits for the server to print its start line or to exit.
const BOUND: Duration = Duration::from_secs(10);

/// A running server, killed and reaped when dropped, so a failed assertion (or a wait
/// that ran out) never leaves it running or hangs the test.
struct Running(Child);

impl Running {
    fn pid(&self) -> i32 {
        i32::try_from(self.0.id()).expect("pid")
    }

    /// Waits up to `BOUND` for the server to exit; panics (and so kills it) if it has
    /// not.
    fn exit_status(&mut self, what: &str) -> ExitStatus {
        let deadline = Instant::now() + BOUND;
        loop {
            if let Some(status) = self.0.try_wait().expect("try_wait") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: kbf-server still running {BOUND:?} after SIGINT"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The first non-empty line `out` yields within `BOUND`. The reading thread ends when
/// the server exits (or is killed) and the pipe closes.
fn first_line(what: &str, out: impl Read + Send + 'static) -> String {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let line = BufReader::new(out)
            .lines()
            .map_while(Result::ok)
            .find(|l| !l.is_empty());
        let _ = tx.send(line);
    });
    match rx.recv_timeout(BOUND) {
        Ok(Some(line)) => line,
        Ok(None) => panic!("{what}: kbf-server closed stdout without a start line"),
        Err(e) => panic!("{what}: no start line within {BOUND:?} ({e})"),
    }
}

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

/// Runs to exit and returns the exit code and stderr. A server that is still running
/// after `BOUND` (it started when it should have refused) is killed and reaped, and
/// the test fails.
fn fails(mut c: Command) -> (Option<i32>, String) {
    let mut child = Running(
        c.stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kbf-server"),
    );
    let deadline = Instant::now() + BOUND;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "kbf-server still running {BOUND:?} after it should have refused to start"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    let mut pipe = child.0.stderr.take().expect("stderr");
    pipe.read_to_string(&mut stderr).expect("UTF-8 stderr");
    (status.code(), stderr)
}

/// Starts the server, reads its start line, and returns it with the REAPI address.
fn started(mut c: Command) -> (Running, String, SocketAddr) {
    let mut child = Running(
        c.stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kbf-server"),
    );
    let line = first_line("start", child.0.stdout.take().expect("stdout"));
    let reapi = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix("reapi="))
        .unwrap_or_else(|| panic!("no reapi= in {line:?}"))
        .parse()
        .expect("an address");
    (child, line, reapi)
}

/// Stops the server with SIGINT and checks it exits 0 within `BOUND`.
fn interrupt(mut child: Running) {
    let sent = Command::new("kill")
        .args(["-INT", &child.pid().to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success());
    let status = child.exit_status("interrupt");
    assert!(status.success(), "kbf-server exited with {status}");
}

/// The version this binary must report, worked out here rather than taken from the
/// library: the package version, `+`, and the commit built, which is
/// `KBF_BUILD_COMMIT_OVERRIDE` when the build set it and the checkout's HEAD otherwise.
fn built_version() -> String {
    let commit = if let Some(stamp) = option_env!("KBF_BUILD_COMMIT_OVERRIDE") {
        stamp.to_owned()
    } else {
        let head = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .expect("git runs");
        assert!(head.status.success(), "the tests run in a git checkout");
        String::from_utf8(head.stdout)
            .expect("UTF-8")
            .trim()
            .to_owned()
    };
    format!("{}+{commit}", env!("CARGO_PKG_VERSION"))
}

/// Catches: a binary that reports another name or version, or a version without the
/// commit it was built from (two builds of one package version would read the same).
#[test]
fn prints_name_and_version() {
    let out = server(&["--version"]).output().expect("spawn kbf-server");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).expect("UTF-8"),
        format!("kbf-server {}\n", built_version())
    );
}

/// Catches: a server that does not serve REAPI with execution enabled in memory mode,
/// that prints a start line without its version and commit or the addresses it bound,
/// or that does not stop cleanly on SIGINT.
#[cfg(unix)]
#[test]
fn memory_mode_serves_execution_and_stops_on_interrupt() {
    let mut c = server(&["--role", "all", "--store", "memory"]);
    c.args(ANY_PORT);
    let (child, line, reapi) = started(c);
    assert!(
        line.starts_with(&format!("kbf-server {} reapi=", built_version())),
        "{line}"
    );
    assert!(line.contains(" worker=127.0.0.1:"), "{line}");
    assert!(
        !line.contains(" api="),
        "no operator API unless asked: {line}"
    );
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

/// Catches: an `--api-listen` flag that binds nothing, a start line without the API's
/// address, an API listener that does not answer `GET /v1/nodes`, and an answer
/// without the `server` field naming this binary's version and commit.
#[cfg(unix)]
#[test]
fn api_listen_serves_the_operator_api() {
    let mut c = server(&["--api-listen", "127.0.0.1:0"]);
    c.args(ANY_PORT);
    let (child, line, _) = started(c);
    let api: SocketAddr = line
        .rsplit_once(" api=")
        .unwrap_or_else(|| panic!("no api= at the end of {line:?}"))
        .1
        .parse()
        .expect("an address");
    let mut stream = std::net::TcpStream::connect(api).expect("connect to the API");
    stream
        .write_all(b"GET /v1/nodes HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n")
        .expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let version = built_version();
    let (_, commit) = version.split_once('+').expect("a + in the version");
    let body = format!(
        "{{\"server\":{{\"version\":\"{version}\",\"commit\":\"{commit}\"}},\"nodes\":[]}}"
    );
    assert!(response.ends_with(&format!("\r\n\r\n{body}")), "{response}");
    interrupt(child);
}

/// A token file holding `content` with `mode`, under `name`.
#[cfg(unix)]
fn token_file(name: &str, content: &str, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-binary");
    std::fs::create_dir_all(&dir).expect("a token directory");
    let path = dir.join(format!("{name}-{}", std::process::id()));
    std::fs::write(&path, content).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// One POST to `api` with `headers`; the response.
#[cfg(unix)]
fn post(api: SocketAddr, path: &str, headers: &str) -> String {
    let mut stream = std::net::TcpStream::connect(api).expect("connect to the API");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: 0\r\n{headers}\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    response
}

/// Catches: `--api-token-file` read but not given to the API (every write refused as
/// "writes are off", or none checked), and the token's bytes in the start line.
#[cfg(unix)]
#[test]
fn api_token_file_gates_writes() {
    const TOKEN: &str = "kbf-binary-token-0123456789abcdef0123456789";
    let path = token_file("good", &format!("{TOKEN}\n"), 0o600);
    let path = path.to_str().expect("a UTF-8 path");
    let mut c = server(&["--api-listen", "127.0.0.1:0", "--api-token-file", path]);
    c.args(ANY_PORT);
    let (child, line, _) = started(c);
    assert!(!line.contains(TOKEN), "{line}");
    let api: SocketAddr = line
        .rsplit_once(" api=")
        .expect("an API address")
        .1
        .parse()
        .expect("an address");
    let json = "Content-Type: application/json\r\n";
    let refused = post(api, "/v1/nodes/ghost:cordon", json);
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
    let headers = format!("Authorization: Bearer {TOKEN}\r\n{json}");
    let passed = post(api, "/v1/nodes/ghost:cordon", &headers);
    assert!(
        passed.starts_with("HTTP/1.1 404"),
        "past every gate: {passed}"
    );
    interrupt(child);
}

/// Catches: a token file others can read accepted at start (or refused only at the
/// first write), its path or mode left out of the message, and `--api-token-file`
/// accepted without an API to use it.
#[cfg(unix)]
#[test]
fn an_unsafe_api_token_file_stops_the_start() {
    let open = token_file("open", "kbf-binary-token-0123456789abcdef0123456789", 0o644);
    let open = open.to_str().expect("a UTF-8 path");
    let mut c = server(&["--api-listen", "127.0.0.1:0", "--api-token-file", open]);
    c.args(ANY_PORT);
    let (code, stderr) = fails(c);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("--api-token-file: "), "{stderr}");
    assert!(stderr.contains(open) && stderr.contains("0644"), "{stderr}");

    let mut c = server(&["--api-token-file", open]);
    c.args(ANY_PORT);
    let (code, stderr) = fails(c);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("--api-listen"), "{stderr}");
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
    let (stdout, full) = full_pipe();
    let mut child = Running(
        c.stdout(full)
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kbf-server"),
    );
    drop(c); // the server now holds the only write end
    let pid = child.pid();
    let deadline = Instant::now() + BOUND;
    while !catches_sigint(pid) {
        assert!(
            Instant::now() < deadline,
            "{mode}: SIGINT not caught while the start line is being printed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: kill(2) on the child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let line = first_line(mode, stdout);
    assert!(line.starts_with("kbf-server "), "{mode}: {line}");
    let status = child.exit_status(mode);
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

/// Catches: `--finished-retention-secs` parsed but not handed to the scheduler, a
/// default other than the scheduler's, and a zero refused (it keeps nothing).
#[test]
fn the_finished_retention_comes_from_its_flag() {
    use clap::Parser;
    use std::time::Duration;
    let retention = |args: &[&str]| {
        kbf_server::Args::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied()))
            .expect("flags parse")
            .listeners()
            .expect("listeners")
            .finished_retention
    };
    assert_eq!(retention(&[]), kbf_sched::FINISHED_RETENTION);
    assert_eq!(
        retention(&["--finished-retention-secs", "0"]),
        Duration::ZERO
    );
    assert_eq!(
        retention(&["--finished-retention-secs", "600"]),
        Duration::from_secs(600)
    );
}
