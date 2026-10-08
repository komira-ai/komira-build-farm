//! The erase rules of M4 and S5.2, one behaviour per test. Each names the planted
//! mutant (M10, S10) that turns it red.

use super::*;
use crate::clock::{DAY, HOUR};
use crate::gate::fixture::{Fixture, NOW, default_policy};
use crate::request::Purpose;
use crate::signers::{FLAG_USER_PRESENT, FLAG_USER_VERIFIED, NAMESPACE, VerifyError};

fn lease(l: &str) -> Purpose {
    Fixture::lease(l)
}

#[tokio::test]
async fn erase_now_runs_at_once_and_alerts() {
    let mut f = Fixture::new("erase-now");
    let request = f.signed("MAC0", Purpose::EraseNow);
    assert_eq!(
        f.gate.erase("MAC0", &request).await,
        Ok(EraseOutcome::Erased)
    );
    assert_eq!(f.mdm.erases(), ["erase UDID-0"]);
    assert_eq!(f.alerts.summary(), ["erase erased MAC0"]);
    let event = f.alerts.0.lock().unwrap()[0].clone();
    assert!(
        event
            .detail
            .contains("signed by alice@example.org (sk-ssh-ed25519@openssh.com): test"),
        "{}",
        event.detail
    );
    let fleet = f.gate.fleet().await;
    assert_eq!(fleet.outstanding_erase.as_deref(), Some("MAC0"));
    assert_eq!(fleet.erases_last_24h, 1);
    assert!(fleet.macs["MAC0"].erasing);
}

#[tokio::test]
async fn a_bad_signature_erases_nothing_and_alerts() {
    // Catches: an unsigned erase verb left for the server (S10 mutant "keep an unsigned
    // erase verb").
    let mut f = Fixture::new("erase-bad-sig");
    let mut request = f.signed("MAC0", Purpose::EraseNow);
    request.signature = f.key.sign(b"another message");
    let refusal = f.gate.erase("MAC0", &request).await.unwrap_err();
    assert_eq!(refusal, Refusal::Signature(VerifyError::BadSignature));
    assert_eq!(refusal.status(), 403);
    request.signature = String::new();
    assert_eq!(
        f.gate.erase("MAC0", &request).await,
        Err(Refusal::Signature(VerifyError::Malformed))
    );
    assert!(f.mdm.erases().is_empty());
    assert_eq!(
        f.alerts.summary(),
        ["erase refused MAC0", "erase refused MAC0"]
    );
}

#[tokio::test]
async fn an_untouched_or_unverified_signature_is_refused() {
    // Catches: ignoring the flags byte (M10).
    let policy = crate::gate::Policy {
        require_user_verified: true,
        ..default_policy()
    };
    let mut f = Fixture::with_policy("erase-flags", policy);
    let request = f.request("MAC0", Purpose::EraseNow);
    let message = crate::request::render(&request);
    for (flags, expected) in [
        (0, VerifyError::NoTouch),
        (FLAG_USER_PRESENT, VerifyError::NotUserVerified),
    ] {
        let signed = SignedRequest {
            signature: f.key.sign_with(NAMESPACE, message.as_bytes(), flags),
            message: message.clone(),
        };
        assert_eq!(
            f.gate.erase("MAC0", &signed).await,
            Err(Refusal::Signature(expected))
        );
    }
    let both = SignedRequest {
        signature: f.key.sign_with(
            NAMESPACE,
            message.as_bytes(),
            FLAG_USER_PRESENT | FLAG_USER_VERIFIED,
        ),
        message,
    };
    assert_eq!(f.gate.erase("MAC0", &both).await, Ok(EraseOutcome::Erased));
}

#[tokio::test]
async fn a_request_forwarded_under_another_serial_erases_nothing() {
    // Catches: taking the serial from the server's request instead of the signed
    // message (M10 mutant).
    let mut f = Fixture::new("erase-mismatch");
    let request = f.signed("MAC0", Purpose::EraseNow);
    assert_eq!(
        f.gate.erase("MAC1", &request).await,
        Err(Refusal::SerialMismatch {
            signed: "MAC0".into(),
            asked: "MAC1".into()
        })
    );
    assert!(f.mdm.erases().is_empty());
}

