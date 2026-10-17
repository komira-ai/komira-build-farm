//! Enrollment from the node, against an in-process fake frontend built from the P0 proto.
//! Each test says what it proves and the defect it would catch.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use kbf_enroll::answer::{AnswerError, CertFault};
use kbf_enroll::client::{
    CallError, ConnectError, EnrollError, Enrollment, Frontend, connect, enroll,
};
use kbf_enroll::config::{Config, FrontendUrl, HostName};
use kbf_enroll::pin::Anchor;
use kbf_enroll::store::Store;
use kbf_types::{RefusalReason, Role, RoleSet, Token};
use support::fake::{self, Running, Spoil};
use support::pki::now;
use support::{config_for, facts, scratch};

struct Harness {
    running: Running,
    root: PathBuf,
    config: Config,
    token: Token,
    store: Store,
}

fn harness(name: &str, extra: &str, uses: u32) -> Harness {
    let running = fake::start();
    let root = scratch(name);
    let config = config_for(&root, running.enroll_addr.port(), extra);
    let token = running.fake.mint(0x1234_5678, uses);
    std::fs::write(&config.token_file, token.to_line()).expect("token file");
    std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600))
        .expect("mode");
    let store = Store::open(&config.state_dir).expect("store");
    Harness {
        running,
        root,
        config,
        token,
        store,
    }
}

fn both() -> RoleSet {
    RoleSet::new([Role::Storage, Role::Daemon]).expect("roles")
}

impl Harness {
    async fn frontend(&self) -> Frontend {
        connect(&self.config.frontend, Anchor::Pin(*self.token.pin()))
            .await
            .expect("connect")
    }

    async fn enroll(&self, roles: RoleSet) -> Result<kbf_enroll::store::Node, EnrollError> {
        let frontend = self.frontend().await;
        enroll(
            &frontend,
            &self.store,
            &Enrollment {
                config: &self.config,
                token: &self.token,
                roles,
                facts: facts(),
                now: self.running.fake.now(),
            },
        )
        .await
    }

    fn pki(&self) -> PathBuf {
        self.config.state_dir.join("pki")
    }

    /// Nothing the answer held is installed: no `node.json`, bundle, certificate or key.
    fn assert_nothing_installed(&self) {
        for name in [
            "node.json",
            "ca.pem",
            "storage.pem",
            "daemon.pem",
            "storage.key",
            "daemon.key",
        ] {
            assert!(!self.pki().join(name).exists(), "{name} was installed");
        }
        assert!(
            self.config.token_file.exists(),
            "the token file was removed"
        );
        assert_eq!(self.store.load().expect("load"), None);
    }
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("meta").permissions().mode() & 0o777
}

