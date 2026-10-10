//! The `kbf-cell` binary: its certificates, its daemon and its check, as `m1/run.sh`
//! uses them. The daemon test is the M1 cell in small: a real `kbf-server` core (in
//! this process, memory store) and the harness daemon as a child process over mutual
//! TLS with the certificates `kbf-cell pki` wrote. An Execute runs on the daemon, and
//! the same Execute again is answered from the action cache.

#![cfg(target_os = "linux")]

use std::future::pending;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::Arc;
use std::time::Duration;

use kbf_front::Cache;
use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    Action, BatchUpdateBlobsRequest, Command as ReapiCommand, Digest, Directory, ExecuteRequest,
    ExecuteResponse, batch_update_blobs_request,
};
use kbf_server::{Listeners, WorkerTls, bind_server};
use prost::Message;
use sha2::{Digest as _, Sha256};
use tokio::time::timeout;
use tonic::transport::{Certificate, Channel, Endpoint, Identity, ServerTlsConfig};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-cell");

/// How long an Execute may take, the daemon's connection included.
const PROMPT: Duration = Duration::from_secs(30);

/// A fresh directory for one run of one test.
fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-cell-tests")
        .join(format!("{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

fn cell(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run kbf-cell")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Kills the daemon if a test fails before it stops it.
struct Running(Option<Child>);

impl Running {
    /// Stops the daemon with SIGINT, as `run.sh` does, and returns its exit status.
    fn interrupt(mut self) -> std::process::ExitStatus {
        let mut child = self.0.take().expect("running");
        let sent = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(sent.success());
        child.wait().expect("wait for the daemon")
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: i64::try_from(bytes.len()).expect("small"),
    }
}

/// Uploads an action that writes `out.txt` and returns its digest.
async fn upload_action(reapi: &Channel) -> Digest {
    let command = ReapiCommand {
        arguments: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "echo from the cell > out.txt".to_owned(),
        ],
        output_paths: vec!["out.txt".to_owned()],
        ..ReapiCommand::default()
    }
    .encode_to_vec();
    let root = Directory::default().encode_to_vec();
    let action = Action {
        command_digest: Some(digest_of(&command)),
        input_root_digest: Some(digest_of(&root)),
        ..Action::default()
    }
    .encode_to_vec();
    let requests = [&command, &root, &action]
        .into_iter()
        .map(|bytes| batch_update_blobs_request::Request {
            digest: Some(digest_of(bytes)),
            data: bytes.clone(),
            ..batch_update_blobs_request::Request::default()
        })
        .collect();
    let response = ContentAddressableStorageClient::new(reapi.clone())
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests,
            ..BatchUpdateBlobsRequest::default()
        })
        .await
        .expect("BatchUpdateBlobs")
        .into_inner();
    for r in response.responses {
        assert_eq!(r.status.map_or(0, |s| s.code), 0, "upload refused");
    }
    digest_of(&action)
}

/// Executes `action` (with the cache lookup) and returns the final response.
async fn execute(reapi: &Channel, action: &Digest) -> ExecuteResponse {
    let mut ops = ExecutionClient::new(reapi.clone())
        .execute(ExecuteRequest {
            action_digest: Some(action.clone()),
            ..ExecuteRequest::default()
        })
        .await
        .expect("Execute")
        .into_inner();
    let done = timeout(PROMPT, async {
        loop {
            let op = ops.message().await.expect("stream").expect("an update");
            if op.done {
                return op;
            }
        }
    })
    .await
    .expect("the operation finishes in time");
    let Some(operation::Result::Response(any)) = done.result else {
        panic!("no response: {done:?}");
    };
    ExecuteResponse::decode(any.value.as_slice()).expect("an ExecuteResponse")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read a PEM file")
}

