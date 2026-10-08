use ed25519_dalek::Verifier as _;

use super::testing::{key_line, rfc3339, sign, text, text_until};
use super::*;

const NOW: i64 = 1_800_000_000;

fn keys() -> GrantKeys {
    GrantKeys::parse(&format!("# the gate\n{}\n\n", key_line(1))).unwrap()
}

fn expect() -> Expect<'static> {
    Expect {
        serial: "C02XYZ",
        lease: "4.2",
        now: UNIX_EPOCH + Duration::from_secs(NOW.cast_unsigned()),
    }
}

fn check(grant: &AdminGrant) -> Result<(), String> {
    verify(grant, &keys(), expect())
}

#[test]
fn a_grant_for_this_mac_and_lease_is_accepted() {
    assert_eq!(check(&sign(1, &text("C02XYZ", "4.2", NOW))), Ok(()));
}

/// The grant `kbf-mdm`'s own grant test pins (seed `[5; 32]`, serial `C02X`, lease
/// `lease-1`, issued at 1 800 000 000), with the signature and key that the gate's
/// `GrantKey::sign` produced for it (printed by running that test), as its
/// `grant-admin` answer serialises them. Catches: the two sides drifting apart in the
/// text, the time form, the base64 alphabet or the key encoding.
#[test]
fn the_gates_own_grant_verifies_here() {
    let answer = r#"{"grant":"kbf-grant-v1\nserial C02X\nlease lease-1\nissued 2027-01-15T08:00:00Z\nnot-after 2027-01-15T09:00:00Z\n","signature":"+8APW/Uwc0eusVoHQxdjI5Djv0Va4O8rdk58tTNUzQiDX9jOi4tZKTl8VItbJx129XOo5d2Wy4NmwtrxrO5bAw==","key":"bnoc3Smwt4/ROvTFWY/v9O8qlxZuPKby5Pv8zYBQW/E=","erase_at":"2027-01-15T10:00:00Z"}"#;
    let grant: AdminGrant = serde_json::from_str(answer).unwrap();
    let keys = GrantKeys::parse("bnoc3Smwt4/ROvTFWY/v9O8qlxZuPKby5Pv8zYBQW/E=\n").unwrap();
    let expect = Expect {
        serial: "C02X",
        lease: "lease-1",
        now: UNIX_EPOCH + Duration::from_secs(1_800_000_060),
    };
    assert_eq!(verify(&grant, &keys, expect), Ok(()));
    assert_eq!(key_line(5), "bnoc3Smwt4/ROvTFWY/v9O8qlxZuPKby5Pv8zYBQW/E=");
}

/// Catches: trusting the text without checking the signature, or accepting a key the
/// Mac does not hold.
#[test]
fn a_grant_signed_by_another_key_is_refused() {
    let error = check(&sign(2, &text("C02XYZ", "4.2", NOW))).unwrap_err();
    assert!(error.contains("does not verify"), "{error}");
}

/// Catches: verifying the signature over something other than the text's bytes (a
/// grant edited after signing still passing).
#[test]
fn an_edited_grant_is_refused() {
    let mut grant = sign(1, &text("C02XYZ", "4.2", NOW));
    grant.grant = text("C02XYZ", "4.3", NOW);
    let error = check(&grant).unwrap_err();
    assert!(error.contains("does not verify"), "{error}");
}

/// Catches: `verify` for `verify_strict`. Under a small-order public key the identity
/// point as `R` with `s = 0` satisfies the plain Ed25519 equation for every message,
/// so anyone can "sign" for such a key; only the strict check refuses it. (The key
/// file refuses weak keys too; the strict check is the second wall.)
#[test]
fn a_small_order_signature_is_refused() {
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let weak = VerifyingKey::from_bytes(&identity).unwrap();
    assert!(weak.is_weak());
    let mut forged = [0u8; 64];
    forged[0] = 1;
    let text = text("C02XYZ", "4.2", NOW);
    let signature = Signature::from_bytes(&forged);
    assert!(weak.verify(text.as_bytes(), &signature).is_ok());
    let grant = AdminGrant {
        grant: text,
        signature: STANDARD.encode(forged),
    };
    let error = verify(&grant, &GrantKeys(vec![weak]), expect()).unwrap_err();
    assert!(error.contains("does not verify"), "{error}");
}

/// Catches: dropping the serial check, so one grant serves every Mac.
#[test]
fn a_grant_for_another_mac_is_refused() {
    let error = check(&sign(1, &text("OTHER", "4.2", NOW))).unwrap_err();
    assert!(error.contains("serial"), "{error}");
}

/// Catches: dropping the lease check, so one grant serves every lease.
#[test]
fn a_grant_for_another_lease_is_refused() {
    let error = check(&sign(1, &text("C02XYZ", "4.3", NOW))).unwrap_err();
    assert!(error.contains("lease 4.3"), "{error}");
}

/// Catches: dropping the expiry check, or `<` for `<=` at the instant of expiry.
#[test]
fn an_expired_grant_is_refused() {
    for not_after in [NOW - 1, NOW] {
        let grant = sign(1, &text_until("C02XYZ", "4.2", NOW - 3600, not_after));
        assert!(check(&grant).unwrap_err().contains("expired"));
    }
    let grant = sign(1, &text_until("C02XYZ", "4.2", NOW - 3599, NOW + 1));
    assert_eq!(check(&grant), Ok(()));
}

