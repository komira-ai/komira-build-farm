//! REAPI bearer-token authentication (`--reapi-token-file`): every method of the REAPI
//! listener needs a token the file admits, the principal's QoS reaches the scheduler,
//! its name reaches the Execute log, and edits and a broken file take effect without
//! a restart.

#![cfg(unix)]

mod support;

use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader, Read as _};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{BatchUpdateBlobsRequest, ExecuteRequest, batch_update_blobs_request};
use kbf_server::principal::{ClientRole, TokenStore, token_line};
use kbf_server::reapi_auth::{FILE_UNUSABLE, HOW_TO_SEND};
use kbf_types::Qos;
use prost::Message as _;
use support::{Blob, Cell, Job, PROMPT};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};

const DEV: &str = "kbf-test-token-dev-0123456789abcdef0123456789";
const CI: &str = "kbf-test-token-ci-0123456789abcdef01234567890";
const OTHER: &str = "kbf-test-token-other-0123456789abcdef01234567";

/// A directory unique to this call.
fn dir() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-server-reapi-auth")
        .join(format!("{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    dir
}

fn line(principal: &str, qos: &Qos, token: &str) -> String {
    token_line(principal, ClientRole::Client, qos, token.as_bytes()).expect("a line") + "\n"
}

/// Replaces `path` with `content` and `mode`: a new file renamed over the old one.
fn replace(path: &Path, content: &str, mode: u32) {
    let next = path.with_extension("next");
    std::fs::write(&next, content).expect("write");
    std::fs::set_permissions(&next, std::fs::Permissions::from_mode(mode)).expect("chmod");
    std::fs::rename(&next, path).expect("rename");
}

/// A token file admitting `dev` (QoS interactive, token [`DEV`]) and `ci-bot` (QoS ci,
/// token [`CI`]).
fn tokens_file() -> PathBuf {
    let path = dir().join("tokens");
    let content = line("dev", &Qos::Interactive, DEV) + &line("ci-bot", &Qos::Ci, CI);
    replace(&path, &content, 0o600);
    path
}

async fn channel(addr: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect")
}

/// `request` with each of `authorization` as an `authorization` header.
fn with_auth<T>(mut request: Request<T>, authorization: &[&str]) -> Request<T> {
    for value in authorization {
        request
            .metadata_mut()
            .append("authorization", value.parse().expect("an ASCII value"));
    }
    request
}

/// Every method of every service the compiled protos define, as its gRPC path.
fn every_method() -> Vec<String> {
    let set = prost_types::FileDescriptorSet::decode(kbf_proto::FILE_DESCRIPTOR_SET)
        .expect("the descriptor set decodes");
    let mut paths = Vec::new();
    for file in &set.file {
        for service in &file.service {
            for method in &service.method {
                paths.push(format!(
                    "/{}.{}/{}",
                    file.package(),
                    service.name(),
                    method.name()
                ));
            }
        }
    }
    paths
}

/// The service part of a method path.
fn service_of(path: &str) -> &str {
    path.rsplit_once('/').expect("a method path").0
}

/// Calls the method at `path` with an empty message (which every proto3 request
/// decodes from) and `authorization` headers, and returns its status (OK when it
/// answered). Any response decodes as `Empty`; a streaming method's first message is
/// enough.
async fn call(channel: &Channel, path: &str, authorization: &[&str]) -> Status {
    let mut grpc = tonic::client::Grpc::new(channel.clone());
    grpc.ready().await.expect("ready");
    let request = with_auth(Request::new(()), authorization);
    let codec = tonic_prost::ProstCodec::<(), ()>::default();
    let at = PathAndQuery::try_from(path).expect("a path");
    let answer = tokio::time::timeout(PROMPT, grpc.unary(request, at, codec)).await;
    match answer.unwrap_or_else(|_| panic!("{path:?} answered in time")) {
        Ok(_) => Status::ok(""),
        Err(status) => status,
    }
}

/// Whether `status` is the router's answer for a path no service serves.
fn unrouted(status: &Status) -> bool {
    status.code() == Code::Unimplemented && status.message().is_empty()
}

/// Catches: any method of the REAPI listener served without a token, with a token the
/// file does not hold, with the right token under another scheme, or with two
/// `authorization` headers (one method or service left out of the layer, or a check
/// that admits whatever it is shown); and a check that changes what an admitted call
/// gets. The methods come from the compiled protos and the services from what the
/// listener routes, so a service added to the listener later is in the table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_method_needs_a_token_the_file_admits() {
    let methods = every_method();
    let open = Cell::start().await;
    let open = channel(open.reapi).await;
    let mut answers = Vec::new();
    for path in &methods {
        answers.push(call(&open, path, &[]).await);
    }
    let routed: BTreeSet<&str> = methods
        .iter()
        .zip(&answers)
        .filter(|(_, status)| !unrouted(status))
        .map(|(path, _)| service_of(path))
        .collect();
    assert!(
        routed.contains("/build.bazel.remote.execution.v2.Execution")
            && routed.contains("/google.bytestream.ByteStream")
            && routed.len() == 5,
        "the REAPI listener routes Capabilities, CAS, ByteStream, ActionCache and \
         Execution: {routed:?}"
    );

    let store = TokenStore::open(&tokens_file()).expect("the token file");
    let cell = Cell::start_with_reapi_tokens(Arc::new(store)).await;
    let authed = channel(cell.reapi).await;
    let dev = format!("Bearer {DEV}");
    let refused: [(&str, Vec<String>); 7] = [
        ("no header", vec![]),
        (
            "a token the file does not hold",
            vec![format!("Bearer {OTHER}")],
        ),
        (
            "the token with a trailing byte",
            vec![format!("Bearer {DEV}x")],
        ),
        (
            "the token under another scheme",
            vec![format!("Basic {DEV}")],
        ),
        ("the token with no scheme", vec![DEV.to_owned()]),
        ("an empty bearer", vec!["Bearer ".to_owned()]),
        ("two headers", vec![dev.clone(), dev.clone()]),
    ];
    for (path, open_answer) in methods.iter().zip(&answers) {
        for (what, headers) in &refused {
            let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
            let status = call(&authed, path, &headers).await;
            assert_eq!(status.code(), Code::Unauthenticated, "{path} with {what}");
            assert_eq!(status.message(), HOW_TO_SEND, "{path} with {what}");
        }
        let admitted = call(&authed, path, &[&dev]).await;
        assert_eq!(
            (admitted.code(), admitted.message()),
            (open_answer.code(), open_answer.message()),
            "{path} with the right token is answered as without the check"
        );
        let lower = call(&authed, path, &[&format!("bearer {CI}")]).await;
        assert_eq!(
            lower.code(),
            open_answer.code(),
            "{path}: the scheme in lower case"
        );
    }
}

