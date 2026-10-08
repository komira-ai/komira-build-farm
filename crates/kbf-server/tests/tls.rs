//! The worker listener under mutual TLS, configured by the server's flags.

use std::future::pending;
use std::path::{Path, PathBuf};

use clap::Parser;
use futures::channel::mpsc::unbounded;
use kbf_front::Cache;
use kbf_proto::worker::{
    Capability, DaemonMessage, Hello, daemon_message, server_message, worker_client::WorkerClient,
};
use kbf_server::{Args, bind_server};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use std::sync::Arc;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

struct Pki {
    dir: PathBuf,
    ca: String,
    client_cert: String,
    client_key: String,
}

fn pki() -> Pki {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-tls");
    std::fs::create_dir_all(&dir).expect("create the TLS directory");
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "kbf test CA");
    ca.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("CA key")).expect("CA");
    let leaf = |names: Vec<String>, cn: &str, eku: ExtendedKeyUsagePurpose| {
        let mut params = CertificateParams::new(names).expect("leaf params");
        params.distinguished_name.push(DnType::CommonName, cn);
        params.extended_key_usages = vec![eku];
        let key = KeyPair::generate().expect("leaf key");
        let cert = params.signed_by(&key, &ca).expect("sign leaf");
        (cert.pem(), key.serialize_pem())
    };
    let (server_cert, server_key) = leaf(
        vec!["localhost".to_owned()],
        "kbf test server",
        ExtendedKeyUsagePurpose::ServerAuth,
    );
    let (client_cert, client_key) = leaf(
        vec!["node-1".to_owned()],
        "node-1",
        ExtendedKeyUsagePurpose::ClientAuth,
    );
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    std::fs::write(dir.join("server.pem"), server_cert).expect("write");
    std::fs::write(dir.join("server.key"), server_key).expect("write");
    Pki {
        dir,
        ca: ca.pem(),
        client_cert,
        client_key,
    }
}

fn path(dir: &Path, file: &str) -> String {
    dir.join(file).to_str().expect("UTF-8 path").to_owned()
}

/// Opens a session as `identity` (or with no client certificate) and returns whether
/// the server welcomed it.
async fn welcomed(addr: std::net::SocketAddr, pki: &Pki, identity: Option<Identity>) -> bool {
    let mut tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&pki.ca))
        .domain_name("localhost");
    if let Some(identity) = identity {
        tls = tls.identity(identity);
    }
    let endpoint = Endpoint::from_shared(format!("https://{addr}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("tls");
    session_welcomed(endpoint).await
}

/// Opens a session in plain text and returns whether the server welcomed it.
async fn welcomed_in_plain_text(addr: std::net::SocketAddr) -> bool {
    session_welcomed(Endpoint::from_shared(format!("http://{addr}")).expect("endpoint")).await
}

async fn session_welcomed(endpoint: Endpoint) -> bool {
    let Ok(channel) = endpoint.connect().await else {
        return false;
    };
    let (tx, rx) = unbounded();
    let hello = Hello {
        protocol_version: 1,
        node_id: "node-1".to_owned(),
        capabilities: vec![
            Capability {
                key: "cpus".to_owned(),
                value: "1".to_owned(),
            },
            Capability {
                key: "mem_gib".to_owned(),
                value: "1".to_owned(),
            },
            Capability {
                key: "arch".to_owned(),
                value: "x86_64".to_owned(),
            },
        ],
        ..Hello::default()
    };
    tx.unbounded_send(DaemonMessage {
        message: Some(daemon_message::Message::Hello(hello)),
    })
    .expect("queue Hello");
    let Ok(response) = WorkerClient::new(channel).session(rx).await else {
        return false;
    };
    let first = response.into_inner().message().await;
    matches!(
        first,
        Ok(Some(m)) if matches!(m.message, Some(server_message::Message::Welcome(_)))
    )
}

/// Catches: TLS flags that are read but not applied (the worker listener serves plain
/// text), and a listener that welcomes a daemon presenting no client certificate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_listener_requires_a_client_certificate() {
    let pki = pki();
    let args = Args::parse_from([
        "kbf-server",
        "--listen",
        "127.0.0.1:0",
        "--worker-listen",
        "127.0.0.1:0",
        "--worker-tls-cert",
        &path(&pki.dir, "server.pem"),
        "--worker-tls-key",
        &path(&pki.dir, "server.key"),
        "--worker-client-ca",
        &path(&pki.dir, "ca.pem"),
    ]);
    let listeners = args.listeners().expect("listeners");
    let bound = bind_server(Arc::new(Cache::memory()), listeners, pending()).expect("bind");
    let addr = bound.worker;
    tokio::spawn(async move { bound.serving.await.expect("serve") });

    let identity = Identity::from_pem(&pki.client_cert, &pki.client_key);
    assert!(
        welcomed(addr, &pki, Some(identity)).await,
        "mutual TLS refused"
    );
    assert!(
        !welcomed(addr, &pki, None).await,
        "welcomed without a certificate"
    );
    assert!(
        !welcomed_in_plain_text(addr).await,
        "welcomed in plain text"
    );
}

/// Catches: a worker listener that wants TLS when no TLS flag is given; plain text is
/// the documented default (`Listeners::worker_tls` of `None`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_tls_flags_the_worker_listener_is_plain_text() {
    let args = Args::parse_from([
        "kbf-server",
        "--listen",
        "127.0.0.1:0",
        "--worker-listen",
        "127.0.0.1:0",
    ]);
    let listeners = args.listeners().expect("listeners");
    assert!(listeners.worker_tls.is_none());
    let bound = bind_server(Arc::new(Cache::memory()), listeners, pending()).expect("bind");
    let addr = bound.worker;
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    assert!(welcomed_in_plain_text(addr).await, "plain text refused");
}
