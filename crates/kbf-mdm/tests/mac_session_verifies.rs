//! The gate's grants, made by `GrantKey::sign`, checked by the Mac's own verifier
//! (`kbf-mac-session`'s `grant::verify`) in one test binary: the two sides of the
//! admin grant (fleet-updates-security.md S5.2) are linked here, so a change to the
//! format on either side that the other does not share goes red, with no pinned bytes
//! to update by hand. The pinned tokens in both crates' tests stay as the record of
//! the format itself.

use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use kbf_mac_session::grant::{Expect, GrantKeys, verify};
use kbf_mdm::grant::{Grant, GrantKey};

/// The gate's key of seed `seed`, as its key file holds it.
fn gate_key(seed: u8) -> GrantKey {
    GrantKey::parse(&STANDARD.encode([seed; 32])).unwrap()
}

/// The Mac's key file holding the public key a `grant-admin` answer carries.
fn mac_keys(grant: &Grant) -> GrantKeys {
    GrantKeys::parse(&grant.key).unwrap()
}

fn expect<'a>(serial: &'a str, lease: &'a str, now: i64) -> Expect<'a> {
    Expect {
        serial,
        lease,
        now: UNIX_EPOCH + Duration::from_secs(u64::try_from(now).unwrap()),
    }
}

fn refused(result: Result<(), String>, why: &str, what: &str) {
    let error = result.expect_err(what);
    assert!(error.contains(why), "{what}: {error}");
}

/// Issue times that stress the time form: the epoch's first hour, a leap day's last
/// second, a year's and a century's turn, and a spread of times over two centuries.
fn issue_times() -> Vec<i64> {
    let mut times = vec![0, 1_835_481_599, 1_924_988_399, 4_102_441_200];
    // Every 3.3 years, at an hour, minute and second that each step moves (debug-build
    // signing is slow, so the spread stays small).
    times.extend((0..60).map(|n| 1_700_000_000 + n * 105_189_753));
    times
}

/// Catches: the gate and the Mac disagreeing on anything a valid grant carries (the
/// line layout, the time form, the lifetime, the base64 alphabets of token or key,
/// the signature scheme), which would leave every Mac refusing every grant; and a
/// window other than issue to not-after.
#[test]
fn the_mac_accepts_every_grant_the_gate_signs_for_its_hour() {
    let serial = "C02ZK1ABCDEF";
    let lease = "kbf-lease.7_a-1";
    for (n, issued) in issue_times().into_iter().enumerate() {
        let grant = gate_key(u8::try_from(n % 251).unwrap()).sign(serial, lease, issued);
        let keys = mac_keys(&grant);
        let check = |now| verify(&grant.token, &keys, expect(serial, lease, now));
        assert_eq!(check(issued), Ok(()), "{}", grant.grant);
        assert_eq!(check(issued + 3600), Ok(()), "{}", grant.grant);
        refused(check(issued + 3601), "expired", &grant.grant);
        // The answer's separate fields are the token's bytes.
        let (payload, signature) = grant.token.split_once('.').unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.decode(payload).unwrap(),
            grant.grant.as_bytes()
        );
        assert_eq!(
            URL_SAFE_NO_PAD.decode(signature).unwrap(),
            STANDARD.decode(&grant.signature).unwrap()
        );
    }
}

/// Catches: the Mac taking a gate grant that was altered after signing (its payload
/// edited, its signature changed, another grant's signature attached), one for
/// another Mac or lease, or one signed by a gate key the Mac does not hold.
#[test]
fn the_mac_refuses_a_tampered_or_misdirected_gate_grant() {
    let issued = 1_800_000_000;
    let at = expect("C02X", "lease-1", issued + 60);
    let grant = gate_key(5).sign("C02X", "lease-1", issued);
    let keys = mac_keys(&grant);
    let (payload, signature) = grant.token.split_once('.').unwrap();

    for text in [
        grant.grant.replace("lease lease-1", "lease lease-2"),
        grant.grant.replace("serial C02X", "serial C02Y"),
        grant.grant.replace("T09:00:00Z", "T10:00:00Z"),
        grant.grant.clone() + "admin yes\n",
    ] {
        let token = format!("{}.{signature}", URL_SAFE_NO_PAD.encode(&text));
        refused(verify(&token, &keys, at), "does not verify", &text);
    }

    let mut flipped = URL_SAFE_NO_PAD.decode(signature).unwrap();
    flipped[0] ^= 1;
    let token = format!("{payload}.{}", URL_SAFE_NO_PAD.encode(&flipped));
    refused(verify(&token, &keys, at), "does not verify", "flipped bit");

    let other = gate_key(6).sign("C02X", "lease-2", issued);
    let (_, other_signature) = other.token.split_once('.').unwrap();
    let both = GrantKeys::parse(&format!("{}\n{}\n", grant.key, other.key)).unwrap();
    let spliced = format!("{payload}.{other_signature}");
    refused(verify(&spliced, &both, at), "does not verify", "spliced");

    refused(
        verify(&grant.token, &mac_keys(&other), at),
        "does not verify",
        "another gate's key",
    );
    refused(
        verify(&grant.token, &keys, expect("C02Y", "lease-1", issued + 60)),
        "serial",
        "another Mac",
    );
    refused(
        verify(&grant.token, &keys, expect("C02X", "lease-10", issued + 60)),
        "lease",
        "another lease",
    );
}
