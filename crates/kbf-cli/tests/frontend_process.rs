//! `kbf frontend` as a separate process in front of a `kbf storage` process
//! (docs/design/architecture-v2.md, section 13 P2): the REAPI conformance suite
//! (`kbf-cas-conformance`) run by the daemon's transfer engine over real gRPC to the
//! frontend, which proxies to the node; and the start, health and stop of the frontend
//! process. Nothing is in-process: the frontend, its proxy and the node's store are
//! children.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use kbf_cas_conformance::Subject;
use kbf_testkit::dir::TestDir;
use kbf_transfer::{TransferConfig, Transfers};
use tonic::transport::Endpoint;

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// A running `kbf` child that has printed its start line; killed when dropped.
struct Proc {
    child: Child,
    line: String,
}

impl Proc {
    /// `kbf <args>`, once it has printed its start line.
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn kbf");
        // Read on a thread so that a child that never prints its start line fails the
        // test instead of hanging it.
        let stdout = child.stdout.take().expect("stdout");
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = sent.send(line);
        });
        let line = match received.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok(line) => line,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("kbf {args:?} printed no start line within 60 seconds");
            }
        };
        Self { child, line }
    }

    /// The `key=` field of the start line.
    fn field(&self, key: &str) -> String {
        self.line
            .split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} in the start line {:?}", self.line))
            .to_owned()
    }

    /// SIGTERM, then the exit status (killed if it has not exited in ten seconds).
    fn stop(&mut self) -> Option<std::process::ExitStatus> {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        for _ in 0..200 {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A plain-text `kbf storage` over new stores under `dir`.
fn node(dir: &Path) -> Proc {
    std::fs::create_dir_all(dir).expect("directory");
    let cold = format!("--cold-store=file://{}", dir.join("cold").display());
    let meta = format!("--meta-dir={}", dir.join("meta").display());
    Proc::start(&["storage", "--storage-listen=127.0.0.1:0", &cold, &meta])
}

/// A `kbf frontend` in front of the node at `storage`, on free loopback ports with the
/// operator API.
fn frontend(storage: &str, more: &[&str]) -> Proc {
    let storage = format!("--storage=http://{storage}");
    let mut args = vec![
        "frontend",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
        // A stop is bounded here (`Proc::stop`), and no load balancer needs the wait.
        "--drain-lb-wait=0",
        storage.as_str(),
    ];
    args.extend_from_slice(more);
    let frontend = Proc::start(&args);
    assert!(
        frontend.line.starts_with("kbf-frontend "),
        "{}",
        frontend.line
    );
    frontend
}

/// One node and one frontend process per [`Subject::fresh`].
struct Through {
    // The processes come first: fields drop in order, and the directory they write in
    // must outlive them.
    procs: Mutex<Vec<Proc>>,
    scratch: TestDir,
}

impl Subject for Through {
    type Cas = Transfers;

    async fn fresh(&self) -> Arc<Transfers> {
        let mut procs = self.procs.lock().expect("lock");
        let n = procs.len();
        let node = node(&self.scratch.join(format!("node-{n}")));
        let state = self.scratch.join(format!("state-{n}")).join("state.db");
        let state = format!("--state-store=sqlite://{}", state.display());
        let front = frontend(&node.field("storage="), &[&state]);
        let endpoint =
            Endpoint::from_shared(format!("http://{}", front.field("reapi="))).expect("endpoint");
        procs.push(node);
        procs.push(front);
        Arc::new(Transfers::new(endpoint, TransferConfig::default()))
    }

    fn scratch(&self) -> &Path {
        self.scratch.path()
    }
}

impl Through {
    /// A subject with a scratch directory of its own: each test makes one.
    fn new() -> Self {
        Self {
            procs: Mutex::new(Vec::new()),
            scratch: TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-process"),
        }
    }
}

kbf_cas_conformance::tests!(Through::new());

/// One `GET` on the operator API: the status code and the body.
fn get(api: &str, path: &str) -> (u16, String) {
    let mut stream = std::net::TcpStream::connect(api).expect("connect to the API");
    let bound = Some(std::time::Duration::from_secs(30));
    stream.set_read_timeout(bound).expect("a read timeout");
    stream.set_write_timeout(bound).expect("a write timeout");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n"
    )
    .expect("send");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");
    let (head, body) = text.split_once("\r\n\r\n").expect("a head and a body");
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok());
    (code.expect("a status code"), body.to_owned())
}