#[tokio::test]
async fn a_serial_outside_the_inventory_is_refused() {
    let mut f = Fixture::new("erase-inventory");
    let request = f.signed("STRANGER", Purpose::EraseNow);
    let refusal = f.gate.erase("STRANGER", &request).await.unwrap_err();
    assert_eq!(refusal, Refusal::NotInInventory("STRANGER".into()));
    assert_eq!(refusal.status(), 404);
}

#[tokio::test]
async fn expired_far_ahead_and_replayed_requests_are_refused() {
    // Catches: skipping the nonce log (M10), and the not-after window.
    let mut f = Fixture::new("erase-window");
    let mut request = f.request("MAC0", Purpose::EraseNow);
    request.not_after = NOW - 1;
    assert_eq!(
        f.gate.erase("MAC0", &f.sign(&request)).await,
        Err(Refusal::Expired)
    );
    request.not_after = NOW + HOUR + 1;
    assert_eq!(
        f.gate.erase("MAC0", &f.sign(&request)).await,
        Err(Refusal::TooFarAhead)
    );
    request.not_after = NOW + HOUR;
    let signed = f.sign(&request);
    assert_eq!(
        f.gate.erase("MAC0", &signed).await,
        Ok(EraseOutcome::Erased)
    );
    f.mdm.report("UDID-0", NOW + 1);
    f.advance(2);
    f.gate.tick().await.unwrap();
    assert_eq!(f.gate.erase("MAC0", &signed).await, Err(Refusal::Replay));
    assert_eq!(f.mdm.erases(), ["erase UDID-0"]);
}

#[tokio::test]
async fn a_nonce_is_spent_even_when_the_caps_refuse_the_request() {
    // Catches: spending the nonce only on success, which would let the server keep a
    // refused request and replay it once the caps allow.
    let mut f = Fixture::new("erase-spent");
    let first = f.signed("MAC0", Purpose::EraseNow);
    f.gate.erase("MAC0", &first).await.unwrap();
    let second = f.signed("MAC1", Purpose::EraseNow);
    assert!(matches!(
        f.gate.erase("MAC1", &second).await,
        Err(Refusal::Busy(_))
    ));
    f.mdm.report("UDID-0", NOW + 1);
    f.advance(2);
    f.gate.tick().await.unwrap();
    assert_eq!(f.gate.erase("MAC1", &second).await, Err(Refusal::Replay));
}

#[tokio::test]
async fn one_erase_at_a_time_across_the_fleet() {
    // Catches: dropping the outstanding-erase check (S10 mutant).
    let mut f = Fixture::new("erase-one");
    let first = f.signed("MAC0", Purpose::EraseNow);
    f.gate.erase("MAC0", &first).await.unwrap();
    let second = f.signed("MAC1", Purpose::EraseNow);
    let refusal = f.gate.erase("MAC1", &second).await.unwrap_err();
    assert_eq!(
        refusal,
        Refusal::Busy("an erase of MAC0 is outstanding".into())
    );
    assert_eq!(refusal.status(), 409);
    // The outstanding erase clears when MAC0 reports after it, not before.
    f.mdm.report("UDID-0", NOW - 10);
    f.gate.tick().await.unwrap();
    assert_eq!(
        f.gate.fleet().await.outstanding_erase.as_deref(),
        Some("MAC0")
    );
    f.mdm.report("UDID-0", NOW + 5);
    f.gate.tick().await.unwrap();
    let fleet = f.gate.fleet().await;
    assert_eq!(fleet.outstanding_erase, None);
    assert!(!fleet.macs["MAC0"].erasing);
    let third = f.signed("MAC1", Purpose::EraseNow);
    assert_eq!(f.gate.erase("MAC1", &third).await, Ok(EraseOutcome::Erased));
}

#[tokio::test]
async fn an_outstanding_erase_clears_after_24_hours_but_the_mac_stays_out_of_the_floor() {
    let mut f = Fixture::new("erase-24h");
    let first = f.signed("MAC0", Purpose::EraseNow);
    f.gate.erase("MAC0", &first).await.unwrap();
    f.advance(DAY - 1);
    f.gate.tick().await.unwrap();
    assert_eq!(
        f.gate.fleet().await.outstanding_erase.as_deref(),
        Some("MAC0")
    );
    f.advance(1);
    f.gate.tick().await.unwrap();
    let fleet = f.gate.fleet().await;
    assert_eq!(fleet.outstanding_erase, None);
    assert!(fleet.macs["MAC0"].erasing);
    assert_eq!(fleet.available, 3);
}

