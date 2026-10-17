//! The daemons' blob path on the worker listener (`kbf_server::blobs`): `ByteStream`,
//! `ContentAddressableStorage` and `kbf.worker.v1.Segments` over mutual TLS, each call admitted by the
//! node-certificate rules and the deny list of the worker stream, and no other REAPI
//! service served there.

use std::future::pending;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use futures::stream;
use kbf_front::Cache;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{QueryWriteStatusRequest, ReadRequest, WriteRequest};
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest, Directory, ExecuteRequest, FileNode,
    FindMissingBlobsRequest, GetActionResultRequest, GetCapabilitiesRequest,
    GetChunkMappingRequest, GetTreeRequest, RegisterChunkMappingRequest, SpliceBlobRequest,
    SplitBlobRequest, batch_update_blobs_request,
};
use kbf_proto::worker::segments_client::SegmentsClient;
use kbf_proto::worker::{
    BeginRequest, CommitRequest, LocateRequest, PutRangeRequest, ReadRangeRequest, UploadSegment,
};
use kbf_server::{Args, bind_server};
use prost::Message;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use sha2::Digest as _;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
use tonic::{Code, Status};

/// A cell CA, the worker listener's certificate, and daemon certificates on demand.
struct Pki {
    dir: PathBuf,
    ca: CertifiedIssuer<'static, KeyPair>,
}

/// A daemon certificate: its identity and what the deny list can name it by.
struct Node {
    identity: Identity,
    serial: u64,
    spki_sha256: String,
}

impl Pki {
    fn new(name: &str) -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-server-worker-blobs")
            .join(name);
        std::fs::create_dir_all(&dir).expect("create the TLS directory");
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
        let pki = Self { dir, ca };
        let mut server = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("key");
        let cert = server.signed_by(&key, &pki.ca).expect("sign");
        pki.write("ca.pem", &pki.ca.pem());
        pki.write("server.pem", &cert.pem());
        pki.write("server.key", &key.serialize_pem());
        pki
    }

    /// A daemon certificate with `names` as its DNS subjectAltNames and the daemon role.
    fn node(&self, names: &[&str], serial: u64) -> Node {
        self.node_as(names, &["daemon"], serial)
    }

    /// [`Pki::node`] with a `urn:kbf:role:` URI per `roles` instead of the daemon role.
    fn node_as(&self, names: &[&str], roles: &[&str], serial: u64) -> Node {
        let (cert, key) = self.signed(names, roles, serial);
        let (_, parsed) = x509_parser::parse_x509_certificate(cert.der()).expect("parse");
        let spki = sha2::Sha256::digest(parsed.public_key().raw);
        Node {
            identity: Identity::from_pem(cert.pem(), key.serialize_pem()),
            serial,
            spki_sha256: spki.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    /// An identity for node-1 presenting a chain of two certificates from the CA: the
    /// leaf naming `first` (whose key the identity holds), then one naming `then`.
    fn chain(&self, first: &str, then: &str, serial: u64) -> Identity {
        let (leaf, key) = self.signed(&["node-1"], &[first], serial);
        let (extra, _) = self.signed(&["node-1"], &[then], serial + 1);
        Identity::from_pem(
            format!("{}\n{}", leaf.pem(), extra.pem()),
            key.serialize_pem(),
        )
    }

    /// A client certificate signed by the CA, with `names` as DNS names, a
    /// `urn:kbf:role:` URI per `roles`, and `serial`; and its key.
    fn signed(&self, names: &[&str], roles: &[&str], serial: u64) -> (rcgen::Certificate, KeyPair) {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let mut params = CertificateParams::new(names).expect("params");
        params.subject_alt_names.extend(roles.iter().map(|role| {
            SanType::URI(
                format!("urn:kbf:role:{role}")
                    .try_into()
                    .expect("an IA5 name"),
            )
        }));
        params
            .distinguished_name
            .push(DnType::CommonName, "cn-is-not-read");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.serial_number = Some(SerialNumber::from(serial));
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        (cert, key)
    }

    fn write(&self, file: &str, text: &str) -> String {
        let path = self.dir.join(file);
        std::fs::write(&path, text).expect("write");
        path.to_str().expect("UTF-8 path").to_owned()
    }

    fn path(&self, file: &str) -> String {
        self.dir.join(file).to_str().expect("UTF-8 path").to_owned()
    }

    /// Starts a server under mutual TLS with these flags added; returns its REAPI and
    /// worker addresses.
    fn serve(&self, extra: &[&str]) -> (SocketAddr, SocketAddr) {
        serve(&self.args(extra))
    }

    /// [`Pki::serve`] with no other flag, over `cache`, which the test keeps to read.
    fn serve_cache(&self, cache: Arc<MemoryCache>) -> (SocketAddr, SocketAddr) {
        serve_cache(&self.args(&[]), cache)
    }

    /// The flags of a server under mutual TLS, with `extra` added.
    fn args(&self, extra: &[&str]) -> Args {
        let base = [
            "kbf-server",
            "--listen",
            "127.0.0.1:0",
            "--worker-listen",
            "127.0.0.1:0",
            "--worker-tls-cert",
            &self.path("server.pem"),
            "--worker-tls-key",
            &self.path("server.key"),
            "--worker-client-ca",
            &self.path("ca.pem"),
        ]
        .map(str::to_owned);
        Args::parse_from(base.iter().map(String::as_str).chain(extra.iter().copied()))
    }

    /// A channel to the worker listener, presenting `identity` if given.
    async fn channel(&self, worker: SocketAddr, identity: Option<&Identity>) -> Channel {
        let mut tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost");
        if let Some(identity) = identity {
            tls = tls.identity(identity.clone());
        }
        Endpoint::from_shared(format!("https://{worker}"))
            .expect("endpoint")
            .tls_config(tls)
            .expect("tls")
            .connect_lazy()
    }
}

fn serve(args: &Args) -> (SocketAddr, SocketAddr) {
    serve_cache(args, Arc::new(Cache::memory()))
}

/// The server of `args` over `cache`.
fn serve_cache(args: &Args, cache: Arc<MemoryCache>) -> (SocketAddr, SocketAddr) {
    let listeners = args.listeners().expect("listeners");
    let bound = bind_server(cache, listeners, pending()).expect("bind");
    let addrs = (bound.reapi, bound.worker);
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    addrs
}

type MemoryCache = Cache<kbf_front::MemoryMetaLog, kbf_objstore::MemoryStore>;

fn plain(addr: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy()
}

fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        size_bytes: i64::try_from(bytes.len()).expect("small"),
    }
}

