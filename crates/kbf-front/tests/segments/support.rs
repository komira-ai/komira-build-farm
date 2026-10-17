//! The `Segments` tests' server, store and segment builders.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_front::{
    Cache, Connections, MAX_MESSAGE_BYTES, MemoryMetaLog, MetaLog as _, SegmentsConfig,
    SegmentsService, WriteConfig,
};
use kbf_meta::{BlobAnswer, Retention};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::worker::segments_client::SegmentsClient;
use kbf_proto::worker::segments_server::SegmentsServer;
use kbf_proto::worker::{
    BeginRequest, BlobLocation, LocateRequest, PutRangeRequest, UploadSegment,
};
use kbf_segments::layout::{encode_footer, footer_len};
use kbf_segments::{Footer, IndexEntry, SegmentWriter};
use kbf_storage::service::OpenWrites;
use kbf_types::{Digest, DigestFunction};
use tokio::sync::{Notify, Semaphore};
use tonic::Status;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};

/// `len` bytes, different for each `seed`.
pub fn blob(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + seed * 7 + seed / 251) % 251) as u8)
        .collect()
}

/// One segment as a daemon would send it: its records, its footer, its length.
#[derive(Clone, Debug)]
pub struct Seg {
    pub records: Vec<u8>,
    pub footer: Vec<u8>,
    pub length: u64,
    pub digests: Vec<Digest>,
}

impl Seg {
    pub fn upload_segment(&self) -> UploadSegment {
        UploadSegment {
            length: self.length,
            footer: self.footer.clone(),
        }
    }
}

/// A segment of `blobs`, as `SegmentWriter` packs it.
pub fn seg(blobs: &[Vec<u8>]) -> Seg {
    seg_with(blobs, |e| e)
}

/// A segment of `blobs` whose footer entries are changed by `f` (and re-encoded).
pub fn seg_with(blobs: &[Vec<u8>], f: impl Fn(IndexEntry) -> IndexEntry) -> Seg {
    let mut writer = SegmentWriter::new(u64::MAX);
    let digests = blobs
        .iter()
        .map(|b| writer.push(b).expect("push"))
        .collect();
    let bytes = writer.finish();
    let parsed = Footer::parse(&bytes, bytes.len() as u64).expect("footer");
    let records_len = usize::try_from(parsed.records_len()).expect("len");
    let entries: Vec<IndexEntry> = parsed.entries().iter().copied().map(f).collect();
    let footer = encode_footer(parsed.records_len(), &entries);
    Seg {
        records: bytes[..records_len].to_vec(),
        length: (records_len + footer.len()) as u64,
        footer,
        digests,
    }
}

/// A `Begin` segment of exactly `length` bytes: two records whose bytes are never sent.
pub fn declared_only(length: u64) -> UploadSegment {
    let records_len = length - footer_len(2);
    let half = records_len / 2;
    let entry = |byte: u8, offset: u64, size: u64| IndexEntry {
        digest: Digest::new(DigestFunction::Sha256, [byte; 32], size),
        offset,
        crc32c: 0,
    };
    let entries = [entry(1, 0, half), entry(2, half, records_len - half)];
    UploadSegment {
        length,
        footer: encode_footer(records_len, &entries),
    }
}

/// A segment of `length` bytes declared but never sent, its records area cut into
/// records of at most `kbf_front::MAX_UNSTAGED_BLOB_BYTES`: what a server without a
/// write tier may hold in memory.
pub fn declared_small(length: u64) -> UploadSegment {
    let most = kbf_front::MAX_UNSTAGED_BLOB_BYTES;
    let mut n = 1;
    while (length - footer_len(n)).div_ceil(most) > n {
        n += 1;
    }
    let records_len = length - footer_len(n);
    let entries: Vec<IndexEntry> = (0..n)
        .map(|i| {
            let offset = i * most;
            let mut hash = [9; 32];
            hash[..8].copy_from_slice(&i.to_be_bytes());
            IndexEntry {
                digest: Digest::new(DigestFunction::Sha256, hash, most.min(records_len - offset)),
                offset,
                crc32c: 0,
            }
        })
        .collect();
    UploadSegment {
        length,
        footer: encode_footer(records_len, &entries),
    }
}

/// A segment declared but never sent: `n` one-byte records with distinct digests
/// whose first byte is `tag`.
pub fn declared_records(n: u32, tag: u8) -> UploadSegment {
    let entries: Vec<IndexEntry> = (0..n)
        .map(|i| {
            let mut hash = [tag; 32];
            hash[1..5].copy_from_slice(&i.to_be_bytes());
            IndexEntry {
                digest: Digest::new(DigestFunction::Sha256, hash, 1),
                offset: u64::from(i),
                crc32c: 0,
            }
        })
        .collect();
    let records_len = u64::from(n);
    UploadSegment {
        length: records_len + footer_len(u64::from(n)),
        footer: encode_footer(records_len, &entries),
    }
}

