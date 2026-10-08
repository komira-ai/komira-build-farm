//! Grants the MDM gate's own code signed (`kbf-mdm`'s `GrantKey::sign`, PR #126),
//! checked through this crate's public `grant::verify`: the gate's tokens must be
//! accepted exactly as issued, and every tampered form refused.
//!
//! The fixtures were printed by running `GrantKey::sign` in `kbf-mdm` for each row
//! below (seed `[seed; 32]`); `kbf-mdm`'s test
//! `the_tokens_kbf_mac_session_pins_are_what_sign_produces` pins the same bytes on
//! the gate's side, so a change to the format on either side goes red on that side.
//! The crates are not linked: `kbf-mdm` is the Linux gate, this is the Mac's root
//! helper.

use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer as _, SigningKey};
use kbf_mac_session::grant::{Expect, GrantKeys, verify};

/// One `grant-admin` answer from the gate.
struct Fixture {
    seed: u8,
    /// The answer's `key`: the line of the Mac's key file.
    key: &'static str,
    serial: &'static str,
    lease: &'static str,
    issued: u64,
    /// The answer's `grant`.
    text: &'static str,
    /// The answer's `token`: what the server hands the helper.
    token: &'static str,
}

const FIXTURES: [Fixture; 3] = [
    Fixture {
        seed: 5,
        key: "bnoc3Smwt4/ROvTFWY/v9O8qlxZuPKby5Pv8zYBQW/E=",
        serial: "C02X",
        lease: "lease-1",
        issued: 1_800_000_000,
        text: "kbf-grant-v1\nserial C02X\nlease lease-1\nissued 2027-01-15T08:00:00Z\nnot-after 2027-01-15T09:00:00Z\n",
        token: "a2JmLWdyYW50LXYxCnNlcmlhbCBDMDJYCmxlYXNlIGxlYXNlLTEKaXNzdWVkIDIwMjctMDEtMTVUMDg6MDA6MDBaCm5vdC1hZnRlciAyMDI3LTAxLTE1VDA5OjAwOjAwWgo.-8APW_Uwc0eusVoHQxdjI5Djv0Va4O8rdk58tTNUzQiDX9jOi4tZKTl8VItbJx129XOo5d2Wy4NmwtrxrO5bAw",
    },
    // Issued in the last second of a leap day: not-after falls on the next month.
    Fixture {
        seed: 7,
        key: "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=",
        serial: "C02ZK1ABCDEF",
        lease: "kbf-lease.7_a-1",
        issued: 1_835_481_599,
        text: "kbf-grant-v1\nserial C02ZK1ABCDEF\nlease kbf-lease.7_a-1\nissued 2028-02-29T23:59:59Z\nnot-after 2028-03-01T00:59:59Z\n",
        token: "a2JmLWdyYW50LXYxCnNlcmlhbCBDMDJaSzFBQkNERUYKbGVhc2Uga2JmLWxlYXNlLjdfYS0xCmlzc3VlZCAyMDI4LTAyLTI5VDIzOjU5OjU5Wgpub3QtYWZ0ZXIgMjAyOC0wMy0wMVQwMDo1OTo1OVoK.iYeWfQ1bSqsI0zuDZCxrlPMECRWVcikNumCJuYBaiEoNMz0sTXnHHrVdOMSHYl3LjUhXlmRO7tLcAP-Bg4n8Cw",
    },
    // not-after crosses into a new year and century.
    Fixture {
        seed: 9,
        key: "/RckOFqgx1tk+3jNYC+h2ZH96/drE8WO1wLqyDXp9hg=",
        serial: "Z9",
        lease: "0",
        issued: 4_102_441_200,
        text: "kbf-grant-v1\nserial Z9\nlease 0\nissued 2099-12-31T23:00:00Z\nnot-after 2100-01-01T00:00:00Z\n",
        token: "a2JmLWdyYW50LXYxCnNlcmlhbCBaOQpsZWFzZSAwCmlzc3VlZCAyMDk5LTEyLTMxVDIzOjAwOjAwWgpub3QtYWZ0ZXIgMjEwMC0wMS0wMVQwMDowMDowMFoK.O4X1QvAOY8XS3qMKBJB7kTKbl6rUSMe8nXHvGKYbo5vRo9u6_cFdeuxuEgsCx2QbF6eq2ba1rS04ihnV-2CWCw",
    },
];

impl Fixture {
    fn keys(&self) -> GrantKeys {
        GrantKeys::parse(self.key).unwrap()
    }

    fn at(&self, now: u64) -> Expect<'static> {
        Expect {
            serial: self.serial,
            lease: self.lease,
            now: UNIX_EPOCH + Duration::from_secs(now),
        }
    }

    /// `verify` of `token` under this fixture's key, for its Mac and lease, a minute
    /// after it was issued.
    fn check(&self, token: &str) -> Result<(), String> {
        verify(token, &self.keys(), self.at(self.issued + 60))
    }

    /// `text` signed with this fixture's (the gate's) key: what a gate with a changed
    /// format, or a thief of the key, would issue.
    fn resign(&self, text: &str) -> String {
        let signature = SigningKey::from_bytes(&[self.seed; 32]).sign(text.as_bytes());
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(text),
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }

    fn signature(&self) -> &'static str {
        self.token.split_once('.').unwrap().1
    }
}

fn refused(result: Result<(), String>, why: &str, what: &str) {
    let error = result.expect_err(what);
    assert!(error.contains(why), "{what}: {error}");
}