#[tokio::test]
async fn the_daily_cap_holds_whatever_the_signature() {
    // Catches: letting a signature lift the caps (M10 mutant); the cap is a rolling 24
    // hours.
    let mut f = Fixture::new("erase-cap");
    for (serial, udid) in [("MAC0", "UDID-0"), ("MAC1", "UDID-1")] {
        let request = f.signed(serial, Purpose::EraseNow);
        f.gate.erase(serial, &request).await.unwrap();
        f.advance(60);
        f.mdm.report(udid, f.now());
        f.gate.tick().await.unwrap();
    }
    let third = f.signed("MAC2", Purpose::EraseNow);
    let refusal = f.gate.erase("MAC2", &third).await.unwrap_err();
    assert_eq!(refusal, Refusal::DailyCap(2));
    assert_eq!(refusal.status(), 409);
    f.advance(DAY - 121);
    let again = f.signed("MAC2", Purpose::EraseNow);
    assert_eq!(
        f.gate.erase("MAC2", &again).await,
        Err(Refusal::DailyCap(2))
    );
    f.advance(1);
    f.gate.tick().await.unwrap();
    let later = f.signed("MAC2", Purpose::EraseNow);
    assert_eq!(f.gate.erase("MAC2", &later).await, Ok(EraseOutcome::Erased));
}

#[tokio::test]
async fn an_erase_below_the_mac_floor_is_refused() {
    // Catches: skipping the floor check.
    let policy = crate::gate::Policy {
        mac_floor: 4,
        ..default_policy()
    };
    let mut f = Fixture::with_policy("erase-floor", policy);
    let request = f.signed("MAC0", Purpose::EraseNow);
    assert_eq!(
        f.gate.erase("MAC0", &request).await,
        Err(Refusal::Floor {
            floor: 4,
            available: 4
        })
    );
    assert!(f.mdm.erases().is_empty());
}

#[tokio::test]
async fn erase_now_is_never_held_and_never_makes_a_grant() {
    // Catches: holding every request (M10 mutant), and ignoring `purpose` so an
    // erase-now request makes a grant.
    let mut f = Fixture::new("erase-never-held");
    let busy = f.signed("MAC1", Purpose::EraseNow);
    f.gate.erase("MAC1", &busy).await.unwrap();
    let request = f.signed("MAC0", Purpose::EraseNow);
    assert!(matches!(
        f.gate.erase("MAC0", &request).await,
        Err(Refusal::Busy(_))
    ));
    assert!(f.gate.fleet().await.macs["MAC0"].held_leases.is_empty());
    assert_eq!(
        f.gate.grant_admin("MAC0", "L1").await,
        Err(Refusal::NoHeldRequest {
            serial: "MAC0".into(),
            lease: "L1".into()
        })
    );
}

#[tokio::test]
async fn a_held_request_makes_only_its_own_leases_grant_once() {
    // Catches: ignoring `purpose` (M10 mutant): a request held for lease L1 must not
    // grant lease L2, and is used up by its grant.
    let mut f = Fixture::new("erase-held");
    let request = f.signed("MAC0", lease("L1"));
    assert_eq!(
        f.gate.erase("MAC0", &request).await,
        Ok(EraseOutcome::Held { lease: "L1".into() })
    );
    assert!(f.mdm.erases().is_empty());
    assert_eq!(f.gate.fleet().await.macs["MAC0"].held_leases, ["L1"]);
    assert!(matches!(
        f.gate.grant_admin("MAC0", "L2").await,
        Err(Refusal::NoHeldRequest { .. })
    ));
    assert!(matches!(
        f.gate.grant_admin("MAC1", "L1").await,
        Err(Refusal::NoHeldRequest { .. })
    ));
    let granted = f.gate.grant_admin("MAC0", "L1").await.unwrap();
    assert_eq!(
        granted.erase_at,
        crate::clock::format_rfc3339(NOW + 8 * HOUR)
    );
    assert!(
        granted
            .grant
            .grant
            .starts_with("kbf-grant-v1\nserial MAC0\nlease L1\n")
    );
    assert!(matches!(
        f.gate.grant_admin("MAC0", "L1").await,
        Err(Refusal::NoHeldRequest { .. })
    ));
    let fleet = f.gate.fleet().await;
    assert_eq!(fleet.macs["MAC0"].scheduled_erase, Some(granted.erase_at));
    assert_eq!((fleet.erases_last_24h, fleet.erases_scheduled), (0, 1));
    assert_eq!(
        f.alerts.summary(),
        [
            "erase held MAC0",
            "grant-admin refused MAC0",
            "grant-admin refused MAC1",
            "grant-admin granted MAC0",
            "grant-admin refused MAC0"
        ]
    );
}