/// Catches a frontend process that does not start, does not serve its operator API, does
/// not follow its storage node in `/readyz` (200 while the node is up, 503 naming
/// `storage` once it is stopped) or does not exit 0 on SIGTERM.
#[test]
fn the_frontend_process_is_ready_with_its_node_and_stops_on_sigterm() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-health");
    let mut node = node(&dir);
    let state = dir.join("state").join("state.db");
    let state = format!("--state-store=sqlite://{}", state.display());
    let mut front = frontend(
        &node.field("storage="),
        &["--readyz-store-timeout-ms=500", &state],
    );
    let api = front.field("api=");
    assert_eq!(get(&api, "/healthz").0, 200);
    let (code, body) = get(&api, "/readyz");
    assert_eq!(code, 200, "{body}");
    assert!(node.stop().is_some_and(|status| status.success()));
    let (code, body) = get(&api, "/readyz");
    assert_eq!(code, 503, "{body}");
    assert!(body.contains("\"check\":\"storage\""), "{body}");
    assert_eq!(get(&api, "/healthz").0, 200);
    assert!(front.stop().is_some_and(|status| status.success()));
}

/// Catches a frontend that starts when it should not: with no `--storage`, with a flag
/// of a storage node (a local store), and with a plain-text storage URL off loopback it
/// exits 2 with the reason on stderr and prints no start line.
#[test]
fn a_refused_frontend_start_exits_2() {
    let run = |args: &[&str]| {
        Command::new(BIN)
            .arg("frontend")
            .args(args)
            .output()
            .expect("run kbf")
    };
    for (args, want) in [
        (&[][..], "--storage"),
        (
            &["--storage=http://127.0.0.1:1", "--cold-store=file:///x"][..],
            "kbf frontend refuses --cold-store: ",
        ),
        (
            &["--storage=http://storage.example:8982"][..],
            "not loopback",
        ),
        (
            &[
                "--storage=http://127.0.0.1:1",
                "--state-store=postgres://user:hunter2@db.example/kbf",
            ][..],
            "must carry no password",
        ),
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(want), "{args:?}: {err}");
        assert!(!err.contains("hunter2"), "{args:?}: {err}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

/// Asserts that a frontend given neither `--state-store` nor a settings file with the key
/// exits 2 naming the flag, at a start and under `--check` alike and with the same text,
/// and that it prints nothing on standard output: the start line, which comes once the
/// listeners are bound, is absent, so nothing listened.
#[test]
fn a_frontend_with_no_state_store_is_refused_at_a_start_and_by_check() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-no-state-store");
    let config = kbf_testkit::config::flag(&dir);
    let flags = [
        config.as_str(),
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--storage=http://127.0.0.1:1",
    ];
    let run = |extra: &[&str]| {
        Command::new(BIN)
            .arg("frontend")
            .args(flags)
            .args(extra)
            .output()
            .expect("run kbf")
    };
    let started = run(&[]);
    let checked = run(&["--check"]);
    for (how, out) in [("a start", &started), ("--check", &checked)] {
        assert_eq!(out.status.code(), Some(2), "{how}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("kbf frontend: --state-store is not set"),
            "{how}: {err}"
        );
        assert!(out.stdout.is_empty(), "{how}");
    }
    assert_eq!(started.stderr, checked.stderr);
}

/// Catches a frontend process that takes `--state-store=sqlite://...` and does not open
/// it (the file would not exist), whose `/readyz` does not follow the store with the node
/// up (200), or that leaves the file to a second frontend: the second exits 2 naming the
/// holder, prints no start line.
#[test]
fn a_frontend_process_with_a_sqlite_state_store_is_ready_and_holds_the_file() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-state");
    let node = node(&dir);
    let state = dir.join("state").join("state.db");
    let flag = format!("--state-store=sqlite://{}", state.display());
    let mut front = frontend(&node.field("storage="), &[&flag]);
    assert!(state.exists());
    let (code, body) = get(&front.field("api="), "/readyz");
    assert_eq!(code, 200, "{body}");

    let storage = format!("--storage=http://{}", node.field("storage="));
    let second = Command::new(BIN)
        .args([
            "frontend",
            "--listen=127.0.0.1:0",
            "--worker-listen=127.0.0.1:0",
        ])
        .args([storage.as_str(), flag.as_str()])
        .output()
        .expect("run kbf");
    assert_eq!(second.status.code(), Some(2));
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains("another kbf process"), "{err}");
    assert!(second.stdout.is_empty());
    assert!(front.stop().is_some_and(|status| status.success()));
}