/// Sends every range of `s`'s records, `chunk` bytes each, last first when `reverse`.
pub async fn put_all(
    client: &mut SegmentsClient<Channel>,
    upload: u64,
    segment: usize,
    s: &Seg,
    chunk: usize,
    reverse: bool,
) {
    if let Some(refused) = try_put_all(client, upload, segment, s, chunk, reverse).await {
        panic!("put range: {refused:?}");
    }
}

/// As [`put_all`], stopping at the first range the server refuses and returning why.
pub async fn try_put_all(
    client: &mut SegmentsClient<Channel>,
    upload: u64,
    segment: usize,
    s: &Seg,
    chunk: usize,
    reverse: bool,
) -> Option<tonic::Status> {
    let mut starts: Vec<usize> = (0..s.records.len()).step_by(chunk).collect();
    if reverse {
        starts.reverse();
    }
    for start in starts {
        let end = s.records.len().min(start + chunk);
        let sent = client
            .put_range(PutRangeRequest {
                upload,
                segment: u32::try_from(segment).expect("segment"),
                offset: start as u64,
                bytes: s.records[start..end].to_vec(),
            })
            .await;
        if let Err(refused) = sent {
            return Some(refused);
        }
    }
    None
}

/// A memory bucket whose PUTs can be held at a gate, or fail.
#[derive(Clone)]
pub struct GatedStore {
    inner: Arc<MemoryStore>,
    holding: Arc<AtomicBool>,
    gate: Arc<Semaphore>,
    entered: Arc<Notify>,
    /// PUTs waiting at the gate now.
    at_gate: Arc<AtomicUsize>,
    /// How many of the next PUTs fail without reaching the bucket.
    failing: Arc<AtomicUsize>,
    /// Every PUT's key, in the order they arrived.
    put_keys: Arc<std::sync::Mutex<Vec<ObjectKey>>>,
    /// What the next PUT stores instead of its body, answering OK.
    replace: Arc<std::sync::Mutex<Option<Bytes>>>,
    /// How many of the next range GETs fail without reaching the bucket.
    failing_gets: Arc<AtomicUsize>,
}

impl GatedStore {
    fn new() -> Self {
        Self {
            inner: Arc::new(MemoryStore::new(Capabilities::default())),
            holding: Arc::new(AtomicBool::new(false)),
            gate: Arc::new(Semaphore::new(0)),
            entered: Arc::new(Notify::new()),
            at_gate: Arc::new(AtomicUsize::new(0)),
            failing: Arc::new(AtomicUsize::new(0)),
            put_keys: Arc::default(),
            replace: Arc::default(),
            failing_gets: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The next PUT stores `bytes` instead of its body and answers OK, as a store that
    /// mixed up two objects would.
    pub fn replace_next_put(&self, bytes: Bytes) {
        *self.replace.lock().expect("replace") = Some(bytes);
    }

    /// The next `n` range GETs fail as a reset connection would.
    pub fn fail_next_gets(&self, n: usize) {
        self.failing_gets.store(n, Ordering::SeqCst);
    }

    /// The next `n` PUTs fail as a reset connection would, storing nothing.
    pub fn fail_next_puts(&self, n: usize) {
        self.failing.store(n, Ordering::SeqCst);
    }

    /// How many PUTs are waiting at the gate now.
    pub fn puts_at_gate(&self) -> usize {
        self.at_gate.load(Ordering::SeqCst)
    }

    /// Every PUT's key so far, failed ones included, in the order they arrived.
    pub fn put_keys(&self) -> Vec<ObjectKey> {
        self.put_keys.lock().expect("keys").clone()
    }

    /// Holds every later PUT until [`GatedStore::release`].
    pub fn hold(&self) {
        self.holding.store(true, Ordering::SeqCst);
    }

    /// Lets the held PUTs, and every later one, through.
    pub fn release(&self) {
        self.holding.store(false, Ordering::SeqCst);
        self.gate.add_permits(1 << 20);
    }

    /// Waits until a PUT is at the gate.
    pub async fn wait_for_put(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.notified())
            .await
            .expect("a PUT reached the gate");
    }
}

impl ObjectStore for GatedStore {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.put_keys.lock().expect("keys").push(key.clone());
        if self.holding.load(Ordering::SeqCst) {
            self.at_gate.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            let _ = self.gate.acquire().await.map(|p| p.forget());
            self.at_gate.fetch_sub(1, Ordering::SeqCst);
        }
        let failed = self
            .failing
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        if failed.is_ok() {
            return Err(reset());
        }
        let body = self.replace.lock().expect("replace").take().unwrap_or(body);
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let failed = self
            .failing_gets
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        if failed.is_ok() {
            return Err(reset());
        }
        self.inner.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

/// What a reset connection to the store looks like.
fn reset() -> ObjectStoreError {
    ObjectStoreError::Transport(Arc::new(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "connection reset (test)",
    )))
}

/// A `Segments` server over loopback, its connections tracked.
pub struct Front {
    addr: SocketAddr,
    pub service: SegmentsService<MemoryMetaLog, GatedStore>,
    pub cache: Arc<Cache<MemoryMetaLog, GatedStore>>,
    pub store: GatedStore,
}

impl Front {
    pub async fn start(config: SegmentsConfig) -> Self {
        Self::start_with(config, WriteConfig::DEFAULT).await
    }

