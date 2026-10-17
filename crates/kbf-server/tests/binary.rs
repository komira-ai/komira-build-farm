//! The `kbf-server` binary: flags, the cold and hot stores, the metadata log, the start
//! line, and a clean stop.

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

/// The server with exactly `args`.
fn bare(args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.args(args)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY");
    c
}

/// The server with `args`, over a `file://` cold store and metadata directory of its own
/// (in a fresh directory under the target directory) unless `args` name a cold store:
/// every start needs one, and most tests are about something else.
fn server(args: &[&str]) -> Command {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    if args.contains(&"--cold-store") {
        return bare(args);
    }
    let base = fresh_dir(&format!(
        "default-stores-{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let cold = format!("file://{}", base.join("cold").display());
    let mut c = bare(args);
    c.args(["--cold-store", &cold, "--meta-dir"])
        .arg(base.join("meta"));
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

/// Catches: a server that does not serve REAPI with execution enabled over a `file://`
/// cold store, that prints a start line without its version and commit or the addresses
/// it bound, or that does not stop cleanly on SIGINT.
#[cfg(unix)]
#[test]
fn file_mode_serves_execution_and_stops_on_interrupt() {
    let base = fresh_dir("file-mode");
    let mut c = file_server(&base);
    c.args(["--role", "all"]);
    let (child, line, reapi) = started(c);
    assert!(
        line.starts_with(&format!("kbf-server {} reapi=", built_version())),
        "{line}"
    );
    assert!(line.contains(" worker=127.0.0.1:"), "{line}");
    // Catches: the plain-text warning printed on a loopback bind (both listeners here),
    // where nothing is exposed (scripts would learn to ignore it).
    assert!(!line.contains(" warning="), "{line}");
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

/// The thread flags reach the runtime: the server logs the sizes it resolved and runs
/// exactly that many async workers (the `kbf-async` threads of `/proc`).
///
/// Catches: flags parsed and dropped (the runtime built with tokio's default, one
/// worker per CPU, which the thread count shows on a machine of four CPUs or more), and
/// a start that logs nothing about its sizes. The runtime names its blocking threads
/// as it names its workers, and the metadata log runs its commits on the blocking pool,
/// so the count is the two workers plus the blocking threads that have run, at most two.
#[cfg(target_os = "linux")]
#[test]
fn thread_flags_size_the_runtime_and_are_logged() {
    let mut c = server(&[
        "--async-threads=2",
        "--blocking-threads=2",
        "--cpu-threads=2",
    ]);
    c.args(ANY_PORT)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Running(c.spawn().expect("spawn kbf-server"));
    first_line("start", child.0.stdout.take().expect("stdout"));
    // A thread carries its name only once it has run its first instructions, which can
    // be after the start line: wait for the count (up to 10 s) rather than read it once.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let (workers, names) = loop {
        let names: Vec<String> = std::fs::read_dir(format!("/proc/{}/task", child.pid()))
            .expect("task dir")
            .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
            .map(|name| name.trim().to_owned())
            .collect();
        let workers = names.iter().filter(|name| *name == "kbf-async").count();
        if workers >= 2 || std::time::Instant::now() > deadline {
            break (workers, names);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(
        (2..=4).contains(&workers),
        "threads of the server: {names:?}"
    );
    let mut stderr = child.0.stderr.take().expect("stderr");
    interrupt(child);
    let mut log = String::new();
    stderr.read_to_string(&mut log).expect("UTF-8 stderr");
    assert!(
        log.contains("threads: async=2 blocking=2 cpu=2 (cpus="),
        "{log}"
    );
    assert!(!log.contains("reserve"), "{log}");
}

/// Catches a server that sizes its threads from anything but the CPUs it may use (the
/// default was a reserve and the physical cores): with no thread flag the start line
/// names one async worker and one CPU thread per available CPU and tokio's 512 blocking
/// threads, and a `--reserve-cpus` or `--reserve-share` (a daemon flag, left in an old
/// unit) is refused with status 2 and the flag's name instead of being ignored.
#[cfg(unix)]
#[test]
fn the_server_uses_every_cpu_and_refuses_a_reserve() {
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let mut c = server(&[]);
    c.args(ANY_PORT)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Running(c.spawn().expect("spawn kbf-server"));
    first_line("start", child.0.stdout.take().expect("stdout"));
    let mut stderr = child.0.stderr.take().expect("stderr");
    interrupt(child);
    let mut log = String::new();
    stderr.read_to_string(&mut log).expect("UTF-8 stderr");
    let want = format!("threads: async={cpus} blocking=512 cpu={cpus} (cpus={cpus} ");
    assert!(log.contains(&want), "{want}\n{log}");

    for flag in ["--reserve-cpus=4", "--reserve-share=2"] {
        // Spawned and bounded, not `output()`: a server that accepted the flag would
        // run until killed, and the test must go red, not hang.
        let mut c = server(&[flag]);
        c.args(ANY_PORT)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = Running(c.spawn().expect("spawn kbf-server"));
        let status = child.exit_status(flag);
        assert_eq!(status.code(), Some(2), "{flag}");
        let mut stderr = String::new();
        child
            .0
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("UTF-8 stderr");
        let name = flag.split('=').next().expect("name");
        assert!(stderr.contains(name), "{flag}: {stderr}");
    }
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
    let api = api_of(&line);
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
    let api = api_of(&line);
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
    let file = file_server(&fresh_dir("sigint-file"));
    let s3 = raft_server(&fresh_dir("sigint-s3"));
    for (mode, c) in [("file", file), ("s3", s3)] {
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

/// Catches: an `s3://` cold store that does not build an S3 store from its URI and the
/// key pair (the store does no I/O until used, so the server starts).
#[cfg(unix)]
#[test]
fn s3_mode_starts_with_a_uri_and_the_key_pair() {
    let meta = fresh_dir("s3-starts");
    let mut c = with_keys(server(&[
        "--cold-store",
        "s3://kbf-test/farm?endpoint=http://127.0.0.1:9&conditional-put=true&region=eu-1&min-put-rate=4096",
        "--meta-dir",
    ]));
    c.arg(&meta).args(ANY_PORT);
    let (child, _, _) = started(c);
    interrupt(child);
}

/// Catches: an S3 server that starts without its key pair, or with a configuration the
/// store refuses, instead of exiting 2 and saying why; and a store URI that cannot name
/// a store (no endpoint, an unknown scheme, `memory:`, no `--cold-store` at all) taken
/// as one.
#[test]
fn s3_mode_refuses_what_is_missing_or_wrong() {
    let meta = fresh_dir("s3-refused");
    let s3 = |uri: &str| {
        let mut c = server(&["--cold-store", uri, "--meta-dir"]);
        c.arg(&meta).args(ANY_PORT);
        c
    };
    let good = "s3://kbf-test?endpoint=http://127.0.0.1:9";
    let (code, stderr) = fails(s3(good));
    assert_eq!(code, Some(2));
    assert!(stderr.contains("AWS_ACCESS_KEY_ID must be set"), "{stderr}");

    let mut no_secret = s3(good);
    no_secret.env("AWS_ACCESS_KEY_ID", "kbf-test-access");
    let (code, stderr) = fails(no_secret);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("AWS_SECRET_ACCESS_KEY must be set"),
        "{stderr}"
    );

    let (code, stderr) = fails(with_keys(s3("s3://B?endpoint=http://127.0.0.1:9")));
    assert_eq!(code, Some(2));
    assert!(stderr.contains("bucket name"), "{stderr}");

    for (uri, says) in [
        ("s3://kbf-test", "needs endpoint="),
        (
            "s3://kbf-test/no%20spaces?endpoint=http://127.0.0.1:9",
            "s3 URI prefix",
        ),
        (
            "s3://kbf-test?endpoint=http://127.0.0.1:9&typo=1",
            "use endpoint, region",
        ),
        ("memory://", "no memory: store"),
        ("gs://bucket/p", "unknown scheme"),
        ("/var/lib/kbf", "not a store URI"),
        ("file://host/x", "file:///absolute/path"),
    ] {
        let (code, stderr) = fails(with_keys(s3(uri)));
        assert_eq!(code, Some(2), "{uri}: {stderr}");
        assert!(stderr.contains(says), "{uri}: {stderr}");
    }
}

/// A fresh, absent directory for `name`, under the target directory.
fn fresh_dir(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-server-binary")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The `--cold-store` URI of an S3 store that does no I/O until used (nothing listens
/// at the endpoint).
const IDLE_S3: &str = "s3://kbf-test?endpoint=http://127.0.0.1:9";

/// A server over a `file://` cold store in `base/cold` with its metadata log in
/// `base/meta`, on any port.
fn file_server(base: &std::path::Path) -> Command {
    let cold = format!("file://{}", base.join("cold").display());
    let mut c = server(&["--cold-store", &cold, "--meta-dir"]);
    c.arg(base.join("meta")).args(ANY_PORT);
    c
}

/// An `s3://` cold store that does no I/O until used, with its metadata log in `dir`
/// (the store does no I/O until used).
fn raft_server(dir: &std::path::Path) -> Command {
    raft_server_on(dir, &ANY_PORT)
}

/// [`raft_server`] with the listener flags `listen` instead of [`ANY_PORT`].
fn raft_server_on(dir: &std::path::Path, listen: &[&str]) -> Command {
    let mut c = with_keys(server(&["--cold-store", IDLE_S3, "--meta-dir"]));
    c.arg(dir).args(listen);
    c
}

/// The operator API's address on a start line: the word after ` api=`.
fn api_of(line: &str) -> SocketAddr {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix("api="))
        .unwrap_or_else(|| panic!("no api= in {line:?}"))
        .parse()
        .expect("an address")
}

/// The ` log=` id of a start line.
fn log_id(line: &str) -> String {
    let id = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix("log="))
        .unwrap_or_else(|| panic!("no log= in {line:?}"));
    assert_eq!(id.len(), 16, "{line}");
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{line}");
    id.to_owned()
}

/// Catches: a metadata log that does not open in `--meta-dir`, or that draws a new log
/// id on every start (two starts over one directory must print the same `log=`, and two
/// directories two different ones).
#[cfg(unix)]
#[test]
fn raft_mode_keeps_its_log_id_across_starts() {
    let dir = fresh_dir("raft-restart");
    let (child, line, _) = started(raft_server(&dir));
    let first = log_id(&line);
    interrupt(child);
    assert!(dir.join("log-record").is_file(), "no log record in {dir:?}");
    let (child, line, _) = started(raft_server(&dir));
    assert_eq!(log_id(&line), first, "a restart drew a new log id");
    interrupt(child);

    let other = fresh_dir("raft-other");
    let (child, line, _) = started(raft_server(&other));
    assert_ne!(log_id(&line), first, "two directories, one log id");
    interrupt(child);
    for d in [&dir, &other] {
        std::fs::remove_dir_all(d).expect("remove the raft dir");
    }
}

/// Catches: `--hot-store` parsed and then ignored (no tier attached: its `TIER` file is
/// never written, and every write would still go to the cold store); the metadata log
/// not kept in `<hot>/meta` (so a restart would draw a new log id, or the log would
/// land where the operator did not point) or in `--meta-dir` when that is given
/// (which wins); the tier attached without the log raised to machine version 2 (its
/// first commit cannot be encoded: the start fails); and a server without a hot store
/// that raises its log anyway (an older binary could no longer read it).
#[cfg(unix)]
#[test]
fn a_hot_store_attaches_a_tier_and_holds_the_metadata_log() {
    let hot = fresh_dir("hot-store");
    let uri = format!("file://{}", hot.display());
    let with_hot = |meta: Option<&std::path::Path>| {
        let mut c = with_keys(server(&["--cold-store", IDLE_S3, "--hot-store", &uri]));
        if let Some(meta) = meta {
            c.arg("--meta-dir").arg(meta);
        }
        c.args(ANY_PORT);
        c
    };
    let (child, line, _) = started(with_hot(None));
    let first = log_id(&line);
    let named = std::fs::read_to_string(hot.join("TIER")).expect("a TIER file");
    assert!(
        named.ends_with('\n') && named.trim().parse::<u64>().is_ok(),
        "{named:?}"
    );
    interrupt(child);
    assert!(
        hot.join("meta/log-record").is_file(),
        "the log is not in <hot>/meta"
    );
    assert!(
        !opens_as_version_1(&hot.join("meta")),
        "a log not raised for the tier"
    );
    let (child, line, _) = started(with_hot(None));
    assert_eq!(log_id(&line), first, "a restart drew a new log id");
    interrupt(child);

    // `--meta-dir` wins: the log is there, and `<hot>/meta` is not made.
    let hot2 = fresh_dir("hot-store-meta-dir");
    let meta = fresh_dir("hot-store-meta-elsewhere");
    let uri2 = format!("file://{}", hot2.display());
    let mut c = with_keys(server(&["--cold-store", IDLE_S3, "--hot-store", &uri2]));
    c.arg("--meta-dir").arg(&meta).args(ANY_PORT);
    let (child, _, _) = started(c);
    interrupt(child);
    assert!(hot2.join("TIER").is_file());
    assert!(meta.join("log-record").is_file(), "no log in --meta-dir");
    assert!(
        !hot2.join("meta").exists(),
        "<hot>/meta made beside --meta-dir"
    );

    // Without a hot store the log is left at version 1.
    let raft = fresh_dir("write-tier-raft");
    let (child, _, _) = started(raft_server(&raft));
    interrupt(child);
    assert!(
        opens_as_version_1(&raft),
        "a raft log raised without a tier"
    );
    for dir in [&hot, &hot2, &meta, &raft] {
        std::fs::remove_dir_all(dir).expect("remove the directory");
    }
}

/// Catches a start that resolves its stores and logs none of it: an operator reading
/// the log must see the cold store, the hot store with the budget it was sized to, and
/// where the metadata log is (and whether the hot store's directory or `--meta-dir` put
/// it there), and, without a hot store, that there is none.
#[cfg(unix)]
#[test]
fn the_start_logs_every_resolved_store_path_and_budget() {
    let log_of = |mut c: Command| {
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = Running(c.spawn().expect("spawn kbf-server"));
        first_line("start", child.0.stdout.take().expect("stdout"));
        let mut stderr = child.0.stderr.take().expect("stderr");
        interrupt(child);
        let mut log = String::new();
        stderr.read_to_string(&mut log).expect("UTF-8 stderr");
        log
    };
    let base = fresh_dir("start-log");
    let hot = base.join("hot");
    let cold = format!("file://{}", base.join("cold").display());
    let hot_uri = format!("file://{}", hot.display());
    let mut c = server(&["--cold-store", &cold, "--hot-store", &hot_uri]);
    c.args(ANY_PORT);
    let log = log_of(c);
    assert!(log.contains(&format!("cold store: {cold}")), "{log}");
    assert!(
        log.contains(&format!("hot store: {hot_uri} (budget ")),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "metadata log: {}/meta (in the hot store)",
            hot.display()
        )),
        "{log}"
    );

    let meta = base.join("meta");
    let mut c = server(&["--cold-store", &cold, "--meta-dir"]);
    c.arg(&meta).args(ANY_PORT);
    let log = log_of(c);
    assert!(log.contains("hot store: none"), "{log}");
    assert!(
        log.contains(&format!("metadata log: {} (--meta-dir)", meta.display())),
        "{log}"
    );
}

/// Whether the Raft log in `dir` opens for a binary that runs machine version 1 only;
/// a log raised to 2 is refused at the raise.
fn opens_as_version_1(dir: &std::path::Path) -> bool {
    use kbf_log::MachineVersion;
    use kbf_server::{RaftLogError, RaftMetaLog};
    let opened = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(async {
            let fs = kbf_store::StdFs::new(dir).expect("the raft dir");
            let exists = || Err(std::io::Error::other("the log exists"));
            let retention = kbf_meta::Retention::default();
            RaftMetaLog::open_running(fs, retention, "kbf/", exists, MachineVersion::V1)
                .await
                .map(drop)
        });
    match opened {
        Ok(()) => true,
        Err(RaftLogError::Decode { .. }) => false,
        Err(e) => panic!("{e}"),
    }
}

/// Catches: a store flag set that cannot run starting anyway. No `--cold-store`; a cold
/// store with neither a `--hot-store` directory nor `--meta-dir` (the metadata log would
/// have nowhere to live); an `s3://` hot store (the write tier is block files on a local
/// disk); the flags of an older binary (`--store`, `--blob-dir`, `--raft-dir`,
/// `--meta-log`, `--s3-*`, `--write-tier-*`), which a unit that was not updated would
/// pass; and a metadata directory holding someone else's files taken as a new log.
/// Each exits 2 and says why.
#[test]
fn store_flags_that_cannot_run_are_refused() {
    let dir = fresh_dir("stores-refused");
    let dir_arg = dir.to_str().expect("a UTF-8 path");
    let cold = format!("file://{dir_arg}/cold");
    let refused = |flags: &[&str]| {
        let mut c = with_keys(bare(flags));
        c.args(ANY_PORT);
        fails(c)
    };
    let (code, stderr) = refused(&["--meta-dir", dir_arg]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("--cold-store is required"), "{stderr}");

    let (code, stderr) = refused(&["--cold-store", &cold]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("--meta-dir is required"), "{stderr}");

    let (code, stderr) = refused(&[
        "--cold-store",
        &cold,
        "--hot-store",
        IDLE_S3,
        "--meta-dir",
        dir_arg,
    ]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("must be a file:// directory"), "{stderr}");

    for flag in [
        "--store=memory",
        "--blob-dir=/b",
        "--raft-dir=/r",
        "--meta-log=raft",
        "--s3-endpoint=http://e",
        "--write-tier-dir=/t",
    ] {
        let (code, stderr) = refused(&["--cold-store", &cold, "--meta-dir", dir_arg, flag]);
        assert_eq!(code, Some(2), "{flag}: {stderr}");
        let name = flag.split_once('=').map_or(flag, |(name, _)| name);
        assert!(
            stderr.contains("unexpected argument") && stderr.contains(name),
            "{flag}: {stderr}"
        );
    }

    std::fs::create_dir_all(&dir).expect("a meta dir");
    std::fs::write(dir.join("stray"), "someone else's").expect("a stray file");
    let (code, stderr) = fails(raft_server(&dir));
    assert_eq!(code, Some(2));
    assert!(stderr.contains("holds files but no log-record"), "{stderr}");
    std::fs::remove_dir_all(&dir).expect("remove the directory");
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

/// Catches: a plain-text listener on an address other machines reach refused (or
/// stopped) instead of started; a start line without the warning, with it twice, or
/// naming the wrong listener; and a plain-text REAPI listener off loopback that binds
/// but does not answer. `0.0.0.0` stands for any address that is not loopback (the unit
/// tests of `loopback_only` cover the others).
#[cfg(unix)]
#[test]
fn a_plain_text_listener_off_loopback_starts_and_serves_with_one_warning() {
    for (reapi, worker, warned) in [
        ("0.0.0.0:0", "127.0.0.1:0", "reapi"),
        ("127.0.0.1:0", "0.0.0.0:0", "worker"),
        ("0.0.0.0:0", "0.0.0.0:0", "reapi,worker"),
    ] {
        let (child, line, bound) = started(server(&["--listen", reapi, "--worker-listen", worker]));
        let want = format!(" warning=plaintext-off-loopback:{warned}");
        assert!(
            line.ends_with(&want),
            "want {want:?} at the end of {line:?}"
        );
        assert_eq!(line.matches("warning=").count(), 1, "{line}");
        let local = SocketAddr::from(([127, 0, 0, 1], bound.port()));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                CapabilitiesClient::connect(format!("http://{local}"))
                    .await
                    .expect("connect in plain text")
                    .get_capabilities(GetCapabilitiesRequest::default())
                    .await
                    .expect("GetCapabilities");
            });
        interrupt(child);
    }
}

/// Catches: a start line that drops the plain-text warning, or
/// prints it before ` log=` instead of at the very end (the two fields came from two
/// branches that each tested its own alone).
#[cfg(unix)]
#[test]
fn a_raft_start_line_names_the_log_then_the_plain_text_warning() {
    let listen = ["--listen", "127.0.0.1:0", "--worker-listen", "0.0.0.0:0"];
    let c = raft_server_on(&fresh_dir("raft-plain-text"), &listen);
    let (child, line, _) = started(c);
    let id = log_id(&line);
    let want = format!(" log={id} warning=plaintext-off-loopback:worker");
    assert!(
        line.ends_with(&want),
        "want {want:?} at the end of {line:?}"
    );
    interrupt(child);
}

/// Catches: the removed `--reapi-plaintext-bind` escape still accepted (silently or as
/// a no-op), so a script written for the old guard keeps passing a flag that means
/// nothing. clap's unknown-flag error exits 2 and names it.
#[test]
fn the_reapi_plaintext_bind_flag_is_gone() {
    let mut c = server(&ANY_PORT);
    c.arg("--reapi-plaintext-bind");
    let (code, stderr) = fails(c);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("unexpected argument '--reapi-plaintext-bind'"),
        "{stderr}"
    );
}

/// Catches: a `--storage` the started binary parses but does not hand to the listener
/// (the blob would land in the server's own cache while the operator believes the node
/// holds it): a blob written to the started server is held by the node.
#[test]
fn the_storage_flag_makes_a_started_server_a_proxy() {
    use std::sync::Arc;

    use kbf_front::{Cache, CacheStorage, SegmentsConfig};
    use kbf_proto::google::bytestream::WriteRequest;
    use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
    use kbf_server::{StorageListener, bind_storage};

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let _entered = runtime.enter();
    let store: Arc<dyn kbf_storage::Storage> = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let node = bind_storage(
        Arc::clone(&store),
        StorageListener {
            listen: "127.0.0.1:0".parse().expect("address"),
            tls: None,
            service: kbf_storage::service::Config {
                write_bytes_in_flight: 64 << 20,
            },
        },
        std::future::pending(),
    )
    .expect("bind the node");
    let addr = node.addr;
    let serving = node.serving;
    runtime.spawn(async move { serving.await.expect("serve the node") });

    let url = format!("http://{addr}");
    let mut c = server(&ANY_PORT);
    c.args(["--storage", &url]);
    let (child, _, reapi) = started(c);
    let blob = b"written to a started server".to_vec();
    let digest = kbf_segments::sha256(&blob);
    runtime.block_on(async {
        let write = WriteRequest {
            resource_name: format!("uploads/u/blobs/{}/{}", digest.hash_hex(), blob.len()),
            data: blob.clone(),
            finish_write: true,
            ..Default::default()
        };
        ByteStreamClient::connect(format!("http://{reapi}"))
            .await
            .expect("connect")
            .write(futures::stream::iter([write]))
            .await
            .expect("write");
        assert!(
            store
                .find_missing(&[digest])
                .await
                .expect("node")
                .is_empty()
        );
    });
    interrupt(child);
}

/// Catches: a `--storage` that is not a URL starting the server anyway (it would serve
/// from its own cache while the operator believes it proxies): exit 2, the flag and the
/// value on stderr.
#[test]
fn a_storage_flag_that_is_not_a_url_stops_the_start() {
    let mut c = server(&ANY_PORT);
    c.args(["--storage", "not a url"]);
    let (code, stderr) = fails(c);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("--storage \"not a url\": "), "{stderr}");
}

