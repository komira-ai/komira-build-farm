//! Retire mode (`GET` and `POST /v1/frontend/retire`, `kbf_server::retire`): two frontends
//! in this process over one real Postgres database (the server `KBF_TEST_POSTGRES_URL` names;
//! those tests fail without it), each started as a start is, from `--state-store`, so each
//! on a store handle of its own and with no generation taken, as the frontends of one farm
//! hold the store. A fake daemon and a client play the farm's node and Buck2. The routes'
//! answers on a frontend with no state store need no Postgres.

use kbf_server::api::FRONTEND_RETIRE;
use kbf_server::frontend::start_frontend;
use kbf_server::{ServeError, StateStoreUri};
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase, test_database};
use serde_json::json;
use support::done_within_quiet;
use tokio::task::JoinHandle;
use tonic::Code;

use super::*;

/// Waits until `look` says yes, for at most twenty seconds.
pub(super) async fn until(what: &str, mut look: impl AsyncFnMut() -> bool) {
    for _ in 0..1000 {
        if look().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never happened: {what}");
}

pub(super) async fn readyz(api: SocketAddr) -> u16 {
    get(api, "/readyz").await.0
}

/// One request and its answer, bounded: a process that never answers fails the test
/// instead of hanging it.
pub(super) async fn exchange(addr: SocketAddr, request: &str) -> (u16, Value) {
    let exchanged = async {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(request.as_bytes()).await.expect("send");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        response
    };
    let response = tokio::time::timeout(Duration::from_secs(30), exchanged)
        .await
        .unwrap_or_else(|_| panic!("{addr} did not answer within 30 seconds"));
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok());
    (
        code.expect("a status code"),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

/// A scratch directory of the test's own, removed when dropped.
struct Scratch(kbf_testkit::dir::TestDir);

impl Scratch {
    fn new(name: &str) -> Self {
        Self(kbf_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("kbf-managed-{name}"),
        ))
    }

    /// The `--api-token-file` flag for a token file of this directory, 0600.
    fn token_flag(&self) -> String {
        let file = self.0.join("token");
        std::fs::write(&file, format!("{TOKEN}\n")).expect("token file");
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("mode");
        format!("--api-token-file={}", file.display())
    }
}

const TOKEN: &str = "a-token-that-is-long-enough-for-a-test";

async fn refused(addr: SocketAddr) -> bool {
    TcpStream::connect(addr).await.is_err()
}

/// The code an Execute is refused with, however it is surfaced.
pub(super) async fn execute_refused(client: &Client, job: &Job) -> Option<Code> {
    match client.execute_with(&job.action, false).await {
        Err(status) => Some(status.code()),
        Ok(mut ops) => match ops.message().await {
            Err(status) => Some(status.code()),
            Ok(_) => None,
        },
    }
}

/// A frontend serving in this test, until its stop signal is given.
pub(super) struct Member {
    pub(super) reapi: SocketAddr,
    pub(super) worker: SocketAddr,
    pub(super) api: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    pub(super) served: JoinHandle<Result<(), ServeError>>,
}

impl Member {
    /// Starts a frontend as a start does, over the `--state-store` the flags name.
    fn start(flags: &FrontendFlags) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let handle = start_frontend(flags, || {
            Ok(async move {
                let _ = stopped.await;
            })
        })
        .expect("starts");
        let (reapi, worker, api) = (handle.reapi, handle.worker, handle.api);
        Self {
            reapi,
            worker,
            api: api.expect("the operator API"),
            stop: Some(stop),
            served: tokio::spawn(handle.serve()),
        }
    }

    /// Gives the frontend its stop signal, as SIGTERM does, and goes on at once.
    pub(super) fn signal(&mut self) {
        drop(self.stop.take());
    }

    /// Stops the frontend and waits until it has ended.
    async fn stop(mut self) {
        self.signal();
        let ended = tokio::time::timeout(Duration::from_secs(30), self.served).await;
        let served = ended.expect("the frontend ends").expect("its task");
        served.expect("served without an error");
    }
}