#[tokio::test]
async fn a_node_with_two_roles_enrolls_and_everything_is_installed_with_its_mode() {
    let h = harness("two-roles", "", 2);
    let node = h.enroll(both()).await.expect("enrolled");

    // The registry (the fake) saw one request with a key per role, the secret and the id.
    let sent = &h.running.fake.enrolls()[0];
    assert_eq!(sent.token_id, h.token.id().to_string());
    assert_eq!(sent.token_secret, h.token.secret().as_bytes());
    assert_eq!(sent.node_id, "node-02");
    assert_eq!(
        sent.csrs
            .iter()
            .map(|c| c.role.as_str())
            .collect::<Vec<_>>(),
        ["storage", "daemon"]
    );
    assert_ne!(
        sent.csrs[0].csr_der, sent.csrs[1].csr_der,
        "one key per role"
    );
    assert_eq!(sent.facts.as_ref().expect("facts").os, "linux");
    assert_eq!(
        h.running.fake.uses(&h.token),
        1,
        "one use for the whole request"
    );

    // The files and their modes.
    assert_eq!(mode(&h.config.state_dir), 0o700);
    assert_eq!(mode(&h.pki()), 0o700);
    for role in ["storage", "daemon"] {
        assert_eq!(
            mode(&h.pki().join(format!("{role}.key"))),
            0o600,
            "{role} key"
        );
        assert_eq!(
            mode(&h.pki().join(format!("{role}.pem"))),
            0o644,
            "{role} certificate"
        );
    }
    assert_eq!(mode(&h.pki().join("node.json")), 0o600);
    assert!(!h.pki().join("staging").exists(), "staging is gone");
    assert!(
        !h.config.token_file.exists(),
        "the token file is removed after enrolling"
    );

    // node.json is what the answer said.
    assert_eq!(h.store.load().expect("load"), Some(node.clone()));
    assert_eq!(node.node_id, "node-02");
    assert_eq!(node.roles(), vec![Role::Storage, Role::Daemon]);
    assert_eq!(node.config.generation, 1);
    assert_eq!(node.config.storage_port, 8983);
    assert_eq!(node.pin().expect("pin"), *h.token.pin());
    assert_eq!(node.renew_after, h.running.fake.now() + fake::LIFE_S / 2);

    // Each certificate is for the key in the file next to it.
    for role in Role::ALL {
        let key = h.store.key_pair(role).expect("key");
        let pem = std::fs::read_to_string(h.store.cert_path(role)).expect("cert");
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        let cert = rustls::pki_types::CertificateDer::from_pem_slice(pem.as_bytes()).expect("pem");
        let (_, parsed) = x509_parser::parse_x509_certificate(&cert).expect("x509");
        assert_eq!(
            parsed.public_key().raw,
            rcgen::PublicKeyData::subject_public_key_info(&key)
        );
    }
    let ca = std::fs::read(h.store.ca_path()).expect("ca");
    assert_eq!(ca, h.running.fake.pki.root_pem().into_bytes());
}

use rustls::pki_types::pem::PemObject;

#[tokio::test]
async fn private_keys_and_the_token_secret_are_written_nowhere_but_where_they_belong() {
    // Catches: a key in a temp file, a log, a copy, or the world-readable staging area;
    // and a token secret left behind after it was used.
    let h = harness("nowhere-else", "", 1);
    h.enroll(both()).await.expect("enrolled");
    let secret = h.token.to_line();
    let secret_part = secret.split('.').nth(2).expect("secret part").to_owned();
    let mut keys_seen = 0;
    for path in files(&h.root) {
        let bytes = std::fs::read(&path).expect("read");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(&secret_part),
            "token secret in {}",
            path.display()
        );
        if text.contains("PRIVATE KEY") {
            keys_seen += 1;
            assert_eq!(mode(&path), 0o600, "{}", path.display());
            assert!(
                path.starts_with(h.pki()),
                "key outside pki/: {}",
                path.display()
            );
            assert!(
                path.extension().is_some_and(|e| e == "key"),
                "{}",
                path.display()
            );
        }
    }
    assert_eq!(keys_seen, 2, "exactly one key file per role");
}

#[tokio::test]
async fn advertise_and_zone_are_sent_and_a_storage_certificate_names_the_advertised_host() {
    let extra = "advertise = \"store-02.farm.example.com:8983\"\nzone = \"rack-a\"";
    let h = harness("advertise", extra, 1);
    h.enroll(RoleSet::new([Role::Storage]).expect("roles"))
        .await
        .expect("enrolled");
    let sent = &h.running.fake.enrolls()[0];
    assert_eq!(sent.advertise, "store-02.farm.example.com:8983");
    assert_eq!(sent.zone, "rack-a");
    assert_eq!(sent.csrs.len(), 1);
    let pem = std::fs::read_to_string(h.store.cert_path(Role::Storage)).expect("cert");
    let der = rustls::pki_types::CertificateDer::from_pem_slice(pem.as_bytes()).expect("pem");
    let (_, parsed) = x509_parser::parse_x509_certificate(&der).expect("x509");
    let names: Vec<_> = parsed
        .subject_alternative_name()
        .expect("san")
        .expect("some")
        .value
        .general_names
        .iter()
        .map(|n| n.to_string())
        .collect();
    assert!(
        names
            .iter()
            .any(|n| n.contains("store-02.farm.example.com")),
        "{names:?}"
    );
}

