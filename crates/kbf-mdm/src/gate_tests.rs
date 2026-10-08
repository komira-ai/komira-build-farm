//! Status, enforce, withdraw, profile and reconciliation (M2.2, S5.2). Each test names
//! the planted mutant that turns it red.

use super::fixture::{Fixture, NOW, default_policy};
use super::*;
use crate::sets::fixture as sets;

const DEADLINE: &str = "2027-01-16T02:00:00";

fn enforce_request(pool: &str, serial: u64, min_serial: u64, build: &str) -> EnforceRequest {
    EnforceRequest {
        key_statement: sets::statement(4, "2030-01-01T00:00:00Z"),
        set: sets::set(pool, serial, min_serial, build),
        deadline: DEADLINE.into(),
    }
}

#[tokio::test]
async fn status_reports_the_mdm_and_the_gate() {
    let f = Fixture::new("gate-status");
    f.mdm.report("UDID-0", NOW - 60);
    let status = f.gate.status("MAC0").await.unwrap();
    assert_eq!(status.pool, "mac-arm64");
    assert_eq!(status.last_seen.as_deref(), Some("2027-01-15T07:59:00Z"));
    assert_eq!(status.items["softwareupdate.failure-reason"], "");
    assert_eq!(status.gate, MacGate::default());
    assert_eq!(
        f.gate.status("NOPE").await,
        Err(Refusal::NotInInventory("NOPE".into()))
    );
    f.mdm.set_fail(true);
    assert_eq!(f.gate.status("MAC0").await.unwrap_err().status(), 502);
    let fleet = f.gate.fleet().await;
    assert_eq!(fleet.macs.len(), 4);
    assert_eq!(
        (fleet.mac_floor, fleet.available, fleet.daily_erase_cap),
        (1, 4, 2)
    );
}

#[tokio::test]
async fn enforce_posts_the_build_the_signed_set_names() {
    let f = Fixture::new("gate-enforce");
    let enforced = f
        .gate
        .enforce("MAC0", &enforce_request("mac-arm64", 12, 10, "27B5"))
        .await
        .unwrap();
    assert_eq!((enforced.build.as_str(), enforced.set_serial), ("27B5", 12));
    assert_eq!(
        f.mdm.calls(),
        [format!("enforce UDID-0 27.1 27B5 {DEADLINE}")]
    );
    assert_eq!(f.alerts.summary(), ["enforce enforced MAC0"]);
    let status = f.gate.status("MAC0").await.unwrap();
    assert_eq!(status.gate.enforcement, Some(enforced));
    assert_eq!(f.gate.fleet().await.available, 3);
}

#[tokio::test]
async fn enforce_refuses_what_the_set_does_not_allow() {
    // Catches: skipping the gate's expiry check and its pool floor (S10 mutants), and
    // the pool and platform checks.
    let f = Fixture::new("gate-enforce-refusals");
    let mut bad_deadline = enforce_request("mac-arm64", 12, 10, "27B5");
    bad_deadline.deadline = "tomorrow".into();
    assert_eq!(
        f.gate
            .enforce("MAC0", &bad_deadline)
            .await
            .unwrap_err()
            .status(),
        400
    );
    let mut expired = sets::set_doc("mac-arm64", 12, 10, "27B5");
    expired["expires"] = "2027-01-01T00:00:00Z".into();
    let request = EnforceRequest {
        set: sets::seal(&sets::key(sets::PLATFORM), &expired),
        ..enforce_request("mac-arm64", 0, 0, "")
    };
    assert_eq!(
        f.gate.enforce("MAC0", &request).await,
        Err(Refusal::Set(SetError::SetExpired))
    );
    assert_eq!(
        f.gate
            .enforce("MAC0", &enforce_request("mac-x86", 12, 10, "27B5"))
            .await,
        Err(Refusal::WrongPool {
            set: "mac-x86".into(),
            mac: "mac-arm64".into()
        })
    );
    let mut linux = sets::set_doc("mac-arm64", 12, 10, "27B5");
    linux["platform"]["os"] = "linux".into();
    let request = EnforceRequest {
        set: sets::seal(&sets::key(sets::PLATFORM), &linux),
        ..enforce_request("mac-arm64", 0, 0, "")
    };
    assert_eq!(
        f.gate.enforce("MAC0", &request).await,
        Err(Refusal::NotMacos("linux".into()))
    );
    assert!(f.mdm.calls().is_empty());
    assert_eq!(f.alerts.summary(), ["enforce refused MAC0"].repeat(4));
}

