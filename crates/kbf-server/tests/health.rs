//! `GET /healthz` and `GET /readyz` on the operator API listener, and
//! `grpc.health.v1.Health` on the REAPI listener: in-process servers over a
//! fault-injecting store, and the binary across a SIGTERM.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::StreamExt;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_meta::{Epoch, Retention};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
use kbf_proto::grpc::health::v1::health_client::HealthClient;
use kbf_proto::grpc::health::v1::health_server::Health;
use kbf_proto::grpc::health::v1::{HealthCheckRequest, HealthCheckResponse, HealthListRequest};
use kbf_server::grpc_health::{HealthService, SERVICES};
use kbf_server::{Api, BUILD_COMMIT, Listeners, Readiness, SERVER_VERSION, bind_server_with_api};
use serde_json::{Value, json};
use support::{HELLO_WAIT, INTERVAL, PROMPT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tonic::transport::Channel;

/// How the [`FaultStore`] answers reads.
const ANSWERS: u8 = 0;
const FAILS: u8 = 1;
const HANGS: u8 = 2;

/// What the [`FaultStore`] was asked, and how it answers reads.
#[derive(Default)]
struct Faults {
    mode: AtomicU8,
    reads: AtomicUsize,
    writes: AtomicUsize,
    deletes: AtomicUsize,
    lists: AtomicUsize,
}

impl Faults {
    fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    /// Reads, writes, deletes and lists so far.
    fn counts(&self) -> [usize; 4] {
        [&self.reads, &self.writes, &self.deletes, &self.lists].map(|n| n.load(Ordering::SeqCst))
    }
}

/// A [`MemoryStore`] that counts every call, and whose reads can fail or never answer.
struct FaultStore {
    inner: Arc<MemoryStore>,
    faults: Arc<Faults>,
}

impl ObjectStore for FaultStore {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.faults.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.faults.reads.fetch_add(1, Ordering::SeqCst);
        match self.faults.mode.load(Ordering::SeqCst) {
            FAILS => Err(ObjectStoreError::Service {
                status: 503,
                code: "SlowDown".to_owned(),
                message: "planted fault".to_owned(),
            }),
            HANGS => std::future::pending().await,
            _ => self.inner.get_range(key, range).await,
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.faults.deletes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.faults.lists.fetch_add(1, Ordering::SeqCst);
        self.inner.list(prefix, after, max_keys).await
    }
}

/// A server in this process over a [`FaultStore`], with the operator API; it is
/// stopped when the test's runtime ends.
struct Server {
    api: SocketAddr,
    reapi: SocketAddr,
    /// The bucket behind the [`FaultStore`], written to without counting.
    bucket: Arc<MemoryStore>,
    faults: Arc<Faults>,
    readiness: Arc<Readiness>,
    /// Completes the server's shutdown future (a stop signal).
    stop: Option<oneshot::Sender<()>>,
    /// The serving future: its result once it returns.
    serving: tokio::task::JoinHandle<Result<(), kbf_server::ServeError>>,
}

/// The store probe timeout of [`start`]'s servers.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

fn start() -> Server {
    start_with_prefix("kbf/1/")
}

/// [`start`], with the cache's keys under `prefix`.
fn start_with_prefix(prefix: &str) -> Server {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    let faults = Arc::new(Faults::default());
    let bucket = Arc::new(MemoryStore::new(Capabilities::default()));
    let store = FaultStore {
        inner: Arc::clone(&bucket),
        faults: Arc::clone(&faults),
    };
    let prefix = KeyPrefix::new(prefix).expect("a prefix");
    let cache = Arc::new(Cache::new(
        MemoryMetaLog::new(Retention::default()),
        store,
        prefix,
        Epoch::new(1),
    ));
    let listeners = Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: PROBE_TIMEOUT,
    };
    let api = Api {
        listen: loopback,
        token: None,
    };
    let (stop, stopped) = oneshot::channel::<()>();
    let shutdown = async move {
        let _ = stopped.await;
    };
    let bound = bind_server_with_api(cache, listeners, Some(api), shutdown).expect("bind");
    let api = bound.api.expect("an API address");
    let readiness = Arc::clone(&bound.readiness);
    let reapi = bound.reapi;
    let serving = tokio::spawn(bound.serving);
    Server {
        api,
        reapi,
        bucket,
        faults,
        readiness,
        stop: Some(stop),
        serving,
    }
}

