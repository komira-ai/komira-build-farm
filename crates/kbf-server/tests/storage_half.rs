//! `kbf storage-and-daemon`'s storage half (`kbf_server::storage_half`): the storage
//! node `kbf storage` starts, with the storage it serves handed to the caller and the
//! node's one cache honoured.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures::stream;
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_server::storage_half::start;
use kbf_server::storage_node::{StorageFlags, command, parse};
use kbf_server::{BoundStorage, StorageListener, bind_storage};
use kbf_storage::Storage as _;
use kbf_storage::local_cache::LocalCache;
use kbf_storage::service::RemoteStorage;
use sha2::Digest as _;

fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("storage-half")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("directory");
    dir
}

/// The flags of `kbf storage`, plain text on a loopback port, and these.
fn flags(more: &[String]) -> StorageFlags {
    let mut argv = vec!["storage".to_owned()];
    if !more.iter().any(|m| m.starts_with("--storage-listen")) {
        argv.push("--storage-listen=127.0.0.1:0".to_owned());
    }
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv).expect("flags")).expect("taken")
}

/// A `file://` cold store and a hot store, both in `dir`.
fn file_stores(dir: &Path) -> Vec<String> {
    vec![
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--hot-store=file://{}", dir.join("hot").display()),
    ]
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

/// Catches a handle that is not the storage the listener serves (the daemon half would
/// read and write another store than other nodes reach over gRPC), and a start line that
/// is not `kbf storage`'s: a blob put through the handle is read back over gRPC from the
/// listener, and one written over gRPC is read through the handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handle_is_the_storage_the_listener_serves() {
    let half = start(&flags(&file_stores(&dir("same"))), never)
        .await
        .expect("starts");
    let line = half.start_line().to_owned();
    let want = format!(
        "kbf-storage {} storage={} log=",
        kbf_server::SERVER_VERSION,
        half.addr
    );
    assert!(line.starts_with(&want), "{line}");
    let (storage, addr) = (half.storage(), half.addr);
    tokio::spawn(half.serve());
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let remote = RemoteStorage::new(channel, 0);
    let here = Bytes::from_static(b"put in-process, read over gRPC");
    let digest = storage.put(here.clone()).await.expect("put");
    assert_eq!(remote.read_blob(&digest).await.expect("read"), here);
    let there = Bytes::from_static(b"put over gRPC, read in-process");
    let digest = remote.put(there.clone()).await.expect("put");
    assert_eq!(storage.read_blob(&digest).await.expect("read"), there);
}

/// Catches the node's one cache not given the storage half's bytes (its budget would
/// leave the hot store out): with `local_cache` set, the half attaches to it at start,
/// so a second attach is refused; without one there is nothing to attach to.
#[tokio::test]
async fn the_one_cache_counts_the_storage_halfs_bytes() {
    let mut shared = flags(&file_stores(&dir("one-cache")));
    let local = Arc::new(LocalCache::new());
    shared.server.local_cache = Some(Arc::clone(&local));
    let half = start(&shared, never).await.expect("starts");
    assert!(
        !local.attach_storage(Box::new(|| 0)),
        "the half did not attach"
    );
    drop(half);
}

