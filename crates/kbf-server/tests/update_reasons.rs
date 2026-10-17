//! The small classifications the controller reads off the update module: how loudly each
//! hold and alert is delivered, a release's name, and which host rows the picker may open.
//!
//! Each test names the defect it would catch.

#[path = "update_support/mod.rs"]
mod support;

use kbf_server::update::{
    Alert, HoldReason, HostPhase, HostRecord, OrderId, Release, Serial, Severity,
};
use support::{NOW, SERIAL, t};

/// Catches: a hold that leaves the binary healthy, or the node already put back, paging a
/// person as if data availability were at risk (or the other way round).
#[test]
fn every_hold_reason_has_the_severity_its_cause_calls_for() {
    for reason in [
        HoldReason::SecondMemberDown,
        HoldReason::RestartDeadline,
        HoldReason::MemberGone,
        HoldReason::RevertRefused,
    ] {
        assert_eq!(reason.severity(), Severity::Page, "{reason:?}");
    }
    for reason in [
        HoldReason::ActivateStalled,
        HoldReason::HealthLimit,
        HoldReason::RebuildLimit,
        HoldReason::RepairWait,
        HoldReason::ConfirmStalled,
        HoldReason::Reverted,
        HoldReason::StillCordoned,
    ] {
        assert_eq!(reason.severity(), Severity::Warn, "{reason:?}");
    }
}

/// Catches: an alert for a held run delivered at a fixed severity instead of its reason's.
#[test]
fn a_held_alert_is_as_loud_as_its_reason() {
    assert_eq!(
        Alert::Held(HoldReason::SecondMemberDown).severity(),
        Severity::Page
    );
    assert_eq!(
        Alert::Held(HoldReason::StillCordoned).severity(),
        Severity::Warn
    );
    assert_eq!(Alert::RebuildWaiting.severity(), Severity::Warn);
}

/// Catches: a release that does not give back the name it was made from.
#[test]
fn a_release_keeps_its_name() {
    assert_eq!(Release::new("0123abcd").as_str(), "0123abcd");
}

/// Catches: a host that is in a step, or held, read as open, so the picker could start it a
/// second time while it holds the slot.
#[test]
fn only_pending_deferred_and_earlier_done_rows_are_open() {
    let row = |phase| HostRecord {
        phase,
        since: t(NOW),
        serial: SERIAL,
        order: OrderId(1),
        boot_before: None,
    };
    let later = Serial(SERIAL.0 + 1);
    assert!(row(HostPhase::Pending).is_open_in(SERIAL));
    assert!(row(HostPhase::Deferred).is_open_in(SERIAL));
    assert!(!row(HostPhase::Done).is_open_in(SERIAL));
    assert!(row(HostPhase::Done).is_open_in(later));
    for phase in [
        HostPhase::Selected,
        HostPhase::Cordoned,
        HostPhase::WaitingLeases,
        HostPhase::PlannedDown,
        HostPhase::Gated,
        HostPhase::Activating,
        HostPhase::RejoiningStorage,
        HostPhase::Repairing,
        HostPhase::Confirming,
        HostPhase::RejoiningWork,
        HostPhase::Held {
            reason: HoldReason::RepairWait,
        },
    ] {
        assert!(!row(phase.clone()).is_open_in(later), "{phase:?}");
    }
}
