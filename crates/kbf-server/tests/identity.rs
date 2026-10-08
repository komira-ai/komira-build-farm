//! The node binding and the deny list of the worker listener (issue #79), without a
//! network: certificates are made here and handed over as DER.

use std::path::PathBuf;

use kbf_server::identity::{Denied, DenyList, DenyListError, PeerCert, Peers};
use rcgen::{CertificateParams, DnType, KeyPair, SanType, SerialNumber};
use tonic::Code;

/// A self-signed certificate with these subjectAltNames, common name and serial.
fn cert(sans: Vec<SanType>, cn: &str, serial: &[u8]) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = sans;
    params.distinguished_name.push(DnType::CommonName, cn);
    params.serial_number = Some(SerialNumber::from(serial.to_vec()));
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("sign").der().to_vec()
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

fn peer(sans: Vec<SanType>) -> PeerCert {
    PeerCert::from_der(&cert(sans, "ignored", &[1])).expect("a certificate")
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-identity");
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir.join(name)
}

/// Catches: a node id taken from the common name (a certificate with CN `node-b` and
/// no DNS name would pass as `node-b`), a certificate naming several nodes accepted for
/// any of them, an IP or other subjectAltName read as a node, and a mismatch accepted.
#[test]
fn a_certificate_names_exactly_one_node_by_dns_name() {
    let ip = SanType::IpAddress([10, 0, 0, 1].into());
    let a = peer(vec![ip.clone(), dns("node-a")]);
    assert!(a.names("node-a").is_ok());
    let other = a
        .names("node-b")
        .expect_err("node-a's certificate as node-b");
    assert_eq!(other.code(), Code::PermissionDenied);
    assert!(
        other.message().contains("\"node-a\""),
        "{}",
        other.message()
    );
    assert_eq!(
        a.names("NODE-A").expect_err("compared exactly").code(),
        Code::PermissionDenied
    );

    let cn_only = PeerCert::from_der(&cert(vec![], "node-b", &[1])).expect("certificate");
    let two = peer(vec![dns("node-a"), dns("node-b")]);
    let ip_only = peer(vec![ip]);
    for (bad, count) in [(cn_only, 0), (two, 2), (ip_only, 0)] {
        for node in ["node-a", "node-b"] {
            let refused = bad.names(node).expect_err("not exactly one DNS name");
            assert_eq!(refused.code(), Code::PermissionDenied);
            assert!(refused.message().ends_with(&format!("names {count}")));
        }
    }
}