    /// As [`Front::start`], with the cache's uploads bounded as `writes` says.
    pub async fn start_with(config: SegmentsConfig, writes: WriteConfig) -> Self {
        Self::start_counted(config, writes, None).await
    }

    /// As [`Front::start_with`], the service counting its uploads in `open` if given.
    pub async fn start_counted(
        config: SegmentsConfig,
        writes: WriteConfig,
        open: Option<OpenWrites>,
    ) -> Self {
        let store = GatedStore::new();
        let meta = MemoryMetaLog::new(Retention::default());
        let cache = Arc::new(
            Cache::open(meta, store.clone(), KeyPrefix::default())
                .await
                .expect("cache")
                .with_writes(writes),
        );
        let connections = Connections::new();
        let service = SegmentsService::new(Arc::clone(&cache), config, connections.clone());
        let service = match open {
            Some(open) => service.with_open_writes(open),
            None => service,
        };
        let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let addr = incoming.local_addr().expect("addr");
        let incoming = incoming.map(move |accepted| accepted.map(|s| connections.track(s)));
        let server = SegmentsServer::new(service.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
        tokio::spawn(
            Server::builder()
                .add_service(server)
                .serve_with_incoming(incoming),
        );
        Self {
            addr,
            service,
            cache,
            store,
        }
    }

    /// A client on a connection of its own.
    pub async fn client(&self) -> SegmentsClient<Channel> {
        let channel = Endpoint::from_shared(format!("http://{}", self.addr))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        SegmentsClient::new(channel)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    pub async fn begin(&self, client: &mut SegmentsClient<Channel>, segs: &[Seg]) -> u64 {
        client
            .begin(BeginRequest {
                segments: segs.iter().map(Seg::upload_segment).collect(),
            })
            .await
            .expect("begin")
            .into_inner()
            .upload
    }

    pub async fn put(
        &self,
        client: &mut SegmentsClient<Channel>,
        upload: u64,
        segment: u32,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), Status> {
        client
            .put_range(PutRangeRequest {
                upload,
                segment,
                offset,
                bytes: bytes.to_vec(),
            })
            .await
            .map(drop)
    }

    pub async fn locate(
        &self,
        client: &mut SegmentsClient<Channel>,
        digests: &[Digest],
    ) -> Vec<BlobLocation> {
        client
            .locate(LocateRequest {
                digests: digests.iter().map(kbf_front::digest_to_proto).collect(),
            })
            .await
            .expect("locate")
            .into_inner()
            .locations
    }

    /// Asserts every one of `digests` is present (or that none is).
    pub async fn assert_present(&self, digests: &[Digest], present: bool) {
        let found = self.cache.present(digests).await.expect("present");
        assert!(
            found.iter().all(|&p| p == present),
            "present {found:?}, want {present}"
        );
    }

    /// Asserts the object holding each of `digests` has a footer that parses and
    /// indexes the blob at its location with a CRC-32C its bytes pass.
    pub async fn assert_stored_records_check(&self, digests: &[Digest]) {
        for digest in digests {
            let answer = self
                .cache
                .meta()
                .query({
                    let d = *digest;
                    move |s| s.blob(&d)
                })
                .await
                .expect("query");
            let BlobAnswer::Present { location, .. } = answer else {
                panic!("{digest} not present: {answer:?}");
            };
            let key = self.cache.object_key(location.object).expect("key");
            let whole = self
                .store
                .get_range(&key, ByteRange::new(0, 1 << 40).expect("range"))
                .await
                .expect("object");
            let footer = Footer::parse(&whole, whole.len() as u64).expect("stored footer");
            let entry = footer.find(digest).expect("indexed");
            assert_eq!(entry.offset, location.offset);
            let range = entry.range();
            let record = &whole[range.start as usize..range.end as usize];
            entry
                .verify(record)
                .expect("the stored CRC and digest hold");
        }
    }

    /// Waits until the service holds no upload.
    pub async fn wait_until_no_uploads(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.service.open_uploads() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the upload was dropped");
    }
}