/// Catches: Execute submitting every call at the fixed `ci` QoS, the principal not
/// put into the request (so its QoS never reaches the scheduler), or the QoS taken
/// from another principal than the token's. Two actions are queued with no worker,
/// the `ci` principal's first; the one worker that then registers has room for one,
/// and it must get the `interactive` principal's action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_principal_qos_orders_queued_work() {
    let store = TokenStore::open(&tokens_file()).expect("the token file");
    let cell = Cell::start_with_reapi_tokens(Arc::new(store)).await;
    let ch = channel(cell.reapi).await;
    let (ci, dev) = (format!("Bearer {CI}"), format!("Bearer {DEV}"));
    let first = Job::new("ci build", &[]);
    let second = Job::new("dev build", &[]);
    upload(&ch, &ci, &first.blobs()).await;
    upload(&ch, &dev, &second.blobs()).await;

    let mut exec = ExecutionClient::new(ch.clone());
    let _ci_ops = exec
        .execute(execute(&first.action, &ci))
        .await
        .expect("Execute");
    let _dev_ops = exec
        .execute(execute(&second.action, &dev))
        .await
        .expect("Execute");
    let mut daemon = cell.daemon("node-a", 1, 8).await;
    let start = daemon.start().await;
    assert_eq!(
        start.action_digest.as_ref(),
        Some(&second.action.proto),
        "the interactive principal's action starts first"
    );
    assert!(
        tokio::time::timeout(support::QUIET, daemon.start())
            .await
            .is_err(),
        "a second action placed on a node with room for one"
    );
}

fn execute(action: &Blob, authorization: &str) -> Request<ExecuteRequest> {
    let request = Request::new(ExecuteRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(action.proto.clone()),
        ..Default::default()
    });
    with_auth(request, &[authorization])
}

