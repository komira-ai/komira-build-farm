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
use kbf_daemon::cas::{Cas as _, CasClient};
use kbf_driver_native::network::Isolation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{Action, Command, Digest, Directory, ExecuteRequest, Platform};
use kbf_server::{Api, Listeners, WorkerTls, bind_server_with_api};
use prost::Message as _;
use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tonic::transport::{Certificate, Channel, Endpoint, Identity, ServerTlsConfig};

use super::tests::{scratch, surveyed};
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

/// Stores an action naming `xcode=<build>` that writes its `DEVELOPER_DIR` to `marker`.
async fn action(cas: &CasClient, build: &str, marker: &Path) -> Digest {
    let put = |message: Vec<u8>| async move { cas.put(message).await.expect("upload") };
    // On the Action, where REAPI v2.2 reads it (the Command's field is deprecated).
    let platform = Some(Platform {
        properties: vec![Property {
            name: "xcode".to_owned(),
            value: build.to_owned(),
        }],
    });
    let script = format!("echo \"$DEVELOPER_DIR\" > '{}'", marker.display());
    let command = Command {
        arguments: ["/bin/sh", "-c", script.as_str()]
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

/// Executes `action` until its operation is done; returns the action's exit code.
async fn execute(reapi: Channel, action: Digest) -> i32 {
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
    response.result.expect("a result").exit_code
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
/// (or after 20 s, so a red run ends).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_says_hello_before_its_first_survey_ends() {
    let dir = scratch("startup");
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    let listeners = Listeners {
        reapi: loopback,
        worker: loopback,
        worker_tls: Some(WorkerTls {
            server: pki(&dir),
            deny_list: None,
        }),
        heartbeat_interval: Duration::from_millis(100),
        hello_wait: Duration::from_secs(2),
        tick: Duration::from_millis(50),
        unservable_wait: Duration::from_secs(300),
        finished_retention: Duration::from_secs(60),
        shutdown_timeout: Duration::from_secs(10),
    };
    let api = Api {
        listen: loopback,
        token: None,
    };
    let cache = Arc::new(kbf_front::Cache::memory());
    let bound = bind_server_with_api(cache, listeners, Some(api), pending()).expect("bind");
    let (reapi, worker, api) = (bound.reapi, bound.worker, bound.api.expect("the API"));
    tokio::spawn(async move { bound.serving.await.expect("serve") });

    let go = dir.join("go");
    let write = |path: &Path, script: &str| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, script).expect("script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    };
    let apps = dir.join("Applications");
    let developer_dir = apps.join("Xcode_1.app/Contents/Developer");
    write(
        &developer_dir.join(kbf_driver_native::xcode::XCODEBUILD),
        &format!(
            "#!/bin/sh\n\
             i=0; while [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.1; i=$((i+1)); done\n\
             case \"$*\" in -version) echo 'Build version 1A1' ;; esac\n",
            go.display()
        ),
    );
    let xcrun = dir.join("bin/xcrun");
    write(&xcrun, "#!/bin/sh\necho /x/clang\n");
    let sandbox_exec = dir.join("bin/sandbox-exec");
    write(&sandbox_exec, "#!/bin/sh\nshift 4\nexec \"$@\"\n");
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
        format!("--cas=http://{reapi}"),
        flag("scratch", &dir.join("leases")),
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
    let named = action(&CasClient::new(channel.clone()), "1A1", &marker).await;
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
    let code = tokio::time::timeout(PROMPT, running)
        .await
        .expect("placed once surveyed")
        .expect("execute");
    assert_eq!(code, 0);
    let ran_with = std::fs::read_to_string(&marker).expect("ran");
    assert_eq!(
        ran_with.trim_end(),
        real_developer_dir.display().to_string()
    );
    drop(reports);
    kbf_outputs::remove_tree(&dir).expect("clean");
}