#[tokio::test]
async fn a_second_request_for_a_held_lease_is_refused() {
    let mut f = Fixture::new("erase-held-twice");
    let first = f.signed("MAC0", lease("L1"));
    f.gate.erase("MAC0", &first).await.unwrap();
    let second = f.signed("MAC0", lease("L1"));
    assert_eq!(
        f.gate.erase("MAC0", &second).await,
        Err(Refusal::AlreadyHeld {
            serial: "MAC0".into(),
            lease: "L1".into()
        })
    );
    let other = f.signed("MAC0", lease("L2"));
    assert!(f.gate.erase("MAC0", &other).await.is_ok());
    // The same lease id on another Mac is another request.
    let elsewhere = f.signed("MAC1", lease("L1"));
    assert!(f.gate.erase("MAC1", &elsewhere).await.is_ok());
}

#[tokio::test]
async fn bring_forward_matches_the_lease_and_reports_the_macs_own_earlier_erase() {
    let mut f = Fixture::new("erase-bring-lease");
    for l in ["L1", "L2"] {
        let request = f.signed("MAC0", lease(l));
        f.gate.erase("MAC0", &request).await.unwrap();
        f.gate.grant_admin("MAC0", l).await.unwrap();
    }
    assert!(matches!(
        f.gate.bring_forward("MAC0", "L3").await,
        Err(Refusal::NoGrant { .. })
    ));
    assert!(matches!(
        f.gate.bring_forward("MAC1", "L1").await,
        Err(Refusal::NoGrant { .. })
    ));
    assert_eq!(
        f.gate.bring_forward("MAC0", "L1").await,
        Ok(BroughtForward::Erased)
    );
    f.advance(1);
    assert_eq!(
        f.gate.bring_forward("MAC0", "L2").await,
        Ok(BroughtForward::Waiting {
            behind: "MAC0".into()
        })
    );
}

#[tokio::test]
async fn the_gate_erases_a_granted_mac_on_time_whatever_the_requests_not_after() {
    // Catches: re-checking not-after when the held erase runs (M10 mutant), and
    // leaving the erase to the server (S10 mutant): no call from the server is needed.
    let mut f = Fixture::new("erase-scheduled");
    let request = f.signed("MAC0", lease("L1"));
    f.gate.erase("MAC0", &request).await.unwrap();
    f.gate.grant_admin("MAC0", "L1").await.unwrap();
    f.advance(HOUR);
    f.gate.tick().await.unwrap();
    assert!(
        f.mdm.erases().is_empty(),
        "the request's not-after has passed; nothing is due yet"
    );
    f.advance(7 * HOUR - 1);
    f.gate.tick().await.unwrap();
    assert!(f.mdm.erases().is_empty());
    f.advance(1);
    f.gate.tick().await.unwrap();
    assert_eq!(f.mdm.erases(), ["erase UDID-0"]);
    let fleet = f.gate.fleet().await;
    assert_eq!(
        (fleet.erases_last_24h, fleet.erases_scheduled),
        (1, 0),
        "counted as scheduled until sent, then as sent"
    );
    assert_eq!(
        f.alerts.summary().last().unwrap(),
        "scheduled-erase erased MAC0"
    );
}