/// Catches: a serial compared in a spelling the deny list does not use (the DER sign
/// byte, or upper case), and a public-key hash of the wrong bytes.
#[test]
fn serial_and_public_key_are_spelled_as_the_deny_list_spells_them() {
    // 0x8f has its high bit set, so DER prefixes a zero byte; the list never writes it.
    let der = cert(vec![dns("node-a")], "node-a", &[0x8f, 0x0a]);
    let peer = PeerCert::from_der(&der).expect("certificate");
    assert_eq!(peer.serial(), "8f0a");
    let (_, parsed) = x509_parser::parse_x509_certificate(&der).expect("parse");
    let spki = {
        use sha2::Digest;
        sha2::Sha256::digest(parsed.public_key().raw)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert_eq!(peer.spki_sha256(), spki);
    assert_eq!(peer.spki_sha256().len(), 64);

    let zero = PeerCert::from_der(&cert(vec![], "z", &[0])).expect("certificate");
    assert_eq!(zero.serial(), "0");
}

/// Catches: bytes that are not a certificate taken for one with no names.
#[test]
fn garbage_is_not_a_certificate() {
    let error = PeerCert::from_der(b"not a certificate").expect_err("garbage");
    assert!(error.contains("does not parse"), "{error}");
}

/// Catches: entries silently dropped or misread (a typo would leave a certificate
/// trusted), comments and blank lines refused, and spellings of the same serial that
/// do not match.
#[test]
fn the_deny_list_parses_every_entry_or_names_the_bad_line() {
    let denied = Denied::parse(
        "# retired nodes\n\
         \n\
         serial 00:8F:0A   # leading zeros, colons and case are spelling only\n\
         spki-sha256 ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n\
         node mac-07\n   \n",
    )
    .expect("a good list");
    let a = PeerCert::from_der(&cert(vec![dns("node-a")], "node-a", &[0x8f, 0x0a])).expect("c");
    assert_eq!(
        denied.refuses(Some(&a), "node-a"),
        Some("certificate serial 8f0a".to_owned())
    );
    assert_eq!(
        denied.refuses(None, "mac-07"),
        Some("node mac-07".to_owned())
    );
    assert_eq!(denied.refuses(None, "mac-08"), None);
    let other = peer(vec![dns("node-a")]);
    assert_eq!(denied.refuses(Some(&other), "node-a"), None);

    let by_key = Denied::parse(&format!(
        "spki-sha256 {}",
        other.spki_sha256().to_uppercase()
    ))
    .expect("a key entry");
    assert_eq!(
        by_key.refuses(Some(&other), "node-a"),
        Some(format!("public key {}", other.spki_sha256()))
    );
    assert_eq!(by_key.refuses(Some(&a), "node-a"), None);
    assert_eq!(Denied::parse(""), Ok(Denied::default()));

    for (text, line, why) in [
        ("serial", 1, "serial needs a value"),
        ("\nserial 0a extra", 2, "one kind and one value"),
        ("serial :", 1, "a serial is hex digits"),
        ("serial 0x1", 1, "a serial is hex digits"),
        ("spki-sha256 abcd", 1, "64 hex digits"),
        (
            &format!("spki-sha256 {}", "g".repeat(64)),
            1,
            "64 hex digits",
        ),
        ("fingerprint ab", 1, "unknown entry kind \"fingerprint\""),
    ] {
        let (at, message) = Denied::parse(text).expect_err(text);
        assert_eq!(at, line, "{text}");
        assert!(message.contains(why), "{text}: {message}");
    }
}

/// Catches: a server that starts with a deny list it cannot read or parse (the first
/// daemon would find out instead of the operator).
#[test]
fn a_bad_deny_list_is_refused_at_open() {
    let missing = scratch("missing.deny");
    let _ = std::fs::remove_file(&missing);
    assert!(matches!(
        DenyList::open(&missing),
        Err(DenyListError::Read { path, .. }) if path == missing
    ));
    let bad = scratch("bad.deny");
    std::fs::write(&bad, "node a\nserial zz\n").expect("write");
    let error = DenyList::open(&bad).expect_err("a bad line");
    assert!(matches!(error, DenyListError::Parse { line: 2, .. }));
    assert!(
        error
            .to_string()
            .ends_with("bad.deny:2: a serial is hex digits, colons allowed: \"zz\""),
        "{error}"
    );
}

/// Catches: plain text that asks for a certificate (every test cell would be refused),
/// mutual TLS that lets a stream through without one or with one it cannot read, and
/// a certificate that is read but not returned for the stream.
#[test]
fn a_certified_listener_needs_a_readable_certificate() {
    let der = cert(vec![dns("node-a")], "node-a", &[7]);
    assert_eq!(
        Peers::Unauthenticated.peer(Some(&der)).expect("plain"),
        None
    );
    assert_eq!(Peers::Unauthenticated.peer(None).expect("plain"), None);
    let certified = Peers::Certified { deny_list: None };
    let none = certified.peer(None).expect_err("no certificate");
    assert_eq!(none.code(), Code::Unauthenticated);
    let garbage = certified.peer(Some(b"junk")).expect_err("junk");
    assert_eq!(garbage.code(), Code::Unauthenticated);
    let peer = certified
        .peer(Some(&der))
        .expect("a certificate")
        .expect("some");
    assert_eq!(peer.serial(), "7");
}

/// Catches (issue #79): the node check skipped at admission, the deny list read only
/// when the server starts (an entry added later must refuse the next check), and a
/// deny list that breaks after start letting every stream through (it must fail
/// closed).
#[tokio::test]
async fn admission_binds_the_node_and_reads_the_deny_list_each_time() {
    let a = peer(vec![dns("node-a")]);
    assert!(Peers::Unauthenticated.admit(None, "anything").await.is_ok());
    let without_list = Peers::Certified { deny_list: None };
    assert!(without_list.admit(Some(&a), "node-a").await.is_ok());
    assert_eq!(
        without_list
            .admit(Some(&a), "node-b")
            .await
            .expect_err("bound")
            .code(),
        Code::PermissionDenied
    );

    let path = scratch("admit.deny");
    std::fs::write(&path, "# nothing denied yet\n").expect("write");
    let with_list = Peers::Certified {
        deny_list: Some(DenyList::open(&path).expect("open")),
    };
    assert!(with_list.admit(Some(&a), "node-a").await.is_ok());
    assert_eq!(
        with_list
            .admit(Some(&a), "node-b")
            .await
            .expect_err("bound")
            .code(),
        Code::PermissionDenied
    );

    std::fs::write(&path, format!("serial {}\n", a.serial())).expect("deny node-a's cert");
    let denied = with_list
        .admit(Some(&a), "node-a")
        .await
        .expect_err("denied");
    assert_eq!(denied.code(), Code::PermissionDenied);
    assert!(
        denied.message().contains("certificate serial"),
        "{}",
        denied.message()
    );

    std::fs::write(&path, "serial nothex\n").expect("break the list");
    let broken = with_list
        .admit(Some(&a), "node-a")
        .await
        .expect_err("fail closed");
    assert_eq!(broken.code(), Code::Unavailable);
    std::fs::remove_file(&path).expect("remove the list");
    let gone = with_list
        .admit(Some(&a), "node-a")
        .await
        .expect_err("fail closed");
    assert_eq!(gone.code(), Code::Unavailable);
}