/// One HTTP/1.1 GET of `path`; the status code and the body as JSON.
async fn get(api: SocketAddr, path: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{head}"
    );
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, serde_json::from_str(body).expect("a JSON body"))
}

/// [`get`], failing the test if the answer takes longer than `bound`.
async fn get_within(api: SocketAddr, path: &str, bound: Duration) -> (u16, Value) {
    tokio::time::timeout(bound, get(api, path))
        .await
        .unwrap_or_else(|_| panic!("GET {path}: no answer within {bound:?}"))
}

/// The checks a `/readyz` body names as failing.
fn failing(body: &Value) -> Vec<&str> {
    body["failing"]
        .as_array()
        .expect("a failing list")
        .iter()
        .map(|f| f["check"].as_str().expect("a check"))
        .collect()
}

/// Catches: a `/healthz` that is missing, that names another build, or that waits
/// on the store (here a store that never answers, so a route that probes it does not
/// answer within the bound).
#[tokio::test]
async fn healthz_answers_without_the_store() {
    let server = start();
    server.faults.set(HANGS);
    let (code, body) = get_within(server.api, "/healthz", PROMPT).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        body,
        json!({ "status": "alive", "version": SERVER_VERSION, "commit": BUILD_COMMIT })
    );
    assert_eq!(
        server.faults.counts(),
        [0, 0, 0, 0],
        "/healthz used the store"
    );
}

/// Catches: a `/readyz` that answers 200 without asking the store (no read per poll);
/// a probe that writes, deletes or lists (a poll every second from a front would
/// write to the bucket each time); and a probe that takes the absent probe key for an
/// unreachable store.
#[tokio::test]
async fn readyz_reads_the_store_once_per_poll_and_never_writes() {
    let server = start();
    for poll in 1..=5 {
        let (code, body) = get_within(server.api, "/readyz", PROMPT).await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(
            body,
            json!({ "ready": true, "version": SERVER_VERSION, "commit": BUILD_COMMIT,
                    "failing": [] })
        );
        assert_eq!(server.faults.counts(), [poll, 0, 0, 0], "after poll {poll}");
    }
}

/// Catches: a probe that takes an existing probe key for a fault, whether the object
/// has bytes (the read answers them) or is empty (the one-byte range starts past its
/// end). Nothing in kbf writes the key, but an operator or another tool may.
#[tokio::test]
async fn readyz_passes_whatever_the_probe_key_holds() {
    let server = start();
    let key = ObjectKey::new("kbf/1/readyz-probe").expect("a key");
    for body in [&b"x"[..], &b""[..]] {
        server.bucket.delete(&key).await.expect("delete");
        let body = Bytes::copy_from_slice(body);
        server.bucket.put_new(&key, body, None).await.expect("put");
        let (code, body) = get_within(server.api, "/readyz", PROMPT).await;
        assert_eq!(code, 200, "{body}");
    }
    assert_eq!(server.faults.counts(), [2, 0, 0, 0]);
}

/// Catches: a probe that cannot form its key (the cache's prefix leaves no room for
/// it under the 1024-byte key limit) yet reports the server ready, or reads some
/// other key.
#[tokio::test]
async fn readyz_is_503_when_the_probe_key_does_not_fit_the_prefix() {
    let server = start_with_prefix(&format!("{}/", "p".repeat(1020)));
    let (code, body) = get_within(server.api, "/readyz", PROMPT).await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(failing(&body), ["store"], "{body}");
    let reason = body["failing"][0]["reason"].as_str().expect("a reason");
    assert!(reason.contains("1021 bytes"), "{reason}");
    assert_eq!(server.faults.counts(), [0, 0, 0, 0]);
}

