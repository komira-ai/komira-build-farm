//! The REAPI and worker listeners' certificates replaced on disk while the server runs:
//! the flags (`--tls-reload-interval-secs`) and the metrics, over the real listeners and
//! tonic clients.

use std::future::pending;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use futures::channel::mpsc::unbounded;
use kbf_front::Cache;
use kbf_proto::reapi::GetCapabilitiesRequest;
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;
use kbf_proto::worker::{
    Capability, DaemonMessage, Hello, daemon_message, server_message, worker_client::WorkerClient,
};
use kbf_server::{Api, Args, bind_server_with_api};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

struct Pair {
    cert: String,
    key: String,
}

fn ca(name: &str) -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    Ca {
        issuer: CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key"))
            .expect("CA"),
    }
}

impl Ca {
    fn pem(&self) -> String {
        self.issuer.pem()
    }

    fn leaf(&self, name: &str, usage: ExtendedKeyUsagePurpose, role: Option<&str>) -> Pair {
        let mut params = CertificateParams::new(vec![name.to_owned()]).expect("params");
        // The worker listener reads the role from one `urn:kbf:role:<name>` URI.
        params.subject_alt_names.extend(role.map(|role| {
            SanType::URI(
                format!("urn:kbf:role:{role}")
                    .try_into()
                    .expect("an IA5 name"),
            )
        }));
        params.distinguished_name.push(DnType::CommonName, name);
        params.extended_key_usages = vec![usage];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.issuer).expect("sign");
        Pair {
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }

    fn server(&self) -> Pair {
        self.leaf("localhost", ExtendedKeyUsagePurpose::ServerAuth, None)
    }
}

fn dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kbf-server-reload-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the directory");
    dir
}

/// Replaces `dir/file` whole, as a renewal tool does.
fn put(dir: &Path, file: &str, contents: &str) {
    let aside = dir.join(format!("{file}.new"));
    std::fs::write(&aside, contents).expect("write");
    std::fs::rename(&aside, dir.join(file)).expect("rename");
}

fn path(dir: &Path, file: &str) -> String {
    dir.join(file).to_str().expect("UTF-8 path").to_owned()
}

