use ed25519_dalek::Verifier as _;

use super::testing::{key_line, sign, text, text_until, token, utc};
use super::*;

const NOW: i64 = 1_800_000_000;

fn keys() -> GrantKeys {
    GrantKeys::parse(&format!("# the gate\n{}\n\n", key_line(1))).unwrap()
}

fn at(now: i64) -> Expect<'static> {
    Expect {
        serial: "C02XYZ",
        lease: "4.2",
        now: UNIX_EPOCH + Duration::from_secs(now.cast_unsigned()),
    }
}

fn check(grant: &str) -> Result<(), String> {
    verify(grant, &keys(), at(NOW))
}

#[test]
fn a_grant_for_this_mac_and_lease_is_accepted() {
    assert_eq!(check(&sign(1, &text("C02XYZ", "4.2", NOW))), Ok(()));
}

/// The `grant-admin` answer the gate's own code produced for `kbf-mdm`'s grant test
/// (seed `[5; 32]`, serial `C02X`, lease `lease-1`, issued at 1 800 000 000), printed
/// by running that test: its `token` is what the server hands this helper, its `key`
/// the line of the Mac's key file. Catches: the two sides drifting apart in the text,
/// the time form, the base64 alphabets or the key encoding.
#[test]
fn the_gates_own_grant_verifies_here() {
    const TOKEN: &str = "a2JmLWdyYW50LXYxCnNlcmlhbCBDMDJYCmxlYXNlIGxlYXNlLTEKaXNzdWVkIDIwMjctMDEtMTVUMDg6MDA6MDBaCm5vdC1hZnRlciAyMDI3LTAxLTE1VDA5OjAwOjAwWgo.-8APW_Uwc0eusVoHQxdjI5Djv0Va4O8rdk58tTNUzQiDX9jOi4tZKTl8VItbJx129XOo5d2Wy4NmwtrxrO5bAw";
    const KEY: &str = "bnoc3Smwt4/ROvTFWY/v9O8qlxZuPKby5Pv8zYBQW/E=";
    let keys = GrantKeys::parse(KEY).unwrap();
    let expect = |now| Expect {
        serial: "C02X",
        lease: "lease-1",
        now: UNIX_EPOCH + Duration::from_secs(now),
    };
    assert_eq!(verify(TOKEN, &keys, expect(1_800_000_060)), Ok(()));
    // Its last valid second, and the one after.
    assert_eq!(verify(TOKEN, &keys, expect(1_800_003_600)), Ok(()));
    assert!(
        verify(TOKEN, &keys, expect(1_800_003_601))
            .unwrap_err()
            .contains("expired")
    );
    // The same seed signs the same way here.
    assert_eq!(key_line(5), KEY);
    let text = text("C02X", "lease-1", 1_800_000_000);
    assert_eq!(
        text,
        "kbf-grant-v1\nserial C02X\nlease lease-1\nissued 2027-01-15T08:00:00Z\nnot-after 2027-01-15T09:00:00Z\n"
    );
    assert_eq!(sign(5, &text), TOKEN);
}

/// Catches: trusting the text without checking the signature, or accepting a key the
/// Mac does not hold.
#[test]
fn a_grant_signed_by_another_key_is_refused() {
    let error = check(&sign(2, &text("C02XYZ", "4.2", NOW))).unwrap_err();
    assert!(error.contains("does not verify"), "{error}");
}