async fn upload(channel: &Channel, authorization: &str, blobs: &[&Blob]) {
    let request = Request::new(BatchUpdateBlobsRequest {
        requests: blobs
            .iter()
            .map(|b| batch_update_blobs_request::Request {
                digest: Some(b.proto.clone()),
                data: b.data.clone(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    let answer = ContentAddressableStorageClient::new(channel.clone())
        .batch_update_blobs(with_auth(request, &[authorization]))
        .await
        .expect("BatchUpdateBlobs")
        .into_inner();
    assert!(
        answer
            .responses
            .iter()
            .all(|r| r.status.as_ref().is_some_and(|s| s.code == 0))
    );
}

const CAPS: &str = "/build.bazel.remote.execution.v2.Capabilities/GetCapabilities";

/// Polls `path` with `authorization` until it is answered with `code`.
async fn until(channel: &Channel, authorization: &str, code: Code) -> Status {
    let deadline = tokio::time::Instant::now() + PROMPT;
    loop {
        let status = call(channel, CAPS, &[authorization]).await;
        if status.code() == code {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "still {:?}, not {code:?}",
            status.code()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// What this test process logs, from the first call on (a global subscriber).
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut log = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        log.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn get() -> &'static Self {
        static LOG: OnceLock<Captured> = OnceLock::new();
        LOG.get_or_init(|| {
            let log = Self::default();
            let writer = log.clone();
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .init();
            log
        })
    }

    /// How many logged lines contain `text`.
    fn count(&self, text: &str) -> usize {
        let log = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&log)
            .lines()
            .filter(|l| l.contains(text))
            .count()
    }
}

/// Catches: the layer reading the token file only once (a removed line still served,
/// an added one refused until a restart); a file broken after start (here chmod-ed to
/// 0644) that keeps serving the entries read before (fail open), or keeps refusing
/// once fixed; and the file's error not logged, or logged at every refused call
/// rather than once while it stays broken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_file_edits_apply_without_a_restart() {
    let log = Captured::get();
    let path = tokens_file();
    let store = TokenStore::open_with_interval(&path, Duration::from_millis(20)).expect("open");
    let cell = Cell::start_with_reapi_tokens(Arc::new(store)).await;
    let ch = channel(cell.reapi).await;
    let (dev, other) = (format!("Bearer {DEV}"), format!("Bearer {OTHER}"));
    assert_eq!(call(&ch, CAPS, &[&dev]).await.code(), Code::Ok);
    assert_eq!(
        call(&ch, CAPS, &[&other]).await.code(),
        Code::Unauthenticated
    );

    replace(&path, &line("other", &Qos::Ci, OTHER), 0o600);
    until(&ch, &other, Code::Ok).await;
    let removed = call(&ch, CAPS, &[&dev]).await;
    assert_eq!(
        removed.code(),
        Code::Unauthenticated,
        "a removed line still served"
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let broken = until(&ch, &other, Code::Unauthenticated).await;
    assert_eq!(broken.message(), FILE_UNUSABLE);
    assert!(!broken.message().contains(path.to_str().expect("UTF-8")));
    for _ in 0..5 {
        let again = call(&ch, CAPS, &[&other]).await;
        assert_eq!(again.message(), FILE_UNUSABLE);
    }
    let unusable = "--reapi-token-file is unusable: every REAPI call is refused";
    assert_eq!(log.count(unusable), 1, "the file's error is logged once");
    assert_eq!(log.count("0644"), 1, "the log says why");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    until(&ch, &other, Code::Ok).await;
}

/// A `kbf-server` process, killed when dropped.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn server(tokens: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_kbf-server"));
    c.args(["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"])
        .arg("--reapi-token-file")
        .arg(tokens);
    c
}

/// Catches: `--reapi-token-file` parsed but not applied to the listener; the principal
/// missing from the Execute log line; a token or its digest in the log; and a token
/// file others can read accepted at start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_checks_tokens_and_logs_the_principal() {
    let path = tokens_file();
    let mut child = Running(
        server(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kbf-server"),
    );
    let mut start = String::new();
    BufReader::new(child.0.stdout.take().expect("stdout"))
        .read_line(&mut start)
        .expect("a start line");
    let reapi: SocketAddr = start
        .split_whitespace()
        .find_map(|w| w.strip_prefix("reapi="))
        .unwrap_or_else(|| panic!("no reapi= in {start:?}"))
        .parse()
        .expect("an address");
    let ch = channel(reapi).await;
    assert_eq!(call(&ch, CAPS, &[]).await.code(), Code::Unauthenticated);
    let job = Job::new("logged", &[]);
    let missing = ExecutionClient::new(ch)
        .execute(execute(&job.action, &format!("Bearer {DEV}")))
        .await
        .expect_err("the action was never uploaded");
    assert_eq!(missing.code(), Code::FailedPrecondition, "{missing:?}");

    let _ = child.0.kill();
    let _ = child.0.wait();
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("UTF-8 stderr");
    let stderr = plain(&stderr);
    let execute_line = stderr
        .lines()
        .find(|l| l.contains("Execute") && l.contains(&job.action.digest.hash_hex()))
        .unwrap_or_else(|| panic!("no Execute line in {stderr}"));
    assert!(execute_line.contains("principal=\"dev\""), "{execute_line}");
    assert!(!stderr.contains(DEV), "the token is in the log: {stderr}");
    let digest = line("dev", &Qos::Interactive, DEV);
    let digest = digest.split("sha256:").nth(1).expect("a digest").trim();
    assert!(
        !stderr.contains(digest),
        "the digest is in the log: {stderr}"
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let refused = server(&path)
        .stdout(Stdio::null())
        .output()
        .expect("run kbf-server");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("--reapi-token-file: "), "{stderr}");
}

/// `text` without ANSI escape sequences (the log colours its field names).
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
