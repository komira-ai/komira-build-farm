//! The checks on an answer, called directly: the cases the fake frontend cannot easily
//! produce (a rotation's cross-signed root, the names of a certificate).

mod support;

use kbf_enroll::answer::{AnswerError, CertFault, Expect, check, pem_encode};
use kbf_proto::kbf::enroll::v1::IssuedCert;
use kbf_types::{CaPin, NodeId, Role};
use rcgen::{CertificateParams, DistinguishedName, KeyPair, PublicKeyData};
use support::pki::{Issue, Pki, now, pem};

struct Node {
    id: NodeId,
    key: KeyPair,
    csr: Vec<u8>,
}

fn node() -> Node {
    let key = KeyPair::generate().expect("key");
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    let csr = params
        .serialize_request(&key)
        .expect("csr")
        .der()
        .as_ref()
        .to_vec();
    Node {
        id: "node-02".parse().expect("id"),
        key,
        csr,
    }
}

fn issue(pki: &Pki, node: &Node, role: Role, extra_dns: Option<&str>) -> IssuedCert {
    pki.issue(&Issue {
        csr_der: &node.csr,
        role,
        node_name: node.id.as_str(),
        extra_dns,
        role_uri: None,
        not_before: now() - 300,
        not_after: now() + 7 * 86_400,
    })
    .expect("issued")
}

fn expect<'a>(pin: &'a CaPin, node: &'a Node, keys: &'a [(Role, Vec<u8>)]) -> Expect<'a> {
    Expect {
        pin,
        node_id: &node.id,
        keys,
        now: now(),
    }
}

fn run(
    pin: &CaPin,
    node: &Node,
    role: Role,
    cert: &IssuedCert,
    bundle: &str,
) -> Result<(), AnswerError> {
    let keys = [(role, node.key.subject_public_key_info())];
    check(
        &expect(pin, node, &keys),
        std::slice::from_ref(cert),
        bundle.as_bytes(),
        now(),
        now() + 3 * 86_400,
    )
    .map(|_| ())
}

#[test]
fn a_leaf_from_a_rotated_root_is_trusted_through_its_cross_signature() {
    // Design 6.4: a token minted before the rotation carries the old root's hash. The
    // answer's bundle holds the old root and a copy of the new root signed by it.
    let (old, next) = (Pki::new(), Pki::named("kbf test CA next"));
    let node = node();
    let leaf = issue(&next, &node, Role::Daemon, None);
    let cross = pem(&old.cross_sign(&next));
    let bundle = format!("{}{}{}", old.root_pem(), next.root_pem(), cross);
    run(&old.pin(), &node, Role::Daemon, &leaf, &bundle)
        .expect("trusted through the cross-signature");

    // Catches: a rotation that strands nodes, and its opposite: trusting any root in the
    // bundle. Without the cross-signature the new root is just another certificate.
    let bundle = format!("{}{}", old.root_pem(), next.root_pem());
    let err = run(&old.pin(), &node, Role::Daemon, &leaf, &bundle).unwrap_err();
    assert_eq!(
        err,
        AnswerError::Cert {
            role: Role::Daemon,
            fault: CertFault::Untrusted
        }
    );

    // A token minted after activation pins the new root, and the same leaf is trusted directly.
    run(&next.pin(), &node, Role::Daemon, &leaf, &next.root_pem()).expect("new pin");
}

#[test]
fn a_daemon_certificate_names_exactly_the_node_and_a_storage_one_at_least() {
    let pki = Pki::new();
    let node = node();
    let bundle = pki.root_pem();

    let daemon_extra = issue(&pki, &node, Role::Daemon, Some("other.example.com"));
    let err = run(&pki.pin(), &node, Role::Daemon, &daemon_extra, &bundle).unwrap_err();
    assert_eq!(
        err,
        AnswerError::Cert {
            role: Role::Daemon,
            fault: CertFault::Name
        }
    );

    // A storage certificate may add the advertised host.
    let storage = issue(
        &pki,
        &node,
        Role::Storage,
        Some("store-02.farm.example.com"),
    );
    run(&pki.pin(), &node, Role::Storage, &storage, &bundle)
        .expect("storage with an advertise name");
}

#[test]
fn two_certificates_for_one_role_are_not_the_requested_set() {
    let pki = Pki::new();
    let node = node();
    let storage = issue(&pki, &node, Role::Storage, None);
    let keys = [
        (Role::Storage, node.key.subject_public_key_info()),
        (Role::Daemon, node.key.subject_public_key_info()),
    ];
    let err = check(
        &expect(&pki.pin(), &node, &keys),
        &[storage.clone(), storage],
        pki.root_pem().as_bytes(),
        now(),
        now() + 100,
    )
    .unwrap_err();
    assert_eq!(err, AnswerError::RoleSet);
}

#[test]
fn a_clock_exactly_at_the_limit_is_accepted_and_one_second_past_is_not() {
    let pki = Pki::new();
    let node = node();
    let leaf = issue(&pki, &node, Role::Daemon, None);
    let keys = [(Role::Daemon, node.key.subject_public_key_info())];
    let at = now();
    let ex = Expect {
        pin: &pki.pin(),
        node_id: &node.id,
        keys: &keys,
        now: at,
    };
    let bundle = pki.root_pem();
    check(
        &ex,
        std::slice::from_ref(&leaf),
        bundle.as_bytes(),
        at + 120,
        at + 100,
    )
    .expect("120 s is fine");
    let err = check(
        &ex,
        std::slice::from_ref(&leaf),
        bundle.as_bytes(),
        at + 121,
        at + 100,
    )
    .unwrap_err();
    assert_eq!(
        err,
        AnswerError::ClockSkew {
            ours: at,
            theirs: at + 121
        }
    );
    assert!(err.to_string().contains("more than 120 s"));
}

#[test]
fn pem_is_wrapped_at_64_columns_and_reads_back() {
    let der: Vec<u8> = (0..200u8).collect();
    let text = pem_encode("CERTIFICATE", &der);
    assert!(text.starts_with("-----BEGIN CERTIFICATE-----\n"));
    assert!(text.ends_with("\n-----END CERTIFICATE-----\n"));
    assert!(
        text.lines()
            .skip(1)
            .take_while(|l| !l.starts_with("-----"))
            .all(|l| l.len() <= 64)
    );
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let back = CertificateDer::from_pem_slice(text.as_bytes()).expect("reads back");
    assert_eq!(back.as_ref(), der.as_slice());
}