/// Catches: the helper refusing the gate's real grants (the time form, the line
/// layout, the base64 alphabets or the key encoding drifting apart), or a lifetime
/// other than the gate's hour.
#[test]
fn the_gates_grants_are_accepted_from_issue_to_not_after() {
    for fixture in &FIXTURES {
        // The fixture's payload is the gate's text, and resigning it is deterministic.
        assert_eq!(fixture.resign(fixture.text), fixture.token);
        let keys = fixture.keys();
        let check = |now| verify(fixture.token, &keys, fixture.at(now));
        assert_eq!(check(fixture.issued), Ok(()), "{}", fixture.text);
        assert_eq!(check(fixture.issued + 3600), Ok(()), "{}", fixture.text);
        refused(check(fixture.issued + 3601), "expired", fixture.text);
        // A Mac clock up to five minutes behind the gate's still takes it.
        assert_eq!(check(fixture.issued - 300), Ok(()), "{}", fixture.text);
        refused(check(fixture.issued - 301), "ahead", fixture.text);
    }
}

/// Catches: a grant used on another Mac, for another lease, or under a key this Mac
/// does not hold.
#[test]
fn a_gate_grant_is_bound_to_its_mac_lease_and_key() {
    for (n, fixture) in FIXTURES.iter().enumerate() {
        let other_mac = Expect {
            serial: "C02OTHER",
            ..fixture.at(fixture.issued + 60)
        };
        refused(
            verify(fixture.token, &fixture.keys(), other_mac),
            "serial",
            fixture.text,
        );
        let other_lease = Expect {
            lease: "another-lease",
            ..fixture.at(fixture.issued + 60)
        };
        refused(
            verify(fixture.token, &fixture.keys(), other_lease),
            "lease",
            fixture.text,
        );
        let other = &FIXTURES[(n + 1) % FIXTURES.len()];
        refused(
            verify(
                fixture.token,
                &other.keys(),
                fixture.at(fixture.issued + 60),
            ),
            "does not verify",
            fixture.text,
        );
    }
}

/// Catches: the lease compared by prefix in either direction, so a grant for lease
/// `lease-1` would admin the user of lease `lease-10`, or of lease `lease-`.
#[test]
fn a_gate_grant_is_refused_for_a_lease_sharing_its_prefix() {
    for fixture in &FIXTURES {
        let longer = format!("{}0", fixture.lease);
        let mut shorter = fixture.lease.to_owned();
        shorter.pop();
        for lease in [longer.as_str(), shorter.as_str()] {
            let expect = Expect {
                lease,
                ..fixture.at(fixture.issued + 60)
            };
            refused(
                verify(fixture.token, &fixture.keys(), expect),
                "lease",
                &format!("{} as lease {lease:?}", fixture.lease),
            );
        }
    }
}

/// Catches: a signature checked over anything but the exact payload bytes, so a
/// grant edited in transit (another lease, a later not-after) or paired with another
/// grant's signature still passes.
#[test]
fn a_tampered_gate_grant_is_refused() {
    let [first, second, _] = &FIXTURES;
    for fixture in &FIXTURES {
        let edits = [
            fixture.text.replace("kbf-grant-v1", "kbf-grant-v2"),
            fixture
                .text
                .replace(&format!("lease {}", fixture.lease), "lease x"),
            fixture.text.replace("not-after 2", "not-after 3"),
            fixture.text.to_owned() + "admin yes\n",
        ];
        for text in &edits {
            let token = format!("{}.{}", URL_SAFE_NO_PAD.encode(text), fixture.signature());
            refused(fixture.check(&token), "does not verify", text);
        }
        // One bit of the signature flipped.
        let mut signature = URL_SAFE_NO_PAD.decode(fixture.signature()).unwrap();
        signature[10] ^= 1;
        let (payload, _) = fixture.token.split_once('.').unwrap();
        let token = format!("{payload}.{}", URL_SAFE_NO_PAD.encode(&signature));
        refused(fixture.check(&token), "does not verify", fixture.text);
    }
    // One grant's payload with another's signature.
    let (payload, _) = first.token.split_once('.').unwrap();
    let spliced = format!("{payload}.{}", second.signature());
    let keys = GrantKeys::parse(&format!("{}\n{}\n", first.key, second.key)).unwrap();
    refused(
        verify(&spliced, &keys, first.at(first.issued + 60)),
        "does not verify",
        "spliced",
    );
}

/// Catches: the helper taking a grant whose format differs from the gate's even when
/// the gate's key signed it: the earlier draft's integer `not-after` with no
/// `issued`, an offset instead of `Z`, a lifetime other than exactly an hour, or CRLF.
#[test]
fn a_gate_signed_grant_in_another_format_is_refused() {
    let fixture = &FIXTURES[0];
    let not_after = fixture.issued + 3600;
    let malformed = [
        format!("kbf-grant-v1\nserial C02X\nlease lease-1\nnot-after {not_after}\n"),
        format!(
            "kbf-grant-v1\nserial C02X\nlease lease-1\nissued {}\nnot-after {not_after}\n",
            fixture.issued
        ),
        fixture.text.replace('Z', "+00:00"),
        fixture.text.replace('\n', "\r\n"),
        fixture.text.replace("kbf-grant-v1\n", ""),
    ];
    for text in &malformed {
        refused(fixture.check(&fixture.resign(text)), "malformed", text);
    }
    for lifetime in ["2027-01-15T09:00:01Z", "2027-01-15T08:59:59Z"] {
        let text = fixture.text.replace("2027-01-15T09:00:00Z", lifetime);
        refused(
            fixture.check(&fixture.resign(&text)),
            "not 60 minutes after",
            &text,
        );
    }
}