#[tokio::test]
async fn every_refusal_comes_back_typed_with_its_retry_class_and_installs_nothing() {
    // Catches: refusals flattened into strings (a caller cannot tell an expired token from
    // a used-up one) or a retryable one treated as final.
    for reason in RefusalReason::ALL {
        let h = harness(&format!("refusal-{}", reason.name()), "", 1);
        h.running.fake.refuse_with(Some(reason));
        let err = h.enroll(both()).await.unwrap_err();
        match &err {
            EnrollError::Call(call @ CallError::Refused(got)) => {
                assert_eq!(*got, reason);
                assert_eq!(call.retryable(), reason.code().retryable(), "{reason}");
                assert!(err.to_string().contains(reason.message()), "{err}");
            }
            other => panic!("{reason}: {other:?}"),
        }
        h.assert_nothing_installed();
    }
}

#[tokio::test]
async fn an_expired_token_and_a_used_up_token_are_reported_as_such() {
    let h = harness("token-states", "", 1);
    h.running.fake.expire(&h.token);
    let err = h.enroll(both()).await.unwrap_err();
    assert!(
        matches!(
            err,
            EnrollError::Call(CallError::Refused(RefusalReason::TokenExpired))
        ),
        "{err:?}"
    );
    assert_eq!(h.running.fake.uses(&h.token), 0, "no use consumed");
    h.assert_nothing_installed();

    // One use, two nodes: the second is told the token is used up.
    let h = harness("used-up", "", 1);
    h.enroll(both()).await.expect("first node");
    let second_root = scratch("used-up-second");
    let second_cfg = config_for(&second_root, h.running.enroll_addr.port(), "");
    let second_store = Store::open(&second_cfg.state_dir).expect("store");
    let frontend = h.frontend().await;
    let err = enroll(
        &frontend,
        &second_store,
        &Enrollment {
            config: &second_cfg,
            token: &h.token,
            roles: both(),
            facts: facts(),
            now: h.running.fake.now(),
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            EnrollError::Call(CallError::Refused(RefusalReason::TokenUsedUp))
        ),
        "{err:?}"
    );
    assert_eq!(h.running.fake.uses(&h.token), 1);
}

#[test]
fn a_status_outside_the_closed_set_is_kept_whole() {
    for (status, retryable) in [
        (tonic::Status::internal("boom"), false),
        (tonic::Status::unavailable("down"), true),
        (tonic::Status::deadline_exceeded("slow"), true),
        (tonic::Status::resource_exhausted("other words"), true),
        // The right code with a message the closed set does not hold is not a refusal.
        (tonic::Status::unauthenticated("something new"), false),
    ] {
        let error = CallError::from(status.clone());
        assert!(
            matches!(&error, CallError::Status(s) if s.code() == status.code()),
            "{error:?}"
        );
        assert_eq!(error.retryable(), retryable, "{status:?}");
        assert!(error.to_string().starts_with("the frontend answered"));
    }
}

#[tokio::test]
async fn after_a_lost_answer_the_retry_sends_the_same_requests_and_costs_no_second_use() {
    // Catches: a node that makes new keys on every attempt (a use burned per retry, a
    // registered node whose key it threw away).
    let h = harness("lost-answer", "", 1);
    h.running.fake.lose_next_answer();
    let err = h.enroll(both()).await.unwrap_err();
    assert!(
        matches!(&err, EnrollError::Call(c) if c.retryable()),
        "{err:?}"
    );
    assert_eq!(
        h.running.fake.uses(&h.token),
        1,
        "the frontend committed before the answer was lost"
    );
    h.assert_nothing_installed();

    h.enroll(both()).await.expect("the retry installs");
    let sent = h.running.fake.enrolls();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].csrs, sent[1].csrs, "the same CSR bytes");
    assert_eq!(h.running.fake.uses(&h.token), 1, "still one use");
    assert_eq!(
        h.store
            .load()
            .expect("load")
            .expect("enrolled")
            .roles()
            .len(),
        2
    );
}