/// Catches: a `--reapi-auth-policy` the binary does not run on its REAPI listener, and
/// a policy file that is not a policy but does not stop the start (exit 2, with the
/// flag and the mistake on stderr).
#[cfg(unix)]
#[test]
fn the_reapi_auth_policy_flag_is_read_at_start() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-binary");
    std::fs::create_dir_all(&dir).expect("a policy directory");
    let bad = dir.join(format!("bad-policy-{}.json", std::process::id()));
    std::fs::write(&bad, r#"{"authenticationPolicy": {"jwt": {}}}"#).expect("write");
    let deny = dir.join(format!("deny-policy-{}.json", std::process::id()));
    std::fs::write(&deny, r#"{"authenticationPolicy": {"deny": "closed"}}"#).expect("write");

    let mut c = server(&ANY_PORT);
    c.arg("--reapi-auth-policy").arg(&bad);
    let (code, stderr) = fails(c);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("--reapi-auth-policy: "), "{stderr}");
    assert!(stderr.contains("$.authenticationPolicy.jwt"), "{stderr}");

    let mut c = server(&ANY_PORT);
    c.arg("--reapi-auth-policy").arg(&deny);
    let (child, _, reapi) = started(c);
    let refused = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            CapabilitiesClient::connect(format!("http://{reapi}"))
                .await
                .expect("connect")
                .get_capabilities(GetCapabilitiesRequest::default())
                .await
                .expect_err("the policy denies every call")
        });
    assert_eq!(
        (refused.code(), refused.message()),
        (tonic::Code::Unauthenticated, "closed")
    );
    interrupt(child);
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