/// Catches: a `/readyz` that answers 200 whatever the store says, one that does not
/// name the store as the failing check, and one that stays 503 once the store
/// answers again.
#[tokio::test]
async fn readyz_is_503_while_the_store_fails() {
    let server = start();
    server.faults.set(FAILS);
    let (code, body) = get_within(server.api, "/readyz", PROMPT).await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(body["ready"], json!(false), "{body}");
    assert_eq!(failing(&body), ["store"], "{body}");
    let reason = body["failing"][0]["reason"].as_str().expect("a reason");
    assert!(reason.contains("planted fault"), "{reason}");

    server.faults.set(ANSWERS);
    let (code, body) = get_within(server.api, "/readyz", PROMPT).await;
    assert_eq!(code, 200, "{body}");
}

/// Catches: a probe without a timeout. The store never answers; `/readyz` must say
/// 503 within the probe timeout (300 ms here), well inside the 2 s bound, instead of
/// hanging the front's health check.
#[tokio::test]
async fn readyz_is_503_when_the_store_does_not_answer_in_time() {
    let server = start();
    server.faults.set(HANGS);
    let (code, body) = get_within(server.api, "/readyz", Duration::from_secs(2)).await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(failing(&body), ["store"], "{body}");
    let reason = body["failing"][0]["reason"].as_str().expect("a reason");
    assert!(reason.contains("no answer within 300 ms"), "{reason}");
}

/// Catches: a `/readyz` that ignores the scheduler role, so a front would send
/// clients to a server that is not the leader.
#[tokio::test]
async fn readyz_is_503_on_a_server_that_is_not_the_leader() {
    let server = start();
    server.readiness.set_leader(false);
    let (code, body) = get_within(server.api, "/readyz", PROMPT).await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(failing(&body), ["leader"], "{body}");
    server.readiness.set_leader(true);
    let (code, body) = get_within(server.api, "/readyz", PROMPT).await;
    assert_eq!(code, 200, "{body}");
}

/// Catches: a `/readyz` that ignores a stopping server, or names another check for
/// it; and a stopping server whose `/healthz` stops answering 200. The binary test
/// below proves the signal sets it; this one, the answer it gives.
#[tokio::test]
async fn readyz_is_503_once_the_server_is_stopping() {
    let server = start();
    server.readiness.stop();
    let (code, body) = get_within(server.api, "/readyz", PROMPT).await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(failing(&body), ["stopping"], "{body}");
    let (code, body) = get_within(server.api, "/healthz", PROMPT).await;
    assert_eq!(code, 200, "{body}");
}

/// A health client of `server`'s REAPI listener.
async fn health(reapi: SocketAddr) -> HealthClient<Channel> {
    HealthClient::connect(format!("http://{reapi}"))
        .await
        .expect("connect to the REAPI listener")
}

/// One `Check` of `service`, failing the test if it takes longer than [`PROMPT`].
async fn check(reapi: SocketAddr, service: &str) -> Result<ServingStatus, tonic::Status> {
    let request = HealthCheckRequest {
        service: service.to_owned(),
    };
    let mut client = health(reapi).await;
    let answer = tokio::time::timeout(PROMPT, client.check(request))
        .await
        .unwrap_or_else(|_| panic!("Check({service:?}): no answer within {PROMPT:?}"))?;
    Ok(answer.into_inner().status())
}

/// The next message of a Watch stream, failing the test if none comes within
/// [`PROMPT`].
async fn next_status(
    watch: &mut (impl futures::Stream<Item = Result<HealthCheckResponse, tonic::Status>> + Unpin),
) -> Option<Result<ServingStatus, tonic::Status>> {
    tokio::time::timeout(PROMPT, watch.next())
        .await
        .expect("a Watch message within PROMPT")
        .map(|m| m.map(|r| r.status()))
}

