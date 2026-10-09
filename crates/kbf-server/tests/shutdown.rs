//! Stopping the `kbf-server` binary with open REAPI streams (issue #168): SIGTERM
//! ends open Execute and WaitExecution streams UNAVAILABLE, which clients retry, and
//! the server exits 0 once its REAPI connections close, or when
//! `--shutdown-timeout-secs` runs out.

#![cfg(unix)]

mod support;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::{ExecuteOperationMetadata, WaitExecutionRequest};
use prost::Message;
use support::{Blob, Client, Job, PROMPT};
use tonic::Code;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-server");

/// How long a test waits for the server to print its start line or to exit.
const BOUND: Duration = Duration::from_secs(10);

/// A running server, killed and reaped when dropped, so a failed assertion never
/// leaves it running.
struct Running(Child);

impl Running {
    /// Starts the server with `args` on free loopback ports; it and the REAPI and
    /// worker addresses of its start line.
    fn start(args: &[&str]) -> (Self, SocketAddr, SocketAddr) {
        let mut child = Self(
            Command::new(BIN)
                .args(["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"])
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
            .recv_timeout(BOUND)
            .expect("a start line in time")
            .expect("a start line");
        let addr = |key: &str| -> SocketAddr {
            line.split_whitespace()
                .find_map(|w| w.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} in {line:?}"))
                .parse()
                .expect("an address")
        };
        let (reapi, worker) = (addr("reapi="), addr("worker="));
        (child, reapi, worker)
    }

    fn terminate(&self) {
        let pid = i32::try_from(self.0.id()).expect("pid");
        // SAFETY: kill(2) on the child this test spawned and has not reaped.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    }

    /// Waits up to `within` for the server to exit; panics (and so kills it) if it
    /// has not.
    async fn exited(&mut self, within: Duration) -> ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.0.try_wait().expect("try_wait") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "kbf-server still running {within:?} after SIGTERM"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Catches issue #168: a server that dies on SIGTERM (no handler), or stops serving
/// at once, so an open Execute or WaitExecution stream fails on the client with
/// UNKNOWN (an h2 error) instead of UNAVAILABLE; and one that does not exit 0 promptly
/// once the streams are ended (well within the default 10 s shutdown timeout, so a
/// drain that waits it out fails). No daemon is connected, so the operation waits,
/// with the reason that no live worker can run it, until the signal; the test signals
/// only once it has seen that reason, so the run is the same every time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_ends_open_execute_streams_unavailable() {
    let (mut server, reapi, worker) = Running::start(&[]);
    let client = Client::connect(reapi, worker).await;
    let job = Job::new("open across a SIGTERM", &[]);
    client.upload(&job.blobs()).await;
    let mut executed = client.execute(&job.action).await;
    let queued = loop {
        let op = tokio::time::timeout(PROMPT, executed.message())
            .await
            .expect("an update in time")
            .expect("healthy")
            .expect("an update");
        if waits_for_a_worker(&op) {
            break op;
        }
    };
    let mut waited = client
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: queued.name.clone(),
        })
        .await
        .expect("WaitExecution")
        .into_inner();
    tokio::time::timeout(PROMPT, waited.message())
        .await
        .expect("an update in time")
        .expect("healthy")
        .expect("waiting");

    server.terminate();
    for (what, stream) in [("Execute", &mut executed), ("WaitExecution", &mut waited)] {
        let ended = tokio::time::timeout(PROMPT, stream.message())
            .await
            .unwrap_or_else(|_| panic!("{what}: no end within {PROMPT:?} of SIGTERM"));
        let status = ended.expect_err(what);
        assert_eq!(
            status.code(),
            Code::Unavailable,
            "{what}: {}: {}",
            status.code(),
            status.message()
        );
        assert!(
            status.message().contains("shutting down"),
            "{what}: {status:?}"
        );
    }
    let status = server.exited(PROMPT).await;
    assert!(status.success(), "kbf-server exited with {status}");
}

/// Whether `op` is queued with the reason no live worker can run it.
fn waits_for_a_worker(op: &Operation) -> bool {
    let any = op.metadata.as_ref().expect("metadata");
    let meta = ExecuteOperationMetadata::decode(any.value.as_slice()).expect("metadata");
    meta.partial_execution_metadata
        .is_some_and(|partial| !partial.auxiliary_metadata.is_empty())
}

/// Catches: a stop that waits on a REAPI connection that never closes (here a
/// ByteStream Write whose client sent part of its blob and went quiet) beyond
/// `--shutdown-timeout-secs` (the default is 10 s, past this test's bound, so a flag
/// not handed to the server fails too); and a stop that does not wait for it at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stuck_upload_is_cut_off_at_the_shutdown_timeout() {
    let (mut server, reapi, worker) = Running::start(&["--shutdown-timeout-secs", "1"]);
    let client = Client::connect(reapi, worker).await;
    let blob = Blob::new("a partial upload");
    let (chunks, requests) = mpsc::unbounded();
    chunks
        .unbounded_send(WriteRequest {
            resource_name: format!(
                "uploads/0e4c5a3c-2b7e-4f0a-9b1e-6d3f2a1c0b9d/blobs/{}/{}",
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
    // Nothing tells the client the server has read the first part; the stream has a
    // moment to reach it (on loopback it takes well under a millisecond).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!upload.is_finished(), "the partial upload was answered");
    assert!(!client.holds(&blob).await, "part of a blob is held");

    let stopped = Instant::now();
    server.terminate();
    let status = server.exited(PROMPT).await;
    assert!(status.success(), "kbf-server exited with {status}");
    let took = stopped.elapsed();
    assert!(
        took >= Duration::from_secs(1),
        "stopped after {took:?}, before the 1 s shutdown timeout"
    );
    drop(chunks);
}