/// Catches: `--memory-ladder-gib` not parsed into the listeners' ladder (a farm that
/// ignores it books 4 GiB whatever the operator set), a default other than 4, 16 and
/// 32 GiB, a comma that does not separate rungs, and a ladder that is not ascending
/// and positive accepted: an empty rung, a zero, a repeated or falling rung, one that
/// overflows bytes, or a word.
#[test]
fn the_memory_ladder_comes_from_its_flag() {
    use clap::Parser;
    use kbf_server::{ConfigError, MemoryLadder};
    let listeners = |args: &[&str]| {
        kbf_server::Args::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied()))
            .map(|args| args.listeners())
    };
    let ladder = |args: &[&str]| listeners(args).expect("flags parse").expect("listeners");
    assert_eq!(
        ladder(&[]).memory_ladder,
        MemoryLadder::from_gib(&[4, 16, 32]).unwrap()
    );
    assert_eq!(
        ladder(&["--memory-ladder-gib", "2,8"]).memory_ladder,
        MemoryLadder::from_gib(&[2, 8]).unwrap()
    );
    assert_eq!(
        ladder(&["--memory-ladder-gib=1"]).memory_ladder,
        MemoryLadder::from_gib(&[1]).unwrap(),
        "one rung, then a whole host"
    );
    for bad in ["0", "0,4", "4,4", "16,4", "4,16,16", "1099511627776"] {
        let refused = listeners(&["--memory-ladder-gib", bad])
            .expect("the flag parses")
            .err();
        assert!(
            matches!(refused, Some(ConfigError::MemoryLadder(_))),
            "{bad}: {refused:?}"
        );
    }
    for bad in ["", "4,", "four", "-4", "4,,8"] {
        assert!(
            listeners(&["--memory-ladder-gib", bad]).is_err(),
            "{bad:?} parsed"
        );
    }
    let error = listeners(&["--memory-ladder-gib", "4,4"])
        .expect("parses")
        .expect_err("refused");
    assert_eq!(
        error.to_string(),
        "--memory-ladder-gib: memory rung 1 does not rise above the one before it"
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

/// One GET of `path` on `api`; the response.
#[cfg(unix)]
fn get(api: SocketAddr, path: &str) -> String {
    let mut stream = std::net::TcpStream::connect(api).expect("connect to the API");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    response
}

/// Catches: a start line that does not say whether the build pages and
/// `GET /v1/blobs` answer (or says `on` when one is off); a policy's
/// `invocations.getAuthorizer` or `blobs.getAuthorizer` read by the binary but not
/// given to its route, or a route on without its key (the empty blob, which every
/// cache holds, is read: `200` only with both keys); blobs on with no `invocations`
/// key; and (mutant "open does nothing") `--invocations-open` not turning both on,
/// with no policy file or with one that names `invocations` only.
#[cfg(unix)]
#[test]
fn the_start_line_says_whether_the_build_pages_and_blob_reads_are_on() {
    const EMPTY: &str =
        "/v1/blobs/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855-0";
    const PAGE: &str = "/v1/invocations/no-such-build";
    let allow = r#"{"getAuthorizer": {"allow": {}}}"#;
    let policy = |name: &str, keys: &str| {
        let text = format!(r#"{{"authenticationPolicy": {{"allow": {{}}}}{keys}}}"#);
        token_file(name, &text, 0o600)
    };
    let unnamed = policy("no-keys-policy", "");
    let blobs = policy("blobs-policy", &format!(r#", "blobs": {allow}"#));
    let pages = policy("pages-policy", &format!(r#", "invocations": {allow}"#));
    let both = policy(
        "pages-blobs-policy",
        &format!(r#", "invocations": {allow}, "blobs": {allow}"#),
    );
    for (policy, open, pages_on, blobs_on) in [
        (None, false, false, false),
        (Some(&unnamed), false, false, false),
        (Some(&blobs), false, false, false),
        (Some(&pages), false, true, false),
        (Some(&both), false, true, true),
        (None, true, true, true),
        (Some(&pages), true, true, true),
    ] {
        let mut c = server(&["--api-listen", "127.0.0.1:0"]);
        if let Some(policy) = policy {
            c.arg("--reapi-auth-policy").arg(policy);
        }
        if open {
            c.arg("--invocations-open");
        }
        c.args(ANY_PORT);
        let (child, line, _) = started(c);
        let case = format!("{policy:?} open={open}");
        let head = line.rsplit_once(" api=").expect("an API address").0;
        let on = |on: bool| if on { "on" } else { "off" };
        let want = format!(" invocations={} blobs={}", on(pages_on), on(blobs_on));
        assert!(head.ends_with(&want), "{case}: {line}");
        let api = api_of(&line);
        let answer = get(api, EMPTY);
        let code = if blobs_on {
            "HTTP/1.1 200"
        } else {
            "HTTP/1.1 404"
        };
        assert!(answer.starts_with(code), "{case}: {answer}");
        let page = get(api, PAGE);
        assert!(page.starts_with("HTTP/1.1 404"), "{case}: {page}");
        let off = page.contains("the build pages are off");
        assert_eq!(off, !pages_on, "{case}: {page}");
        interrupt(child);
    }
}