/// Catches: a health service that is missing from the REAPI listener, that answers
/// SERVING whatever `/readyz` would say (here a stopping server), or that does not
/// know the REAPI services by name. The stop signal's path to [`Readiness::stop`] is
/// proven by the Watch test below; once the signal has come the listener refuses new
/// calls, so this test sets the stop directly to ask Check what it then answers.
#[tokio::test]
async fn grpc_check_is_serving_then_not_serving_once_stopping() {
    let server = start();
    for service in SERVICES {
        assert_eq!(
            check(server.reapi, service).await.expect("Check"),
            ServingStatus::Serving,
            "{service:?}"
        );
    }
    server.readiness.stop();
    for service in SERVICES {
        assert_eq!(
            check(server.reapi, service).await.expect("Check"),
            ServingStatus::NotServing,
            "{service:?}"
        );
    }
}

/// Catches: a Check that does not ask the store (no read per call), that ignores a
/// failing store or the scheduler role, or that stays NOT_SERVING once the fault is
/// gone; that is, a Check that does not give `/readyz`'s answer.
#[tokio::test]
async fn grpc_check_follows_the_store_and_the_leader() {
    let server = start();
    assert_eq!(
        check(server.reapi, "").await.expect("Check"),
        ServingStatus::Serving
    );
    assert_eq!(
        server.faults.counts(),
        [1, 0, 0, 0],
        "one store read per Check"
    );
    server.faults.set(FAILS);
    assert_eq!(
        check(server.reapi, "").await.expect("Check"),
        ServingStatus::NotServing
    );
    server.faults.set(ANSWERS);
    server.readiness.set_leader(false);
    assert_eq!(
        check(server.reapi, "").await.expect("Check"),
        ServingStatus::NotServing
    );
    server.readiness.set_leader(true);
    assert_eq!(
        check(server.reapi, "").await.expect("Check"),
        ServingStatus::Serving
    );
    assert_eq!(server.faults.counts(), [4, 0, 0, 0], "a probe never writes");
}

/// Catches: a Check of a name the listener does not serve that answers a status
/// (the protocol says NOT_FOUND), and a List that leaves out a REAPI service.
#[tokio::test]
async fn grpc_check_of_an_unknown_service_is_not_found_and_list_names_every_service() {
    let server = start();
    let e = check(server.reapi, "kbf.worker.v1.Worker")
        .await
        .expect_err("not served on the REAPI listener");
    assert_eq!(e.code(), tonic::Code::NotFound, "{e:?}");

    let listed = health(server.reapi)
        .await
        .list(HealthListRequest {})
        .await
        .expect("List")
        .into_inner()
        .statuses;
    let mut names: Vec<&str> = listed.keys().map(String::as_str).collect();
    names.sort_unstable();
    let mut want = SERVICES.to_vec();
    want.sort_unstable();
    assert_eq!(names, want);
    assert!(
        listed
            .values()
            .all(|r| r.status() == ServingStatus::Serving),
        "{listed:?}"
    );
}

/// Catches: a stop signal that does not flip the health service before the REAPI
/// listener stops (an open Watch would see its connection drop at the shutdown
/// timeout instead of NOT_SERVING), and a Watch that stays open while the server
/// stops, holding the drain to its 10 s timeout: the serving future must return
/// within [`PROMPT`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_watch_sends_not_serving_on_the_stop_signal_and_ends() {
    let mut server = start();
    let request = HealthCheckRequest {
        service: String::new(),
    };
    let mut watch = health(server.reapi)
        .await
        .watch(request)
        .await
        .expect("Watch")
        .into_inner();
    let first = next_status(&mut watch).await.expect("a first message");
    assert_eq!(first.expect("a status"), ServingStatus::Serving);

    server
        .stop
        .take()
        .expect("not stopped yet")
        .send(())
        .expect("stop");
    let flipped = next_status(&mut watch).await.expect("a second message");
    assert_eq!(flipped.expect("a status"), ServingStatus::NotServing);
    let end = next_status(&mut watch).await.expect("the stream's end");
    let e = end.expect_err("ends with a status");
    assert_eq!(e.code(), tonic::Code::Unavailable, "{e:?}");

    let served = tokio::time::timeout(PROMPT, server.serving)
        .await
        .expect("the drain did not wait for the Watch");
    served.expect("join").expect("serve");
}