/// Catches: harness certificates the worker listener refuses (for the session or for
/// the blob calls the daemon makes there, its only CAS), a daemon that does not
/// run leases (or runs them without uploading outputs), and a result that does not
/// reach the action cache, so the repeat Execute is not a cache hit. Also a daemon
/// that does not stop cleanly on SIGINT, which would leave `run.sh` a stray process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_runs_an_action_and_its_result_is_cached() {
    let dir = scratch("e2e");
    let pki = dir.join("pki");
    let out = cell(&["pki", "--dir", pki.to_str().expect("UTF-8")]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let cache = Arc::new(Cache::memory());
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(
            read(&pki.join("server.pem")),
            read(&pki.join("server.key")),
        ))
        .client_ca_root(Certificate::from_pem(read(&pki.join("ca.pem"))));
    let listeners = Listeners {
        reapi: SocketAddr::from(([127, 0, 0, 1], 0)),
        worker: SocketAddr::from(([127, 0, 0, 1], 0)),
        worker_tls: Some(WorkerTls {
            server: tls,
            deny_list: None,
        }),
        heartbeat_interval: Duration::from_millis(200),
        hello_wait: Duration::from_secs(5),
        tick: Duration::from_millis(50),
        unservable_wait: Duration::from_secs(300),
        finished_retention: Duration::from_secs(60),
        shutdown_timeout: Duration::from_secs(10),
    };
    let bound = bind_server(cache, listeners, pending()).expect("bind");
    let (reapi_addr, worker_addr) = (bound.reapi, bound.worker);
    tokio::spawn(async move { bound.serving.await.expect("serve") });

    let path = |file: &str| pki.join(file).to_str().expect("UTF-8").to_owned();
    let daemon = Command::new(BIN)
        .args(["daemon", "--server"])
        .arg(format!("https://127.0.0.1:{}", worker_addr.port()))
        .arg("--cas")
        .arg(format!("https://127.0.0.1:{}", worker_addr.port()))
        .args(["--ca-cert", &path("ca.pem")])
        .args(["--cert", &path("client.pem"), "--key", &path("client.key")])
        .arg("--scratch")
        .arg(dir.join("leases"))
        .spawn()
        .expect("spawn the daemon");
    let daemon = Running(Some(daemon));

    let reapi = Endpoint::from_shared(format!("http://{reapi_addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let action = upload_action(&reapi).await;
    let ran = execute(&reapi, &action).await;
    assert!(!ran.cached_result);
    let result = ran.result.expect("a result");
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.output_files.len(), 1);
    assert_eq!(result.output_files[0].path, "out.txt");

    let again = execute(&reapi, &action).await;
    assert!(again.cached_result, "the repeat was not a cache hit");
    assert_eq!(again.result, Some(result));

    let status = daemon.interrupt();
    assert!(status.success(), "the daemon exited with {status}");
}

/// Catches: a daemon that starts (and waits forever) on flags it cannot use, instead
/// of exiting 2 with the reason.
#[test]
fn the_daemon_refuses_bad_flags() {
    let dir = scratch("bad-flags");
    let missing = dir.join("missing.pem");
    let missing = missing.to_str().expect("UTF-8");
    let base = [
        "daemon",
        "--server",
        "https://127.0.0.1:1",
        "--ca-cert",
        missing,
        "--cert",
        missing,
        "--key",
        missing,
        "--scratch",
        missing,
    ];
    for (cas, why) in [
        ("not a uri", "must be an https:// URL"),
        ("http://127.0.0.1:1", "must be an https:// URL"),
        ("https://127.0.0.1:1", "missing.pem"),
    ] {
        let out = cell(&[&base[..], &["--cas", cas]].concat());
        assert_eq!(out.status.code(), Some(2), "{cas}");
        let stderr = text(&out.stderr);
        assert!(stderr.contains(why), "{cas}: {stderr}");
    }
}

/// Catches: `kbf-cell pki` reporting success without writing the files, or hiding a
/// directory it cannot write.
#[test]
fn pki_writes_every_file_or_fails() {
    let dir = scratch("pki");
    let out = cell(&["pki", "--dir", dir.to_str().expect("UTF-8")]);
    assert!(out.status.success());
    for file in [
        "ca.pem",
        "server.pem",
        "server.key",
        "client.pem",
        "client.key",
    ] {
        assert!(read(&dir.join(file)).starts_with("-----BEGIN"), "{file}");
    }

    // A file where the directory should be, and a directory where a file should be.
    let blocked = dir.join("blocked");
    std::fs::write(&blocked, "").expect("plant a file");
    let taken = dir.join("taken");
    std::fs::create_dir_all(taken.join("ca.pem")).expect("plant a directory");
    for target in [blocked, taken] {
        let out = cell(&["pki", "--dir", target.to_str().expect("UTF-8")]);
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out.stderr).contains("kbf-cell: write"), "{target:?}");
    }
}

/// Catches: `kbf-cell check` exiting 0 on a failed rule (so CI would pass a broken
/// cache), or on logs it could not read.
#[test]
fn check_exits_by_the_rule() {
    let dir = scratch("check");
    let first = dir.join("first.log");
    let hits = dir.join("hits.log");
    std::fs::write(&first, "INFO: 3 processes: 1 internal, 2 remote.\n").expect("write");
    std::fs::write(
        &hits,
        "INFO: 3 processes: 1 internal, 2 remote cache hit.\n",
    )
    .expect("write");
    let run = |second: &Path| {
        let mut args = vec!["check", "--tool", "bazel", "--first"];
        args.push(first.to_str().expect("UTF-8"));
        args.push("--second");
        args.push(second.to_str().expect("UTF-8"));
        cell(&args)
    };

    let passed = run(&hits);
    assert_eq!(passed.status.code(), Some(0));
    assert!(text(&passed.stdout).contains("OK Bazel: 2 of 2"));

    let failed = run(&first);
    assert_eq!(failed.status.code(), Some(1));
    assert!(text(&failed.stdout).contains("FAIL Bazel"));

    let unreadable = run(&dir.join("absent.log"));
    assert_eq!(unreadable.status.code(), Some(2));
    assert!(text(&unreadable.stderr).contains("absent.log"));
}
