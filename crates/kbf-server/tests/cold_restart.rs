//! A restart as the `kbf-server` binary has it today ([`Cell::cold_restart`]): the old
//! process stopped, a new one over the same bucket with nothing of the old one's
//! memory (issue #156, items 4 and 6).
//!
//! These tests pin what is lost, not what should be: the action cache, the CAS index,
//! cordons and drains are in memory, so a restart forgets them, and every start logs a
//! warning that says so. A change that makes any of it durable turns the matching
//! assertion red and must flip it in the same change.

mod support;

use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use support::{API_TOKEN, Cell, Job, PROMPT, done, output, ran, response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tonic::Code;

/// Catches a cold restart that is not one: a helper that shares the old process's
/// cache (as [`Cell::restart`] does) answers the action-cache entry and the blobs
/// written before, and one that leaves the old process running keeps its daemon's
/// stream open and its cache alive. Also catches a restart that writes under the
/// earlier start's key prefix: the first upload after it replaces an object the old
/// process wrote (the store has no conditional writes, as `--s3-conditional-put` off).
///
/// Today, after a restart, the action cache misses and the CAS reports every blob
/// missing, while their bytes are still in the bucket: the index is in memory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_restart_forgets_the_action_cache_and_the_cas_index() {
    let before = Cell::start().await;
    let mut daemon = before.daemon("node-a", 4, 8).await;
    let job = Job::new("before the restart", &[]);
    before.upload(&job.blobs()).await;
    let mut ops = before.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&before, "ran before the restart", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    assert_eq!(before.cached(&job.action).await, Ok(result));
    assert!(before.holds(&job.input).await);
    let written = before.store.objects().await;
    assert!(!written.is_empty(), "the old process wrote no object");
    let old_cache = Arc::downgrade(&before.cache);

    let after = before.cold_restart().await;

    daemon.closed().await;
    drop(ops);
    let deadline = tokio::time::Instant::now() + PROMPT;
    while old_cache.strong_count() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old process's cache is still held"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        after.cached(&job.action).await,
        Err(Code::NotFound),
        "an action-cache entry survived the restart"
    );
    for blob in job.blobs() {
        assert!(
            !after.holds(blob).await,
            "{:?} survived the restart",
            blob.proto
        );
    }

    // The new process works, and the old objects are still in the bucket, unchanged.
    let mut daemon = after.daemon("node-a", 4, 8).await;
    after.upload(&job.blobs()).await;
    let mut ops = after.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&after, "ran after the restart", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
    let now = after.store.objects().await;
    for (key, bytes) in &written {
        assert_eq!(now.get(key), Some(bytes), "object {key} changed");
    }
    assert!(now.len() > written.len(), "the new process wrote no object");
}

/// Catches a start that does not say its scheduler state starts empty (the warning
/// gone), and a cold restart that keeps the old process's scheduler: cordoned and
/// drained nodes would still be listed, and the cordoned node offered no work.
///
/// Today a restart forgets every cordon and drain: the API lists no node until each
/// daemon registers again, and then lists it serving, and work lands on a node that
/// was cordoned. The server keeps no rollout record: `/v1/rollouts` is not served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_restart_forgets_cordons_and_drains_and_says_so() {
    let before = Cell::start_with_api().await;
    let api = before.api.expect("an API");
    capture();
    let _a = before.daemon("node-a", 4, 8).await;
    let _b = before.daemon("node-b", 4, 8).await;
    assert_eq!(placement(&post(api, "node-a:cordon").await), "cordoned");
    assert_eq!(placement(&post(api, "node-b:drain").await), "drained");

    let after = before.cold_restart().await;

    let api = after.api.expect("an API");
    assert_eq!(get(api, "/v1/nodes").await, (200, json!({ "nodes": [] })));
    assert_eq!(get(api, "/v1/rollouts").await.0, 404);

    let mut a = after.daemon("node-a", 4, 8).await;
    // `Welcome` names the new process's term, which its startup warning carries.
    let warned = lines_with(&[
        "WARN",
        "no cordon, drain, lease or operation",
        "keeps no rollout record",
        &format!("term={}", a.epoch),
    ]);
    assert_eq!(
        warned.len(),
        1,
        "startup warnings: {:?}",
        lines_with(&["WARN"])
    );
    let job = Job::new("on the node cordoned before the restart", &[]);
    after.upload(&job.blobs()).await;
    let _ops = after.execute(&job.action).await;
    a.start().await;
    let _b = after.daemon("node-b", 4, 8).await;
    let (code, nodes) = get(api, "/v1/nodes").await;
    assert_eq!(code, 200);
    let states: Vec<_> = nodes["nodes"]
        .as_array()
        .expect("a node list")
        .iter()
        .map(|n| (n["node_id"].clone(), placement(n)))
        .collect();
    assert_eq!(
        states,
        [
            (json!("node-a"), "serving".to_owned()),
            (json!("node-b"), "serving".to_owned()),
        ]
    );
}

/// A node view's placement state.
fn placement(node: &Value) -> String {
    node["placement"]["state"]
        .as_str()
        .unwrap_or_else(|| panic!("no placement state: {node}"))
        .to_owned()
}

/// Every line logged in this test binary, at info and above.
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Installs the subscriber that writes to [`LOG`], once.
fn capture() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| Sink)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
    });
}

/// The logged lines that contain every one of `parts`.
fn lines_with(parts: &[&str]) -> Vec<String> {
    let log = LOG.lock().unwrap_or_else(PoisonError::into_inner);
    String::from_utf8_lossy(&log)
        .lines()
        .filter(|line| parts.iter().all(|p| line.contains(p)))
        .map(str::to_owned)
        .collect()
}

struct Sink;

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut log = LOG.lock().unwrap_or_else(PoisonError::into_inner);
        log.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `GET path` on the operator API: the status code and the JSON body (`null` if the
/// body is not JSON).
async fn get(api: SocketAddr, path: &str) -> (u16, Value) {
    http(api, "GET", path, "").await
}

/// An operator's `POST /v1/nodes/{target}` with no body: the node as the answer has it.
async fn post(api: SocketAddr, target: &str) -> Value {
    let path = format!("/v1/nodes/{target}");
    let headers =
        format!("Authorization: Bearer {API_TOKEN}\r\nContent-Type: application/json\r\n");
    let (code, body) = http(api, "POST", &path, &headers).await;
    assert_eq!(code, 200, "POST {path}: {body}");
    body
}

/// One HTTP/1.1 request with `headers` (each line ending `\r\n`) and no body.
async fn http(api: SocketAddr, method: &str, path: &str, headers: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: 0\r\n{headers}\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, serde_json::from_str(body).unwrap_or(Value::Null))
}
