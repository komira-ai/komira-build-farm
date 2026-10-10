//! The log line of a blob call the worker listener refuses (`kbf_server::blobs`), in
//! a test binary of its own: the log is read through a global subscriber, which no
//! other test's events can reach or race.

use std::future::pending;
use std::io::Write;
use std::sync::{Arc, Mutex};

use clap::Parser;
use kbf_front::Cache;
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_server::{Args, bind_server};
use tonic::Code;
use tonic::transport::Endpoint;

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

/// Catches: a refused blob call that leaves no trace in the server's log (an operator
/// would not see a denied or misconfigured daemon fail its blob calls), one logged
/// below WARN or under another target, and one that does not say which call was
/// refused, with what code and why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_blob_call_is_logged_as_a_warning_with_its_call_code_and_reason() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    // A plain-text worker listener: every blob call is refused.
    let args = Args::parse_from([
        "kbf-server",
        "--listen",
        "127.0.0.1:0",
        "--worker-listen",
        "127.0.0.1:0",
    ]);
    let listeners = args.listeners().expect("listeners");
    let bound = bind_server(Arc::new(Cache::memory()), listeners, pending()).expect("bind");
    let worker = bound.worker;
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    let channel = Endpoint::from_shared(format!("http://{worker}"))
        .expect("endpoint")
        .connect_lazy();
    let refused = ByteStreamClient::new(channel)
        .read(ReadRequest {
            resource_name: format!("blobs/{}/0", "0".repeat(64)),
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), Code::Unauthenticated);

    let text = String::from_utf8(std::mem::take(&mut *log.0.lock().expect("log"))).expect("UTF-8");
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("blob call refused"))
        .collect();
    let [line] = lines.as_slice() else {
        panic!("one refusal line expected: {text}");
    };
    for part in [
        "WARN kbf_server::blobs: blob call refused",
        "call=\"Read\"",
        "code=Unauthenticated",
        "need mutual TLS",
    ] {
        assert!(line.contains(part), "{part:?} missing from {line}");
    }
}
