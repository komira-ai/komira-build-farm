//! Renewal: at half life, by the registered key, with no token, and never at the cost of
//! the pair the node already has.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use kbf_enroll::ISSUED_AT_OID;
use kbf_enroll::answer::AnswerError;
use kbf_enroll::client::{CallError, Enrollment, Frontend, connect, enroll};
use kbf_enroll::config::Config;
use kbf_enroll::pin::Anchor;
use kbf_enroll::renew::{
    Backoff, JITTER_PERMILLE, RETRY_FIRST_S, RETRY_MAX_S, RenewError, WARN_BELOW_S, first_attempt,
    near_expiry, renew,
};
use kbf_enroll::store::{Node, Store, StoreError};
use kbf_types::{RefusalReason, Role, RoleSet, Token};
use support::fake::{self, LIFE_S, Running, Spoil};
use support::{config_for, facts, scratch};
use x509_parser::prelude::{FromDer, X509CertificationRequest};

struct Harness {
    running: Running,
    config: Config,
    token: Token,
    store: Store,
    node: Node,
}

async fn enrolled(name: &str) -> Harness {
    let running = fake::start();
    let root = scratch(name);
    let config = config_for(&root, running.enroll_addr.port(), "");
    let token = running.fake.mint(0x2222, 1);
    std::fs::write(&config.token_file, token.to_line()).expect("token file");
    std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600))
        .expect("mode");
    let store = Store::open(&config.state_dir).expect("store");
    let frontend = connect(&config.frontend, Anchor::Pin(*token.pin()))
        .await
        .expect("connect");
    let node = enroll(
        &frontend,
        &store,
        &Enrollment {
            config: &config,
            token: &token,
            roles: RoleSet::new([Role::Storage, Role::Daemon]).expect("roles"),
            facts: facts(),
            now: running.fake.now(),
        },
    )
    .await
    .expect("enrolled");
    Harness {
        running,
        config,
        token,
        store,
        node,
    }
}

impl Harness {
    async fn frontend(&self) -> Frontend {
        connect(&self.config.frontend, Anchor::Pin(*self.token.pin()))
            .await
            .expect("connect")
    }

    fn bytes(&self, path: PathBuf) -> Vec<u8> {
        std::fs::read(path).expect("read")
    }
}