/// Catches: a Watch that sends only its first status (never the store's change in
/// either direction), that resends an unchanged status at every probe, or that
/// answers a change of the scheduler role only at its next probe. The service is
/// called in process with a 50 ms probe interval, so the test does not wait out the
/// listener's [`kbf_server::grpc_health::WATCH_INTERVAL`].
#[tokio::test]
async fn grpc_watch_streams_each_change() {
    let faults = Arc::new(Faults::default());
    let store = FaultStore {
        inner: Arc::new(MemoryStore::new(Capabilities::default())),
        faults: Arc::clone(&faults),
    };
    let cache = Arc::new(Cache::new(
        MemoryMetaLog::new(Retention::default()),
        store,
        KeyPrefix::new("kbf/1/").expect("a prefix"),
        Epoch::new(1),
    ));
    let readiness = Arc::new(Readiness::default());
    let service = HealthService::new(
        cache,
        Arc::clone(&readiness),
        PROBE_TIMEOUT,
        Duration::from_millis(50),
    );
    let request = tonic::Request::new(HealthCheckRequest {
        service: String::new(),
    });
    let mut watch = Health::watch(&service, request)
        .await
        .expect("Watch")
        .into_inner();
    let status =
        |m: Option<Result<ServingStatus, tonic::Status>>| m.expect("a message").expect("a status");
    assert_eq!(
        status(next_status(&mut watch).await),
        ServingStatus::Serving
    );
    faults.set(FAILS);
    assert_eq!(
        status(next_status(&mut watch).await),
        ServingStatus::NotServing
    );
    faults.set(ANSWERS);
    assert_eq!(
        status(next_status(&mut watch).await),
        ServingStatus::Serving
    );
    // Several probes, no change: nothing is sent.
    let quiet = tokio::time::timeout(Duration::from_millis(300), watch.next()).await;
    assert!(
        quiet.is_err(),
        "an unchanged status was sent again: {quiet:?}"
    );

    // A probe interval longer than the test's bound: only the change notice can wake
    // the stream in time.
    let readiness = Arc::new(Readiness::default());
    let cache = Arc::new(Cache::memory());
    let service = HealthService::new(
        cache,
        Arc::clone(&readiness),
        PROBE_TIMEOUT,
        Duration::from_secs(3600),
    );
    let request = tonic::Request::new(HealthCheckRequest {
        service: String::new(),
    });
    let mut watch = Health::watch(&service, request)
        .await
        .expect("Watch")
        .into_inner();
    assert_eq!(
        status(next_status(&mut watch).await),
        ServingStatus::Serving
    );
    readiness.set_leader(false);
    assert_eq!(
        status(next_status(&mut watch).await),
        ServingStatus::NotServing
    );
    readiness.set_leader(true);
    assert_eq!(
        status(next_status(&mut watch).await),
        ServingStatus::Serving
    );
}

/// Catches: a Watch of a name the listener does not serve that ends at once or
/// answers a status other than SERVICE_UNKNOWN (the protocol keeps it open), and
/// one that stays open after a stop.
#[tokio::test]
async fn grpc_watch_of_an_unknown_service_is_service_unknown_until_the_stop() {
    let readiness = Arc::new(Readiness::default());
    let service = HealthService::new(
        Arc::new(Cache::memory()),
        Arc::clone(&readiness),
        PROBE_TIMEOUT,
        Duration::from_millis(50),
    );
    let request = tonic::Request::new(HealthCheckRequest {
        service: "no.such.Service".to_owned(),
    });
    let mut watch = Health::watch(&service, request)
        .await
        .expect("Watch")
        .into_inner();
    let first = next_status(&mut watch).await.expect("a message");
    assert_eq!(first.expect("a status"), ServingStatus::ServiceUnknown);
    let quiet = tokio::time::timeout(Duration::from_millis(200), watch.next()).await;
    assert!(quiet.is_err(), "the stream did not stay open: {quiet:?}");
    readiness.stop();
    let end = next_status(&mut watch).await.expect("the stream's end");
    assert_eq!(
        end.expect_err("ends with a status").code(),
        tonic::Code::Unavailable
    );
    assert!(next_status(&mut watch).await.is_none());
}