/// Catches a half that serves past its shutdown future, or never starts serving.
#[tokio::test]
async fn the_half_stops_when_told() {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let half = start(&flags(&file_stores(&dir("stop"))), || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
    .expect("starts");
    let serving = tokio::spawn(half.serve());
    stop.send(()).expect("stop");
    serving.await.expect("task").expect("serves to the end");
}

/// Catches a start whose listener is checked after a store is opened, or not at all: a
/// plain-text listener off loopback is refused as `kbf storage` refuses it, and no store
/// is made.
#[tokio::test]
async fn a_listener_kbf_storage_refuses_is_refused_first() {
    let d = dir("refused");
    let mut wide = file_stores(&d);
    wide.push("--storage-listen=0.0.0.0:0".to_owned());
    let refused = start(&flags(&wide), never).await.err().expect("refused");
    let said = refused.to_string();
    assert!(said.contains("0.0.0.0:0"), "{said}");
    assert!(!d.join("hot").exists(), "a refused start opened a store");
}

/// A storage that tells the test when the service first asks it about a blob: a ByteStream
/// `Write` does that right after it takes the first message off the stream, so the signal
/// means the server has accepted the stream and the write is in flight. Everything else is
/// the inner storage's.
struct Watched {
    inner: Arc<dyn kbf_storage::Storage>,
    asked: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl kbf_storage::Storage for Watched {
    fn find_missing<'a>(
        &'a self,
        digests: &'a [kbf_types::Digest],
    ) -> kbf_storage::Answer<'a, Vec<kbf_types::Digest>> {
        if let Some(asked) = self.asked.lock().expect("lock").take() {
            let _ = asked.send(());
        }
        self.inner.find_missing(digests)
    }
    fn read_blob<'a>(&'a self, digest: &'a kbf_types::Digest) -> kbf_storage::Answer<'a, Bytes> {
        self.inner.read_blob(digest)
    }
    fn read_range<'a>(
        &'a self,
        digest: &'a kbf_types::Digest,
        offset: u64,
        limit: u64,
    ) -> kbf_storage::Answer<'a, kbf_storage::ReadStream> {
        self.inner.read_range(digest, offset, limit)
    }
    fn put(&self, bytes: Bytes) -> kbf_storage::Answer<'_, kbf_types::Digest> {
        self.inner.put(bytes)
    }
    fn stream(&self, size: u64) -> Option<Box<dyn kbf_storage::PutStream>> {
        self.inner.stream(size)
    }
    fn segments(&self) -> Arc<dyn kbf_proto::worker::segments_server::Segments> {
        self.inner.segments()
    }
    fn action_result<'a>(
        &'a self,
        action: &'a kbf_types::Digest,
    ) -> kbf_storage::Answer<'a, Option<(kbf_types::Digest, kbf_proto::reapi::ActionResult)>> {
        self.inner.action_result(action)
    }
    fn prepare_action_result<'a>(
        &'a self,
        result: &'a kbf_proto::reapi::ActionResult,
    ) -> kbf_storage::Answer<'a, kbf_meta::ActionRecord> {
        self.inner.prepare_action_result(result)
    }
    fn commit_action_record(
        &self,
        role: kbf_meta::Role,
        action: kbf_types::Digest,
        record: kbf_meta::ActionRecord,
    ) -> kbf_storage::Answer<'_, ()> {
        self.inner.commit_action_record(role, action, record)
    }
}

/// Catches a stop that cuts a write in flight instead of draining it: a ByteStream write
/// has sent its first half and the server has taken it up (the storage was asked about
/// the blob) when the shutdown future completes; once the runtime has run the stop
/// through the listener, the second half goes, and the write must commit the whole blob,
/// `serving` end only after it, and the blob read back.
///
/// The stop waits for the server, not for the client: a stop that beat the stream's
/// arrival would be refused by the listener, which is right and is not what is tested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_drains_a_write_in_flight() {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (asked, in_flight) = tokio::sync::oneshot::channel::<()>();
    let held: Arc<dyn kbf_storage::Storage> = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let storage: Arc<dyn kbf_storage::Storage> = Arc::new(Watched {
        inner: Arc::clone(&held),
        asked: std::sync::Mutex::new(Some(asked)),
    });
    let BoundStorage { addr, serving } = bind_storage(
        Arc::clone(&storage),
        StorageListener {
            listen: "127.0.0.1:0".parse().expect("address"),
            tls: None,
            service: kbf_storage::service::Config {
                write_bytes_in_flight: 64 << 20,
            },
        },
        async move {
            let _ = stopped.await;
        },
    )
    .expect("binds");
    let serving = tokio::spawn(serving);

    let blob: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
    let (head, tail) = blob.split_at(1024);
    let hash: String = sha2::Sha256::digest(&blob)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let name = format!("uploads/drain/blobs/{hash}/{}", blob.len());
    let first = WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: false,
        data: head.to_vec(),
    };
    let second = WriteRequest {
        resource_name: String::new(),
        write_offset: 1024,
        finish_write: true,
        data: tail.to_vec(),
    };
    // The second request is released once the test says so.
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let requests = stream::unfold(
        (Some(first), Some(second), Some(released)),
        |(first, second, released)| async move {
            if let Some(first) = first {
                return Some((first, (None, second, released)));
            }
            let _ = released?.await;
            let second = second?;
            Some((second, (None, None, None)))
        },
    );
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let writing = tokio::spawn(async move {
        ByteStreamClient::new(channel)
            .write(requests)
            .await
            .map(tonic::Response::into_inner)
    });
    in_flight.await.expect("the server took the write up");
    stop.send(()).expect("stop");
    // Let the runtime run the stop through the listener: a bounded number of turns, not
    // a time. A listener that cut its calls ends `serve` within them.
    for _ in 0..1000 {
        if serving.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(!serving.is_finished(), "serve ended with a write open");
    release.send(()).expect("release");
    let response = writing.await.expect("task").expect("the write drained");
    assert_eq!(response.committed_size, 2048);
    serving.await.expect("task").expect("serves to the end");
    let digest =
        kbf_types::Digest::parse(kbf_types::DigestFunction::Sha256, &format!("{hash}/2048"))
            .expect("digest");
    assert_eq!(held.read_blob(&digest).await.expect("read"), blob);
}