#[tokio::test]
async fn a_scheduled_erase_waits_for_the_outstanding_one_and_is_never_dropped() {
    let mut f = Fixture::new("erase-wait");
    let request = f.signed("MAC0", lease("L1"));
    f.gate.erase("MAC0", &request).await.unwrap();
    f.gate.grant_admin("MAC0", "L1").await.unwrap();
    f.advance(8 * HOUR - 10);
    let now_request = f.signed("MAC1", Purpose::EraseNow);
    f.gate.erase("MAC1", &now_request).await.unwrap();
    f.advance(20);
    f.gate.tick().await.unwrap();
    assert_eq!(f.mdm.erases(), ["erase UDID-1"]);
    // The MDM fails when the way clears: the erase stays scheduled and alerts.
    f.mdm.report("UDID-1", f.now());
    f.advance(1);
    f.mdm.0.lock().unwrap().fail_erase = true;
    f.gate.tick().await.unwrap();
    f.mdm.0.lock().unwrap().fail_erase = false;
    assert_eq!(
        f.alerts.summary().last().unwrap(),
        "scheduled-erase failed MAC0"
    );
    let fleet = f.gate.fleet().await;
    assert_eq!(fleet.outstanding_erase, None);
    assert!(fleet.macs["MAC0"].scheduled_erase.is_some());
    f.gate.tick().await.unwrap();
    assert_eq!(f.mdm.erases(), ["erase UDID-1", "erase UDID-0"]);
}

#[tokio::test]
async fn a_held_request_with_no_grant_is_discarded_after_24_hours_with_an_alert() {
    // Catches: keeping held requests forever (M10 mutant).
    let mut f = Fixture::new("erase-discard");
    let request = f.signed("MAC0", lease("L1"));
    f.gate.erase("MAC0", &request).await.unwrap();
    f.advance(DAY - 1);
    f.gate.tick().await.unwrap();
    assert_eq!(f.gate.fleet().await.macs["MAC0"].held_leases, ["L1"]);
    f.advance(1);
    f.gate.tick().await.unwrap();
    assert!(f.gate.fleet().await.macs["MAC0"].held_leases.is_empty());
    assert_eq!(f.alerts.summary().last().unwrap(), "erase discarded MAC0");
    assert!(matches!(
        f.gate.grant_admin("MAC0", "L1").await,
        Err(Refusal::NoHeldRequest { .. })
    ));
}

#[tokio::test]
async fn bring_forward_needs_a_grant() {
    // Catches: running any held request on bring-forward (M10 mutant).
    let mut f = Fixture::new("erase-bring-no-grant");
    let request = f.signed("MAC0", lease("L1"));
    f.gate.erase("MAC0", &request).await.unwrap();
    let refusal = f.gate.bring_forward("MAC0", "L1").await.unwrap_err();
    assert_eq!(
        refusal,
        Refusal::NoGrant {
            serial: "MAC0".into(),
            lease: "L1".into()
        }
    );
    assert_eq!(refusal.status(), 403);
    assert!(f.mdm.erases().is_empty());
    assert_eq!(
        f.alerts.summary().last().unwrap(),
        "bring-forward refused MAC0"
    );
}

#[tokio::test]
async fn bring_forward_erases_now_or_queues_behind_the_outstanding_erase() {
    let mut f = Fixture::new("erase-bring");
    for serial in ["MAC0", "MAC1"] {
        let request = f.signed(serial, lease("L"));
        f.gate.erase(serial, &request).await.unwrap();
        f.gate.grant_admin(serial, "L").await.unwrap();
    }
    assert_eq!(
        f.gate.bring_forward("MAC0", "L").await,
        Ok(BroughtForward::Erased)
    );
    assert_eq!(
        f.gate.bring_forward("MAC1", "L").await,
        Ok(BroughtForward::Waiting {
            behind: "MAC0".into()
        })
    );
    assert_eq!(f.mdm.erases(), ["erase UDID-0"]);
    f.mdm.report("UDID-0", f.now() + 1);
    f.advance(2);
    f.gate.tick().await.unwrap();
    assert_eq!(f.mdm.erases(), ["erase UDID-0", "erase UDID-1"]);
    // A failing MDM: the erase stays scheduled and bring-forward says it waits.
    let mut g = Fixture::new("erase-bring-fail");
    let request = g.signed("MAC0", lease("L"));
    g.gate.erase("MAC0", &request).await.unwrap();
    g.gate.grant_admin("MAC0", "L").await.unwrap();
    g.mdm.set_fail(true);
    assert_eq!(
        g.gate.bring_forward("MAC0", "L").await,
        Ok(BroughtForward::Waiting {
            behind: String::new()
        })
    );
}