/// Catches: accepting a grant valid for longer than the gate issues, which a gate bug
/// or a stolen grant key would produce.
#[test]
fn a_grant_valid_too_long_is_refused() {
    let limit = NOW + 3600;
    assert_eq!(
        check(&sign(1, &text_until("C02XYZ", "4.2", NOW, limit))),
        Ok(())
    );
    let error = check(&sign(1, &text_until("C02XYZ", "4.2", NOW, limit + 1))).unwrap_err();
    assert!(error.contains("longer than 60 minutes"), "{error}");
}

/// Catches: dropping the check on `issued`, under which a grant dated days ahead (with
/// a matching `not-after`) would stay usable until then.
#[test]
fn a_grant_issued_in_the_future_is_refused() {
    let skew = secs(CLOCK_SKEW);
    assert_eq!(check(&sign(1, &text("C02XYZ", "4.2", NOW + skew))), Ok(()));
    let error = check(&sign(1, &text("C02XYZ", "4.2", NOW + skew + 1))).unwrap_err();
    assert!(error.contains("in the future"), "{error}");
}

#[test]
fn a_clock_before_1970_refuses_every_grant() {
    let before = Expect {
        now: UNIX_EPOCH - Duration::from_secs(1),
        ..expect()
    };
    let grant = sign(1, &text("C02XYZ", "4.2", NOW));
    assert!(
        verify(&grant, &keys(), before)
            .unwrap_err()
            .contains("1970")
    );
}

/// Catches: a lax parser, under which signed text with an extra or missing line, a
/// field out of order or another time form could mean something the gate did not
/// sign.
#[test]
fn a_signed_but_malformed_grant_is_refused() {
    let issued = rfc3339(NOW);
    let until = rfc3339(NOW + 60);
    for bad in [
        format!("kbf-grant-v2\nserial C02XYZ\nlease 4.2\nissued {issued}\nnot-after {until}\n"),
        format!("kbf-grant-v1\nlease 4.2\nserial C02XYZ\nissued {issued}\nnot-after {until}\n"),
        format!("kbf-grant-v1\nserial C02XYZ\nlease 4.2\nissued {issued}\nnot-after {until}"),
        format!(
            "kbf-grant-v1\nserial C02XYZ\nlease 4.2\nissued {issued}\nnot-after {until}\nadmin yes\n"
        ),
        format!("kbf-grant-v1\nserial C02XYZ\nlease 4.2\nnot-after {until}\n"),
        format!("kbf-grant-v1\nserial \nlease 4.2\nissued {issued}\nnot-after {until}\n"),
        format!("kbf-grant-v1\nserialC02XYZ\nlease 4.2\nissued {issued}\nnot-after {until}\n"),
        format!("kbf-grant-v1\nserial C02XYZ\nlease 4.2\nissued {NOW}\nnot-after {until}\n"),
        format!("kbf-grant-v1\nserial C02XYZ\nlease 4.2\nissued {issued}\nnot-after soon\n"),
    ] {
        let error = check(&sign(1, &bad)).unwrap_err();
        assert!(error.contains("malformed"), "{bad:?}: {error}");
    }
}

/// Catches: a signature that is not 64 bytes of standard base64 taken anyway (or the
/// URL-safe alphabet accepted, which the gate never writes).
#[test]
fn a_signature_that_is_not_64_bytes_of_base64_is_refused() {
    let good = sign(1, &text("C02XYZ", "4.2", NOW));
    let with = |signature: String| AdminGrant {
        signature,
        ..good.clone()
    };
    for bad in [
        with(good.signature[..10].to_owned()),
        with(format!("{}AAAA", good.signature)),
        with(good.signature.replace('+', "-").replace('/', "_") + "!"),
        with(STANDARD.encode([0u8; 63])),
    ] {
        let error = check(&bad).unwrap_err();
        assert!(error.contains("not 64 bytes"), "{error}");
    }
}

/// Catches: a key file whose bad line is skipped (a typo then leaving the Mac with
/// fewer keys than the operator thinks), an empty file accepted, or a weak key
/// accepted.
#[test]
fn key_files_are_strict() {
    let two = GrantKeys::parse(&format!("{}\n{}\n", key_line(1), key_line(2))).unwrap();
    assert_eq!(two.0.len(), 2);
    let grant = sign(2, &text("C02XYZ", "4.2", NOW));
    assert_eq!(verify(&grant, &two, expect()), Ok(()));
    assert!(GrantKeys::parse("# none\n").unwrap_err().contains("no key"));
    assert!(GrantKeys::parse("abcd\n").unwrap_err().contains("line 1"));
    let hex = hex::encode(super::testing::key(1).verifying_key().to_bytes());
    assert!(GrantKeys::parse(&hex).unwrap_err().contains("line 1"));
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let weak = format!("{}\n{}", key_line(1), STANDARD.encode(identity));
    assert!(
        GrantKeys::parse(&weak)
            .unwrap_err()
            .contains("line 2: not a usable")
    );
    let not_a_point = format!("{}\n{}", key_line(1), STANDARD.encode([2u8; 32]));
    assert!(
        GrantKeys::parse(&not_a_point)
            .unwrap_err()
            .contains("line 2: not a usable")
    );
}