async fn write(channel: &Channel, bytes: &[u8]) -> Result<i64, Status> {
    let digest = digest_of(bytes);
    let request = WriteRequest {
        resource_name: format!("uploads/test/blobs/{}/{}", digest.hash, digest.size_bytes),
        write_offset: 0,
        finish_write: true,
        data: bytes.to_vec(),
    };
    let response = ByteStreamClient::new(channel.clone())
        .write(stream::iter([request]))
        .await?;
    Ok(response.into_inner().committed_size)
}

async fn read(channel: &Channel, digest: &Digest) -> Result<Vec<u8>, Status> {
    let request = ReadRequest {
        resource_name: format!("blobs/{}/{}", digest.hash, digest.size_bytes),
        read_offset: 0,
        read_limit: 0,
    };
    let mut chunks = ByteStreamClient::new(channel.clone())
        .read(request)
        .await?
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.message().await? {
        bytes.extend_from_slice(&chunk.data);
    }
    Ok(bytes)
}

async fn query(channel: &Channel, digest: &Digest) -> Result<bool, Status> {
    let request = QueryWriteStatusRequest {
        resource_name: format!("uploads/test/blobs/{}/{}", digest.hash, digest.size_bytes),
    };
    let response = ByteStreamClient::new(channel.clone())
        .query_write_status(request)
        .await?;
    Ok(response.into_inner().complete)
}

fn cas(channel: &Channel) -> ContentAddressableStorageClient<Channel> {
    ContentAddressableStorageClient::new(channel.clone())
}

/// The digests of `blobs` that FindMissingBlobs on `channel` reports missing.
async fn missing(channel: &Channel, blobs: &[&[u8]]) -> Result<Vec<Digest>, Status> {
    let request = FindMissingBlobsRequest {
        blob_digests: blobs.iter().map(|b| digest_of(b)).collect(),
        ..FindMissingBlobsRequest::default()
    };
    let response = cas(channel).find_missing_blobs(request).await?;
    Ok(response.into_inner().missing_blob_digests)
}

