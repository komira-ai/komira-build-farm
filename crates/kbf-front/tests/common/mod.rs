//! Helpers shared by the kbf-front tests: a cache served over loopback gRPC in this
//! process, tonic clients for it, and blob builders.

#![allow(dead_code)]

pub mod faulty;

use std::mem;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use kbf_front::{
    Cache, Closing, Dispatch, MAX_MESSAGE_BYTES, MemoryMetaLog, MetaLog, MetaLogError,
};
use kbf_meta::{Applied, BlobAnswer, Command, Location, MetaState, Retention};
use kbf_objstore::{ByteRange, Capabilities, KeyPrefix, MemoryStore, ObjectStore};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{self, BatchUpdateBlobsRequest, FindMissingBlobsRequest};
use kbf_types::{Digest, DigestFunction, FarmTime};
use tonic::service::Routes;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};

pub const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Farm time `n` days after the epoch.
pub fn days(n: u32) -> FarmTime {
    FarmTime::from_millis(0).saturating_add(DAY.saturating_mul(n))
}

/// A [`MemoryMetaLog`] that can run extra commands just before the next touch commits,
/// to play a collection that wins the race between a read and its touch.
#[derive(Debug)]
pub struct RaceLog {
    inner: MemoryMetaLog,
    before_touch: Mutex<Vec<Command>>,
}

impl RaceLog {
    fn new() -> Self {
        Self {
            inner: MemoryMetaLog::new(Retention::default()),
            before_touch: Mutex::new(Vec::new()),
        }
    }