#[tokio::test]
async fn grant_admin_keeps_the_cap_and_the_floor() {
    // Catches: granting on erase budget alone, or past it.
    let policy = crate::gate::Policy {
        daily_erase_cap: 1,
        ..default_policy()
    };
    let mut f = Fixture::with_policy("grant-cap", policy);
    let held = f.signed("MAC0", lease("L1"));
    f.gate.erase("MAC0", &held).await.unwrap();
    let now = f.signed("MAC1", Purpose::EraseNow);
    f.gate.erase("MAC1", &now).await.unwrap();
    assert_eq!(
        f.gate.grant_admin("MAC0", "L1").await,
        Err(Refusal::DailyCap(1))
    );
    let policy = crate::gate::Policy {
        mac_floor: 4,
        ..default_policy()
    };
    let mut g = Fixture::with_policy("grant-floor", policy);
    let held = g.signed("MAC0", lease("L1"));
    g.gate.erase("MAC0", &held).await.unwrap();
    assert_eq!(
        g.gate.grant_admin("MAC0", "L1").await,
        Err(Refusal::Floor {
            floor: 4,
            available: 4
        })
    );
}

#[tokio::test]
async fn a_failed_erase_takes_nothing_from_the_caps() {
    let mut f = Fixture::new("erase-fail");
    f.mdm.set_fail(true);
    let request = f.signed("MAC0", Purpose::EraseNow);
    let refusal = f.gate.erase("MAC0", &request).await.unwrap_err();
    assert_eq!(refusal.status(), 502);
    let fleet = f.gate.fleet().await;
    assert_eq!((fleet.outstanding_erase, fleet.erases_last_24h), (None, 0));
}

#[tokio::test]
async fn state_survives_a_restart() {
    // Catches: keeping the nonce log, held requests or scheduled erases in memory only.
    let mut f = Fixture::new("erase-restart");
    let used = f.signed("MAC1", Purpose::EraseNow);
    f.gate.erase("MAC1", &used).await.unwrap();
    let request = f.signed("MAC0", lease("L1"));
    f.gate.erase("MAC0", &request).await.unwrap();
    f.gate.grant_admin("MAC0", "L1").await.unwrap();
    f.restart();
    assert_eq!(f.gate.erase("MAC1", &used).await, Err(Refusal::Replay));
    f.mdm.report("UDID-1", f.now() + 1);
    f.advance(8 * HOUR);
    f.gate.tick().await.unwrap();
    assert_eq!(f.mdm.erases(), ["erase UDID-1", "erase UDID-0"]);
}

#[tokio::test]
async fn spent_nonces_are_forgotten_after_their_not_after() {
    let mut f = Fixture::new("erase-prune");
    let request = f.signed("MAC0", lease("L1"));
    f.gate.erase("MAC0", &request).await.unwrap();
    f.advance(HOUR);
    f.gate.tick().await.unwrap();
    assert!(f.gate.state.lock().await.nonces.is_empty());
    // A replay now is refused as expired.
    assert_eq!(f.gate.erase("MAC0", &request).await, Err(Refusal::Expired));
}

#[tokio::test]
async fn the_allowed_signers_file_is_reread_and_must_be_trusted() {
    let mut f = Fixture::new("erase-signers");
    f.write_file("allowed_signers", b"# nobody\n");
    let request = f.signed("MAC0", Purpose::EraseNow);
    assert_eq!(
        f.gate.erase("MAC0", &request).await,
        Err(Refusal::Signature(VerifyError::UnknownSigner))
    );
    f.write_file("allowed_signers", b"bad line\n");
    assert!(
        matches!(f.gate.erase("MAC0", &request).await, Err(Refusal::Internal(e)) if e.contains("line 1"))
    );
    std::fs::remove_file(f.dir.join("allowed_signers")).unwrap();
    assert!(matches!(
        f.gate.erase("MAC0", &request).await,
        Err(Refusal::Internal(_))
    ));
}

#[tokio::test]
async fn a_signed_message_that_is_not_a_request_is_refused() {
    let f = Fixture::new("erase-garbage");
    let message = "kbf-erase-v1\nserial MAC0\n".to_owned();
    let signed = SignedRequest {
        signature: f.key.sign(message.as_bytes()),
        message,
    };
    let refusal = f.gate.erase("MAC0", &signed).await.unwrap_err();
    assert!(matches!(refusal, Refusal::Request(_)));
    assert_eq!(refusal.status(), 400);
}