#[tokio::test]
async fn a_token_file_that_cannot_be_removed_does_not_undo_the_enrollment() {
    // The node is enrolled when the certificates are installed; the token file is a
    // courtesy removal. Here its path is a directory, which remove_file refuses.
    let h = harness("token-stuck", "", 1);
    std::fs::remove_file(&h.config.token_file).expect("rm");
    std::fs::create_dir(&h.config.token_file).expect("mkdir");
    let node = h.enroll(both()).await.expect("still enrolled");
    assert_eq!(h.store.load().expect("load"), Some(node));
    assert!(h.config.token_file.is_dir());
}

#[tokio::test]
async fn an_enrolled_node_returns_at_once_without_calling_the_frontend() {
    let h = harness("already", "", 1);
    let first = h.enroll(both()).await.expect("enrolled");
    let again = h.enroll(both()).await.expect("idempotent");
    assert_eq!(first, again);
    assert_eq!(h.running.fake.enrolls().len(), 1);
}

#[tokio::test]
async fn an_answer_that_cannot_be_validated_installs_nothing_and_leaves_the_token() {
    // Each spoiled answer is one thing a hostile or broken frontend could send. The node
    // writes no file, keeps the token file, and names what was wrong without the secret.
    type Expected = Box<dyn Fn(&AnswerError) -> bool>;
    let cases: Vec<(Spoil, Expected)> = vec![
        (
            Spoil::Skew(300),
            Box::new(
                |e| matches!(e, AnswerError::ClockSkew { theirs, ours } if theirs - ours == 300),
            ),
        ),
        (
            Spoil::Skew(-121),
            Box::new(|e| matches!(e, AnswerError::ClockSkew { .. })),
        ),
        (Spoil::DropCert, Box::new(|e| *e == AnswerError::RoleSet)),
        (
            Spoil::DuplicateCert,
            Box::new(|e| *e == AnswerError::RoleSet),
        ),
        (Spoil::ExtraCert, Box::new(|e| *e == AnswerError::RoleSet)),
        (Spoil::RenameRole, Box::new(|e| *e == AnswerError::RoleSet)),
        (
            Spoil::BundleNotPem,
            Box::new(|e| *e == AnswerError::PinNotInBundle),
        ),
        (
            Spoil::BundleBadBase64,
            Box::new(|e| *e == AnswerError::Bundle),
        ),
        (
            Spoil::BundleNotCert,
            Box::new(|e| *e == AnswerError::Bundle),
        ),
        (
            Spoil::BundleWithoutPin,
            Box::new(|e| *e == AnswerError::PinNotInBundle),
        ),
        (Spoil::OtherKey, Box::new(|e| cert_fault(e, CertFault::Key))),
        (
            Spoil::OtherName,
            Box::new(|e| cert_fault(e, CertFault::Name)),
        ),
        (
            Spoil::OtherRoleUri,
            Box::new(|e| cert_fault(e, CertFault::Role)),
        ),
        (
            Spoil::BadSerial,
            Box::new(|e| cert_fault(e, CertFault::Fields)),
        ),
        (
            Spoil::BadNotBefore,
            Box::new(|e| cert_fault(e, CertFault::Fields)),
        ),
        (
            Spoil::BadNotAfter,
            Box::new(|e| cert_fault(e, CertFault::Fields)),
        ),
        (
            Spoil::UntrustedSigner,
            Box::new(|e| cert_fault(e, CertFault::Untrusted)),
        ),
        (
            Spoil::Expired,
            Box::new(|e| cert_fault(e, CertFault::Validity)),
        ),
        (
            Spoil::NotYetValid,
            Box::new(|e| cert_fault(e, CertFault::Validity)),
        ),
        (
            Spoil::LateRenewAfter,
            Box::new(|e| *e == AnswerError::RenewAfter),
        ),
        (
            Spoil::GarbageLeaf,
            Box::new(|e| cert_fault(e, CertFault::Parse)),
        ),
        (
            Spoil::TrailingBytes,
            Box::new(|e| cert_fault(e, CertFault::Parse)),
        ),
    ];
    for (spoil, expected) in cases {
        let h = harness(&format!("spoil-{spoil:?}"), "", 1);
        h.running.fake.spoil(Some(spoil.clone()));
        let err = h.enroll(both()).await.unwrap_err();
        match &err {
            EnrollError::Answer(answer) => assert!(expected(answer), "{spoil:?}: {answer:?}"),
            other => panic!("{spoil:?}: {other:?}"),
        }
        let secret = h
            .token
            .to_line()
            .split('.')
            .nth(2)
            .expect("secret")
            .to_owned();
        assert!(!err.to_string().contains(&secret));
        h.assert_nothing_installed();
    }
}