/// Uploads `blobs` in one BatchUpdateBlobs call; the first blob's refusal fails it.
async fn batch_write(channel: &Channel, blobs: &[&[u8]]) -> Result<(), Status> {
    let requests = blobs
        .iter()
        .map(|b| batch_update_blobs_request::Request {
            digest: Some(digest_of(b)),
            data: b.to_vec(),
            ..batch_update_blobs_request::Request::default()
        })
        .collect();
    let request = BatchUpdateBlobsRequest {
        requests,
        ..BatchUpdateBlobsRequest::default()
    };
    let response = cas(channel).batch_update_blobs(request).await?;
    for blob in response.into_inner().responses {
        let status = blob.status.expect("status");
        if status.code != 0 {
            return Err(Status::new(Code::from(status.code), status.message));
        }
    }
    Ok(())
}

/// Reads `digests` in one BatchReadBlobs call; the first blob's refusal fails it.
async fn batch_read(channel: &Channel, digests: &[Digest]) -> Result<Vec<Vec<u8>>, Status> {
    let request = BatchReadBlobsRequest {
        digests: digests.to_vec(),
        ..BatchReadBlobsRequest::default()
    };
    let response = cas(channel).batch_read_blobs(request).await?;
    let mut out = Vec::new();
    for blob in response.into_inner().responses {
        let status = blob.status.expect("status");
        if status.code != 0 {
            return Err(Status::new(Code::from(status.code), status.message));
        }
        out.push(blob.data);
    }
    Ok(out)
}

/// Every directory GetTree on `channel` sends for `root`, over every page.
async fn tree(channel: &Channel, root: &Digest) -> Result<Vec<Directory>, Status> {
    let request = GetTreeRequest {
        root_digest: Some(root.clone()),
        ..GetTreeRequest::default()
    };
    let mut pages = cas(channel).get_tree(request).await?.into_inner();
    let mut out = Vec::new();
    while let Some(page) = pages.message().await? {
        out.extend(page.directories);
    }
    Ok(out)
}

/// A directory holding `blob` as its one file, encoded.
fn directory_of(blob: &[u8]) -> Vec<u8> {
    Directory {
        files: vec![FileNode {
            name: "f".to_owned(),
            digest: Some(digest_of(blob)),
            ..FileNode::default()
        }],
        ..Directory::default()
    }
    .encode_to_vec()
}

/// Every blob call a daemon can make on `channel` for `blob` (writes first, so the
/// reads find it), each answered with its status code (OK when served). For a batch
/// call, a blob's own refusal is the call's answer.
async fn codes(channel: &Channel, blob: &[u8]) -> Vec<(&'static str, Code)> {
    let digest = digest_of(blob);
    let directory = directory_of(blob);
    let code = |r: Result<(), Status>| r.map_or_else(|s| s.code(), |()| Code::Ok);
    let wrote = code(write(channel, blob).await.map(drop));
    let batch_wrote = code(batch_write(channel, &[blob, &directory]).await);
    vec![
        ("Write", wrote),
        ("Read", code(read(channel, &digest).await.map(drop))),
        (
            "QueryWriteStatus",
            code(query(channel, &digest).await.map(drop)),
        ),
        ("BatchUpdateBlobs", batch_wrote),
        (
            "FindMissingBlobs",
            code(missing(channel, &[blob]).await.map(drop)),
        ),
        (
            "BatchReadBlobs",
            code(batch_read(channel, &[digest]).await.map(drop)),
        ),
        (
            "GetTree",
            code(tree(channel, &digest_of(&directory)).await.map(drop)),
        ),
    ]
    .into_iter()
    .chain(segment_codes(channel, blob).await)
    .collect()
}

