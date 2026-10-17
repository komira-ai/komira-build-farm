//! Reading the role out of a certificate: exactly one `urn:kbf:role:<name>` URI.

use kbf_peer::{PeerRole, ROLE_URI_PREFIX, RoleError, role_of};
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, SanType};

fn uri(text: &str) -> SanType {
    SanType::URI(text.try_into().expect("an IA5 name"))
}

fn role(name: &str) -> SanType {
    uri(&format!("{ROLE_URI_PREFIX}{name}"))
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

/// A self-signed client certificate with these subjectAltNames (and an extended key
/// usage, so the certificate has an extension that is not a subjectAltName).
fn der(sans: Vec<SanType>) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("sign").der().to_vec()
}

/// Catches a role spelled other than the certificate table spells it (`frontend`,
/// `storage`, `daemon`, written out here rather than read from the code), and a DNS
/// name beside the role (a daemon's node id) taken for a second role.
#[test]
fn each_role_is_read_from_its_one_uri() {
    let spelled = ["frontend", "storage", "daemon"];
    assert_eq!(PeerRole::ALL.map(PeerRole::name), spelled);
    for (want, name) in PeerRole::ALL.into_iter().zip(spelled) {
        assert_eq!(want.to_string(), name);
        assert_eq!(PeerRole::from_name(name), Some(want));
        let leaf = der(vec![dns("node-1"), role(name)]);
        assert_eq!(role_of(&leaf), Ok(want), "{name}");
    }
    assert_eq!(ROLE_URI_PREFIX, "urn:kbf:role:");
}

/// Catches a certificate given a role it does not name exactly once: no role, two
/// (also two of the same), a DNS name or a URI of another scheme that spells a role,
/// a name that is no role or differs in case, the `urn:kbf:role:` prefix in another
/// case (the prefix is compared byte for byte; mutant seen red: the prefix compared
/// ignoring ASCII case); and bytes that are no certificate.
#[test]
fn a_certificate_names_exactly_one_known_role_or_none() {
    for (sans, want) in [
        (vec![], RoleError::Count(0)),
        (vec![dns("daemon")], RoleError::Count(0)),
        (vec![dns("urn:kbf:role:daemon")], RoleError::Count(0)),
        (vec![uri("urn:other:role:daemon")], RoleError::Count(0)),
        (vec![uri("URN:KBF:ROLE:daemon")], RoleError::Count(0)),
        (vec![uri("urn:kbf:Role:daemon")], RoleError::Count(0)),
        (vec![role("daemon"), role("storage")], RoleError::Count(2)),
        (vec![role("daemon"), role("daemon")], RoleError::Count(2)),
        (vec![role("admin")], RoleError::Unknown("admin".to_owned())),
        (
            vec![role("Daemon")],
            RoleError::Unknown("Daemon".to_owned()),
        ),
        (vec![role("")], RoleError::Unknown(String::new())),
    ] {
        assert_eq!(role_of(&der(sans.clone())), Err(want), "{sans:?}");
    }
    assert_eq!(PeerRole::from_name("admin"), None);
    assert!(matches!(
        role_of(b"not DER"),
        Err(RoleError::NotACertificate(_))
    ));
}

/// Catches a refusal that does not say what is wrong: each message names the rule or
/// the name it refused.
#[test]
fn each_refusal_says_why() {
    let count = RoleError::Count(2).to_string();
    assert!(
        count.contains("exactly one role (urn:kbf:role:<name>)"),
        "{count}"
    );
    assert!(count.ends_with("it names 2"), "{count}");
    assert_eq!(
        RoleError::Unknown("admin".to_owned()).to_string(),
        "\"admin\" is not a role"
    );
    let garbled = role_of(b"not DER").expect_err("refused").to_string();
    assert!(
        garbled.starts_with("the client certificate does not parse: "),
        "{garbled}"
    );
}