#[tokio::test]
async fn an_erased_mac_that_left_the_inventory_or_whose_status_fails_is_handled() {
    let mut f = Fixture::new("erase-status-fail");
    let request = f.signed("MAC0", Purpose::EraseNow);
    f.gate.erase("MAC0", &request).await.unwrap();
    f.mdm.set_fail(true);
    f.gate.tick().await.unwrap();
    assert!(f.gate.fleet().await.macs["MAC0"].erasing);
    f.mdm.set_fail(false);
    f.gate.state.lock().await.erased.insert("GONE".into(), NOW);
    f.gate.tick().await.unwrap();
    assert!(!f.gate.state.lock().await.erased.contains_key("GONE"));
}

#[tokio::test]
async fn a_refused_signed_request_alerts_with_its_signer_and_purpose() {
    // Catches: refusal alerts that name only the refusal (M4.2: every refused request
    // alerts "naming the serial, the purpose and the signer"), once the signature has
    // verified; before that nothing in the request is trusted, so nothing is named.
    let mut f = Fixture::new("erase-refusal-detail");
    let first = f.signed("MAC0", Purpose::EraseNow);
    f.gate.erase("MAC0", &first).await.unwrap();
    let busy = f.signed("MAC1", Purpose::EraseNow);
    f.gate.erase("MAC1", &busy).await.unwrap_err();
    let held = f.signed("MAC2", lease("L7"));
    f.gate.erase("MAC2", &held).await.unwrap();
    let again = f.signed("MAC2", lease("L7"));
    f.gate.erase("MAC2", &again).await.unwrap_err();
    let mut forged = f.signed("MAC1", Purpose::EraseNow);
    forged.signature = f.key.sign(b"another message");
    f.gate.erase("MAC1", &forged).await.unwrap_err();
    let details: Vec<String> = f
        .alerts
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.detail.clone())
        .collect();
    assert_eq!(
        details[1],
        "an erase of MAC0 is outstanding; request erase-now signed by alice@example.org \
         (sk-ssh-ed25519@openssh.com): test"
    );
    assert_eq!(
        details[3],
        "a request for MAC2 and lease L7 is already held; request privileged-lease L7 \
         signed by alice@example.org (sk-ssh-ed25519@openssh.com): test"
    );
    assert_eq!(details[4], "signature: the signature does not verify");
    assert_eq!(
        f.alerts.summary(),
        [
            "erase erased MAC0",
            "erase refused MAC1",
            "erase held MAC2",
            "erase refused MAC2",
            "erase refused MAC1"
        ]
    );
}

#[tokio::test]
async fn a_signed_request_whose_text_does_not_parse_alerts_with_its_signer() {
    // Catches: an alert naming only the parse error once the signature has verified
    // (M4.2: the signer is known then, though the purpose is not).
    let f = Fixture::new("erase-unparsed-signer");
    let message = "not an erase request\n".to_owned();
    let request = SignedRequest {
        signature: f.key.sign(message.as_bytes()),
        message,
    };
    let refusal = f.gate.erase("MAC0", &request).await.unwrap_err();
    let details: Vec<String> = f
        .alerts
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.detail.clone())
        .collect();
    assert_eq!(
        details,
        [format!(
            "{refusal}; request signed by alice@example.org (sk-ssh-ed25519@openssh.com), \
             its text unparsed"
        )]
    );
    assert_eq!(f.alerts.summary(), ["erase refused MAC0"]);
}

