//! The worker listener under mutual TLS and the REAPI listener under TLS, configured
//! by the server's flags.

use std::future::pending;
use std::path::{Path, PathBuf};

use clap::Parser;
use futures::channel::mpsc::unbounded;
use kbf_auth::Policy;
use kbf_front::Cache;
use kbf_proto::grpc::health::v1::HealthCheckRequest;
use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
use kbf_proto::grpc::health::v1::health_client::HealthClient;
use kbf_proto::reapi::GetCapabilitiesRequest;
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;
use kbf_proto::worker::{
    Capability, DaemonMessage, Hello, daemon_message, server_message, worker_client::WorkerClient,
};
use kbf_server::{Args, bind_server, bind_server_with_policy};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

struct Pki {
    dir: PathBuf,
    ca: String,
    client_cert: String,
    client_key: String,
}

/// A CA, a server certificate for `localhost` and a client certificate for `node-1`,
/// written under a directory of their own named `name`, so tests running at the same
/// time never read each other's half-written files.
fn pki(name: &str) -> Pki {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kbf-server-tls-{name}"));
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
    std::fs::write(dir.join("client.pem"), &client_cert).expect("write");
    std::fs::write(dir.join("client.key"), &client_key).expect("write");
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
    let pki = pki("worker");
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

/// Calls GetCapabilities on `endpoint` and returns whether it was answered.
async fn capabilities_answered(endpoint: Endpoint) -> bool {
    let Ok(channel) = endpoint.connect().await else {
        return false;
    };
    CapabilitiesClient::new(channel)
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .is_ok()
}

/// A TLS client that trusts `pki`'s CA and presents no certificate.
fn reapi_tls_endpoint(addr: std::net::SocketAddr, pki: &Pki) -> Endpoint {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&pki.ca))
        .domain_name("localhost");
    Endpoint::from_shared(format!("https://{addr}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("tls")
}

fn reapi_tls_args(pki: &Pki, cert: &str, key: &str) -> Args {
    Args::parse_from([
        "kbf-server",
        "--listen",
        "127.0.0.1:0",
        "--worker-listen",
        "127.0.0.1:0",
        "--reapi-tls-cert",
        &path(&pki.dir, cert),
        "--reapi-tls-key",
        &path(&pki.dir, key),
    ])
}

/// Catches: REAPI TLS flags that are read but not applied (the listener serves plain
/// text, so the TLS client fails and the plain-text client is answered), and a REAPI
/// listener given a client CA like the worker listener's (the TLS client, which
/// presents no certificate, is refused).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reapi_listener_serves_tls_and_refuses_plain_text() {
    let pki = pki("reapi");
    let listeners = reapi_tls_args(&pki, "server.pem", "server.key")
        .listeners()
        .expect("listeners");
    assert!(listeners.reapi_tls.is_some());
    let bound = bind_server(Arc::new(Cache::memory()), listeners, pending()).expect("bind");
    let addr = bound.reapi;
    tokio::spawn(async move { bound.serving.await.expect("serve") });

    assert!(
        capabilities_answered(reapi_tls_endpoint(addr, &pki)).await,
        "a TLS client was not answered"
    );
    let plain = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    assert!(
        !capabilities_answered(plain).await,
        "a plain-text client was answered on the TLS listener"
    );
}

/// Catches: the authentication layer dropped from the REAPI listener when it serves
/// TLS (the TLS client would be answered although the policy denies every call), and a
/// TLS listener that refuses before the policy is asked (the refusal would not carry
/// the policy's message).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_auth_policy_applies_on_the_tls_reapi_listener() {
    let pki = pki("reapi-auth");
    let listeners = reapi_tls_args(&pki, "server.pem", "server.key")
        .listeners()
        .expect("listeners");
    let policy = Policy::from_json(r#"{"authenticationPolicy": {"deny": "no TLS callers"}}"#)
        .expect("a policy");
    let bound = bind_server_with_policy(
        Arc::new(Cache::memory()),
        listeners,
        None,
        policy,
        pending(),
    )
    .expect("bind");
    let addr = bound.reapi;
    tokio::spawn(async move { bound.serving.await.expect("serve") });

    let channel = reapi_tls_endpoint(addr, &pki)
        .connect()
        .await
        .expect("TLS handshake");
    let status = CapabilitiesClient::new(channel)
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .expect_err("a denied call was answered over TLS");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert_eq!(status.message(), "no TLS callers");
}

/// Calls `grpc.health.v1.Health/Check` for the server as a whole on `endpoint`, and
/// returns its status, or `None` if the call was not answered.
async fn health_checked(endpoint: Endpoint) -> Option<ServingStatus> {
    let channel = endpoint.connect().await.ok()?;
    let request = HealthCheckRequest {
        service: String::new(),
    };
    let answer = tokio::time::timeout(Duration::from_secs(10), {
        let mut client = HealthClient::new(channel);
        async move { client.check(request).await }
    })
    .await
    .ok()?
    .ok()?;
    Some(answer.into_inner().status())
}