#[tokio::test]
async fn the_gate_keeps_its_own_pool_floor_and_newest_statement() {
    // Catches: skipping the floor check: once the gate saw min_serial 10, a set below it
    // is refused even though it verifies.
    let f = Fixture::new("gate-pool-floor");
    f.gate
        .enforce("MAC0", &enforce_request("mac-arm64", 12, 10, "27B5"))
        .await
        .unwrap();
    f.gate.withdraw("MAC0").await.unwrap();
    assert_eq!(
        f.gate
            .enforce("MAC1", &enforce_request("mac-arm64", 9, 0, "27A1"))
            .await,
        Err(Refusal::BelowPoolFloor {
            serial: 9,
            floor: 10
        })
    );
    // An older set above the floor verifies, and does not replace the newer set's
    // profiles.
    f.gate
        .enforce("MAC1", &enforce_request("mac-arm64", 11, 0, "27B4"))
        .await
        .unwrap();
    assert_eq!(f.gate.state.lock().await.pool_sets["mac-arm64"].serial, 12);
    let old = EnforceRequest {
        key_statement: sets::statement(3, "2030-01-01T00:00:00Z"),
        ..enforce_request("mac-x86", 1, 0, "27B5")
    };
    assert_eq!(
        f.gate.enforce("MACX", &old).await,
        Err(Refusal::OldStatement { got: 3, newest: 4 })
    );
}

#[tokio::test]
async fn one_enforcement_at_a_time_per_pool() {
    // Catches: dropping the per-pool outstanding-enforcement check (S10).
    let f = Fixture::new("gate-enforce-one");
    f.gate
        .enforce("MAC0", &enforce_request("mac-arm64", 12, 10, "27B5"))
        .await
        .unwrap();
    let refusal = f
        .gate
        .enforce("MAC1", &enforce_request("mac-arm64", 12, 10, "27B5"))
        .await
        .unwrap_err();
    assert_eq!(
        refusal,
        Refusal::Busy("an enforcement is outstanding in pool mac-arm64 (on MAC0)".into())
    );
    // Another pool is not held back.
    f.gate
        .enforce("MACX", &enforce_request("mac-x86", 3, 0, "27B5"))
        .await
        .unwrap();
    // Withdrawing frees the pool.
    f.gate.withdraw("MAC0").await.unwrap();
    assert!(
        f.mdm
            .calls()
            .contains(&"withdraw MAC0 kbf.osupdate.MAC0".to_owned())
    );
    f.gate
        .enforce("MAC1", &enforce_request("mac-arm64", 12, 10, "27B5"))
        .await
        .unwrap();
    assert_eq!(
        f.gate.withdraw("NOPE").await,
        Err(Refusal::NotInInventory("NOPE".into()))
    );
}