/// Catches: verifying the signature over something other than the payload's bytes (a
/// grant edited after signing still passing).
#[test]
fn an_edited_grant_is_refused() {
    let good = sign(1, &text("C02XYZ", "4.2", NOW));
    let (_, signature) = good.split_once('.').unwrap();
    let edited = URL_SAFE_NO_PAD.encode(text("C02XYZ", "4.3", NOW));
    let error = check(&format!("{edited}.{signature}")).unwrap_err();
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
    let grant = token(text.as_bytes(), &forged);
    let error = verify(&grant, &GrantKeys(vec![weak]), at(NOW)).unwrap_err();
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

/// Catches: dropping the expiry check, or refusing at the instant of `not-after`
/// (the gate's format lets the clock reach it, not pass it).
#[test]
fn an_expired_grant_is_refused() {
    let grant = sign(1, &text("C02XYZ", "4.2", NOW - 3600));
    assert_eq!(verify(&grant, &keys(), at(NOW)), Ok(()));
    let error = verify(&grant, &keys(), at(NOW + 1)).unwrap_err();
    assert!(error.contains("expired"), "{error}");
}

/// Catches: accepting a `not-after` other than exactly an hour after `issued` (longer,
/// which a gate bug or a stolen key would issue, or shorter).
#[test]
fn not_after_is_exactly_an_hour_after_issued() {
    for not_after in [NOW + 3599, NOW + 3601, NOW + 86_400] {
        let grant = sign(1, &text_until("C02XYZ", "4.2", NOW, not_after));
        let error = check(&grant).unwrap_err();
        assert!(error.contains("not 60 minutes after"), "{error}");
    }
}

/// Catches: no limit on how far ahead `not-after` lies, under which a grant dated days
/// ahead stays usable until then; or a limit off by one at 65 minutes.
#[test]
fn a_grant_from_the_future_is_refused() {
    let ahead = secs(MAX_AHEAD);
    let limit = sign(
        1,
        &text_until("C02XYZ", "4.2", NOW + ahead - 3600, NOW + ahead),
    );
    assert_eq!(check(&limit), Ok(()));
    let beyond = sign(
        1,
        &text_until("C02XYZ", "4.2", NOW + ahead - 3599, NOW + ahead + 1),
    );
    let error = check(&beyond).unwrap_err();
    assert!(error.contains("more than 65 minutes ahead"), "{error}");
}

#[test]
fn a_clock_before_1970_refuses_every_grant() {
    let before = Expect {
        now: UNIX_EPOCH - Duration::from_secs(1),
        ..at(NOW)
    };
    let grant = sign(1, &text("C02XYZ", "4.2", NOW));
    assert!(
        verify(&grant, &keys(), before)
            .unwrap_err()
            .contains("1970")
    );
}

/// Catches: a lax parser, under which signed text with an extra or missing line, a
/// field out of order or another time spelling could mean something the gate did not
/// sign.
#[test]
fn a_signed_but_malformed_grant_is_refused() {
    let issued = utc(NOW);
    let until = utc(NOW + 3600);
    let body = |issued: &str, until: &str| {
        format!("kbf-grant-v1\nserial C02XYZ\nlease 4.2\nissued {issued}\nnot-after {until}\n")
    };
    for bad in [
        body(&issued, &until).replace("v1", "v2"),
        format!("kbf-grant-v1\nlease 4.2\nserial C02XYZ\nissued {issued}\nnot-after {until}\n"),
        body(&issued, &until).trim_end().to_owned(),
        body(&issued, &until) + "admin yes\n",
        body(&issued, &until).replace('\n', "\r\n"),
        format!("kbf-grant-v1\nserial C02XYZ\nlease 4.2\nnot-after {until}\n"),
        format!("kbf-grant-v1\nserial \nlease 4.2\nissued {issued}\nnot-after {until}\n"),
        format!("kbf-grant-v1\nserialC02XYZ\nlease 4.2\nissued {issued}\nnot-after {until}\n"),
        body(&NOW.to_string(), &until),
        body(&issued, "soon"),
        body("2027-01-15T08:00:00+00:00", "2027-01-15T09:00:00+00:00"),
        body("2027-01-15T08:00:00.0Z", "2027-01-15T09:00:00.0Z"),
        body("2027-01-15T08:00:00z", "2027-01-15T09:00:00z"),
        body("2027-1-15T08:00:00Z", "2027-1-15T09:00:00Z"),
        body("+2027-01-15T08:00:00Z", "+2027-01-15T09:00:00Z"),
    ] {
        let error = check(&sign(1, &bad)).unwrap_err();
        assert!(error.contains("malformed"), "{bad:?}: {error}");
    }
    let signed = ed25519_dalek::Signer::sign(&super::testing::key(1), b"\xff\n");
    let error = check(&token(b"\xff\n", &signed.to_bytes())).unwrap_err();
    assert!(error.contains("malformed"), "{error}");
}

/// Catches: a token that is not exactly two base64url parts taken anyway (padding,
/// the standard alphabet, a short signature or a second dot).
#[test]
fn a_token_that_is_not_two_base64url_parts_is_refused() {
    let good = sign(1, &text("C02XYZ", "4.2", NOW));
    let (body, signature) = good.split_once('.').unwrap();
    assert!(
        check("no-dot")
            .unwrap_err()
            .contains("<payload>.<signature>")
    );
    for bad in [
        format!("{body}!.{signature}"),
        format!("{body}=.{signature}"),
    ] {
        let error = check(&bad).unwrap_err();
        assert!(error.contains("payload is not base64url"), "{bad}: {error}");
    }
    for bad in [
        format!("{body}.{}", &signature[..10]),
        format!("{body}.{signature}="),
        format!("{body}.{signature}.x"),
        format!("{body}.{}+", signature.replace('-', "+").replace('_', "/")),
    ] {
        let error = check(&bad).unwrap_err();
        assert!(error.contains("64 bytes"), "{bad}: {error}");
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
    assert_eq!(verify(&grant, &two, at(NOW)), Ok(()));
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
