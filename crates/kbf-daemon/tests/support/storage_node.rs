//! Storage nodes for the tests of a daemon that moves its blobs to the storage owners:
//! a real cache served as `kbf.storage` and ByteStream over loopback, with a spy in
//! front that counts the reads and can answer `STALE_MAP`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use kbf_front::{Cache, CacheStorage, MemoryMetaLog, SegmentsConfig};
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::MemoryStore;
use kbf_proto::reapi::{ActionResult, Digest as ReapiDigest};
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::service::{Admission, Config, StorageService};
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError, StreamedPut};
use kbf_types::{Digest, DigestFunction};
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

use super::Pki;

/// A storage node's store, counting what is read from it.
pub struct Spy {
    inner: Arc<CacheStorage<MemoryMetaLog, MemoryStore>>,
    /// Ranged reads begun (a whole read of a blob is one).
    pub reads: AtomicUsize,
    /// Blob bytes written to it.
    pub written: Arc<AtomicUsize>,
    /// The next read answers `STALE_MAP` naming this generation, once.
    stale: Mutex<Option<u64>>,
    /// Whether [`Storage::stream`] hands out a stream (a chain's primary needs one; the
    /// real cache offers none yet).
    streams: bool,
}

impl Spy {
    fn new() -> Arc<Self> {
        Self::with(false)
    }

    /// A store that takes a blob as a stream, as a node of a chain needs.
    pub fn streaming() -> Arc<Self> {
        Self::with(true)
    }

    fn with(streams: bool) -> Arc<Self> {
        let cache = Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory());
        Arc::new(Self {
            inner: Arc::new(CacheStorage::new(cache, SegmentsConfig::DEFAULT)),
            reads: AtomicUsize::new(0),
            written: Arc::new(AtomicUsize::new(0)),
            stale: Mutex::new(None),
            streams,
        })
    }

    /// Makes the next read answer `STALE_MAP` naming `generation`.
    pub fn go_stale(&self, generation: u64) {
        *self.stale.lock().expect("lock") = Some(generation);
    }

    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    /// Stores `bytes` in the node directly, as a node that already holds it.
    pub async fn hold(&self, bytes: &[u8]) -> ReapiDigest {
        let stored = self
            .inner
            .put(Bytes::copy_from_slice(bytes))
            .await
            .expect("hold a blob");
        kbf_transfer::proto_digest(&stored)
    }

    pub async fn holds(&self, digest: &ReapiDigest) -> bool {
        let digest = Digest::parse(DigestFunction::Sha256, &kbf_transfer::cas::label(digest))
            .expect("a SHA-256 digest");
        self.inner
            .find_missing(std::slice::from_ref(&digest))
            .await
            .expect("find missing")
            .is_empty()
    }
}

impl Storage for Spy {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_missing(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        self.inner.read_blob(digest)
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let stale = self.stale.lock().expect("lock").take();
        match stale {
            Some(current) => Box::pin(async move { Err(StorageError::StaleMap { current }) }),
            None => self.inner.read_range(digest, offset, limit),
        }
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.written.fetch_add(bytes.len(), Ordering::SeqCst);
        self.inner.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        if !self.streams {
            return self.inner.stream(size);
        }
        Some(Box::new(Buffered {
            store: Arc::clone(&self.inner),
            written: Arc::clone(&self.written),
            bytes: Vec::new(),
            size,
        }))
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.inner.action_result(action)
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(result)
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, action, record)
    }
}

/// A blob taken as a stream: held until it is finished, then stored whole. It stands in
/// for a store that writes as bytes arrive, which the real cache does not yet.
struct Buffered {
    store: Arc<CacheStorage<MemoryMetaLog, MemoryStore>>,
    written: Arc<AtomicUsize>,
    bytes: Vec<u8>,
    size: u64,
}

impl PutStream for Buffered {
    fn append(&mut self, chunk: &[u8]) -> Result<(), StorageError> {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() as u64 > self.size {
            return Err(StorageError::Invalid(
                "more bytes than the stream was made for".into(),
            ));
        }
        Ok(())
    }

    fn finish(
        self: Box<Self>,
        digest: Digest,
        _crc32c: u32,
    ) -> Result<Box<dyn StreamedPut>, StorageError> {
        if kbf_segments::sha256(&self.bytes) != digest {
            return Err(StorageError::Invalid(
                "the bytes are not the digest's".into(),
            ));
        }
        Ok(self)
    }
}

impl StreamedPut for Buffered {
    fn commit(self: Box<Self>) -> Answer<'static, ()> {
        Box::pin(async move {
            self.written.fetch_add(self.bytes.len(), Ordering::SeqCst);
            self.store.put(Bytes::from(self.bytes)).await.map(drop)
        })
    }
}

/// A node serving on loopback.
pub struct Node {
    pub store: Arc<Spy>,
    pub port: u16,
}

impl Node {
    /// `localhost:<port>`: the name the test certificate carries.
    pub fn name(&self) -> String {
        format!("localhost:{}", self.port)
    }

    /// `127.0.0.1:<port>`: an address the test certificate does not name.
    pub fn by_ip(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

/// A node over mutual TLS, its certificate `pki`'s server certificate (for `localhost`).
pub fn serve_tls(pki: &Pki) -> Node {
    serve(Some(pki))
}

/// A node over plain text: the frontend's CAS in the tests that need one.
pub fn serve_plain() -> Node {
    serve(None)
}

fn serve(pki: Option<&Pki>) -> Node {
    let store = Spy::new();
    let service = StorageService::new(
        Arc::clone(&store) as Arc<dyn Storage>,
        Admission::Open,
        Config {
            write_bytes_in_flight: 64 << 20,
        },
    );
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let port = incoming.local_addr().expect("address").port();
    let mut builder = Server::builder();
    if let Some(pki) = pki {
        builder = builder.tls_config(pki.server_tls()).expect("TLS config");
    }
    tokio::spawn(
        builder
            .add_routes(service.routes())
            .serve_with_incoming(incoming),
    );
    Node { store, port }
}