#[tokio::test]
async fn at_half_life_the_node_renews_with_its_own_key_and_no_token() {
    let h = enrolled("renew-half").await;
    let old_key: Vec<_> = Role::ALL
        .iter()
        .map(|r| h.bytes(h.store.key_path(*r)))
        .collect();
    let old_cert: Vec<_> = Role::ALL
        .iter()
        .map(|r| h.bytes(h.store.cert_path(*r)))
        .collect();
    assert!(
        !h.config.token_file.exists(),
        "there is no token to present"
    );

    let half = h.node.renew_after;
    h.running.fake.set_now(half);
    let renewed = renew(&h.frontend().await, &h.store, half)
        .await
        .expect("renewed");

    // The request: the registered node, the time, the generation the node holds.
    let sent = &h.running.fake.renews()[0];
    assert_eq!(
        (
            sent.node_id.as_str(),
            sent.issued_at,
            sent.config_generation
        ),
        ("node-02", half, 1)
    );
    assert_eq!(sent.csrs.len(), 2);

    // New serials and expiry; the same keys, byte for byte; certificates replaced.
    for (i, role) in Role::ALL.iter().enumerate() {
        assert_eq!(
            h.bytes(h.store.key_path(*role)),
            old_key[i],
            "{role}: the key is not rewritten"
        );
        assert_ne!(
            h.bytes(h.store.cert_path(*role)),
            old_cert[i],
            "{role}: the certificate is replaced"
        );
        assert_eq!(
            std::fs::metadata(h.store.key_path(*role))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let before = &h
            .node
            .certs
            .iter()
            .find(|c| c.role == role.name())
            .expect("cert");
        let after = &renewed
            .certs
            .iter()
            .find(|c| c.role == role.name())
            .expect("cert");
        assert_ne!(before.serial, after.serial);
        assert!(after.not_after > before.not_after);
    }
    assert_eq!(h.store.load().expect("load"), Some(renewed.clone()));
    assert_eq!(renewed.renew_after, half + LIFE_S / 2);
    assert_eq!(
        renewed.config, h.node.config,
        "no config in the answer keeps the one held"
    );
    assert!(!h.store.ca_path().with_file_name("staging").exists());
    assert_eq!(
        h.running.fake.uses(&h.token),
        1,
        "renewal costs no token use"
    );
}

#[tokio::test]
async fn a_changed_configuration_arrives_with_the_renewal() {
    let h = enrolled("renew-config").await;
    h.running.fake.set_generation(2);
    let renewed = renew(&h.frontend().await, &h.store, h.running.fake.now())
        .await
        .expect("renewed");
    assert_eq!(renewed.config.generation, 2);
    assert_eq!(
        h.store
            .load()
            .expect("load")
            .expect("node")
            .config
            .generation,
        2
    );
}

#[tokio::test]
async fn a_node_whose_certificates_expired_recovers_without_a_token() {
    // Catches: a node locked out by an outage or a sleep longer than the certificate's life.
    let h = enrolled("renew-expired").await;
    let later = h.running.fake.now() + 2 * LIFE_S;
    h.running.fake.set_now(later);
    let renewed = renew(&h.frontend().await, &h.store, later)
        .await
        .expect("renewed after expiry");
    assert!(renewed.certs.iter().all(|c| c.not_after > later));
}

#[tokio::test]
async fn the_request_is_signed_by_the_registered_key_and_binds_its_time() {
    let h = enrolled("renew-csr").await;
    renew(&h.frontend().await, &h.store, h.running.fake.now())
        .await
        .expect("renewed");
    let sent = &h.running.fake.renews()[0];
    for csr in &sent.csrs {
        let (_, parsed) = X509CertificationRequest::from_der(&csr.csr_der).expect("csr");
        parsed.verify_signature().expect("proof of possession");
        let oid = ISSUED_AT_OID
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(".");
        let attrs = parsed
            .certification_request_info
            .iter_attributes()
            .collect::<Vec<_>>();
        assert!(
            attrs.iter().any(|a| a.oid.to_id_string() == oid),
            "issued_at attribute present"
        );
        // Changing the time inside the request breaks the signature: it is inside the signed part.
        let mut stamp = vec![0x31, 0x0a, 0x04, 0x08];
        stamp.extend_from_slice(&sent.issued_at.to_be_bytes());
        let at = csr
            .csr_der
            .windows(stamp.len())
            .position(|w| w == stamp.as_slice())
            .expect("stamp");
        let mut forged = csr.csr_der.clone();
        forged[at + 11] ^= 1;
        let (_, parsed) = X509CertificationRequest::from_der(&forged).expect("still parses");
        assert!(
            parsed.verify_signature().is_err(),
            "a changed time must not verify"
        );
    }
}

#[tokio::test]
async fn a_node_that_has_not_enrolled_has_nothing_to_renew() {
    let running = fake::start();
    let root = scratch("renew-none");
    let config = config_for(&root, running.enroll_addr.port(), "");
    let store = Store::open(&config.state_dir).expect("store");
    let frontend = connect(&config.frontend, Anchor::Pin(running.fake.pki.pin()))
        .await
        .expect("connect");
    let err = renew(&frontend, &store, 1).await.unwrap_err();
    assert!(matches!(err, RenewError::NotEnrolled), "{err:?}");
    assert_eq!(err.to_string(), "this node is not enrolled");
}

#[tokio::test]
async fn refusals_to_renew_are_typed_and_leave_the_installed_pair_alone() {
    let h = enrolled("renew-refused").await;
    let before: Vec<_> = Role::ALL
        .iter()
        .map(|r| h.bytes(h.store.cert_path(*r)))
        .collect();
    for reason in [
        RefusalReason::NodeRevoked,
        RefusalReason::UnknownNode,
        RefusalReason::RenewalExpired,
        RefusalReason::WrongKey,
        RefusalReason::RenewTooSoon,
        RefusalReason::StoreUnavailable,
    ] {
        h.running.fake.refuse_with(Some(reason));
        let err = renew(&h.frontend().await, &h.store, h.running.fake.now())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RenewError::Call(CallError::Refused(r)) if *r == reason),
            "{err:?}"
        );
    }
    let after: Vec<_> = Role::ALL
        .iter()
        .map(|r| h.bytes(h.store.cert_path(*r)))
        .collect();
    assert_eq!(before, after);
    assert_eq!(h.store.load().expect("load"), Some(h.node.clone()));
}

