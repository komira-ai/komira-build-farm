//! A listener's certificate, key and client CA, replaced on disk while it serves.
//!
//! Every test makes real TLS handshakes against an [`Incoming`] and reads which
//! certificate the client was served, so what is checked is what a client sees and not
//! what the source says it holds.

mod support;

use std::time::Duration;

use kbf_tls::{Outcome, Reloads, TlsError, TlsSource, render_metrics};
use support::{
    Files, ca, client_leaf, connect, dir, echo, echo_with, ping, put, served, server_leaf, tcp,
    until,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The client sees the certificate that was on disk when its handshake began: the old
/// one before a renewal, the new one after.
///
/// Kills: a source that reads the files once at start.
#[tokio::test]
async fn a_renewed_certificate_serves_new_handshakes() {
    let ca = ca("reload CA");
    let (old, new) = (server_leaf(&ca), server_leaf(&ca));
    let files = Files::write(&dir("renewed"), &old, None);
    let source = files.source();
    let server = echo(tcp(), Some(source.clone()));

    let mut before = connect(server.addr, &ca.pem, None).await.expect("connect");
    ping(&mut before).await.expect("ping");
    assert_eq!(served(&before), old.der);

    files.install(&new);
    until("the renewal is applied", || source.reloads().applied == 1).await;
    let mut after = connect(server.addr, &ca.pem, None).await.expect("connect");
    ping(&mut after).await.expect("ping");
    assert_eq!(served(&after), new.der);
    assert_eq!(source.reloads().rejected, 0);
}

/// A connection that was open at the reload goes on working, on the certificate it was
/// accepted under, and new ones are served the new certificate beside it.
///
/// Kills: a reload that ends the connections of the old configuration.
#[tokio::test]
async fn an_open_connection_survives_a_reload() {
    let ca = ca("survive CA");
    let (old, new) = (server_leaf(&ca), server_leaf(&ca));
    let files = Files::write(&dir("survive"), &old, None);
    let source = files.source();
    let server = echo(tcp(), Some(source.clone()));

    let mut open = connect(server.addr, &ca.pem, None).await.expect("connect");
    ping(&mut open).await.expect("ping before");
    files.install(&new);
    until("the renewal is applied", || source.reloads().applied == 1).await;
    ping(&mut open).await.expect("ping after the reload");
    assert_eq!(served(&open), old.der);

    let mut fresh = connect(server.addr, &ca.pem, None).await.expect("connect");
    ping(&mut fresh).await.expect("ping");
    assert_eq!(served(&fresh), new.der);
    ping(&mut open).await.expect("the old one still answers");
}

/// A renewal written in two steps (certificate, then key) is a mismatch in between: the
/// old pair keeps serving, the mismatch is counted, and the whole pair is applied when
/// the second file lands.
///
/// Kills: a mismatched pair accepted; a swap before the pair is validated.
#[tokio::test]
async fn a_half_written_pair_keeps_the_old_one_until_it_is_whole() {
    let ca = ca("half CA");
    let (old, new) = (server_leaf(&ca), server_leaf(&ca));
    let files = Files::write(&dir("half"), &old, None);
    let source = files.source();
    let server = echo(tcp(), Some(source.clone()));

    put(&files.files.cert, &new.cert);
    until("the mismatch is refused", || source.reloads().rejected == 1).await;
    let mut during = connect(server.addr, &ca.pem, None).await.expect("connect");
    ping(&mut during).await.expect("ping");
    assert_eq!(served(&during), old.der, "a mismatched pair was served");
    assert_eq!(source.reloads().applied, 0);

    put(&files.files.key, &new.key);
    until("the whole pair is applied", || {
        source.reloads().applied == 1
    })
    .await;
    let mut after = connect(server.addr, &ca.pem, None).await.expect("connect");
    ping(&mut after).await.expect("ping");
    assert_eq!(served(&after), new.der);
}

/// What each kind of damage to each file does: nothing to the service, one refusal in
/// the count, and the good files applied when they are back.
///
/// Kills: garbage, a missing file or a CA bundle with nothing usable applied; a bad state
/// counted again at every look.
#[tokio::test]
async fn damaged_files_keep_the_old_configuration_and_count_one_refusal() {
    let ca = ca("garbage CA");
    let client = client_leaf(&ca, "node-1");
    let old = server_leaf(&ca);
    let files = Files::write(&dir("garbage"), &old, Some(&ca.pem));
    let source = files.source();
    let server = echo(tcp(), Some(source.clone()));
    let client_ca = files.files.client_ca.clone().expect("client CA file");

    // PEM with a body that is not base64, text with no PEM in it, a certificate-shaped
    // PEM that is not X.509, and a missing file.
    let not_base64 = "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
    let not_x509 = "-----BEGIN CERTIFICATE-----\naGVsbG8=\n-----END CERTIFICATE-----\n";
    let key_not_base64 = "-----BEGIN PRIVATE KEY-----\n!!!!\n-----END PRIVATE KEY-----\n";
    let damage: [(&std::path::Path, &str, Option<&str>); 8] = [
        (&files.files.cert, "cert is text", Some("not a certificate")),
        (&files.files.cert, "cert is not base64", Some(not_base64)),
        (&files.files.cert, "cert is not X.509", Some(not_x509)),
        (&files.files.cert, "cert is missing", None),
        (&files.files.key, "key is text", Some("not a key")),
        (&files.files.key, "key is not base64", Some(key_not_base64)),
        (&client_ca, "CA is text", Some("not a CA")),
        (&client_ca, "CA holds no usable certificate", Some(not_x509)),
    ];
    let good: [(&std::path::Path, &str); 3] = [
        (&files.files.cert, old.cert.as_str()),
        (&files.files.key, old.key.as_str()),
        (&client_ca, ca.pem.as_str()),
    ];
    for (n, (path, what, bad)) in damage.into_iter().enumerate() {
        let good_text = good
            .iter()
            .find(|(p, _)| *p == path)
            .expect("a good file")
            .1;
        let before = source.reloads();
        match bad {
            Some(text) => put(path, text),
            None => std::fs::remove_file(path).expect("remove"),
        }
        until(what, || source.reloads().rejected == before.rejected + 1).await;

        // Several more looks at the same damage: not counted again, none applied.
        let looks = source.reloads().unchanged;
        until("more looks", || source.reloads().unchanged >= looks + 3).await;
        let after = source.reloads();
        assert_eq!(after.rejected, before.rejected + 1, "{what}: counted again");
        assert_eq!(after.applied, before.applied, "{what}: applied");

        let mut conn = connect(server.addr, &ca.pem, Some(&client))
            .await
            .expect("connect");
        ping(&mut conn).await.expect("ping");
        assert_eq!(
            served(&conn),
            old.der,
            "{what}: the served certificate moved"
        );

        put(path, good_text);
        until("the good file is back", || {
            source.reloads().applied == before.applied + 1
        })
        .await;
        assert_eq!(source.reloads().rejected, before.rejected + 1, "case {n}");
    }
}

/// A renewed certificate whose validity has already ended is not applied; the pair that
/// is still valid keeps serving.
///
/// Kills: an expired certificate replacing a working one.
#[tokio::test]
async fn an_expired_certificate_is_not_applied() {
    let ca = ca("expiry CA");
    let old = server_leaf(&ca);
    let expired = support::leaf(
        &ca,
        "localhost",
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        true,
    );
    let files = Files::write(&dir("expired"), &old, None);
    let source = files.source();
    let server = echo(tcp(), Some(source.clone()));

    put(&files.files.key, &expired.key);
    until("the mismatch is refused", || source.reloads().rejected == 1).await;
    put(&files.files.cert, &expired.cert);
    until("the expired pair is refused", || {
        source.reloads().rejected == 2
    })
    .await;
    assert_eq!(source.reloads().applied, 0);
    let mut conn = connect(server.addr, &ca.pem, None).await.expect("connect");
    ping(&mut conn).await.expect("ping");
    assert_eq!(served(&conn), old.der);
}

/// The server starts with the files it is given, an expired certificate included, as it
/// always has; files that do not make a configuration stop it.
///
/// Kills: a start that refuses an expired certificate; a start that accepts bad files.
#[test]
fn the_first_load_must_work_but_may_be_expired() {
    let ca = ca("start CA");
    let expired = support::leaf(
        &ca,
        "localhost",
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        true,
    );
    let files = Files::write(&dir("start-expired"), &expired, None);
    TlsSource::files("test", files.files.clone(), Duration::from_secs(1))
        .expect("an expired certificate starts");

    let good = server_leaf(&ca);
    let files = Files::write(&dir("start-bad"), &good, Some(&ca.pem));
    let (cert, other) = (good.cert.clone(), server_leaf(&ca));
    let start =
        |files: &Files| TlsSource::files("test", files.files.clone(), Duration::from_secs(1));
    assert!(start(&files).is_ok());

    put(&files.files.key, &other.key);
    assert!(matches!(start(&files), Err(TlsError::Config(_))));
    put(&files.files.key, &good.key);
    put(&files.files.cert, "nothing");
    assert!(matches!(start(&files), Err(TlsError::Pem { .. })));
    put(&files.files.cert, &cert);
    std::fs::remove_file(&files.files.key).expect("remove");
    assert!(matches!(start(&files), Err(TlsError::Read { .. })));
    // The message names the file.
    let message = start(&files).expect_err("an error").to_string();
    assert!(message.contains("server.key"), "{message}");
}

/// The trust roots are reloaded the same way: after the client CA file changes, a
/// certificate of the old CA is refused on a new connection and one of the new CA is
/// accepted, while the connection that was open keeps working; the server sees the
/// client's chain and address on its connection.
///
/// Kills: a source that reloads the identity but keeps the client verifier.
#[tokio::test]
async fn a_reloaded_client_ca_changes_who_may_connect() {
    let (server_ca, ca_one, ca_two) = (ca("server CA"), ca("client CA one"), ca("client CA two"));
    let (one, two) = (
        client_leaf(&ca_one, "node-1"),
        client_leaf(&ca_two, "node-2"),
    );
    let files = Files::write(&dir("mtls"), &server_leaf(&server_ca), Some(&ca_one.pem));
    let source = files.source();
    let mut server = echo(tcp(), Some(source.clone()));
    let client_ca = files.files.client_ca.clone().expect("client CA file");

    let mut kept = connect(server.addr, &server_ca.pem, Some(&one))
        .await
        .expect("connect");
    ping(&mut kept).await.expect("one is accepted");
    let info = server.infos.recv().await.expect("connection info");
    let chain = info.certs.expect("the client's chain");
    assert_eq!(chain[0].as_ref(), one.der);
    let local = kept.get_ref().0.local_addr().expect("local address");
    assert_eq!(info.tcp.remote_addr, Some(local));
    let mut none = connect(server.addr, &server_ca.pem, None)
        .await
        .expect("connect");
    assert!(
        ping(&mut none).await.is_err(),
        "a client with no certificate"
    );

    put(&client_ca, &ca_two.pem);
    until("the new CA is applied", || source.reloads().applied == 1).await;
    let mut old_ca = connect(server.addr, &server_ca.pem, Some(&one))
        .await
        .expect("the handshake ends on the client's side");
    assert!(
        ping(&mut old_ca).await.is_err(),
        "the old CA is still trusted"
    );
    let mut new_ca = connect(server.addr, &server_ca.pem, Some(&two))
        .await
        .expect("connect");
    ping(&mut new_ca).await.expect("two is accepted");
    ping(&mut kept).await.expect("the open connection goes on");

    // A bundle of both CAs trusts both.
    put(&client_ca, &format!("{}{}", ca_one.pem, ca_two.pem));
    until("the bundle is applied", || source.reloads().applied == 2).await;
    for who in [&one, &two] {
        let mut conn = connect(server.addr, &server_ca.pem, Some(who))
            .await
            .expect("connect");
        ping(&mut conn).await.expect("accepted by the bundle");
    }
}

/// A client that connects and says nothing, or says something that is not TLS, is
/// dropped, and delays nobody else.
///
/// Kills: handshakes run one at a time on the accept path; no limit on a silent client.
#[tokio::test]
async fn a_client_that_does_not_handshake_delays_no_one() {
    let ca = ca("slow CA");
    let leaf = server_leaf(&ca);
    let files = Files::write(&dir("slow"), &leaf, None);
    let server = echo_with(
        tcp(),
        Some(files.source()),
        Some(Duration::from_millis(200)),
    );

    let mut silent = TcpStream::connect(server.addr).await.expect("connect");
    let mut babble = TcpStream::connect(server.addr).await.expect("connect");
    babble
        .write_all(b"GET / HTTP/1.1\r\n\r\n")
        .await
        .expect("write");
    let mut conn = connect(server.addr, &ca.pem, None).await.expect("connect");
    ping(&mut conn).await.expect("served while the others hang");

    let mut buf = [0u8; 16];
    for dropped in [&mut silent, &mut babble] {
        let read = tokio::time::timeout(Duration::from_secs(10), dropped.read(&mut buf))
            .await
            .expect("the server dropped it");
        // The close, or a TLS alert (record type 21): never a byte of a session.
        if let Ok(n) = read {
            assert!(n == 0 || buf[0] == 0x15, "{n} bytes: {:?}", &buf[..n]);
        }
    }
}

/// Without a source the connections are plain, with no certificates, and the handshake
/// limit is not a thing.
#[tokio::test]
async fn without_tls_connections_are_plain() {
    let mut server = echo_with(tcp(), None, Some(Duration::from_secs(1)));
    let mut conn = TcpStream::connect(server.addr).await.expect("connect");
    conn.write_all(b"abc").await.expect("write");
    let mut back = [0u8; 3];
    conn.read_exact(&mut back).await.expect("read");
    assert_eq!(&back, b"abc");
    let info = server.infos.recv().await.expect("connection info");
    assert!(info.certs.is_none());
    assert_eq!(info.tcp.remote_addr, conn.local_addr().ok());
}

/// The counters and the expiry gauge: one header per family however many sources, a row
/// per listener and outcome, and nothing for no source.
#[tokio::test]
async fn the_metrics_name_each_listener_and_outcome() {
    let ca = ca("metrics CA");
    let (a, b) = (server_leaf(&ca), server_leaf(&ca));
    let files = Files::write(&dir("metrics"), &a, None);
    let first =
        TlsSource::files("reapi", files.files.clone(), Duration::from_secs(3600)).expect("source");
    let second =
        TlsSource::files("worker", files.files.clone(), Duration::from_secs(3600)).expect("source");

    assert_eq!(first.reload(), Outcome::Unchanged);
    files.install(&b);
    assert_eq!(first.reload(), Outcome::Applied);
    put(&files.files.cert, "garbage");
    assert_eq!(first.reload(), Outcome::Rejected);
    assert_eq!(
        first.reloads(),
        Reloads {
            applied: 1,
            rejected: 1,
            unchanged: 1
        }
    );

    let mut text = String::new();
    render_metrics([&first, &second], &mut text);
    for want in [
        "kbf_tls_reloads_total{listener=\"reapi\",outcome=\"applied\"} 1\n",
        "kbf_tls_reloads_total{listener=\"reapi\",outcome=\"rejected\"} 1\n",
        "kbf_tls_reloads_total{listener=\"reapi\",outcome=\"unchanged\"} 1\n",
        "kbf_tls_reloads_total{listener=\"worker\",outcome=\"applied\"} 0\n",
    ] {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
    assert_eq!(text.matches("# TYPE kbf_tls_reloads_total").count(), 1);
    assert_eq!(
        text.matches("# TYPE kbf_tls_leaf_not_after_seconds")
            .count(),
        1
    );
    let gauge = |listener: &str| -> i64 {
        let prefix = format!("kbf_tls_leaf_not_after_seconds{{listener=\"{listener}\"}} ");
        let line = text
            .lines()
            .find(|l| l.starts_with(&prefix))
            .expect("gauge row");
        line[prefix.len()..].parse().expect("a number")
    };
    // The reapi source serves the pair it applied (b), the worker one still serves a: two
    // leaves made a moment apart, valid for years.
    assert!(gauge("reapi") > 1_800_000_000 && gauge("worker") > 1_800_000_000);

    let mut nothing = String::new();
    render_metrics(std::iter::empty(), &mut nothing);
    assert!(nothing.is_empty());
    assert!(format!("{first:?}").contains("reapi"));
}

/// The helpers the services call: the chain and address of a request that came through
/// an [`Incoming`], and the absence of both on one that did not.
#[test]
fn request_helpers_read_the_connection_info() {
    use kbf_tls::{ConnInfo, peer_certs, peer_certs_of, remote_addr};
    use std::sync::Arc;
    use tonic::transport::server::TcpConnectInfo;

    let bare = tonic::Request::new(());
    assert!(peer_certs(&bare).is_none() && remote_addr(&bare).is_none());
    assert!(peer_certs_of(&tonic::codegen::http::Extensions::new()).is_none());

    let addr = "127.0.0.1:9".parse().expect("address");
    let ca = ca("helper CA");
    let der = rustls::pki_types::CertificateDer::from(server_leaf(&ca).der);
    let mut request = tonic::Request::new(());
    request.extensions_mut().insert(ConnInfo {
        tcp: TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(addr),
        },
        certs: Some(Arc::new(vec![der.clone()])),
    });
    assert_eq!(peer_certs(&request).expect("chain")[0], der);
    assert_eq!(remote_addr(&request), Some(addr));
    // What a layer sees: the HTTP request's extensions carry the same connect info.
    let mut extensions = tonic::codegen::http::Extensions::new();
    extensions.insert(
        request
            .extensions()
            .get::<ConnInfo>()
            .expect("info")
            .clone(),
    );
    assert_eq!(peer_certs_of(&extensions).expect("chain")[0], der);
}

/// A source made of PEM text has no files: it serves what it was given, a look at it
/// finds nothing new, and a bad text is refused when it is made.
#[tokio::test]
async fn a_fixed_source_serves_its_text_and_never_changes() {
    let ca = ca("fixed CA");
    let leaf = server_leaf(&ca);
    let source = TlsSource::fixed("test", leaf.cert.as_bytes(), leaf.key.as_bytes(), None)
        .expect("a fixed source");
    let server = echo(tcp(), Some(source.clone()));
    assert_eq!(source.reload(), Outcome::Unchanged);
    let mut conn = connect(server.addr, &ca.pem, None).await.expect("connect");
    ping(&mut conn).await.expect("ping");
    assert_eq!(served(&conn), leaf.der);

    let bad = TlsSource::fixed("test", b"", leaf.key.as_bytes(), Some(ca.pem.as_bytes()));
    assert!(matches!(bad, Err(TlsError::Pem { .. })));
    let named = bad.expect_err("an error").to_string();
    assert!(named.contains("<certificate>"), "{named}");
    let with_ca = TlsSource::fixed(
        "test",
        leaf.cert.as_bytes(),
        leaf.key.as_bytes(),
        Some(ca.pem.as_bytes()),
    );
    assert!(with_ca.is_ok());
}
