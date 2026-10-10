//! What the server logs about REAPI callers, in a test binary of its own: the log is
//! read through a global subscriber, which no other test's events can reach or race.

mod support;

use std::future::pending;
use std::io::Write;
use std::sync::{Arc, Mutex};

use clap::Parser;
use kbf_auth::Policy;
use kbf_front::Cache;
use kbf_proto::reapi::ExecuteRequest;
use kbf_server::{Args, bind_server_with_policy};
use support::{Client, Job};
use tonic::Code;

/// Where the global subscriber writes: every line, without colour.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log")).into_owned()
    }

    /// The lines that contain every one of `parts`.
    fn lines_with(&self, parts: &[&str]) -> Vec<String> {
        self.text()
            .lines()
            .filter(|l| parts.iter().all(|p| l.contains(p)))
            .map(str::to_owned)
            .collect()
    }
}

async fn client(policy: &str) -> Client {
    let args = Args::parse_from([
        "kbf-server",
        "--listen",
        "127.0.0.1:0",
        "--worker-listen",
        "127.0.0.1:0",
    ]);
    let listeners = args.listeners().expect("listeners");
    let policy = Policy::from_json(policy).expect("a policy");
    let bound = bind_server_with_policy(
        Arc::new(Cache::memory()),
        listeners,
        None,
        policy,
        pending(),
    )
    .expect("bind");
    let (reapi, worker) = (bound.reapi, bound.worker);
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Client::connect(reapi, worker).await
}

/// Catches: an Execute whose trace does not carry the caller's public metadata and
/// instance (the metadata lost between the authentication layer and the handler); a
/// refused call that leaves no WARN line, or one without its call, instance and
/// caller; an unauthenticated call that is not logged; and the private metadata in
/// any line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_caller_is_on_the_execute_trace_and_on_every_refusal() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let cell = client(
        r#"{
            "authenticationPolicy": {"allow": {
                "public": {"user": "ci-bot"},
                "private": {"secret": "hunter2"}
            }},
            "executeAuthorizer": {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["main"]}}
        }"#,
    )
    .await;
    let job = Job::new("traced", &[]);
    cell.upload(&job.blobs()).await;
    // `Client::execute` submits under the instance `main`, which the policy allows.
    let mut ops = cell.execute(&job.action).await;
    ops.message()
        .await
        .expect("a stream")
        .expect("an operation");

    let e = cell
        .exec()
        .execute(ExecuteRequest {
            instance_name: "other".to_owned(),
            action_digest: Some(job.action.proto.clone()),
            ..Default::default()
        })
        .await
        .expect_err("not under main");
    assert_eq!(e.code(), Code::PermissionDenied);

    let denied = client(r#"{"authenticationPolicy": {"deny": "who are you"}}"#).await;
    let e = denied
        .exec()
        .execute(ExecuteRequest::default())
        .await
        .expect_err("unauthenticated");
    assert_eq!(e.code(), Code::Unauthenticated);

    let caller = r#"caller={"user":"ci-bot"}"#;
    let traced = log.lines_with(&["execute{", "main", caller, "submitted"]);
    assert_eq!(traced.len(), 1, "{}", log.text());

    let refused = log.lines_with(&[" WARN ", "REAPI call refused"]);
    assert_eq!(refused.len(), 1, "{}", log.text());
    for part in [
        "/build.bazel.remote.execution.v2.Execution/Execute",
        "instance=\"other\"",
        r#"public={"user":"ci-bot"}"#,
        "Permission denied",
    ] {
        assert!(refused[0].contains(part), "{part} not in {}", refused[0]);
    }

    let unauthenticated = log.lines_with(&[" WARN ", "REAPI call not authenticated"]);
    assert_eq!(unauthenticated.len(), 1, "{}", log.text());
    assert!(
        unauthenticated[0].contains("who are you"),
        "{}",
        unauthenticated[0]
    );
    assert!(
        unauthenticated[0].contains("/build.bazel.remote.execution.v2.Execution/Execute"),
        "{}",
        unauthenticated[0]
    );

    assert!(!log.text().contains("hunter2"), "{}", log.text());
}