/// The `Segments` calls on `channel`: an upload of `blob` (Begin, PutRange, Commit),
/// then Locate and a ReadRange of it, each answered with its status code.
async fn segment_codes(channel: &Channel, blob: &[u8]) -> [(&'static str, Code); 5] {
    let code = |r: Result<(), Status>| r.map_or_else(|s| s.code(), |()| Code::Ok);
    let mut client = SegmentsClient::new(channel.clone());
    let mut writer = kbf_segments::SegmentWriter::new(u64::MAX);
    let digest = writer.push(blob).expect("push");
    let segment = writer.finish();
    let begin = client
        .begin(BeginRequest {
            segments: vec![UploadSegment {
                length: segment.len() as u64,
                footer: segment[blob.len()..].to_vec(),
            }],
        })
        .await;
    let upload = begin.as_ref().map_or(0, |r| r.get_ref().upload);
    let put = client
        .put_range(PutRangeRequest {
            upload,
            segment: 0,
            offset: 0,
            bytes: blob.to_vec(),
        })
        .await;
    let commit = client.commit(CommitRequest { upload }).await;
    let located = client
        .locate(LocateRequest {
            digests: vec![kbf_front::digest_to_proto(&digest)],
        })
        .await;
    let at = located
        .as_ref()
        .ok()
        .and_then(|r| r.get_ref().locations.first().cloned())
        .unwrap_or_default();
    let read = client
        .read_range(ReadRangeRequest {
            object: Some(at.object.unwrap_or_default()),
            offset: at.offset,
            length: digest.size_bytes,
        })
        .await;
    [
        ("Segments.Begin", code(begin.map(drop))),
        ("Segments.PutRange", code(put.map(drop))),
        ("Segments.Commit", code(commit.map(drop))),
        ("Segments.Locate", code(located.map(drop))),
        ("Segments.ReadRange", code(read.map(drop))),
    ]
}

/// The four chunking calls on `channel`, each answered with its status code.
async fn chunking_codes(channel: &Channel) -> [(&'static str, Code); 4] {
    let digest = digest_of(b"chunked");
    let code = |r: Result<(), Status>| r.map_or_else(|s| s.code(), |()| Code::Ok);
    let split = cas(channel)
        .split_blob(SplitBlobRequest {
            blob_digest: Some(digest.clone()),
            ..SplitBlobRequest::default()
        })
        .await;
    let mapping = cas(channel)
        .get_chunk_mapping(GetChunkMappingRequest::default())
        .await;
    let splice = cas(channel)
        .splice_blob(SpliceBlobRequest {
            blob_digest: Some(digest),
            ..SpliceBlobRequest::default()
        })
        .await;
    let register = cas(channel)
        .register_chunk_mapping(stream::iter([RegisterChunkMappingRequest::default()]))
        .await;
    [
        ("SplitBlob", code(split.map(drop))),
        ("GetChunkMapping", code(mapping.map(drop))),
        ("SpliceBlob", code(splice.map(drop))),
        ("RegisterChunkMapping", code(register.map(drop))),
    ]
}

/// Catches: no `ByteStream` on the worker listener (a daemon's `--cas` there fails),
/// a Write that is not durable in the farm's one cache (a client on the REAPI port
/// would not find the daemon's output), and a Read or QueryWriteStatus that does not
/// answer for a blob a daemon wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_certificate_reads_and_writes_blobs_on_the_worker_listener() {
    let pki = Pki::new("serves");
    let deny = pki.write("deny", "# nothing denied\n");
    let (reapi, worker) = pki.serve(&["--worker-deny-list", &deny]);
    let node = pki.node(&["node-1"], 11);
    let channel = pki.channel(worker, Some(&node.identity)).await;

    let output = b"an output the daemon uploads";
    let digest = digest_of(output);
    assert!(!query(&channel, &digest).await.expect("query"));
    assert_eq!(
        write(&channel, output).await.expect("write"),
        digest.size_bytes
    );
    assert!(query(&channel, &digest).await.expect("query"));
    assert_eq!(read(&channel, &digest).await.expect("read"), output);
    assert_eq!(
        read(&plain(reapi), &digest).await.expect("read on REAPI"),
        output,
        "a blob written on the worker listener is in the cache clients read"
    );

    let input = b"an input a client uploaded";
    write(&plain(reapi), input).await.expect("client upload");
    assert_eq!(
        read(&channel, &digest_of(input)).await.expect("read"),
        input
    );
}

/// Catches the daemons' uploads and reads on the worker listener left out of the
/// cache's metrics (`GET /metrics`): the worker listener's services must count into
/// the same cache's families as the REAPI listener's, or the uploads that matter on a
/// farm node would not show.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemons_write_and_read_on_the_worker_listener_are_counted() {
    let pki = Pki::new("metrics");
    let cache = Arc::new(Cache::memory());
    let (_, worker) = pki.serve_cache(Arc::clone(&cache));
    let node = pki.node(&["node-1"], 13);
    let channel = pki.channel(worker, Some(&node.identity)).await;
    let output = b"an output the daemon uploads, counted";
    write(&channel, output).await.expect("write");
    read(&channel, &digest_of(output)).await.expect("read");
    let metrics = cache.metrics();
    assert_eq!(metrics.write.total.get(), 1);
    assert_eq!(metrics.write.bytes.get(), output.len() as u64);
    assert_eq!(metrics.segment_put.count(), 1);
    assert_eq!(metrics.object_get.count(), 1);
}

