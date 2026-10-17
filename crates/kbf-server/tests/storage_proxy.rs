//! `--storage`: the REAPI listener proxies a client's CAS, ByteStream and action-cache
//! calls to a storage node (`kbf_front::Proxy`), and the node, not this server's own
//! cache, ends up holding what was written.

mod support;

use std::future::pending;
use std::sync::Arc;

use clap::Parser;
use kbf_auth::Policy;
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::reapi::FindMissingBlobsRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_server::{
    Args, Bound, BoundStorage, ConfigError, StorageListener, bind_server_with_storage, bind_storage,
};
use support::{Client, Job, PROMPT};
use tonic::Request;

fn args(extra: &[&str]) -> Args {
    let mut argv = vec![
        "kbf-server",
        "--listen",
        "127.0.0.1:0",
        "--worker-listen",
        "127.0.0.1:0",
    ];
    argv.extend_from_slice(extra);
    Args::parse_from(argv)
}

/// Catches: a `--storage` flag that is parsed but does not select the proxy (the blob
/// would land in the server's own cache), a blob read back from the wrong place, and a
/// FindMissing answered by the local cache. Without the flag the server answers itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_flag_sends_client_calls_to_the_node() {
    let node_cache = Arc::new(Cache::memory());
    let store: Arc<dyn kbf_storage::Storage> = Arc::new(CacheStorage::new(
        Arc::clone(&node_cache),
        SegmentsConfig::DEFAULT,
    ));
    let BoundStorage { addr, serving } = bind_storage(
        Arc::clone(&store),
        StorageListener {
            listen: "127.0.0.1:0".parse().unwrap(),
            tls: None,
            service: kbf_storage::service::Config {
                write_bytes_in_flight: 64 << 20,
            },
        },
        pending(),
    )
    .expect("bind the node");
    tokio::spawn(async move { serving.await.expect("serve the node") });

    for (flags, lands_on_node) in [
        (vec!["--storage", &format!("http://{addr}")], true),
        (vec![], false),
    ] {
        // A blob of its own per run: the node keeps what the first run wrote.
        let blob = format!("written through the frontend, proxy: {lands_on_node}").into_bytes();
        let digest = kbf_segments::sha256(&blob);
        let wire = kbf_front::digest_to_proto(&digest);
        let args = args(&flags);
        let local = Arc::new(Cache::memory());
        let Bound { reapi, serving, .. } = bind_server_with_storage(
            Arc::clone(&local),
            args.listeners().expect("listeners"),
            None,
            Policy::allow_all(),
            args.storage().expect("the flag"),
            pending(),
        )
        .expect("bind the frontend");
        tokio::spawn(async move { serving.await.expect("serve the frontend") });

        let url = format!("http://{reapi}");
        let mut bytestream = ByteStreamClient::connect(url.clone())
            .await
            .expect("connect");
        let name = format!("uploads/u/blobs/{}/{}", digest.hash_hex(), blob.len());
        let write = WriteRequest {
            resource_name: name,
            data: blob.clone(),
            finish_write: true,
            ..Default::default()
        };
        let done = bytestream
            .write(Request::new(futures::stream::iter([write])))
            .await
            .expect("write");
        assert_eq!(done.into_inner().committed_size, blob.len() as i64);
        let on_node = store
            .find_missing(&[digest])
            .await
            .expect("node")
            .is_empty();
        let on_local = local
            .find_missing(&[digest])
            .await
            .expect("local")
            .is_empty();
        assert_eq!(
            (on_node, on_local),
            (lands_on_node, !lands_on_node),
            "proxy: {lands_on_node}"
        );

        let read = ReadRequest {
            resource_name: format!("blobs/{}/{}", digest.hash_hex(), blob.len()),
            ..Default::default()
        };
        let mut stream = bytestream.read(read).await.expect("read").into_inner();
        let mut got = Vec::new();
        while let Some(m) = stream.message().await.expect("message") {
            got.extend(m.data);
        }
        assert_eq!(got, blob);
        let mut cas = ContentAddressableStorageClient::connect(url)
            .await
            .expect("connect");
        let ask = FindMissingBlobsRequest {
            blob_digests: vec![wire.clone()],
            ..Default::default()
        };
        let missing = cas
            .find_missing_blobs(ask)
            .await
            .expect("find missing")
            .into_inner();
        assert!(missing.missing_blob_digests.is_empty());
    }
}

/// Catches: a `--storage` value that is not a URL starting a server that cannot reach
/// its node (the error must name the flag), and the flag set when it was not given.
#[test]
fn the_storage_flag_is_a_url() {
    assert!(args(&[]).storage().expect("no flag").is_none());
    let node = args(&["--storage", "http://127.0.0.1:8990"])
        .storage()
        .expect("a url");
    assert_eq!(
        node.expect("given").uri().to_string(),
        "http://127.0.0.1:8990/"
    );
    let e = args(&["--storage", "not a url"])
        .storage()
        .expect_err("refused");
    assert!(matches!(e, ConfigError::Storage { .. }), "{e:?}");
    assert!(
        e.to_string().starts_with("--storage \"not a url\": "),
        "{e}"
    );
}

/// Catches (komira-build-farm#791 with `--storage`): proxied REAPI routes built without
/// the progress ticker, so an Execute stream waiting for a worker sends nothing and a
/// load balancer's idle timeout cuts it. The action is in the frontend's own cache (the
/// scheduler still reads there); the node is unreachable and never asked. With
/// `--progress-interval-secs 1` the idle stream is sent the same operation again, not
/// done. Mutant: `routes_proxied` without `with_progress`: the read times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proxied_execute_stream_is_sent_progress() {
    let args = args(&[
        "--storage",
        "http://127.0.0.1:9",
        "--progress-interval-secs",
        "1",
    ]);
    let local = Arc::new(Cache::memory());
    let job = Job::new("idle with progress, proxied", &[]);
    let blobs = [&job.action, &job.command, &job.root, &job.input];
    local
        .store_blobs(
            blobs
                .iter()
                .map(|b| {
                    kbf_front::VerifiedBlob::new(b.digest, b.data.clone().into()).expect("digest")
                })
                .collect(),
        )
        .await
        .expect("store");
    let Bound {
        reapi,
        worker,
        serving,
        ..
    } = bind_server_with_storage(
        local,
        args.listeners().expect("listeners"),
        None,
        Policy::allow_all(),
        args.storage().expect("the flag"),
        pending(),
    )
    .expect("bind the frontend");
    tokio::spawn(async move { serving.await.expect("serve the frontend") });

    let client = Client::connect(reapi, worker).await;
    let mut executed = client.execute(&job.action).await;
    let mut seen = Vec::new();
    while seen.len() < 3 {
        let op = tokio::time::timeout(PROMPT, executed.message())
            .await
            .expect("an update in time (no progress ticker on the proxied routes?)")
            .expect("healthy")
            .expect("an update");
        assert!(!op.done, "{op:?}");
        seen.push(op.name);
    }
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
}