fn client(addr: SocketAddr, trust: &str, identity: Option<Identity>) -> Endpoint {
    let mut tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(trust))
        .domain_name("localhost");
    if let Some(identity) = identity {
        tls = tls.identity(identity);
    }
    Endpoint::from_shared(format!("https://{addr}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("tls")
}

async fn capabilities(channel: tonic::transport::Channel) -> bool {
    CapabilitiesClient::new(channel)
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .is_ok()
}

async fn capabilities_answered(endpoint: Endpoint) -> bool {
    match endpoint.connect().await {
        Ok(channel) => capabilities(channel).await,
        Err(_) => false,
    }
}

async fn session_welcomed(endpoint: Endpoint) -> bool {
    let Ok(channel) = endpoint.connect().await else {
        return false;
    };
    let (tx, rx) = unbounded();
    let capability = |key: &str, value: &str| Capability {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    let hello = Hello {
        protocol_version: 1,
        node_id: "node-1".to_owned(),
        capabilities: vec![
            capability("cpus", "1"),
            capability("mem_gib", "1"),
            capability("arch", "x86_64"),
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
    matches!(
        response.into_inner().message().await,
        Ok(Some(m)) if matches!(m.message, Some(server_message::Message::Welcome(_)))
    )
}

async fn until<F: std::future::Future<Output = bool>>(what: &str, mut check: impl FnMut() -> F) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn scrape(api: SocketAddr) -> String {
    let mut tcp = TcpStream::connect(api).await.expect("connect");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.expect("read");
    response
}

struct Running {
    reapi: SocketAddr,
    worker: SocketAddr,
    api: SocketAddr,
}

fn start(args: &[String]) -> Running {
    let mut all = vec![
        "kbf-server".to_owned(),
        "--listen".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--worker-listen".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--tls-reload-interval-secs".to_owned(),
        "1".to_owned(),
    ];
    all.extend(args.iter().cloned());
    let listeners = Args::parse_from(all).listeners().expect("listeners");
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: None,
        local_cache: None,
    };
    let bound = bind_server_with_api(Arc::new(Cache::memory()), listeners, Some(api), pending())
        .expect("bind");
    let running = Running {
        reapi: bound.reapi,
        worker: bound.worker,
        api: bound.api.expect("an API address"),
    };
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    running
}

/// A REAPI certificate renewed by replacing the files: a connection open at the time
/// keeps being answered, a client that trusts only the new certificate's issuer is
/// answered once the files are looked at again, the old issuer's client is not, and a
/// bad file in between is counted in `/metrics` while the good pair keeps serving.
///
/// Kills: a listener that reads its certificate once; a renewal that ends the open
/// connections; garbage replacing the working certificate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewed_reapi_certificate_is_served_without_a_restart() {
    let (old_ca, new_ca) = (ca("old issuer"), ca("new issuer"));
    let d = dir("reapi");
    let old = old_ca.server();
    put(&d, "server.pem", &old.cert);
    put(&d, "server.key", &old.key);
    let running = start(&[
        "--reapi-tls-cert".to_owned(),
        path(&d, "server.pem"),
        "--reapi-tls-key".to_owned(),
        path(&d, "server.key"),
    ]);

    let open = client(running.reapi, &old_ca.pem(), None)
        .connect()
        .await
        .expect("TLS handshake");
    assert!(capabilities(open.clone()).await);

    put(&d, "server.pem", "garbage in the middle of a renewal");
    until("the garbage to be refused", || async {
        scrape(running.api)
            .await
            .contains("kbf_tls_reloads_total{listener=\"reapi\",outcome=\"rejected\"} 1")
    })
    .await;
    assert!(
        capabilities_answered(client(running.reapi, &old_ca.pem(), None)).await,
        "garbage replaced the working certificate"
    );

    let new = new_ca.server();
    put(&d, "server.pem", &new.cert);
    put(&d, "server.key", &new.key);
    until("the renewed certificate to be served", || async {
        capabilities_answered(client(running.reapi, &new_ca.pem(), None)).await
    })
    .await;
    assert!(
        !capabilities_answered(client(running.reapi, &old_ca.pem(), None)).await,
        "the old certificate is still served to new connections"
    );
    assert!(capabilities(open).await, "the open connection was ended");
    let metrics = scrape(running.api).await;
    assert!(
        metrics.contains("kbf_tls_reloads_total{listener=\"reapi\",outcome=\"applied\"} 1"),
        "{metrics}"
    );
    assert!(metrics.contains("kbf_tls_leaf_not_after_seconds{listener=\"reapi\"} "));
}

/// The worker listener's three files: after its client CA is replaced, a daemon with a
/// certificate of the new CA is welcomed and one of the old CA is not; after its server
/// certificate is replaced, a daemon that trusts only the new issuer is welcomed.
///
/// Kills: a worker listener whose trust roots or identity are read once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_listener_reloads_its_client_ca_and_identity() {
    let (server_ca, new_server_ca) = (ca("server issuer"), ca("new server issuer"));
    let (old_clients, new_clients) = (ca("old clients"), ca("new clients"));
    let d = dir("worker");
    let server = server_ca.server();
    put(&d, "server.pem", &server.cert);
    put(&d, "server.key", &server.key);
    put(&d, "ca.pem", &old_clients.pem());
    let running = start(&[
        "--worker-tls-cert".to_owned(),
        path(&d, "server.pem"),
        "--worker-tls-key".to_owned(),
        path(&d, "server.key"),
        "--worker-client-ca".to_owned(),
        path(&d, "ca.pem"),
    ]);
    let node = |issuer: &Ca| {
        let pair = issuer.leaf(
            "node-1",
            ExtendedKeyUsagePurpose::ClientAuth,
            Some("daemon"),
        );
        Identity::from_pem(pair.cert, pair.key)
    };
    let (old_node, new_node) = (node(&old_clients), node(&new_clients));

    assert!(
        session_welcomed(client(
            running.worker,
            &server_ca.pem(),
            Some(old_node.clone())
        ))
        .await
    );
    put(&d, "ca.pem", &new_clients.pem());
    until("the new client CA to be trusted", || async {
        session_welcomed(client(
            running.worker,
            &server_ca.pem(),
            Some(new_node.clone()),
        ))
        .await
    })
    .await;
    assert!(
        !session_welcomed(client(running.worker, &server_ca.pem(), Some(old_node))).await,
        "a certificate of the replaced CA is still welcomed"
    );

    let renewed = new_server_ca.server();
    put(&d, "server.pem", &renewed.cert);
    put(&d, "server.key", &renewed.key);
    until("the renewed server certificate to be served", || async {
        session_welcomed(client(
            running.worker,
            &new_server_ca.pem(),
            Some(new_node.clone()),
        ))
        .await
    })
    .await;
}

/// The flag's range: zero and over an hour are refused, and the default is half a minute.
#[test]
fn the_reload_interval_flag_is_bounded() {
    let parse = |extra: &[&str]| {
        let mut all = vec!["kbf-server"];
        all.extend(extra);
        Args::try_parse_from(all)
    };
    assert_eq!(parse(&[]).expect("default").tls_reload_interval_secs, 30);
    assert!(parse(&["--tls-reload-interval-secs", "0"]).is_err());
    assert!(parse(&["--tls-reload-interval-secs", "3601"]).is_err());
    let long = parse(&["--tls-reload-interval-secs", "3600"]).expect("an hour");
    assert_eq!(long.tls_reload_interval_secs, 3600);
}