#[tokio::test]
async fn enforce_keeps_the_mac_floor_and_leaves_erasing_macs_alone() {
    let policy = Policy {
        mac_floor: 4,
        ..default_policy()
    };
    let f = Fixture::with_policy("gate-enforce-floor", policy);
    assert_eq!(
        f.gate
            .enforce("MAC0", &enforce_request("mac-arm64", 12, 10, "27B5"))
            .await,
        Err(Refusal::Floor {
            floor: 4,
            available: 4
        })
    );
    let mut g = Fixture::new("gate-enforce-erasing");
    let request = g.signed("MAC0", crate::request::Purpose::EraseNow);
    g.gate.erase("MAC0", &request).await.unwrap();
    assert_eq!(
        g.gate
            .enforce("MAC0", &enforce_request("mac-arm64", 12, 10, "27B5"))
            .await,
        Err(Refusal::Busy("MAC0 is being erased".into()))
    );
    g.mdm.set_fail(true);
    assert_eq!(
        g.gate
            .enforce("MAC1", &enforce_request("mac-arm64", 12, 10, "27B5"))
            .await
            .unwrap_err()
            .status(),
        502
    );
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[tokio::test]
async fn a_profile_is_installed_only_by_an_allowlisted_digest_from_the_gates_own_copy() {
    // Catches: installing a profile the allowlist does not name.
    let f = Fixture::new("gate-profile");
    let profile = b"<plist>fda</plist>";
    let d = digest(profile);
    f.write_file(&format!("profiles/{d}.mobileconfig"), profile);
    assert_eq!(
        f.gate.profile("MAC0", &d).await,
        Err(Refusal::NotAllowlisted(d.clone()))
    );
    f.write_file("allowlist", format!("# fda\n{d}\n").as_bytes());
    f.gate.profile("MAC0", &d).await.unwrap();
    assert_eq!(f.mdm.calls(), ["profile UDID-0 <plist>fda</plist>"]);
    assert_eq!(
        f.alerts.summary(),
        ["profile refused MAC0", "profile installed MAC0"]
    );
    assert_eq!(
        f.gate.profile("MAC0", "ABC").await.unwrap_err().status(),
        400
    );
}

#[tokio::test]
async fn a_profile_named_by_a_verified_set_for_the_pool_is_allowed() {
    let f = Fixture::new("gate-profile-set");
    let profile = b"<plist>set</plist>";
    let d = digest(profile);
    f.write_file(&format!("profiles/{d}.mobileconfig"), profile);
    let mut doc = sets::set_doc("mac-arm64", 12, 10, "27B5");
    doc["profiles"] = serde_json::json!([d]);
    let request = EnforceRequest {
        set: sets::seal(&sets::key(sets::PLATFORM), &doc),
        ..enforce_request("mac-arm64", 0, 0, "")
    };
    f.gate.enforce("MAC0", &request).await.unwrap();
    f.gate.profile("MAC1", &d).await.unwrap();
    // Only for that pool.
    assert_eq!(
        f.gate.profile("MACX", &d).await,
        Err(Refusal::NotAllowlisted(d.clone()))
    );
}

#[tokio::test]
async fn a_missing_or_altered_profile_copy_is_refused() {
    let f = Fixture::new("gate-profile-bad");
    let d = digest(b"real");
    f.write_file("allowlist", d.as_bytes());
    assert_eq!(f.gate.profile("MAC0", &d).await.unwrap_err().status(), 500);
    f.write_file(&format!("profiles/{d}.mobileconfig"), b"altered");
    let refusal = f.gate.profile("MAC0", &d).await.unwrap_err();
    assert!(
        refusal.to_string().ends_with("does not match its digest"),
        "{refusal}"
    );
    std::fs::remove_file(f.dir.join("allowlist")).unwrap();
    assert!(matches!(
        f.gate.profile("MAC0", &d).await,
        Err(Refusal::Internal(_))
    ));
    assert!(f.mdm.calls().is_empty());
}

#[tokio::test]
async fn reconcile_withdraws_only_stale_kbf_enforcements() {
    // Catches: matching identifiers by substring (M10 mutant): the operator's own
    // declarations, whatever they contain, are never touched.
    let f = Fixture::new("gate-reconcile");
    f.gate
        .enforce("MAC0", &enforce_request("mac-arm64", 12, 10, "27B5"))
        .await
        .unwrap();
    f.mdm.0.lock().unwrap().declarations = [
        "kbf.osupdate.MAC0",
        "kbf.osupdate.MAC1",
        "kbf.status-subscriptions",
        "com.example.kbf.osupdate.MAC2",
        "x.kbf.osupdate.MAC2",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(f.gate.reconcile().await.unwrap(), ["kbf.osupdate.MAC1"]);
    let calls = f.mdm.calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("subscribe ")).count(),
        4
    );
    let withdrawals: Vec<_> = calls
        .iter()
        .filter(|c| c.starts_with("withdraw "))
        .collect();
    assert_eq!(withdrawals, ["withdraw MAC1 kbf.osupdate.MAC1"]);
    f.mdm.set_fail(true);
    assert_eq!(f.gate.reconcile().await.unwrap_err().status(), 502);
}

#[tokio::test]
async fn a_verb_that_cannot_save_its_state_fails() {
    let f = Fixture::new("gate-state-fail");
    std::fs::create_dir_all(f.dir.join("state.json").join("blocked")).unwrap();
    let refusal = f.gate.withdraw("MAC0").await.unwrap_err();
    assert_eq!(refusal.status(), 500);
    assert!(
        refusal.to_string().starts_with("internal: state file "),
        "{refusal}"
    );
}

/// Linux has a device whose writes always fail.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_verb_that_cannot_write_the_audit_log_fails() {
    let mut f = Fixture::new("gate-audit-fail");
    f.restart_with_audit_log(std::path::Path::new("/dev/full"));
    let refusal = f.gate.withdraw("MAC0").await.unwrap_err();
    assert!(
        refusal.to_string().starts_with("internal: audit log: "),
        "{refusal}"
    );
}

#[test]
fn inventory_files_are_checked() {
    assert_eq!(
        Inventory::parse(r#"{"macs": []}"#).map(|i| (i.len(), i.is_empty())),
        Ok((0, true))
    );
    let dup = r#"{"macs": [{"serial": "A", "enrollment": "1", "pool": "p"}, {"serial": "A", "enrollment": "2", "pool": "p"}]}"#;
    assert_eq!(Inventory::parse(dup), Err("serial A listed twice".into()));
    let bad = r#"{"macs": [{"serial": "A-1", "enrollment": "1", "pool": "p"}]}"#;
    assert_eq!(Inventory::parse(bad), Err("bad serial \"A-1\"".into()));
    assert!(Inventory::parse(r#"{"macs": [], "extra": 1}"#).is_err());
}

#[test]
fn refusals_map_to_http_statuses() {
    let cases = [
        (Refusal::BadRequest(String::new()), 400),
        (Refusal::Expired, 403),
        (
            Refusal::AlreadyHeld {
                serial: String::new(),
                lease: String::new(),
            },
            409,
        ),
        (Refusal::Internal(String::new()), 500),
    ];
    for (refusal, status) in cases {
        assert_eq!(refusal.status(), status, "{refusal}");
    }
}