/// Runs `f` on a thread of its own, which is in no runtime, and returns its result; a panic
/// in `f` is raised again in the caller. The test server's client (`TestDatabase`) blocks on
/// a runtime of its own to make and to drop a database, and tokio refuses that on a thread
/// that is running tasks.
fn off_runtime<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| match scope.spawn(f).join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

/// A database of its own on the test server, with the state schema, made and dropped off
/// the runtime (see [`off_runtime`]), so that it is also dropped when a test fails.
struct Database(Option<TestDatabase>);

impl Database {
    fn new() -> Self {
        Self(Some(off_runtime(test_database)))
    }

    /// The URL of the database.
    fn url(&self) -> String {
        self.0.as_ref().expect("held until dropped").url()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some(database) = self.0.take() {
            off_runtime(move || drop(database));
        }
    }
}

/// Two frontends started over one Postgres database, a storage node they share, and a handle
/// of the test's own on the database.
pub(super) struct Pair {
    pub(super) a: Member,
    pub(super) b: Member,
    store: Arc<PostgresStore>,
    _node: Node,
    _scratch: Scratch,
    /// Declared last, so the database is dropped after the stores of this struct.
    _database: Database,
}

impl Pair {
    /// Two frontends that drain without the load balancer's wait (`--drain-lb-wait=0`), as a
    /// test that stops them has no load balancer to wait for.
    async fn start(name: &str) -> Self {
        Self::start_with(name, &["--drain-lb-wait=0"]).await
    }

    /// Two frontends given the flags `more` besides the ones every test gives.
    pub(super) async fn start_with(name: &str, more: &[&str]) -> Self {
        let node = Node::start().await;
        let scratch = Scratch::new(name);
        let token = scratch.token_flag();
        let database = Database::new();
        let storage = format!("--storage=http://{}", node.addr);
        let mut argv = vec![storage.as_str(), token.as_str()];
        argv.push("--heartbeat-interval-ms=100");
        argv.extend_from_slice(more);
        let mut given = flags(&argv).expect("flags");
        // The flag refuses a URL that carries a password, which the test server's does.
        let uri: StateStoreUri = database.url().parse().expect("a state store URL");
        given.server.state_store = Some(uri);
        let a = Member::start(&given);
        let b = Member::start(&given);
        let config = PostgresConfig::parse(&database.url()).expect("a config");
        Self {
            a,
            b,
            store: Arc::new(PostgresStore::open(&config).expect("a connection")),
            _node: node,
            _scratch: scratch,
            _database: database,
        }
    }

    /// How many rows the schema holds under `prefix`.
    pub(super) fn count(&self, prefix: &str) -> usize {
        self.store.scan_prefix(prefix).expect("scan").len()
    }

    /// Stops both frontends.
    pub(super) async fn stop(self) {
        for member in [self.a, self.b] {
            member.stop().await;
        }
    }
}

/// The header that carries the operator API's token.
pub(super) fn bearer() -> String {
    format!("Authorization: Bearer {TOKEN}\r\n")
}