/// 48 concurrent 4 MiB Writes in 1 MiB messages over one mutual-TLS connection to
/// the worker listener (the daemons' upload path), against a 12 MiB
/// `--write-bytes-in-flight`, so 45 Writes wait for admission with unread data on the
/// connection: every Write finishes, and the receive buffers never held more than the
/// bound.
///
/// Catches: the worker listener's HTTP/2 windows left at the transport's defaults
/// (`write_admission.rs` covers the REAPI listener only), with which the waiting
/// Writes take the whole connection window and the admitted ones never receive their
/// last bytes. Mutant seen red: `flow_control` not applied to the worker listener's
/// builder in `serve.rs` (the timeout fires with a few Writes done).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_on_the_worker_listener_all_finish_under_the_bound() {
    const BLOB: usize = 4 << 20;
    const CHUNK: usize = 1 << 20;
    const WRITES: u64 = 48;
    const BOUND: u64 = 3 * BLOB as u64;
    let pki = Pki::new("flow-control");
    let writes = kbf_front::WriteConfig {
        bytes_in_flight: BOUND,
        ..kbf_front::WriteConfig::DEFAULT
    };
    let cache = Arc::new(Cache::memory().with_writes(writes));
    let (_, worker) = pki.serve_cache(Arc::clone(&cache));
    let node = pki.node(&["node-1"], 21);
    let channel = pki.channel(worker, Some(&node.identity)).await;
    let mut calls = Vec::new();
    for i in 0..WRITES {
        let mut data = vec![0_u8; BLOB];
        data[..8].copy_from_slice(&i.to_le_bytes());
        let d = digest_of(&data);
        let name = format!("uploads/u{i}/blobs/{}/{}", d.hash, d.size_bytes);
        let messages: Vec<WriteRequest> = data
            .chunks(CHUNK)
            .enumerate()
            .map(|(j, c)| WriteRequest {
                resource_name: if j == 0 { name.clone() } else { String::new() },
                write_offset: (j * CHUNK) as i64,
                finish_write: (j + 1) * CHUNK == BLOB,
                data: c.to_vec(),
            })
            .collect();
        let mut client = ByteStreamClient::new(channel.clone());
        calls.push(tokio::spawn(async move {
            client.write(stream::iter(messages)).await
        }));
    }
    let mut done = 0_u64;
    let all = async {
        for call in calls {
            let response = call.await.expect("join").expect("write").into_inner();
            assert_eq!(response.committed_size, BLOB as i64);
            done += 1;
        }
    };
    let finished = tokio::time::timeout(std::time::Duration::from_secs(60), all)
        .await
        .is_ok();
    assert!(finished, "{done} of {WRITES} Writes finished in 60 s");
    let peak = cache.write_buffer_peak();
    assert!(
        peak <= BOUND,
        "{peak} bytes buffered over a {BOUND}-byte bound"
    );
}