#[tokio::test]
async fn a_renewal_answer_that_cannot_be_validated_keeps_the_old_pair() {
    // The same answer checks as enrollment, on the renew path.
    for spoil in [
        Spoil::Skew(500),
        Spoil::OtherKey,
        Spoil::UntrustedSigner,
        Spoil::Expired,
        Spoil::LateRenewAfter,
    ] {
        let h = enrolled(&format!("renew-spoil-{spoil:?}")).await;
        let before: Vec<_> = Role::ALL
            .iter()
            .map(|r| h.bytes(h.store.cert_path(*r)))
            .collect();
        h.running.fake.spoil(Some(spoil.clone()));
        let err = renew(&h.frontend().await, &h.store, h.running.fake.now())
            .await
            .unwrap_err();
        assert!(matches!(err, RenewError::Answer(_)), "{spoil:?}: {err:?}");
        let after: Vec<_> = Role::ALL
            .iter()
            .map(|r| h.bytes(h.store.cert_path(*r)))
            .collect();
        assert_eq!(before, after, "{spoil:?}");
        assert_eq!(h.store.load().expect("load"), Some(h.node.clone()));
        if spoil == Spoil::Skew(500) {
            assert!(matches!(
                err,
                RenewError::Answer(AnswerError::ClockSkew { .. })
            ));
        }
    }
}

#[tokio::test]
async fn a_damaged_node_json_is_reported_not_trusted() {
    let h = enrolled("renew-damaged").await;
    let path = h.store.ca_path().with_file_name("node.json");
    let mut node = h.node.clone();
    node.node_id = "NOT A NODE ID".to_owned();
    std::fs::write(&path, serde_json::to_vec(&node).expect("json")).expect("write");
    let err = renew(&h.frontend().await, &h.store, 1).await.unwrap_err();
    assert!(
        matches!(err, RenewError::Store(StoreError::Damaged(_))),
        "{err:?}"
    );
    assert!(h.running.fake.renews().is_empty(), "nothing was sent");

    // A pin that is not 32 bytes of hex, the same way.
    let mut node = h.node.clone();
    node.pin = "zz".to_owned();
    std::fs::write(&path, serde_json::to_vec(&node).expect("json")).expect("write");
    let err = renew(&h.frontend().await, &h.store, 1).await.unwrap_err();
    assert!(
        matches!(err, RenewError::Store(StoreError::Damaged(_))),
        "{err:?}"
    );
    node.pin = "ab".repeat(31);
    std::fs::write(&path, serde_json::to_vec(&node).expect("json")).expect("write");
    assert!(matches!(
        renew(&h.frontend().await, &h.store, 1).await.unwrap_err(),
        RenewError::Store(_)
    ));

    // A key file that is gone is an I/O error naming it.
    std::fs::write(&path, serde_json::to_vec(&h.node).expect("json")).expect("write");
    std::fs::remove_file(h.store.key_path(Role::Daemon)).expect("rm");
    let err = renew(&h.frontend().await, &h.store, 1).await.unwrap_err();
    assert!(err.to_string().contains("daemon.key"), "{err}");
}

// ---- the schedule: numbers in, numbers out ----------------------------------------

#[test]
fn the_wait_after_failures_starts_at_five_minutes_doubles_and_stops_at_an_hour() {
    let mut backoff = Backoff::new();
    let waits: Vec<i64> = (0..8).map(|_| backoff.failed()).collect();
    assert_eq!(waits, [300, 600, 1200, 2400, 3600, 3600, 3600, 3600]);
    assert_eq!((RETRY_FIRST_S, RETRY_MAX_S), (300, 3600));
    backoff.succeeded();
    assert_eq!(backoff.failed(), 300, "success starts it over");
    // Many failures neither overflow nor go negative.
    let mut long = Backoff::new();
    assert_eq!((0..200).map(|_| long.failed()).last(), Some(3600));
}

#[test]
fn the_first_attempt_is_at_renew_after_spread_by_at_most_ten_percent() {
    let (start, renew_after) = (1_000_000, 1_000_000 + 3 * 86_400 + 43_200);
    let wait = renew_after - start;
    assert_eq!(first_attempt(start, renew_after, 0), renew_after);
    assert_eq!(
        first_attempt(start, renew_after, 100),
        start + wait * 11 / 10
    );
    assert_eq!(
        first_attempt(start, renew_after, -100),
        start + wait * 9 / 10
    );
    // Beyond the bound is clamped to it, in both directions.
    assert_eq!(
        first_attempt(start, renew_after, 5000),
        first_attempt(start, renew_after, JITTER_PERMILLE)
    );
    assert_eq!(
        first_attempt(start, renew_after, -5000),
        first_attempt(start, renew_after, -JITTER_PERMILLE)
    );
    // A renew_after in the past means now, not before the answer was made.
    assert_eq!(first_attempt(start, start - 5, 0), start);
}

#[tokio::test]
async fn the_node_warns_when_less_than_a_day_is_left() {
    let h = enrolled("renew-warn").await;
    let not_after = h.node.earliest_not_after();
    assert!(!near_expiry(&h.node, not_after - WARN_BELOW_S));
    assert!(near_expiry(&h.node, not_after - WARN_BELOW_S + 1));
    assert!(
        near_expiry(&h.node, not_after + 10),
        "expired is near expiry"
    );
}