/// One call to the operator API at `api`: `auth` is the header lines to send besides the
/// content type, and `body` the JSON body.
async fn call(api: SocketAddr, method: &str, auth: &str, body: &str) -> (u16, Value) {
    let request = format!(
        "{method} {FRONTEND_RETIRE} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    exchange(api, &request).await
}

/// `GET /v1/frontend/retire`, which answers 200.
async fn status(api: SocketAddr) -> Value {
    let (code, status) = call(api, "GET", "", "").await;
    assert_eq!(code, 200, "{status}");
    status
}

/// `POST /v1/frontend/retire` with the token and no body.
async fn retire(api: SocketAddr) -> (u16, Value) {
    call(api, "POST", &bearer(), "").await
}

/// The operations in the scheduler's queue of the frontend at `api`.
pub(super) async fn queued(api: SocketAddr) -> u64 {
    let (_, drain) = get(api, "/v1/frontend/drain").await;
    drain["queued_operations"].as_u64().expect("a count")
}

/// Asserts that retiring a frontend hands its work to the other and keeps its caller. A
/// daemon is connected to the first frontend (its status counts one session) with an
/// Execute open on it and its action running. On `POST` the first frontend's worker
/// listener closes, the daemon's stream ends `UNAVAILABLE` and the status says retired
/// with the stream still open. The daemon dials the second frontend, which takes up the
/// lease it lists and its result, and the caller on the first frontend gets that result
/// from the state store. The action ran once: the daemon is given no second start and the
/// store holds one result and no operation. A second `POST` answers the same status.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retired_frontend_hands_its_daemon_to_the_other_and_still_answers_its_caller() {
    let pair = Pair::start("retire-handover").await;
    let (a, b) = (&pair.a, &pair.b);
    let client = Client::connect(a.reapi, a.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a build across a retire", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let before = status(a.api).await;
    assert_eq!(before["retired"], false, "{before}");
    assert_eq!(before["open_streams"], 1, "{before}");
    assert_eq!(before["open_sessions"], 1, "{before}");

    let (code, retired) = retire(a.api).await;
    assert_eq!(code, 200, "{retired}");
    assert_eq!(retired["retired"], true, "{retired}");
    assert_eq!(retired["open_streams"], 1, "{retired}");
    until("its worker listener is closed", async || {
        refused(a.worker).await
    })
    .await;
    let (code, again) = retire(a.api).await;
    assert_eq!(code, 200, "{again}");
    assert_eq!(again, retired, "a second retire changed the status");
    assert_eq!(status(a.api).await, retired);

    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    let reached = Client::connect(b.reapi, b.worker).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert_eq!(daemon.epoch, lease.term, "the epoch was kept");
    assert!(
        daemon.heartbeat(&[lease]).await,
        "the other frontend did not keep the lease"
    );
    assert!(
        !done_within_quiet(&mut ops).await,
        "the caller was answered before the result"
    );
    let result = support::output(&reached, "built across a retire", 0).await;
    let ack = daemon.report(ran(Some(lease), &result)).await;
    assert!(ack.accepted, "the other frontend did not take the lease");
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));

    daemon.no_work().await;
    assert_eq!(pair.count("done/"), 1);
    assert_eq!((pair.count("op/"), pair.count("lease/")), (0, 0));
    drop((ops, daemon, reached, client));
    pair.stop().await;
}

/// Asserts that a retired frontend is not ready and refuses a new Execute `UNAVAILABLE`
/// without queueing it (the store holds no queue row and no operation), while the other
/// frontend of the store stays ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retired_frontend_is_not_ready_and_refuses_a_new_execute() {
    let pair = Pair::start("retire-refuses").await;
    let (a, b) = (&pair.a, &pair.b);
    let client = Client::connect(a.reapi, a.worker).await;
    let job = Job::new("refused by a retired frontend", &[]);
    client.upload(&job.blobs()).await;
    assert_eq!(readyz(a.api).await, 200);

    let (code, retired) = retire(a.api).await;
    assert_eq!(code, 200, "{retired}");
    assert_eq!(retired["open_streams"], 0, "{retired}");
    let (code, ready) = get(a.api, "/readyz").await;
    assert_eq!(code, 503, "{ready}");
    assert!(failing(&ready).contains(&"stopping"), "{ready}");
    assert_eq!(readyz(b.api).await, 200, "the other frontend");
    let refused = execute_refused(&client, &job).await;
    assert_eq!(refused, Some(Code::Unavailable));
    assert_eq!((pair.count("queue/"), pair.count("op/")), (0, 0));

    drop(client);
    pair.stop().await;
}