/// Catches: blob calls served to a caller with no client certificate (the listener's
/// TLS not applied to them, or applied without the client CA), and to a plain-text
/// client on the worker port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_call_without_a_node_certificate_fails() {
    let pki = Pki::new("no-cert");
    let (_, worker) = pki.serve(&[]);
    let blob = b"some blob";
    let anonymous = pki.channel(worker, None).await;
    for (call, code) in codes(&anonymous, blob).await {
        assert_ne!(code, Code::Ok, "{call} without a client certificate");
    }
    for (call, code) in codes(&plain(worker), blob).await {
        assert_ne!(code, Code::Ok, "{call} in plain text to a mutual-TLS port");
    }
    // The same calls with a certificate are served: the refusals above are TLS's.
    let node = pki.node(&["node-1"], 12);
    let channel = pki.channel(worker, Some(&node.identity)).await;
    for (call, code) in codes(&channel, blob).await {
        assert_eq!(code, Code::Ok, "{call}");
    }
}

/// Catches: a worker listener in plain text (no TLS flags) that serves blobs, where no
/// certificate can be checked; each call must be refused UNAUTHENTICATED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_text_worker_listener_serves_no_blob() {
    let args = Args::parse_from([
        "kbf-server",
        "--listen",
        "127.0.0.1:0",
        "--worker-listen",
        "127.0.0.1:0",
    ]);
    let (_, worker) = serve(&args);
    for (call, code) in codes(&plain(worker), b"some blob").await {
        assert_eq!(code, Code::Unauthenticated, "{call}");
    }
}

/// Catches: a peer check on some `ByteStream` calls but not all (Read checked, Write
/// or QueryWriteStatus not), a deny list read once (at start or per connection) so an
/// entry added later is not seen by the next call on an open connection, an entry
/// removed that still refuses, the node and public-key entries not consulted for blob
/// calls, and a deny list that breaks after start letting calls through (it must fail
/// closed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deny_list_refuses_each_blob_call_from_the_next_one_on() {
    let pki = Pki::new("deny");
    let deny = pki.write("deny", "# nothing denied\n");
    let (_, worker) = pki.serve(&["--worker-deny-list", &deny]);
    let node = pki.node(&["node-1"], 0x0d);
    let other = pki.node(&["node-2"], 0x0e);
    // One connection each, kept open across every edit below.
    let channel = pki.channel(worker, Some(&node.identity)).await;
    let bystander = pki.channel(worker, Some(&other.identity)).await;
    let blob = b"a blob";
    let all = |codes: Vec<(&str, Code)>, want: Code, why: &str| {
        for (call, code) in codes {
            assert_eq!(code, want, "{call}: {why}");
        }
    };
    all(codes(&channel, blob).await, Code::Ok, "nothing denied yet");

    let entries = [
        format!("serial {:x}\n", node.serial),
        "node node-1\n".to_owned(),
        format!("spki-sha256 {}\n", node.spki_sha256),
    ];
    for entry in &entries {
        // Rewritten in place, as an operator's editor may; the size changes each time.
        std::fs::write(&deny, format!("# denied:\n{entry}")).expect("deny");
        all(codes(&channel, blob).await, Code::PermissionDenied, entry);
        all(codes(&bystander, blob).await, Code::Ok, "another node");
        // Replaced atomically: a new file renamed over the list.
        let next = pki.write("deny.next", "# nothing denied again\n");
        std::fs::rename(&next, &deny).expect("rename");
        all(
            codes(&channel, blob).await,
            Code::Ok,
            "the entry was removed",
        );
    }

    std::fs::write(&deny, entries[0].as_str()).expect("deny");
    let refused = read(&channel, &digest_of(blob)).await.expect_err("denied");
    assert!(
        refused.message().contains("certificate serial d"),
        "{}",
        refused.message()
    );

    std::fs::write(&deny, "serial nothex\n").expect("break the list");
    all(
        codes(&bystander, blob).await,
        Code::Unavailable,
        "a broken list",
    );
    std::fs::remove_file(&deny).expect("remove the list");
    all(codes(&bystander, blob).await, Code::Unavailable, "no list");
}

/// Catches: a blob call admitted on a certificate that names no node, or several (the
/// deny list's `node` entries could not be checked against it), or an IP address.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_must_name_exactly_one_node() {
    let pki = Pki::new("names");
    let (_, worker) = pki.serve(&[]);
    for names in [&[][..], &["node-1", "node-2"][..], &["127.0.0.1"][..]] {
        let node = pki.node(names, 21);
        let channel = pki.channel(worker, Some(&node.identity)).await;
        for (call, code) in codes(&channel, b"blob").await {
            assert_eq!(code, Code::PermissionDenied, "{call} with names {names:?}");
        }
    }
}