#[tokio::test]
async fn a_certificate_for_a_role_nobody_asked_for_is_refused_not_ignored() {
    // Catches: an answer carrying more than was requested being accepted because every
    // requested role happens to be present.
    let h = harness("extra-role", "", 1);
    h.running.fake.spoil(Some(Spoil::ExtraCert));
    let err = h
        .enroll(RoleSet::new([Role::Storage]).expect("roles"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, EnrollError::Answer(AnswerError::RoleSet)),
        "{err:?}"
    );
    h.assert_nothing_installed();
}

fn cert_fault(error: &AnswerError, want: CertFault) -> bool {
    matches!(error, AnswerError::Cert { role: Role::Storage, fault } if *fault == want)
}

fn url(host: HostName, port: u16) -> FrontendUrl {
    FrontendUrl { host, port }
}

fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

#[tokio::test]
async fn a_frontend_that_cannot_be_reached_is_unreachable_not_untrusted() {
    let pin = Anchor::Pin(kbf_types::CaPin::from_bytes([1; 32]));
    let localhost = || HostName::Dns("localhost".to_owned());

    let err = connect(&url(localhost(), closed_port()), pin.clone())
        .await
        .unwrap_err();
    assert!(matches!(err, ConnectError::Unreachable(_)), "{err}");
    assert!(err.to_string().starts_with("cannot reach the frontend"));

    let err = connect(
        &url(HostName::Ip("::1".parse().expect("ip")), closed_port()),
        pin.clone(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ConnectError::Unreachable(_)), "{err}");

    // A listener that is not TLS at all (it closes the connection) is a failure of the
    // network path, not a verdict on the certificate.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let closer = std::thread::spawn(move || drop(listener.accept()));
    let err = connect(&url(localhost(), port), pin.clone())
        .await
        .unwrap_err();
    closer.join().expect("closer");
    assert!(matches!(err, ConnectError::Unreachable(_)), "{err}");

    // A host that is not a name TLS can check is refused before a connection is tried.
    let err = connect(&url(HostName::Dns("not a name".to_owned()), 443), pin)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a valid server name"), "{err}");
}

#[tokio::test]
async fn an_address_literal_frontend_connects_under_the_pin() {
    let h = harness("ip-frontend", "", 1);
    let ip = url(
        HostName::Ip("127.0.0.1".parse().expect("ip")),
        h.running.enroll_addr.port(),
    );
    connect(&ip, Anchor::Pin(*h.token.pin()))
        .await
        .expect("connects by address");
}

#[test]
fn now_is_a_real_clock_in_these_tests() {
    assert!(now() > 1_700_000_000);
}
