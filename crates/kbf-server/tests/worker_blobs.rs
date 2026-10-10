//! The daemons' blob path on the worker listener (`kbf_server::blobs`): `ByteStream`
//! over mutual TLS, each call admitted by the node-certificate rules and the deny list
//! of the worker stream, and nothing else of REAPI served there.

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
    Digest, ExecuteRequest, FindMissingBlobsRequest, GetActionResultRequest, GetCapabilitiesRequest,
};
use kbf_server::{Args, bind_server};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SerialNumber,
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

    /// A daemon certificate with `names` as its DNS subjectAltNames.
    fn node(&self, names: &[&str], serial: u64) -> Node {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let mut params = CertificateParams::new(names).expect("params");
        params
            .distinguished_name
            .push(DnType::CommonName, "cn-is-not-read");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.serial_number = Some(SerialNumber::from(serial));
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        let (_, parsed) = x509_parser::parse_x509_certificate(cert.der()).expect("parse");
        let spki = sha2::Sha256::digest(parsed.public_key().raw);
        Node {
            identity: Identity::from_pem(cert.pem(), key.serialize_pem()),
            serial,
            spki_sha256: spki.iter().map(|b| format!("{b:02x}")).collect(),
        }
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
        let args = Args::parse_from(base.iter().map(String::as_str).chain(extra.iter().copied()));
        serve(&args)
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
    let listeners = args.listeners().expect("listeners");
    let bound = bind_server(Arc::new(Cache::memory()), listeners, pending()).expect("bind");
    let addrs = (bound.reapi, bound.worker);
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    addrs
}

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

/// Every `ByteStream` call on `channel` for `blob` (Write first, so Read finds it),
/// each answered with its status code (OK when served).
async fn codes(channel: &Channel, blob: &[u8]) -> [(&'static str, Code); 3] {
    let digest = digest_of(blob);
    let code = |r: Result<(), Status>| r.map_or_else(|s| s.code(), |()| Code::Ok);
    let wrote = code(write(channel, blob).await.map(drop));
    [
        ("Write", wrote),
        ("Read", code(read(channel, &digest).await.map(drop))),
        (
            "QueryWriteStatus",
            code(query(channel, &digest).await.map(drop)),
        ),
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
    let all = |codes: [(&str, Code); 3], want: Code, why: &str| {
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

/// Catches: REAPI services other than `ByteStream` served on the worker listener: a
/// node certificate must not Execute, read the action cache, or reach the CAS service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_listener_serves_no_other_reapi_service() {
    let pki = Pki::new("only-bytestream");
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
            action_digest: Some(digest.clone()),
            ..GetActionResultRequest::default()
        })
        .await
        .map(drop);
    let find_missing = ContentAddressableStorageClient::new(channel.clone())
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![digest],
            ..FindMissingBlobsRequest::default()
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
        ("FindMissingBlobs", find_missing),
        ("GetCapabilities", capabilities),
    ] {
        let code = result.expect_err(call).code();
        assert_eq!(code, Code::Unimplemented, "{call}");
    }
}
