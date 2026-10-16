//! The native daemon's start against a real server (issue #164, review of PR #173):
//! it says `Hello` without waiting for its first survey of the Xcodes, the server
//! lists the Xcodes as not surveyed and places nothing that names one, and the survey's
//! result reaches the server, and placement, without a restart.

use std::future::pending;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_daemon::Event;
use kbf_daemon::cas::Cas as _;
use kbf_driver_native::network::Isolation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, ActionResult, Command, Digest, Directory, ExecuteRequest, Platform,
};
use kbf_server::{Api, Listeners, WorkerTls, bind_server_with_api};
use kbf_transfer::{TransferConfig, Transfers};
use prost::Message as _;
use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tonic::transport::{Certificate, Channel, Endpoint, Identity, ServerTlsConfig};

use super::tests::{fake, scratch, surveyed};

// The native runtime here starts its keepers from the running binary: this test binary,
// whose `main` (the one that calls `kbf_daemon::keeper::dispatch`) does not run.
kbf_daemon::keeper_test_hook!();
use super::{Cli, native_config, native_with};
use clap::Parser as _;

/// How long the test waits for what should happen promptly.
const PROMPT: Duration = Duration::from_secs(30);

/// How long the daemon may take from its start to the server's Welcome. Its survey is
/// held for 20 s, so a daemon that waits for it takes 20 s at least.
const HELLO_WITHIN: Duration = Duration::from_secs(2);

/// A CA, a client certificate for node `mac-1` (`node.pem`, `node.key`: the server
/// takes the node id from its one DNS name) and a server certificate for `localhost`,
/// the client's files written into `dir`; returns the server's TLS.
fn pki(dir: &Path) -> ServerTlsConfig {
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    let leaf = |names: Vec<String>| {
        let key = KeyPair::generate().expect("key");
        let cert = CertificateParams::new(names)
            .expect("params")
            .signed_by(&key, &ca)
            .expect("sign");
        (cert.pem(), key.serialize_pem())
    };
    let (node, node_key) = leaf(vec!["mac-1".to_owned()]);
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    std::fs::write(dir.join("node.pem"), node).expect("write");
    std::fs::write(dir.join("node.key"), node_key).expect("write");
    let (server, server_key) = leaf(vec!["localhost".to_owned()]);
    ServerTlsConfig::new()
        .identity(Identity::from_pem(server, server_key))
        .client_ca_root(Certificate::from_pem(ca.pem()))
}

/// `GET /v1/nodes`, as JSON.
async fn nodes(api: SocketAddr) -> Value {
    let mut stream = tokio::net::TcpStream::connect(api).await.expect("connect");
    let request = "GET /v1/nodes HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (_, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    serde_json::from_str(body).expect("JSON")
}

