use super::fixture::*;
use super::*;

const NOW: i64 = 1_800_000_000; // 2027-01-15
const LATER: &str = "2030-01-01T00:00:00Z";

fn root() -> VerifyingKey {
    key(ROOT).verifying_key()
}

#[test]
fn a_platform_signed_set_under_a_root_statement_verifies() {
    let set = verify(
        &root(),
        &statement(4, LATER),
        &set("mac-arm64", 12, 10, "27B5"),
        NOW,
    )
    .unwrap();
    assert_eq!(
        set,
        VerifiedSet {
            statement_serial: 4,
            pool: "mac-arm64".into(),
            os: "macos".into(),
            arch: "arm64".into(),
            serial: 12,
            min_serial: 10,
            expires: parse_rfc3339(LATER).unwrap(),
            macos_version: "27.1".into(),
            macos_build: "27B5".into(),
            profiles: vec![],
        }
    );
    assert_eq!(
        parse_root_key(&format!(" {}\n", public(&key(ROOT)))),
        Some(root())
    );
    assert_eq!(parse_root_key("short"), None);
}

#[test]
fn a_statement_not_signed_by_the_root_key_is_refused() {
    // Catches: trusting whatever key a statement carries.
    let forged = seal(
        &key(9),
        &serde_json::json!({"kind": STATEMENT_KIND, "serial": 99, "expires": LATER,
            "platform_keys": [public(&key(9))]}),
    );
    assert_eq!(
        verify(
            &root(),
            &forged,
            &seal(&key(9), &set_doc("p", 1, 0, "b")),
            NOW
        ),
        Err(SetError::NotRoot)
    );
}

#[test]
fn a_tampered_payload_is_refused() {
    let mut statement = statement(4, LATER);
    let tampered = STANDARD.encode(b"{\"kind\":\"kbf-key-statement-v1\"}");
    statement.payload = tampered;
    assert_eq!(
        verify(&root(), &statement, &set("p", 1, 0, "b"), NOW),
        Err(SetError::Signature("key statement"))
    );
    let mut set = set("p", 1, 0, "b");
    set.signature = statement.signature.clone();
    assert_eq!(
        verify(&root(), &super::fixture::statement(4, LATER), &set, NOW),
        Err(SetError::Signature("set"))
    );
}

#[test]
fn malformed_envelopes_are_refused() {
    let good = statement(4, LATER);
    for bad in [
        Envelope {
            key: "!!".into(),
            ..good.clone()
        },
        Envelope {
            payload: "!!".into(),
            ..good.clone()
        },
        Envelope {
            signature: "AAAA".into(),
            ..good.clone()
        },
    ] {
        assert_eq!(
            verify(&root(), &bad, &set("p", 1, 0, "b"), NOW),
            Err(SetError::Envelope("key statement"))
        );
    }
}

#[test]
fn an_expired_statement_or_set_is_refused() {
    // Catches: skipping the gate's expiry check (S10 "refuses enforce of an expired
    // set").
    assert_eq!(
        verify(
            &root(),
            &statement(4, "2027-01-15T08:00:00Z"),
            &set("p", 1, 0, "b"),
            NOW
        ),
        Err(SetError::StatementExpired)
    );
    let mut doc = set_doc("p", 1, 0, "b");
    doc["expires"] = "2027-01-15T08:00:00Z".into();
    assert_eq!(
        verify(
            &root(),
            &statement(4, LATER),
            &seal(&key(PLATFORM), &doc),
            NOW
        ),
        Err(SetError::SetExpired)
    );
}

#[test]
fn a_component_key_set_cannot_carry_a_macos_build() {
    // Catches: letting the component key cover root-level software (S2.2).
    let set = seal(&key(COMPONENT), &set_doc("p", 1, 0, "b"));
    assert_eq!(
        verify(&root(), &statement(4, LATER), &set, NOW),
        Err(SetError::ComponentKey)
    );
    let stranger = seal(&key(8), &set_doc("p", 1, 0, "b"));
    assert_eq!(
        verify(&root(), &statement(4, LATER), &stranger, NOW),
        Err(SetError::UnknownKey)
    );
}

#[test]
fn documents_must_be_well_formed() {
    let root_key = key(ROOT);
    let not_json = Envelope {
        payload: STANDARD.encode(b"not json"),
        key: public(&root_key),
        signature: STANDARD.encode(ed25519_dalek::Signer::sign(&root_key, b"not json").to_bytes()),
    };
    assert!(matches!(
        verify(&root(), &not_json, &set("p", 1, 0, "b"), NOW),
        Err(SetError::Document("key statement", _))
    ));
    let wrong_kind = seal(
        &root_key,
        &serde_json::json!({"kind": "x", "serial": 1, "expires": LATER}),
    );
    assert!(matches!(
        verify(&root(), &wrong_kind, &set("p", 1, 0, "b"), NOW),
        Err(SetError::Document("key statement", e)) if e.contains("kind")
    ));
    let bad_time = seal(
        &root_key,
        &serde_json::json!({"kind": STATEMENT_KIND, "serial": 1, "expires": "soon"}),
    );
    assert!(matches!(
        verify(&root(), &bad_time, &set("p", 1, 0, "b"), NOW),
        Err(SetError::Document("key statement", e)) if e.contains("RFC 3339")
    ));
    let mut doc = set_doc("p", 1, 0, "b");
    doc["kind"] = "kbf-set-v0".into();
    assert!(matches!(
        verify(&root(), &statement(4, LATER), &seal(&key(PLATFORM), &doc), NOW),
        Err(SetError::Document("set", e)) if e.contains("kind")
    ));
    let mut doc = set_doc("p", 1, 0, "b");
    doc.as_object_mut().unwrap().remove("macos");
    assert_eq!(
        verify(
            &root(),
            &statement(4, LATER),
            &seal(&key(PLATFORM), &doc),
            NOW
        ),
        Err(SetError::NoMacosBuild)
    );
    assert_eq!(
        SetError::NotRoot.to_string(),
        "the key statement is not signed by the root key"
    );
}