/// Catches a blob call on the worker listener admitted for a certificate that is not a
/// daemon's: a frontend's or a storage node's from the same CA (naming one node, as a
/// daemon's would), a role that is not one, no role, or two. Every call, the blob
/// calls, the `Segments` calls and the chunking calls, is refused PERMISSION_DENIED; the
/// control is the same node with the daemon role, served.
///
/// Mutant seen red: the role table's layer left off the worker listener.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_blob_call_refuses_a_certificate_that_is_not_a_daemons() {
    let pki = Pki::new("roles");
    let (_, worker) = pki.serve(&[]);
    let blob = b"a blob only a daemon may move";
    let two: &[&str] = &["daemon", "frontend"];
    for (roles, serial) in [
        (&["frontend"][..], 61),
        (&["storage"][..], 62),
        (&["admin"][..], 63),
        (&[][..], 64),
        (two, 65),
    ] {
        let node = pki.node_as(&["node-1"], roles, serial);
        let channel = pki.channel(worker, Some(&node.identity)).await;
        let every = codes(&channel, blob)
            .await
            .into_iter()
            .chain(chunking_codes(&channel).await);
        for (call, code) in every {
            assert_eq!(code, Code::PermissionDenied, "{call} with roles {roles:?}");
        }
    }
    let node = pki.node(&["node-1"], 66);
    let channel = pki.channel(worker, Some(&node.identity)).await;
    for (call, code) in codes(&channel, blob).await {
        assert_eq!(code, Code::Ok, "{call} as a daemon");
    }
}

/// Catches the role read from a certificate of the presented chain other than the
/// leaf (the last, or any of them): a peer holding a frontend key could append a
/// daemon's public certificate after its own and move blobs as a daemon. A chain of
/// two certificates from the same CA, both naming node-1: frontend first, daemon
/// second, has every call refused PERMISSION_DENIED; daemon first, frontend second,
/// is served.
///
/// Mutant seen red: `chain.first()` read as `chain.last()` in the role table's layer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_call_reads_the_role_from_the_leaf_certificate_alone() {
    let pki = Pki::new("leaf-role");
    let (_, worker) = pki.serve(&[]);
    let blob = b"a blob only a daemon leaf may move";
    let channel = pki
        .channel(worker, Some(&pki.chain("frontend", "daemon", 71)))
        .await;
    let every = codes(&channel, blob)
        .await
        .into_iter()
        .chain(chunking_codes(&channel).await);
    for (call, code) in every {
        assert_eq!(code, Code::PermissionDenied, "{call}, frontend leaf");
    }
    let channel = pki
        .channel(worker, Some(&pki.chain("daemon", "frontend", 73)))
        .await;
    for (call, code) in codes(&channel, blob).await {
        assert_eq!(code, Code::Ok, "{call}, daemon leaf");
    }
}

/// Catches: no `ContentAddressableStorage` on the worker listener (the transfer
/// engine's FindMissingBlobs and batches would fall back for good), and a batch write
/// or read that does not reach the farm's one cache: a client on the REAPI port must
/// find what a daemon batched up, and the other way round. GetTree walks a directory
/// a daemon uploaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_certificate_finds_batches_and_walks_trees_on_the_worker_listener() {
    let pki = Pki::new("cas");
    let (reapi, worker) = pki.serve(&[]);
    let node = pki.node(&["node-1"], 41);
    let channel = pki.channel(worker, Some(&node.identity)).await;

    let output: &[u8] = b"an output the daemon batches up";
    let directory = directory_of(output);
    let both = [output, directory.as_slice()];
    let missing_now = missing(&channel, &both).await.expect("find");
    assert_eq!(missing_now, both.map(digest_of));
    batch_write(&channel, &both).await.expect("batch write");
    assert_eq!(
        missing(&channel, &both).await.expect("find"),
        Vec::<Digest>::new()
    );
    let on_reapi = batch_read(&plain(reapi), &[digest_of(output)]).await;
    assert_eq!(on_reapi.expect("read on REAPI"), [output]);

    let input: &[u8] = b"an input a client batched up";
    batch_write(&plain(reapi), &[input]).await.expect("client");
    let read = batch_read(&channel, &[digest_of(input), digest_of(output)]).await;
    assert_eq!(read.expect("batch read"), [input, output]);
    let walked = tree(&channel, &digest_of(&directory)).await.expect("tree");
    assert_eq!(
        walked,
        [Directory::decode(directory.as_slice()).expect("dir")]
    );
}