/// `GET /v1/nodes` until its one node's software holds `done`, for at most [`PROMPT`].
async fn software_until(api: SocketAddr, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + PROMPT;
    loop {
        let software = nodes(api).await["nodes"][0]["software"].clone();
        if done(&software) {
            return software;
        }
        assert!(Instant::now() < deadline, "never: {software}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until `path` is gone, for at most [`PROMPT`].
async fn gone(path: &Path) {
    let deadline = Instant::now() + PROMPT;
    while path.exists() {
        assert!(Instant::now() < deadline, "still there: {}", path.display());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Stores an action naming `xcode=<build>` that, run by `act` (kbf-testkit's act verbs),
/// writes `ran` to `marker` and prints its `DEVELOPER_DIR`.
async fn action(cas: &Transfers, build: &str, act: &Path, marker: &Path) -> Digest {
    let put = |message: Vec<u8>| async move { cas.put(message).await.expect("upload") };
    // On the Action, where REAPI v2.2 reads it (the Command's field is deprecated).
    let platform = Some(Platform {
        properties: vec![Property {
            name: "xcode".to_owned(),
            value: build.to_owned(),
        }],
    });
    let (act, marker) = (act.display().to_string(), marker.display().to_string());
    let command = Command {
        arguments: [&act, "append", &marker, "ran", ";", "env", "DEVELOPER_DIR"]
            .map(str::to_owned)
            .to_vec(),
        ..Command::default()
    };
    let action = Action {
        command_digest: Some(put(command.encode_to_vec()).await),
        input_root_digest: Some(put(Directory::default().encode_to_vec()).await),
        platform,
        ..Action::default()
    };
    put(action.encode_to_vec()).await
}

/// Executes `action` until its operation is done; returns its result.
async fn execute(reapi: Channel, action: Digest) -> ActionResult {
    use kbf_proto::google::longrunning::operation;
    let mut ops = ExecutionClient::new(reapi)
        .execute(ExecuteRequest {
            action_digest: Some(action),
            skip_cache_lookup: true,
            ..ExecuteRequest::default()
        })
        .await
        .expect("Execute")
        .into_inner();
    let done = loop {
        let op = ops.message().await.expect("stream").expect("an update");
        if op.done {
            break op;
        }
    };
    let Some(operation::Result::Response(any)) = done.result else {
        panic!("no response: {done:?}");
    };
    let response =
        kbf_proto::reapi::ExecuteResponse::decode(any.value.as_slice()).expect("response");
    response.result.expect("a result")
}

/// Catches (issue #164, review of PR #173): the native daemon waiting for its first
/// survey of the Xcodes before `Hello` (the first survey of a CI job took up to
/// 18.5 s, and a node's first start after a reboot pays the same, saying nothing to
/// the server meanwhile); an action that names an Xcode placed before that Xcode was
/// surveyed (here the runtime starts configured with it: that too waits for the
/// survey); the Xcodes missing from `GET /v1/nodes` while not surveyed (the silent
/// removal an Xcode must never get), shown as another state, or raised for attention
/// (nothing for a human to do); and the survey's result never reaching the server, or
/// not without a restart. The Xcode's `xcodebuild` answers only once the test lets it
/// (and fails after 20 s, so a red run ends). The action runs kbf-testkit's act verbs,
/// through a link to this test binary. The xcrun warm-up the first survey starts is
/// held until the action has run, and the scratch removed only once it has ended (a
/// clean that raced it failed with `NotFound`, issue #482).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_says_hello_before_its_first_survey_ends() {
    let dir = scratch("startup");
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    let listeners = Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: Some(WorkerTls {
            server: pki(&dir),
            deny_list: None,
        }),
        heartbeat_interval: Duration::from_millis(100),
        hello_wait: Duration::from_secs(2),
        tick: Duration::from_millis(50),
        unservable_wait: Duration::from_secs(300),
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: Duration::from_secs(60),
        shutdown_timeout: Duration::from_secs(10),
        timing: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamLimits::DEFAULT,
    };
    let api = Api {
        listen: loopback,
        token: None,
        local_cache: None,
    };
    let cache = Arc::new(kbf_front::Cache::memory());
    let bound = bind_server_with_api(cache, listeners, Some(api), pending()).expect("bind");
    let (reapi, worker, api) = (bound.reapi, bound.worker, bound.api.expect("the API"));
    tokio::spawn(async move { bound.serving.await.expect("serve") });

    let go = dir.join("go");
    let apps = dir.join("Applications");
    let developer_dir = apps.join("Xcode_1.app/Contents/Developer");
    let wait = format!("wait-for {} 20", go.display());
    fake(
        &developer_dir.join(kbf_driver_native::xcode::XCODEBUILD),
        &format!("-version\t-\t{wait} ; echo Build version 1A1\n*\t-\t{wait}\n"),
    );
    // The xcrun warm-up the first survey starts looks up `cc` first (the survey asks
    // only for `clang`): held on `warm`, it outlasts the action on every run, as it did
    // when the clean below raced it (issue #482).
    let warm = dir.join("warm");
    let xcrun = dir.join("bin/xcrun");
    fake(
        &xcrun,
        &format!(
            "--find cc\t-\twait-for {} 20 ; echo /x/clang\n*\t-\techo /x/clang\n",
            warm.display()
        ),
    );
    let sandbox_exec = dir.join("bin/sandbox-exec");
    fake(&sandbox_exec, "");
    let act = dir.join("bin").join(kbf_testkit::act::NAME);
    kbf_testkit::act::link_self(&act).expect("link");
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let cli = Cli::try_parse_from([
        "kbf-daemon".to_owned(),
        format!("--server=https://127.0.0.1:{}", worker.port()),
        "--tls-server-name=localhost".to_owned(),
        flag("ca-cert", &dir.join("ca.pem")),
        flag("cert", &dir.join("node.pem")),
        flag("key", &dir.join("node.key")),
        "--node-id=mac-1".to_owned(),
        "--reconnect-ms=50".to_owned(),
        "--driver=native".to_owned(),
        format!("--cas=https://127.0.0.1:{}", worker.port()),
        flag("data-dir", &dir),
        flag("xcode-apps", &apps),
    ])
    .expect("flags");
    let mut config = native_config(&cli).expect("config");
    config.user_folders = None;
    config.isolation = Isolation::Sandbox(sandbox_exec);
    let real_developer_dir = std::fs::canonicalize(&developer_dir).expect("real");
    config.xcodes = [("1A1".to_owned(), real_developer_dir.clone())].into();

    let started = Instant::now();
    let (daemon, mut reports) = native_with(&cli, config, &xcrun).expect("the native daemon");
    let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(daemon.with_events(events).run(pending()));
    let mut before = Vec::new();
    let welcomed = tokio::time::timeout(PROMPT, async {
        loop {
            let event = seen.recv().await.expect("the daemon runs");
            if matches!(event, Event::Welcomed { .. }) {
                break;
            }
            before.push(format!("{event:?}"));
        }
    });
    let welcomed = welcomed.await;
    assert!(welcomed.is_ok(), "not welcomed: {before:#?}");
    let hello = started.elapsed();
    eprintln!("startup_tests: welcomed {hello:.2?} after the start, the survey held");
    assert!(hello < HELLO_WITHIN, "Hello waited: {hello:?}");

    let software = software_until(api, |s| !s.is_null()).await;
    assert_eq!(
        software["xcode_builds"],
        serde_json::json!([]),
        "{software}"
    );
    let xcodes = &software["xcodes"];
    assert_eq!(
        xcodes[0]["app"],
        apps.join("Xcode_1.app").display().to_string()
    );
    assert_eq!(xcodes[0]["state"], "not_surveyed", "{software}");
    assert_eq!(
        nodes(api).await["nodes"][0]["needs_attention"],
        serde_json::json!([])
    );

    let channel = Endpoint::from_shared(format!("http://{reapi}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let marker = dir.join("ran");
    let cas = Transfers::over(channel.clone(), TransferConfig::default());
    let named = action(&cas, "1A1", &act, &marker).await;
    let running = tokio::spawn(execute(channel, named));
    // Long enough for the server to place it (it ticks every 50 ms) were it placeable.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!marker.exists(), "placed on an Xcode not surveyed yet");
    assert!(!running.is_finished());

    std::fs::write(&go, "").expect("let the survey answer");
    let report = surveyed(&mut reports).await;
    assert_eq!(report.xcodes[0].build, "1A1", "{report:?}");
    let software = software_until(api, |s| s["xcodes"][0]["state"] == "ready").await;
    assert_eq!(
        software["xcode_builds"],
        serde_json::json!(["1A1"]),
        "{software}"
    );
    let result = tokio::time::timeout(PROMPT, running)
        .await
        .expect("placed once surveyed")
        .expect("execute");
    assert_eq!(result.exit_code, 0);
    assert_eq!(std::fs::read_to_string(&marker).expect("ran"), "ran\n");
    let stdout = cas
        .get(result.stdout_digest.as_ref().expect("stdout"))
        .await
        .expect("stdout in the CAS");
    assert_eq!(
        String::from_utf8_lossy(&stdout).trim_end(),
        real_developer_dir.display().to_string()
    );
    // The clean must not run while the daemon's warm-up does: its own removal of its
    // directory, and its lookups' writes, race the clean's walk (`NotFound`). Its
    // directory was made before the survey's report was sent, and its removal is the
    // warm-up's last step.
    let warm_up = dir.join("leases").join(kbf_driver_native::WARM_UP_DIR);
    std::fs::write(&warm, "").expect("let the warm-up go on");
    gone(&warm_up).await;
    let lookups = std::fs::read_to_string(dir.join("bin/xcrun.log")).expect("xcrun log");
    assert_eq!(
        lookups
            .lines()
            .filter(|l| l.ends_with(" --find ld"))
            .count(),
        2,
        "the warm-up's last lookup, for the node's Xcode and 1A1: {lookups}"
    );
    drop(reports);
    kbf_outputs::remove_tree(&dir).expect("clean");
}