/// Asserts that a retired frontend takes no more work in: an operation another frontend
/// queues before the retire is taken in by the first (its queue shows it), one queued
/// after is not (its queue is as it was, the store holds the row), and both run on a
/// daemon of the other frontend, once each.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retired_frontend_takes_no_more_work_in_from_the_store() {
    let pair = Pair::start("retire-intake").await;
    let (a, b) = (&pair.a, &pair.b);
    let client = Client::connect(b.reapi, b.worker).await;
    let first = Job::new("queued before the retire", &[]);
    let second = Job::new("queued after the retire", &[]);
    client.upload(&first.blobs()).await;
    client.upload(&second.blobs()).await;
    let mut early = client.execute(&first.action).await;
    until("the first frontend takes the operation in", async || {
        queued(a.api).await == 1
    })
    .await;

    let (code, retired) = retire(a.api).await;
    assert_eq!(code, 200, "{retired}");
    let mut late = client.execute(&second.action).await;
    until("the second operation is queued", async || {
        pair.count("queue/") == 2
    })
    .await;
    // Several intake polls of the first frontend.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(queued(a.api).await, 1, "the retired frontend took work in");
    assert_eq!(queued(b.api).await, 2);

    let mut daemon = client.daemon("node-a", 4, 8).await;
    let result = support::output(&client, "run on the frontend that is left", 0).await;
    for _ in 0..2 {
        let start = daemon.start().await;
        let ack = daemon.report(ran(start.lease_id, &result)).await;
        assert!(ack.accepted, "{ack:?}");
    }
    assert_eq!(
        response(&done(&mut early).await).result,
        Some(result.clone())
    );
    assert_eq!(response(&done(&mut late).await).result, Some(result));
    assert_eq!(pair.count("done/"), 2);
    assert_eq!((pair.count("queue/"), pair.count("lease/")), (0, 0));
    daemon.no_work().await;

    drop((early, late, daemon, client));
    pair.stop().await;
}

/// Asserts that `POST` needs the operator token (401 without it) and takes no body but
/// `{}` (400), that neither retires the frontend, that the status of an idle frontend
/// counts nothing open, and that `{}` retires it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retire_needs_the_token_and_takes_no_body() {
    let pair = Pair::start("retire-gates").await;
    let a = &pair.a;
    let (code, refused) = call(a.api, "POST", "", "").await;
    assert_eq!(code, 401, "{refused}");
    let (code, malformed) = call(a.api, "POST", &bearer(), r#"{"deadline_secs": 5}"#).await;
    assert_eq!(code, 400, "{malformed}");
    assert_eq!(
        status(a.api).await,
        json!({"retired": false, "open_streams": 0, "open_sessions": 0})
    );
    assert_eq!(readyz(a.api).await, 200, "a refused POST retired");

    let (code, retired) = call(a.api, "POST", &bearer(), "{}").await;
    assert_eq!(code, 200, "{retired}");
    assert_eq!(retired["retired"], true, "{retired}");
    pair.stop().await;
}

/// Asserts that a frontend with no state store answers both routes `409`, saying it is not
/// a frontend over a state store, and stays ready: it has no other frontend to hand its
/// work to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_no_state_store_cannot_retire() {
    let node = Node::start().await;
    let scratch = Scratch::new("retire-no-store");
    let storage = format!("--storage=http://{}", node.addr);
    let given = flags(&[&storage, &scratch.token_flag()]).expect("flags");
    let front = Front::start(&given).await;

    let (code, refused) = retire(front.api).await;
    assert_eq!(code, 409, "{refused}");
    let said = refused["error"].as_str().unwrap_or_default();
    assert!(
        said.contains("not a frontend over a state store"),
        "{refused}"
    );
    let (code, read) = call(front.api, "GET", "", "").await;
    assert_eq!(code, 409, "{read}");
    assert_eq!(readyz(front.api).await, 200, "the refusal retired it");
}