#[tokio::test]
async fn a_scheduled_erase_counts_toward_the_cap_when_it_is_sent() {
    // Catches: counting a grant's erase only when it is reserved (M4.3 "at most two Macs
    // a day"): with cap 2, a grant at t0 whose erase is sent at t0+8h, and an erase-now
    // at t0+8h, a second erase-now at t0+24h would be a third erase within 16 hours.
    let mut f = Fixture::new("erase-cap-scheduled");
    let held = f.signed("MAC0", lease("L1"));
    f.gate.erase("MAC0", &held).await.unwrap();
    f.gate.grant_admin("MAC0", "L1").await.unwrap();
    f.advance(8 * HOUR);
    f.gate.tick().await.unwrap();
    assert_eq!(f.mdm.erases(), ["erase UDID-0"]);
    f.mdm.report("UDID-0", f.now() + 1);
    f.advance(2);
    f.gate.tick().await.unwrap();
    let second = f.signed("MAC1", Purpose::EraseNow);
    f.gate.erase("MAC1", &second).await.unwrap();
    f.mdm.report("UDID-1", f.now() + 1);
    f.advance(2);
    f.gate.tick().await.unwrap();
    f.clock
        .0
        .store(NOW + DAY + 1, std::sync::atomic::Ordering::SeqCst);
    f.gate.tick().await.unwrap();
    let third = f.signed("MAC2", Purpose::EraseNow);
    assert_eq!(
        f.gate.erase("MAC2", &third).await,
        Err(Refusal::DailyCap(2))
    );
    assert_eq!(f.mdm.erases(), ["erase UDID-0", "erase UDID-1"]);
    // Once the scheduled erase's own send leaves the window, there is room again.
    f.clock
        .0
        .store(NOW + 8 * HOUR + DAY, std::sync::atomic::Ordering::SeqCst);
    let fourth = f.signed("MAC2", Purpose::EraseNow);
    assert_eq!(
        f.gate.erase("MAC2", &fourth).await,
        Ok(EraseOutcome::Erased)
    );
}

#[tokio::test]
async fn scheduled_erases_hold_cap_room_until_they_are_sent() {
    // Catches: leaving scheduled erases out of the cap: two grants with cap 2 leave no
    // room for an erase-now or a third grant, however old the grants are.
    let mut f = Fixture::new("erase-cap-reserved");
    for serial in ["MAC0", "MAC1"] {
        let held = f.signed(serial, lease("L"));
        f.gate.erase(serial, &held).await.unwrap();
        f.gate.grant_admin(serial, "L").await.unwrap();
    }
    let held = f.signed("MAC2", lease("L"));
    f.gate.erase("MAC2", &held).await.unwrap();
    assert_eq!(
        f.gate.grant_admin("MAC2", "L").await,
        Err(Refusal::DailyCap(2))
    );
    let fleet = f.gate.fleet().await;
    assert_eq!((fleet.erases_last_24h, fleet.erases_scheduled), (0, 2));
    let now = f.signed("MAC2", Purpose::EraseNow);
    assert_eq!(f.gate.erase("MAC2", &now).await, Err(Refusal::DailyCap(2)));
}

#[tokio::test]
async fn grant_admin_refuses_a_held_request_24_hours_old_before_the_tick_discards_it() {
    // Catches: granting a held request the tick has not discarded yet (up to
    // --tick-seconds after its 24 hours).
    let mut f = Fixture::new("grant-stale");
    for l in ["L1", "L2"] {
        let held = f.signed("MAC0", lease(l));
        f.gate.erase("MAC0", &held).await.unwrap();
    }
    f.advance(DAY - 1);
    assert!(f.gate.grant_admin("MAC0", "L1").await.is_ok());
    f.advance(1);
    assert_eq!(
        f.gate.grant_admin("MAC0", "L2").await,
        Err(Refusal::NoHeldRequest {
            serial: "MAC0".into(),
            lease: "L2".into()
        })
    );
}

#[tokio::test]
async fn a_report_in_the_same_second_as_the_erase_does_not_clear_it() {
    // Pins the boundary of the re-enrollment signal: only a report later than the
    // erase's second counts (a `>=` would clear on a report from before the erase).
    let mut f = Fixture::new("erase-report-boundary");
    let first = f.signed("MAC0", Purpose::EraseNow);
    f.gate.erase("MAC0", &first).await.unwrap();
    f.mdm.report("UDID-0", NOW);
    f.advance(5);
    f.gate.tick().await.unwrap();
    assert_eq!(
        f.gate.fleet().await.outstanding_erase.as_deref(),
        Some("MAC0")
    );
    f.mdm.report("UDID-0", NOW + 1);
    f.gate.tick().await.unwrap();
    assert_eq!(f.gate.fleet().await.outstanding_erase, None);
}

#[test]
fn outcomes_serialise_with_their_names() {
    let held = serde_json::to_value(EraseOutcome::Held { lease: "L".into() }).unwrap();
    assert_eq!(held, serde_json::json!({"outcome": "held", "lease": "L"}));
    let waiting = serde_json::to_value(BroughtForward::Waiting { behind: "M".into() }).unwrap();
    assert_eq!(
        waiting,
        serde_json::json!({"outcome": "waiting", "behind": "M"})
    );
}
