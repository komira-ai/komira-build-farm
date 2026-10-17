//! First-contact trust. The enroll listener is checked against the CA fingerprint in the
//! token, not against any public CA. Each test names the defect it catches.

mod support;

use std::sync::Arc;

use kbf_enroll::config::{FrontendUrl, HostName};
use kbf_enroll::pin::{Anchor, AnchorError, PinVerifier, client_config, pin_of, system_roots};
use kbf_types::CaPin;
use rcgen::ExtendedKeyUsagePurpose as Eku;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, Error, RootCertStore};
use support::fake::{self, Fake};
use support::pki::{FrontendCert, Pki, now};

fn verifier(pin: CaPin) -> PinVerifier {
    PinVerifier::new(pin, Arc::new(rustls::crypto::ring::default_provider()))
}

fn der(pem: &str) -> Vec<CertificateDer<'static>> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<_, _>>()
        .expect("pem")
}

/// Runs the verifier on a chain `[leaf, ...rest]` for `host`.
fn verify(v: &PinVerifier, chain: &[CertificateDer<'static>], host: &str) -> Result<(), Error> {
    let name = ServerName::try_from(host.to_owned()).expect("name");
    v.verify_server_cert(
        &chain[0],
        &chain[1..],
        &name,
        &[],
        UnixTime::since_unix_epoch(std::time::Duration::from_secs(now() as u64)),
    )
    .map(|_| ())
}

fn fault(e: &Error) -> String {
    format!("{e:?}")
}

#[test]
fn the_pinned_chain_for_the_right_name_is_accepted() {
    let pki = Pki::new();
    let id = pki.frontend(&FrontendCert::good());
    let chain = der(&id.chain_pem);
    assert_eq!(pin_of(chain[1].as_ref()), *pki.pin().as_bytes());
    verify(&verifier(pki.pin()), &chain, "localhost").expect("accepted");
    // An address literal is checked against the address in the certificate.
    let name = ServerName::try_from("127.0.0.1").expect("ip");
    verifier(pki.pin())
        .verify_server_cert(&chain[0], &chain[1..], &name, &[], UnixTime::now())
        .expect("ip accepted");
}

#[test]
fn another_pin_is_refused_even_for_a_chain_that_is_otherwise_valid() {
    // Catches: trusting the first connection, or any chain that verifies under some CA.
    let (pki, other) = (Pki::new(), Pki::named("another"));
    let chain = der(&pki.frontend(&FrontendCert::good()).chain_pem);
    let err = verify(&verifier(other.pin()), &chain, "localhost").unwrap_err();
    assert!(fault(&err).contains("NotInChain"), "{err:?}");
}

#[test]
fn a_chain_that_omits_the_pinned_root_is_refused() {
    // The listener must send the root (design 5.2), or a node holding only the pin cannot verify it.
    let pki = Pki::new();
    let chain = der(&pki.frontend(&FrontendCert::good()).chain_pem);
    let err = verify(&verifier(pki.pin()), &chain[..1], "localhost").unwrap_err();
    assert!(fault(&err).contains("NotInChain"), "{err:?}");
}

#[test]
fn a_leaf_signed_by_another_ca_under_the_pinned_root_in_the_chain_is_refused() {
    // Catches: matching the pin and then verifying nothing. The impostor sends the real
    // root next to its own leaf.
    let (pki, impostor) = (Pki::new(), Pki::named("impostor"));
    let mut chain = der(&impostor.frontend(&FrontendCert::good()).chain_pem);
    chain[1] = der(&pki.root_pem()).remove(0);
    let err = verify(&verifier(pki.pin()), &chain, "localhost").unwrap_err();
    assert!(matches!(err, Error::InvalidCertificate(_)), "{err:?}");
}

#[test]
fn the_name_must_match() {
    let pki = Pki::new();
    let chain = der(&pki.frontend(&FrontendCert::good()).chain_pem);
    let err = verify(&verifier(pki.pin()), &chain, "build.example.com").unwrap_err();
    assert!(
        matches!(
            err,
            Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. }
            )
        ),
        "{err:?}"
    );
}

#[test]
fn an_expired_leaf_is_refused() {
    let pki = Pki::new();
    let spec = FrontendCert {
        not_before: now() - 10 * 86_400,
        not_after: now() - 86_400,
        ..FrontendCert::good()
    };
    let chain = der(&pki.frontend(&spec).chain_pem);
    let err = verify(&verifier(pki.pin()), &chain, "localhost").unwrap_err();
    assert!(
        matches!(
            err,
            Error::InvalidCertificate(
                CertificateError::Expired | CertificateError::ExpiredContext { .. }
            )
        ),
        "{err:?}"
    );
}