/// `url` with the password of its user information removed.
fn without_password(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("a URL");
    match rest.split_once('@') {
        Some((user, tail)) => {
            let user = user.split_once(':').map_or(user, |(name, _)| name);
            format!("{scheme}://{user}@{tail}")
        }
        None => url.to_owned(),
    }
}

/// Catches a frontend process started with a Postgres `--state-store` that dies at start
/// (the blocking client run inside the server's tokio runtime panicked with "Cannot start
/// a runtime from within a runtime"), or whose `/readyz` is not 200 over the schema the
/// installer made, or that creates the tables itself: started on a database nobody ran
/// `setup` on it must exit with the one line that says what to run, and leave no schema.
/// Needs a Postgres server, and fails without one: `KBF_TEST_POSTGRES_URL` names a server the
/// test may create a database on. The database is the test's own and starts with no schema.
#[test]
fn a_frontend_process_with_a_postgres_state_store_is_ready_over_the_installers_schema() {
    let database = kbf_state::postgres::TestDatabase::empty();
    let url = database.url();
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-postgres");
    let node = node(&dir);
    // The flag refuses a password (the command line is world-readable), so the process
    // gets the URL without one; the server the job runs trusts its loopback clients.
    let flag = format!("--state-store={}", without_password(&url));
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");

    let storage = format!("--storage=http://{}", node.field("storage="));
    let refused = Command::new(BIN)
        .args([
            "frontend",
            "--listen=127.0.0.1:0",
            "--worker-listen=127.0.0.1:0",
        ])
        .args([storage.as_str(), flag.as_str()])
        .output()
        .expect("run kbf");
    assert!(
        !refused.status.success(),
        "a start with no schema must fail"
    );
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(err.contains("kbf-installer setup"), "{err}");
    assert!(!err.contains("panicked"), "{err}");
    let made = client
        .query(
            "SELECT 1 FROM pg_namespace WHERE nspname = 'kbf_state'",
            &[],
        )
        .expect("catalog");
    assert!(made.is_empty(), "the refused start created the schema");

    let config = kbf_state::postgres::PostgresConfig::parse(&url).expect("the URL parses");
    let set = kbf_state::postgres::schema::embedded();
    kbf_state::postgres::schema::apply(&config, &set, kbf_state::postgres::schema::latest())
        .expect("the installer's step");
    let mut front = frontend(&node.field("storage="), &[&flag]);
    let (code, body) = get(&front.field("api="), "/readyz");
    assert_eq!(code, 200, "{body}");
    client
        .query_one("SELECT count(*) FROM kbf_state.kv", &[])
        .expect("the installer's tables are there");
    assert!(front.stop().is_some_and(|status| status.success()));
}
