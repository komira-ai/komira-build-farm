//! The listener: only the pinned client key gets an answer.

use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::*;
use crate::gate::fixture::Fixture;
use crate::tlskit::{listen, pki, request};

const HANDSHAKE: Duration = Duration::from_secs(5);

#[tokio::test]
async fn only_the_pinned_server_key_is_answered() {
    // Catches: a listener that accepts any client certificate, or none (S10 "the gate
    // refuses an unauthenticated caller"): a lease user on a bare-metal Mac shares the
    // rack network with the gate's clients.
    let f = Fixture::new("tls-pinned");
    let pki = pki();
    let addr = listen(crate::api::router(f.gate.clone()), &pki, HANDSHAKE).await;
    let (status, body) = request(addr, &pki.ca, Some(&pki.server), "GET", "/v1/macs", b"")
        .await
        .unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"MAC0\""), "{body}");
    for client in [Some(&pki.stranger), None] {
        let refused = request(addr, &pki.ca, client, "GET", "/v1/macs", b"").await;
        assert!(refused.is_err(), "{refused:?}");
    }
    // A CA-issued certificate with another key is refused: no CA is trusted.
    let refused = request(addr, &pki.ca, Some(&pki.gate), "GET", "/v1/macs", b"").await;
    assert!(refused.is_err());
    // TLS 1.2 clients prove the same key.
    let tls12 = [&rustls::version::TLS12];
    let (status, _) = crate::tlskit::request_with(
        &tls12,
        addr,
        &pki.ca,
        Some(&pki.server),
        "GET",
        "/v1/macs",
        b"",
    )
    .await
    .unwrap();
    assert_eq!(status, 200);
    let refused = crate::tlskit::request_with(
        &tls12,
        addr,
        &pki.ca,
        Some(&pki.stranger),
        "GET",
        "/v1/macs",
        b"",
    )
    .await;
    assert!(refused.is_err());
}

#[tokio::test]
async fn a_silent_client_is_dropped_after_the_handshake_timeout() {
    let f = Fixture::new("tls-timeout");
    let pki = pki();
    let addr = listen(
        crate::api::router(f.gate.clone()),
        &pki,
        Duration::from_millis(100),
    )
    .await;
    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(10), tcp.read_to_end(&mut buf)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "{read:?}");
}

#[tokio::test]
async fn garbage_after_the_handshake_ends_the_connection() {
    let f = Fixture::new("tls-garbage");
    let pki = pki();
    let addr = listen(crate::api::router(f.gate.clone()), &pki, HANDSHAKE).await;
    let (status, _) = request(addr, &pki.ca, Some(&pki.server), "NOT A METHOD", "x", b"")
        .await
        .unwrap();
    assert_eq!(status, 400);
    let (status, _) = request(addr, &pki.ca, Some(&pki.server), "GET", "/v1/macs\x01", b"")
        .await
        .unwrap_or_default();
    assert_ne!(status, 200);
}

#[tokio::test]
async fn an_accept_error_does_not_stop_the_listener() {
    let f = Fixture::new("tls-accept-error");
    let pki = pki();
    let config = server_config(pki.gate.chain(), pki.gate.private_key(), pki.server.pin()).unwrap();
    let listener = std::sync::Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
    let addr = listener.local_addr().unwrap();
    let failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    tokio::spawn(serve(
        move || {
            let listener = std::sync::Arc::clone(&listener);
            let failed = std::sync::Arc::clone(&failed);
            async move {
                if !failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return Err(std::io::Error::other("too many open files"));
                }
                listener.accept().await.map(|(s, _)| s)
            }
        },
        config,
        crate::api::router(f.gate.clone()),
        HANDSHAKE,
    ));
    let (status, _) = request(addr, &pki.ca, Some(&pki.server), "GET", "/v1/macs", b"")
        .await
        .unwrap();
    assert_eq!(status, 200);
}

#[test]
fn identities_and_pins_are_read_and_checked() {
    let dir = crate::testkit::scratch("tls-files");
    let pki = pki();
    pki.gate.write(&dir, "gate");
    let (chain, _key) = load_identity(&dir.join("gate.pem"), &dir.join("gate.key")).unwrap();
    assert_eq!(chain, pki.gate.chain());
    assert!(load_identity(&dir.join("missing.pem"), &dir.join("gate.key")).is_err());
    std::fs::write(dir.join("empty.pem"), "").unwrap();
    let e = load_identity(&dir.join("empty.pem"), &dir.join("gate.key")).unwrap_err();
    assert!(e.ends_with("no certificate"), "{e}");
    assert!(load_identity(&dir.join("gate.pem"), &dir.join("missing.key")).is_err());
    assert_eq!(spki_sha256(b"not a certificate"), None);
    assert_ne!(pki.server.pin(), pki.stranger.pin());
}

#[test]
fn a_certificate_that_cannot_be_parsed_is_refused_by_the_verifier() {
    let provider = rustls::crypto::ring::default_provider();
    let verifier = PinnedClient {
        pin: [0; 32],
        algorithms: provider.signature_verification_algorithms,
    };
    let garbage = CertificateDer::from(vec![1, 2, 3]);
    let now = UnixTime::now();
    assert!(matches!(
        verifier.verify_client_cert(&garbage, &[], now),
        Err(rustls::Error::InvalidCertificate(
            CertificateError::BadEncoding
        ))
    ));
    assert!(verifier.root_hint_subjects().is_empty());
    assert!(!verifier.supported_verify_schemes().is_empty());
}
