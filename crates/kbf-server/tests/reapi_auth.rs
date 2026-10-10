//! The REAPI listener's policy (`--reapi-auth-policy`, `docs/reapi-auth.md`) on a whole
//! server: it reaches the REAPI listener, and only that listener.

mod support;

use std::future::pending;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use clap::Parser;
use futures::future::BoxFuture;
use kbf_auth::{AuthenticationMetadata, Authorizer, Authorizers, Policy};
use kbf_front::Cache;
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;
use kbf_proto::reapi::{GetCapabilitiesRequest, WaitExecutionRequest};
use kbf_server::{Args, Bound, ConfigError, bind_server_with_policy};
use support::{Client, FakeDaemon, Job, hello};
use tonic::{Code, Status};

/// A policy file holding `json`, unique to `name`.
fn policy_file(name: &str, json: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-reapi-auth");
    std::fs::create_dir_all(&dir).expect("a policy directory");
    let path = dir.join(format!("{name}-{}.json", std::process::id()));
    std::fs::write(&path, json).expect("write the policy");
    path
}

fn args(extra: &[&str]) -> Args {
    let mut argv = vec![
        "kbf-server",
        "--listen",
        "127.0.0.1:0",
        "--worker-listen",
        "127.0.0.1:0",
        // The interval the support daemon expects in its Welcome.
        "--heartbeat-interval-ms",
        "100",
    ];
    argv.extend_from_slice(extra);
    Args::parse_from(argv)
}

/// Serves `policy` with the listeners of `args`; returns the REAPI and worker
/// addresses.
fn serve(args: &Args, policy: Policy) -> (std::net::SocketAddr, std::net::SocketAddr) {
    let listeners = args.listeners().expect("listeners");
    let Bound {
        reapi,
        worker,
        serving,
        ..
    } = bind_server_with_policy(
        Arc::new(Cache::memory()),
        listeners,
        None,
        policy,
        pending(),
    )
    .expect("bind");
    tokio::spawn(async move { serving.await.expect("serve") });
    (reapi, worker)
}

async fn capabilities(reapi: std::net::SocketAddr) -> Result<(), Status> {
    CapabilitiesClient::connect(format!("http://{reapi}"))
        .await
        .expect("connect")
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .map(|_| ())
}

/// Catches: a `--reapi-auth-policy` file that is read but not run on the REAPI
/// listener, and a REAPI policy that reaches the worker listener (its streams and its
/// blob calls answer by the worker listener's own rules, whatever the REAPI policy
/// says).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_policy_file_runs_on_the_reapi_listener_alone() {
    let path = policy_file(
        "deny",
        r#"{"authenticationPolicy": {"deny": "no REAPI callers today"}}"#,
    );
    let args = args(&["--reapi-auth-policy", path.to_str().expect("UTF-8 path")]);
    let policy = args.reapi_auth_policy().expect("a policy");
    let (reapi, worker) = serve(&args, policy);

    let e = capabilities(reapi).await.expect_err("refused");
    assert_eq!(
        (e.code(), e.message()),
        (Code::Unauthenticated, "no REAPI callers today")
    );

    // A daemon still registers on the plain-text worker listener...
    FakeDaemon::connect(worker, hello("node-a", 4, 8))
        .await
        .expect("a daemon is welcomed");
    // ...and its blob calls are refused by the worker listener's rule, not the policy.
    let e = ByteStreamClient::connect(format!("http://{worker}"))
        .await
        .expect("connect")
        .read(ReadRequest {
            resource_name: format!("blobs/{}/0", "0".repeat(64)),
            ..Default::default()
        })
        .await
        .expect_err("plain text refuses blob calls");
    assert_eq!(e.code(), Code::Unauthenticated);
    assert!(e.message().contains("mutual TLS"), "{e:?}");
}

/// Catches: a server without `--reapi-auth-policy` that refuses anything (the
/// behaviour of a server with no policy must not change).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_flag_every_call_is_accepted() {
    let args = args(&[]);
    let (reapi, _) = serve(&args, args.reapi_auth_policy().expect("allow all"));
    capabilities(reapi).await.expect("GetCapabilities");
}

/// Catches: an unreadable or invalid policy file that does not stop the start, or
/// whose error does not name the flag, the file and the mistake.
#[test]
fn a_bad_policy_file_is_refused_naming_the_flag_and_the_mistake() {
    let missing = "/nonexistent/kbf-reapi-auth.json";
    let e = args(&["--reapi-auth-policy", missing])
        .reapi_auth_policy()
        .expect_err("missing");
    assert!(matches!(e, ConfigError::PolicyRead { .. }), "{e:?}");
    assert!(
        e.to_string()
            .starts_with(&format!("--reapi-auth-policy: read {missing}: ")),
        "{e}"
    );

    let path = policy_file(
        "invalid",
        r#"{"authenticationPolicy": {"allow": {}}, "actionCache": {"putAuthorizer": {"allow": {}}}}"#,
    );
    let shown = path.display().to_string();
    let e = args(&["--reapi-auth-policy", &shown])
        .reapi_auth_policy()
        .expect_err("invalid");
    assert!(matches!(e, ConfigError::Policy { .. }), "{e:?}");
    let text = e.to_string();
    assert!(
        text.starts_with(&format!(
            "--reapi-auth-policy: {shown}: $.actionCache.putAuthorizer: "
        )),
        "{text}"
    );
}

/// Records the instance name of every question, and allows each.
#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

impl Authorizer for Recording {
    fn authorize<'a>(
        &'a self,
        _metadata: &'a AuthenticationMetadata,
        instance_names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(instance_names.iter().map(|n| (*n).to_owned()));
        Box::pin(std::future::ready(vec![Ok(()); instance_names.len()]))
    }
}

/// Catches: a WaitExecution authorized against anything but the instance its
/// operation was submitted under (the farm's tickets leaving the instance empty, or
/// taking it from the wrong place).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_execution_is_authorized_against_the_operations_instance() {
    let recording = Arc::new(Recording::default());
    let policy = Policy {
        authorizers: Authorizers {
            execute: Arc::clone(&recording) as Arc<dyn Authorizer>,
            ..Authorizers::allow_all()
        },
        ..Policy::allow_all()
    };
    let args = args(&[]);
    let (reapi, worker) = serve(&args, policy);
    let client = Client::connect(reapi, worker).await;
    let job = Job::new("wait for me", &[]);
    client.upload(&job.blobs()).await;
    // `Client::execute` submits under the instance `main`.
    let mut ops = client.execute(&job.action).await;
    let name = ops
        .message()
        .await
        .expect("a stream")
        .expect("an operation")
        .name;
    client
        .exec()
        .wait_execution(WaitExecutionRequest { name })
        .await
        .expect("WaitExecution");
    let seen = recording
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(seen, ["main", "main"]);
}