/// Catches: the health service served off the REAPI TLS configuration (a server
/// builder of its own, say: the TLS client would not be answered and the plain-text
/// client would be), and the authentication layer applied to the health service as
/// well when the listener serves TLS (the policy denies every call, so the Check
/// would be refused `UNAUTHENTICATED` instead of answered `SERVING`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_health_check_is_served_over_the_tls_reapi_listener() {
    let pki = pki("reapi-health");
    let listeners = reapi_tls_args(&pki, "server.pem", "server.key")
        .listeners()
        .expect("listeners");
    let policy = Policy::from_json(r#"{"authenticationPolicy": {"deny": "no TLS callers"}}"#)
        .expect("a policy");
    let bound = bind_server_with_policy(
        Arc::new(Cache::memory()),
        listeners,
        None,
        policy,
        pending(),
    )
    .expect("bind");
    let addr = bound.reapi;
    tokio::spawn(async move { bound.serving.await.expect("serve") });

    assert_eq!(
        health_checked(reapi_tls_endpoint(addr, &pki)).await,
        Some(ServingStatus::Serving),
        "a TLS health check was not answered SERVING"
    );
    let plain = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    assert_eq!(
        health_checked(plain).await,
        None,
        "a plain-text health check was answered on the TLS listener"
    );
}

/// Catches: a REAPI listener that wants TLS when no REAPI TLS flag is given; plain
/// text is the documented default (`Listeners::reapi_tls` of `None`), even when the
/// worker listener serves mutual TLS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_reapi_tls_flags_the_reapi_listener_is_plain_text() {
    let pki = pki("reapi-plain");
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
    assert!(listeners.reapi_tls.is_none());
    let bound = bind_server(Arc::new(Cache::memory()), listeners, pending()).expect("bind");
    let addr = bound.reapi;
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    let plain = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    assert!(capabilities_answered(plain).await, "plain text refused");
}

/// Catches: a key that does not match its certificate accepted (the listener would
/// fail every handshake instead of refusing to start), and the certificate and key
/// files read the wrong way round.
#[tokio::test]
async fn a_reapi_key_that_does_not_match_its_certificate_is_refused() {
    let pki = pki("reapi-mismatch");
    for (cert, key) in [("server.pem", "client.key"), ("server.key", "server.pem")] {
        let bound = reapi_tls_args(&pki, cert, key)
            .listeners()
            .map_err(|e| e.to_string())
            .and_then(|listeners| {
                bind_server(Arc::new(Cache::memory()), listeners, pending())
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            });
        assert!(
            bound.is_err(),
            "--reapi-tls-cert {cert} --reapi-tls-key {key} accepted"
        );
    }
}

/// Catches: `--reapi-tls-cert` or `--reapi-tls-key` accepted alone (the listener would
/// serve plain text although TLS was asked for).
#[test]
fn reapi_tls_flags_come_together() {
    for flag in ["--reapi-tls-cert", "--reapi-tls-key"] {
        let parsed = Args::try_parse_from(["kbf-server", flag, "x.pem"]);
        assert!(parsed.is_err(), "{flag} accepted alone");
    }
}

/// Catches: a bind guard that refuses every address but loopback whatever the TLS
/// flags (the REAPI listener over the internal CA's TLS, and the worker listener over
/// mutual TLS, are how a server is reached from other machines), and
/// `--reapi-plaintext-bind` accepted alongside REAPI TLS, where it would claim a plain
/// text the listener does not serve.
#[test]
fn tls_listeners_may_bind_off_loopback() {
    let pki = pki("off-loopback");
    let (cert, key, ca) = (
        path(&pki.dir, "server.pem"),
        path(&pki.dir, "server.key"),
        path(&pki.dir, "ca.pem"),
    );
    let mut argv = vec![
        "kbf-server",
        "--listen",
        "0.0.0.0:0",
        "--worker-listen",
        "[::]:0",
        "--reapi-tls-cert",
        &cert,
        "--reapi-tls-key",
        &key,
        "--worker-tls-cert",
        &cert,
        "--worker-tls-key",
        &key,
        "--worker-client-ca",
        &ca,
    ];
    let args = Args::parse_from(&argv);
    assert!(!args.reapi_plaintext_off_loopback());
    let listeners = args.listeners().expect("TLS listeners off loopback");
    assert!(listeners.reapi_tls.is_some() && listeners.worker_tls.is_some());

    argv.push("--reapi-plaintext-bind");
    assert!(
        Args::try_parse_from(&argv).is_err(),
        "--reapi-plaintext-bind accepted with --reapi-tls-cert"
    );
}

/// Catches: the binary printing its start line (and serving) with a REAPI key that
/// does not match its certificate, instead of exiting 2 before it.
#[test]
fn the_binary_refuses_to_start_with_a_mismatched_reapi_key() {
    let pki = pki("reapi-binary");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kbf-server"))
        .args([
            "--listen",
            "127.0.0.1:0",
            "--worker-listen",
            "127.0.0.1:0",
            "--reapi-tls-cert",
            &path(&pki.dir, "server.pem"),
            "--reapi-tls-key",
            &path(&pki.dir, "client.key"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kbf-server");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("kbf-server still running with a mismatched REAPI key");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().expect("stdout"), &mut stdout)
        .expect("stdout");
    assert_eq!(status.code(), Some(2));
    assert!(stdout.is_empty(), "start line printed: {stdout}");
}
