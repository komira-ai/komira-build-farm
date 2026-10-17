//! The release store mounted where the frontend starts (`docs/design/pull-updates.md`
//! section 6): a server given a store serves its upload route on the operator API and
//! `ReleaseService.Fetch` on the worker listener; a server given none answers 503 on the
//! route and the service refuses a fetch.

mod support;

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::update::FetchRequest;
use kbf_proto::update::release_service_client::ReleaseServiceClient;
use kbf_server::release::{Limits, ReleaseStore};
use kbf_server::token::ApiToken;
use kbf_server::{Api, Listeners, bind_server_with_state};
use sha2::{Digest, Sha256};
use support::{HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tonic::Code;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "kbf-server-release-mount-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("a test directory");
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn listeners() -> Listeners {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

struct Running {
    api: SocketAddr,
    worker: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn start(releases: Option<Arc<ReleaseStore>>, dir: &Dir) -> Running {
    let path = dir.0.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: Some(ApiToken::from_file(&path).expect("a usable token")),
        local_cache: None,
        releases,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        kbf_auth::Policy::allow_all(),
        None,
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let (api, worker) = (bound.api.expect("an API address"), bound.worker);
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Running {
        api,
        worker,
        stop: Some(stop),
    }
}

async fn http(addr: SocketAddr, method: &str, path: &str, auth: bool, body: &[u8]) -> u16 {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let token = if auth {
        format!("Authorization: Bearer {TOKEN}\r\n")
    } else {
        String::new()
    };
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n{token}Content-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.expect("write");
    stream.write_all(body).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    String::from_utf8_lossy(&raw)[9..12]
        .parse()
        .expect("a status")
}

/// Catches a release store that is not mounted where the frontend starts: a tarball put
/// on the operator API with the token is fetched back, byte for byte, through the worker
/// listener's `ReleaseService`; the same put without the token is refused, and nothing
/// of it is served.
///
/// Mutants seen red: the router not merged (the PUT is 404); the service not added to the
/// worker server (the fetch is UNIMPLEMENTED).
#[tokio::test]
async fn a_release_put_on_the_api_is_fetched_through_the_worker_listener() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let running = start(Some(store), &dir);
    let tarball = b"a tarball".to_vec();
    let digest = hex::encode(Sha256::digest(&tarball));
    let path = format!("/v1/releases/{COMMIT}/linux-x86_64/kbf");
    assert_eq!(http(running.api, "PUT", &path, false, &tarball).await, 401);
    assert_eq!(http(running.api, "PUT", &path, true, &tarball).await, 201);
    let mut client = ReleaseServiceClient::connect(format!("http://{}", running.worker))
        .await
        .expect("connect");
    let mut stream = client
        .fetch(FetchRequest { sha256: digest })
        .await
        .expect("fetch")
        .into_inner();
    let mut got = Vec::new();
    while let Some(chunk) = stream.message().await.expect("chunk") {
        got.extend_from_slice(&chunk.data);
    }
    assert_eq!(got, tarball);
}

/// Catches a server started without `--releases-dir` answering something other than 503:
/// the upload route says the store is off, and the service refuses a fetch rather than
/// serving an empty answer.
///
/// Mutant seen red: the routes mounted over a store made out of nothing.
#[tokio::test]
async fn without_a_store_the_update_endpoints_say_so() {
    let dir = Dir::new();
    let running = start(None, &dir);
    let path = format!("/v1/releases/{COMMIT}/linux-x86_64/kbf");
    assert_eq!(http(running.api, "PUT", &path, true, b"x").await, 503);
    assert_eq!(
        http(running.api, "GET", "/v1/releases", false, b"").await,
        503
    );
    let mut client = ReleaseServiceClient::connect(format!("http://{}", running.worker))
        .await
        .expect("connect");
    let refused = client
        .fetch(FetchRequest {
            sha256: "0".repeat(64),
        })
        .await
        .expect_err("refused");
    assert_ne!(refused.code(), Code::Ok);
}