/// Catches (mutant "forward a chunking call without `admit`"): a chunking call passed
/// to the cache's service before the caller is admitted. Admitted, each answers
/// UNIMPLEMENTED as it does on the REAPI listener; from a denied node, each is refused
/// PERMISSION_DENIED first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chunking_calls_are_admitted_first_and_then_unimplemented() {
    let pki = Pki::new("chunking");
    let deny = pki.write("deny", "node node-2\n");
    let (_, worker) = pki.serve(&["--worker-deny-list", &deny]);
    let node = pki.node(&["node-1"], 51);
    let denied = pki.node(&["node-2"], 52);
    let channel = pki.channel(worker, Some(&node.identity)).await;
    for (call, code) in chunking_codes(&channel).await {
        assert_eq!(code, Code::Unimplemented, "{call} admitted");
    }
    let channel = pki.channel(worker, Some(&denied.identity)).await;
    for (call, code) in chunking_codes(&channel).await {
        assert_eq!(code, Code::PermissionDenied, "{call} from a denied node");
    }
}

/// Catches: REAPI services other than the daemons' two served on the worker listener:
/// a node certificate must not Execute, read the action cache, or ask for capabilities.
/// None of them is in the listener's role table, so each is refused PERMISSION_DENIED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_listener_serves_no_other_reapi_service() {
    let pki = Pki::new("only-blobs");
    let (_, worker) = pki.serve(&[]);
    let node = pki.node(&["node-1"], 31);
    let channel = pki.channel(worker, Some(&node.identity)).await;
    let digest = digest_of(b"x");
    let execute = ExecutionClient::new(channel.clone())
        .execute(ExecuteRequest {
            action_digest: Some(digest.clone()),
            ..ExecuteRequest::default()
        })
        .await
        .map(drop);
    let action_result = ActionCacheClient::new(channel.clone())
        .get_action_result(GetActionResultRequest {
            action_digest: Some(digest),
            ..GetActionResultRequest::default()
        })
        .await
        .map(drop);
    let capabilities = CapabilitiesClient::new(channel)
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .map(drop);
    for (call, result) in [
        ("Execute", execute),
        ("GetActionResult", action_result),
        ("GetCapabilities", capabilities),
    ] {
        let code = result.expect_err(call).code();
        assert_eq!(code, Code::PermissionDenied, "{call}");
    }
}

/// Catches the worker listener's connections left untracked (an upload would belong
/// to no connection: any connection could write into it, and a dead daemon's upload
/// would hold its memory until the idle time): an upload begun on one connection is
/// NOT_FOUND on another of the same node, and found on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_segment_upload_belongs_to_the_connection_that_began_it() {
    let pki = Pki::new("segments-owner");
    let (_, worker) = pki.serve(&[]);
    let node = pki.node(&["node-1"], 51);
    let mut own = SegmentsClient::new(pki.channel(worker, Some(&node.identity)).await);
    let mut other = SegmentsClient::new(pki.channel(worker, Some(&node.identity)).await);
    let mut writer = kbf_segments::SegmentWriter::new(u64::MAX);
    writer.push(b"an output").expect("push");
    let segment = writer.finish();
    let upload = own
        .begin(BeginRequest {
            segments: vec![UploadSegment {
                length: segment.len() as u64,
                footer: segment[b"an output".len()..].to_vec(),
            }],
        })
        .await
        .expect("begin")
        .into_inner()
        .upload;
    let elsewhere = other.commit(CommitRequest { upload }).await;
    assert_eq!(
        elsewhere.expect_err("another connection").code(),
        Code::NotFound
    );
    // On its own connection the upload is there: nothing was sent, so the commit is
    // refused for the missing record, not for the upload.
    let here = own.commit(CommitRequest { upload }).await;
    assert_eq!(
        here.expect_err("nothing sent").code(),
        Code::FailedPrecondition
    );
}
