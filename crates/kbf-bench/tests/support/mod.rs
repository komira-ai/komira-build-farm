//! A kbf-server in this process over the in-memory cache, with a mutual-TLS worker
//! listener and a plain-text REAPI listener, and the TLS files a daemon would hold.

#![allow(dead_code)]

use std::future::pending;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser as _;
use futures::{StreamExt as _, stream};
use kbf_bench::connect::Target;
use kbf_bench::tree::Tree;
use kbf_front::Cache;
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_server::{Args, bind_server};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tonic::transport::Endpoint;

/// A running server and the files to reach it with.
pub struct Server {
    pub dir: PathBuf,
    pub reapi: SocketAddr,
    pub worker: SocketAddr,
}

impl Server {
    /// Starts a server whose TLS files live in a directory named after `test`.
    pub fn start(test: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-bench")
            .join(test);
        std::fs::create_dir_all(&dir).expect("create the TLS directory");
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf-bench test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
        let write = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).expect("write a TLS file");
            path.to_str().expect("UTF-8 path").to_owned()
        };
        let signed = |names: Vec<String>, usage: ExtendedKeyUsagePurpose| {
            let mut params = CertificateParams::new(names).expect("params");
            if usage == ExtendedKeyUsagePurpose::ClientAuth {
                // The daemon role, as the worker listener requires.
                let role = "urn:kbf:role:daemon".try_into().expect("an IA5 name");
                params.subject_alt_names.push(SanType::URI(role));
            }
            params.extended_key_usages = vec![usage];
            let key = KeyPair::generate().expect("key");
            let cert = params.signed_by(&key, &ca).expect("sign");
            (cert.pem(), key.serialize_pem())
        };
        let (server_pem, server_key) = signed(
            vec!["localhost".to_owned()],
            ExtendedKeyUsagePurpose::ServerAuth,
        );
        let (node_pem, node_key) = signed(
            vec!["node-1".to_owned()],
            ExtendedKeyUsagePurpose::ClientAuth,
        );
        let ca_path = write("ca.pem", &ca.pem());
        let server_pem = write("server.pem", &server_pem);
        let server_key = write("server.key", &server_key);
        write("node.pem", &node_pem);
        write("node.key", &node_key);
        write("garbage.pem", "not a certificate\n");
        let deny = write("deny", "# nothing denied\n");
        let args = Args::parse_from([
            "kbf-server",
            "--listen",
            "127.0.0.1:0",
            "--worker-listen",
            "127.0.0.1:0",
            "--worker-tls-cert",
            &server_pem,
            "--worker-tls-key",
            &server_key,
            "--worker-client-ca",
            &ca_path,
            "--worker-deny-list",
            &deny,
        ]);
        let listeners = args.listeners().expect("listeners");
        let bound = bind_server(Arc::new(Cache::memory()), listeners, pending()).expect("bind");
        let (reapi, worker) = (bound.reapi, bound.worker);
        tokio::spawn(async move { bound.serving.await.expect("serve") });
        Self { dir, reapi, worker }
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The worker listener, as a daemon reaches it: mutual TLS with a node certificate.
    pub fn worker_target(&self) -> Target {
        Target {
            cas: format!("https://{}", self.worker),
            ca_cert: Some(self.file("ca.pem")),
            cert: Some(self.file("node.pem")),
            key: Some(self.file("node.key")),
            tls_server_name: Some("localhost".to_owned()),
        }
    }

    /// The REAPI listener, in plain text.
    pub fn reapi_target(&self) -> Target {
        plain(self.reapi)
    }

    /// Reads every blob of `tree` over the REAPI listener with a client of its own, and
    /// checks each one's bytes: the proof that what the bench sent arrived whole.
    pub async fn assert_holds(&self, tree: &Tree) {
        let channel = Endpoint::from_shared(format!("http://{}", self.reapi))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let client = ByteStreamClient::new(channel);
        let reads = tree.blobs.iter().map(|blob| {
            let mut client = client.clone();
            async move {
                let request = ReadRequest {
                    resource_name: format!("blobs/{}/{}", blob.digest.hash, blob.digest.size_bytes),
                    ..ReadRequest::default()
                };
                let mut chunks = client.read(request).await.expect("read").into_inner();
                let mut bytes = Vec::new();
                while let Some(chunk) = chunks.message().await.expect("read a chunk") {
                    bytes.extend_from_slice(&chunk.data);
                }
                assert!(
                    bytes == blob.bytes,
                    "blob {} came back different",
                    blob.digest.hash
                );
            }
        });
        // Many at a time: a read can wait 40 ms for a delayed ACK (the server's
        // sockets keep Nagle's algorithm on).
        stream::iter(reads)
            .buffer_unordered(32)
            .collect::<()>()
            .await;
    }
}

/// A plain-text target at `addr`.
pub fn plain(addr: SocketAddr) -> Target {
    Target {
        cas: format!("http://{addr}"),
        ca_cert: None,
        cert: None,
        key: None,
        tls_server_name: None,
    }
}