    /// Commits `commands` just before the next `Command::Touch`.
    pub fn before_next_touch(&self, commands: Vec<Command>) {
        *self.lock() = commands;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Command>> {
        self.before_touch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl MetaLog for RaceLog {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        if matches!(command, Command::Touch(_)) {
            let racing = mem::take(&mut *self.lock());
            for c in racing {
                self.inner.commit(c).await?;
            }
        }
        self.inner.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.inner.query(f).await
    }
}

pub type TestCache = Cache<RaceLog, MemoryStore>;

/// A cache served on a loopback port by this process, and a channel to it.
pub struct Farm {
    pub cache: Arc<TestCache>,
    channel: Channel,
}

impl Farm {
    pub async fn start() -> Self {
        Self::serve(kbf_front::routes).await
    }

    /// The cache services and `Execution` over `dispatch`, whose streams are never
    /// closed (the closer is dropped).
    pub async fn with_execution<D: Dispatch>(dispatch: Arc<D>) -> Self {
        let (_, closing) = kbf_front::closing();
        Self::with_execution_until(dispatch, closing).await
    }

    /// The cache services and `Execution` over `dispatch`, whose streams end when
    /// `closing`'s closer closes.
    pub async fn with_execution_until<D: Dispatch>(dispatch: Arc<D>, closing: Closing) -> Self {
        Self::serve(|cache| kbf_front::routes_with_execution(cache, dispatch, closing)).await
    }

    async fn serve(routes: impl FnOnce(Arc<TestCache>) -> Routes) -> Self {
        let cache = Arc::new(Cache::new(
            RaceLog::new(),
            MemoryStore::new(Capabilities::default()),
            KeyPrefix::default(),
        ));
        let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let addr = incoming.local_addr().expect("local address");
        let routes = routes(Arc::clone(&cache));
        tokio::spawn(async move {
            Server::builder()
                .add_routes(routes)
                .serve_with_incoming(incoming)
                .await
                .expect("serve");
        });
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        Self { cache, channel }
    }

    pub fn caps(&self) -> CapabilitiesClient<Channel> {
        CapabilitiesClient::new(self.channel.clone())
    }

    pub fn cas(&self) -> ContentAddressableStorageClient<Channel> {
        ContentAddressableStorageClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    pub fn bytestream(&self) -> ByteStreamClient<Channel> {
        ByteStreamClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    pub fn exec(&self) -> ExecutionClient<Channel> {
        ExecutionClient::new(self.channel.clone())
    }

    pub fn ac(&self) -> ActionCacheClient<Channel> {
        ActionCacheClient::new(self.channel.clone())
    }

    /// FindMissingBlobs over gRPC.
    pub async fn find_missing(&self, digests: &[&Blob]) -> Vec<Digest> {
        self.cas()
            .find_missing_blobs(FindMissingBlobsRequest {
                blob_digests: digests.iter().map(|b| b.proto.clone()).collect(),
                ..Default::default()
            })
            .await
            .expect("FindMissingBlobs")
            .into_inner()
            .missing_blob_digests
            .iter()
            .map(|d| {
                let text = format!("{}/{}", d.hash, d.size_bytes);
                Digest::parse(DigestFunction::Sha256, &text).expect("a canonical digest")
            })
            .collect()
    }

    /// Uploads `blobs` in one BatchUpdateBlobs call (so into one segment) and checks
    /// every blob was stored.
    pub async fn upload(&self, blobs: &[&Blob]) {
        let response = self
            .cas()
            .batch_update_blobs(BatchUpdateBlobsRequest {
                requests: blobs
                    .iter()
                    .map(|b| reapi::batch_update_blobs_request::Request {
                        digest: Some(b.proto.clone()),
                        data: b.data.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .await
            .expect("BatchUpdateBlobs")
            .into_inner();
        for r in response.responses {
            assert_eq!(
                r.status.expect("status").code,
                0,
                "upload of {:?}",
                r.digest
            );
        }
    }

    /// Deletes the store object that holds `blob`, as an operator or a broken disk
    /// might. The index does not know until a read finds the object gone.
    pub async fn delete_object_of(&self, blob: &Blob) {
        let d = blob.digest;
        let answer = self
            .cache
            .meta()
            .query(move |s| s.blob(&d))
            .await
            .expect("query");
        let BlobAnswer::Present(location) = answer else {
            panic!("{d} is not present: {answer:?}");
        };
        let key = self.cache.object_key(location.object()).expect("key");
        self.cache.objects().delete(&key).await.expect("delete");
    }

    /// Replaces the store object that holds `blob` with the same bytes but one bit of
    /// `blob`'s record flipped, as a broken disk might. The object's length and footer
    /// are unchanged, so only a read that hashes what it got can tell.
    pub async fn corrupt_object_of(&self, blob: &Blob) {
        let d = blob.digest;
        let answer = self
            .cache
            .meta()
            .query(move |s| s.blob(&d))
            .await
            .expect("query");
        let BlobAnswer::Present(location) = answer else {
            panic!("{d} is not present: {answer:?}");
        };
        let offset = match location {
            Location::Segment { offset, .. } => offset,
            Location::Object(_) => 0,
        };
        let key = self.cache.object_key(location.object()).expect("key");
        let objects = self.cache.objects();
        // A range past the end reads what exists: the whole object.
        let whole = ByteRange::new(0, u64::from(u32::MAX)).expect("range");
        let mut bytes = objects.get_range(&key, whole).await.expect("get").to_vec();
        let at = usize::try_from(offset).expect("offset");
        assert!(at < bytes.len(), "{d} at {offset} in {} bytes", bytes.len());
        bytes[at] ^= 0x01;
        objects.delete(&key).await.expect("delete");
        objects
            .put_new(&key, Bytes::from(bytes), None)
            .await
            .expect("put");
    }
}

/// A blob: its bytes and its digest in both forms.
#[derive(Clone, Debug)]
pub struct Blob {
    pub data: Vec<u8>,
    pub digest: Digest,
    pub proto: reapi::Digest,
}

impl Blob {
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        let data = data.into();
        let digest = kbf_segments::sha256(&data);
        let proto = reapi::Digest {
            hash: digest.hash_hex(),
            size_bytes: i64::try_from(digest.size_bytes).expect("size"),
        };
        Self {
            data,
            digest,
            proto,
        }
    }

    /// The protobuf encoding of `message` as a blob.
    pub fn of(message: &impl prost::Message) -> Self {
        Self::new(message.encode_to_vec())
    }
}

/// `len` deterministic pseudo-random bytes (xorshift64*) for seed `seed`.
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    out.truncate(len);
    out
}