/// The binary across a SIGTERM.
#[cfg(unix)]
mod binary {
    use std::io::{BufRead, BufReader};
    use std::net::SocketAddr;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use futures::channel::mpsc;
    use kbf_proto::google::bytestream::WriteRequest;
    use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
    use serde_json::json;

    use super::support::{Blob, PROMPT};
    use super::{failing, get_within};

    const BIN: &str = env!("CARGO_BIN_EXE_kbf-server");

    /// The server, killed and reaped when dropped.
    struct Running(Child);

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Starts the binary with the operator API on free loopback ports; it, and the
    /// REAPI and API addresses of its start line.
    fn start(args: &[&str]) -> (Running, SocketAddr, SocketAddr) {
        let mut child = Running(
            Command::new(BIN)
                .args(["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"])
                .args(["--api-listen", "127.0.0.1:0"])
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn kbf-server"),
        );
        let stdout = child.0.stdout.take().expect("stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let line = BufReader::new(stdout).lines().map_while(Result::ok).next();
            let _ = tx.send(line);
        });
        let line = rx
            .recv_timeout(PROMPT)
            .expect("a start line in time")
            .expect("a start line");
        let addr = |key: &str| -> SocketAddr {
            line.split_whitespace()
                .find_map(|w| w.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} in {line:?}"))
                .parse()
                .expect("an address")
        };
        (child, addr("reapi="), addr("api="))
    }

    /// Catches: readiness that stays 200 after SIGTERM, so a front keeps sending new
    /// work to a server that is shutting down; and a `/healthz` that stops answering
    /// while it drains. A ByteStream Write that sent part of its blob and went quiet
    /// holds the server in its drain (up to `--shutdown-timeout-secs`, 3 s here), so
    /// the API is still there to ask.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readyz_turns_503_on_sigterm_while_healthz_stays_200() {
        let (mut server, reapi, api) = start(&["--shutdown-timeout-secs", "3"]);
        let (code, body) = get_within(api, "/readyz", PROMPT).await;
        assert_eq!(code, 200, "{body}");

        let blob = Blob::new("held across a SIGTERM");
        let (chunks, requests) = mpsc::unbounded();
        chunks
            .unbounded_send(WriteRequest {
                resource_name: format!(
                    "uploads/6b1f0f8e-3c2d-4a5b-8e7f-1a2b3c4d5e6f/blobs/{}/{}",
                    blob.digest.hash_hex(),
                    blob.data.len()
                ),
                write_offset: 0,
                finish_write: false,
                data: blob.data[..4].to_vec(),
            })
            .expect("send the first part");
        let mut bytestream = ByteStreamClient::connect(format!("http://{reapi}"))
            .await
            .expect("connect");
        let upload = tokio::spawn(async move { bytestream.write(requests).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!upload.is_finished(), "the partial upload was answered");

        let pid = i32::try_from(server.0.id()).expect("pid");
        // SAFETY: kill(2) on the child this test spawned and has not reaped.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let deadline = Instant::now() + PROMPT;
        let body = loop {
            let (code, body) = get_within(api, "/readyz", PROMPT).await;
            if code == 503 {
                break body;
            }
            assert_eq!(code, 200, "{body}");
            assert!(Instant::now() < deadline, "/readyz still 200 after SIGTERM");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(failing(&body), ["stopping"], "{body}");
        let (code, body) = get_within(api, "/healthz", PROMPT).await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["status"], json!("alive"));

        let status = server.0.wait().expect("wait");
        assert!(status.success(), "kbf-server exited with {status}");
        drop(chunks);
    }
}
