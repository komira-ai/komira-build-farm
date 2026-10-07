//! The `kbf-server` binary with `--store=s3` against a real S3 store: an Execute runs
//! on a fake daemon, its result is served from the action cache, and the bytes are in
//! the bucket.
//!
//! Ignored by default, because it needs a store. CI's `objstore-s3` job runs it against
//! MinIO and RustFS: `cargo test -p kbf-server --test s3 -- --ignored`, with
//! `KBF_TEST_S3_ENDPOINT` naming the store and the standard `AWS_ACCESS_KEY_ID` and
//! `AWS_SECRET_ACCESS_KEY` holding its throwaway key pair.

mod support;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{Capabilities, KeyPrefix, ObjectStore, PageSize};
use support::{Client, Job, done, output, ran, response};

const BUCKET: &str = "kbf-server-e2e";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for this test"))
}

/// Kills the server when the test ends, pass or fail.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn address(line: &str, key: &str) -> SocketAddr {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} in {line:?}"))
        .parse()
        .expect("an address")
}

/// Catches: `--store=s3` flags that do not reach the cache (blobs kept in memory, or
/// nowhere), and a server that cannot run the Execute join on an S3 store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an S3 store; see the module docs"]
async fn executes_with_blobs_in_s3() {
    let endpoint = env("KBF_TEST_S3_ENDPOINT");
    let store = S3Store::new(S3Config {
        endpoint: endpoint.clone(),
        region: "us-east-1".to_owned(),
        bucket: BUCKET.to_owned(),
        credentials: Credentials::new(env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
    })
    .expect("store");
    store.create_bucket().await.expect("create the bucket");

    let mut child = Command::new(env!("CARGO_BIN_EXE_kbf-server"))
        .args([
            "--store",
            "s3",
            "--s3-endpoint",
            &endpoint,
            "--s3-bucket",
            BUCKET,
        ])
        .args(["--s3-prefix", "e2e/", "--s3-conditional-put"])
        .args(["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"])
        .args(["--heartbeat-interval-ms", "100"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn kbf-server");
    let stdout = child.stdout.take().expect("stdout");
    let _server = Server(child);
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read the start line");
    let client = Client::connect(address(&line, "reapi="), address(&line, "worker=")).await;

    let mut daemon = client.daemon("node-s3", 4, 8).await;
    let job = Job::new("on s3", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&client, "an output in the bucket", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    assert_eq!(client.cached(&job.action).await, Ok(result));

    let prefix = KeyPrefix::new("e2e/").expect("prefix");
    let page = store
        .list(&prefix, None, PageSize::new(10).expect("page size"))
        .await
        .expect("list");
    assert!(!page.objects.is_empty(), "no blob reached the bucket");
}