#[test]
fn a_leaf_without_server_auth_is_refused() {
    let pki = Pki::new();
    let spec = FrontendCert {
        eku: vec![Eku::ClientAuth],
        ..FrontendCert::good()
    };
    let chain = der(&pki.frontend(&spec).chain_pem);
    let err = verify(&verifier(pki.pin()), &chain, "localhost").unwrap_err();
    assert!(matches!(err, Error::InvalidCertificate(_)), "{err:?}");
}

#[test]
fn a_certificate_of_another_role_cannot_stand_in_for_the_frontend() {
    // Catches: any enrolled node impersonating the enrollment server with the certificate
    // it was given. Both the wrong role and no role are refused.
    let pki = Pki::new();
    for uri in [
        Some("urn:kbf:role:storage"),
        Some("urn:kbf:role:daemon"),
        None,
    ] {
        let spec = FrontendCert {
            role_uri: uri,
            ..FrontendCert::good()
        };
        let chain = der(&pki.frontend(&spec).chain_pem);
        let err = verify(&verifier(pki.pin()), &chain, "localhost").unwrap_err();
        assert!(fault(&err).contains("NotFrontend"), "{uri:?}: {err:?}");
    }
}

#[tokio::test]
async fn a_tls12_only_listener_is_verified_by_the_same_pin() {
    // The node does not insist on TLS 1.3: a listener behind a proxy may speak 1.2, and
    // its signature is then checked by the pin verifier's TLS 1.2 path.
    use rustls::pki_types::PrivateKeyDer;
    use rustls::pki_types::pem::PemObject;
    let pki = Pki::new();
    let id = pki.frontend(&FrontendCert::good());
    let server = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_no_client_auth()
        .with_single_cert(
            der(&id.chain_pem),
            PrivateKeyDer::from_pem_slice(id.key_pem.as_bytes()).expect("key"),
        )
        .expect("server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let served = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        acceptor
            .accept(tcp)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    });
    let client = tokio_rustls::TlsConnector::from(Arc::new(
        client_config(Anchor::Pin(pki.pin())).expect("config"),
    ));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let tls = client
        .connect(ServerName::try_from("localhost").expect("name"), tcp)
        .await
        .expect("handshake under the pin");
    assert_eq!(
        tls.get_ref().1.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_2)
    );
    drop(tls);
    let _ = served.await;
}

#[test]
fn the_public_fallback_needs_roots_and_uses_them() {
    // `tls.trust = "public"`: no roots is an error, not a client that trusts nothing silently.
    let err = client_config(Anchor::Roots(Arc::new(RootCertStore::empty()))).unwrap_err();
    assert!(matches!(err, AnchorError::NoRoots(_)), "{err}");
    let mut roots = RootCertStore::empty();
    roots
        .add(der(&Pki::new().root_pem()).remove(0))
        .expect("root");
    let config = client_config(Anchor::Roots(Arc::new(roots))).expect("config");
    assert_eq!(config.alpn_protocols, vec![b"h2".to_vec()]);
    client_config(Anchor::Pin(CaPin::from_bytes([1; 32]))).expect("pin config");
    // Whatever the host trusts, reading it does not fail.
    let _ = system_roots();
}

fn url(port: u16) -> FrontendUrl {
    FrontendUrl {
        host: HostName::Dns("localhost".to_owned()),
        port,
    }
}

#[tokio::test]
async fn a_node_connects_under_the_pin_and_refuses_a_listener_under_another_ca() {
    let running = fake::start();
    let port = running.enroll_addr.port();
    let pin = running.fake.pki.pin();
    kbf_enroll::client::connect(&url(port), Anchor::Pin(pin))
        .await
        .expect("connects under the right pin");

    // Catches: a pin mismatch reported as "the network is down" (a node would retry
    // forever instead of telling the operator), or accepted.
    let err = kbf_enroll::client::connect(&url(port), Anchor::Pin(CaPin::from_bytes([5; 32])))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, kbf_enroll::client::ConnectError::Untrusted(m) if m.contains("NotInChain")),
        "{err}"
    );
}

#[tokio::test]
async fn public_trust_verifies_the_name_with_the_roots_it_is_given() {
    let fake = Fake::new();
    let running = fake::start_with(fake.clone(), &FrontendCert::good());
    let mut roots = RootCertStore::empty();
    roots
        .add(der(&fake.pki.root_pem()).remove(0))
        .expect("root");
    kbf_enroll::client::connect(
        &url(running.enroll_addr.port()),
        Anchor::Roots(Arc::new(roots)),
    )
    .await
    .expect("trusted by roots");
    // Roots that do not include the CA refuse the listener, as untrusted.
    let mut other = RootCertStore::empty();
    other
        .add(der(&Pki::named("elsewhere").root_pem()).remove(0))
        .expect("root");
    let err = kbf_enroll::client::connect(
        &url(running.enroll_addr.port()),
        Anchor::Roots(Arc::new(other)),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, kbf_enroll::client::ConnectError::Untrusted(_)),
        "{err}"
    );
}
