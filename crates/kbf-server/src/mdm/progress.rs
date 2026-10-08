//! Reading a macOS update's progress from the gate, and what the catalogue offers each
//! Mac (`mdm-backend.md` sections M2.2 and M3, `fleet-updates.md` sections 3.2, 4.1, 4.3
//! and 7.2).
//!
//! - [`assess`] reads one `status` answer for one Mac being updated. A non-empty DDM
//!   `failure-reason` is a failure, which holds the rollout. Everything else is
//!   [`Progress::Pending`], **even when DDM already reports the target build**: on
//!   macOS 27 nothing announces completion, and the done signal is the node's own
//!   `Hello` with the target build (4.2 step 7).
//! - [`watch`] polls `status` every interval (60 s by default, [`POLL_INTERVAL`])
//!   until the caller signals done, DDM reports a failure, the enforcement is gone or
//!   names another build, or the deadline plus the macOS return deadline passes. A
//!   gate that cannot be reached is reported and polled again; it ends nothing.
//! - [`stale_enforcements`] and [`reconcile`]: at startup every outstanding
//!   enforcement that matches no durable `updating` step is withdrawn (4.1).
//! - [`updates_for`]: the catalogue's newer releases for a Mac, and when its own build
//!   leaves the catalogue; [`expires_soon`] is the 14-day alert (7.2).

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use kbf_mdm_api::catalogue::CatalogueEntry;
use kbf_mdm_api::names::{Date, OsVersion, Serial};

use super::{Catalogue, Enforcement, GateError, Inventory, MacStatus, MdmGate};

/// How often [`watch`] polls while a node is `updating` (an assumption, M3).
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How many days before a pinned build leaves the catalogue the server alerts (7.2).
pub const EXPIRY_WARNING_DAYS: i64 = 14;

/// One Mac's update, as one `status` answer shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The gate does not list the Mac.
    Missing,
    /// No kbf enforcement is outstanding on it.
    NotEnforced,
    /// The outstanding enforcement names another build.
    OtherTarget(Enforcement),
    /// Under way, as far as DDM says; never the done signal.
    Pending {
        /// `softwareupdate.install-state`.
        install_state: String,
        /// `softwareupdate.pending-version`'s build.
        pending_build: String,
        /// The build DDM last reported the Mac running.
        os_build: String,
    },
    /// DDM reports a non-empty `failure-reason`.
    Failed {
        /// The reason.
        reason: String,
        /// How many times it failed.
        count: u32,
    },
}

/// What `inventory` says of `serial`'s update to `target_build`.
#[must_use]
pub fn assess(inventory: &Inventory, serial: &Serial, target_build: &str) -> Progress {
    let Some(mac) = inventory.mac(serial) else {
        return Progress::Missing;
    };
    let Some(enforcement) = &mac.enforcement else {
        return Progress::NotEnforced;
    };
    if enforcement.target_build != target_build {
        return Progress::OtherTarget(enforcement.clone());
    }
    let su = &mac.software_update;
    if !su.failure_reason.is_empty() {
        return Progress::Failed {
            reason: su.failure_reason.clone(),
            count: su.failure_count,
        };
    }
    Progress::Pending {
        install_state: su.install_state.clone(),
        pending_build: su.pending_build.clone(),
        os_build: mac.os_build.clone(),
    }
}

/// One Mac's update to watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watch {
    /// The Mac.
    pub serial: Serial,
    /// The build the enforcement names.
    pub target_build: String,
    /// The enforcement's deadline plus the macOS return deadline (3.3), milliseconds
    /// since the Unix epoch: past it with no done signal, the update has failed.
    pub give_up_at_unix_ms: u64,
    /// How often to poll.
    pub interval: Duration,
}

/// Why [`watch`] stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEnd {
    /// The caller's done signal (the node's `Hello` with the target build) arrived.
    Done,
    /// DDM reported a failure: hold the rollout (4.3).
    Failed {
        /// The `failure-reason`.
        reason: String,
    },
    /// The give-up time passed with no done signal: hold the rollout.
    Overdue,
    /// The Mac left the inventory, or its enforcement was withdrawn or replaced: hold.
    Lost(Progress),
}

/// Polls the gate for `w` until `done` completes or the update fails. `now` is the
/// clock (milliseconds since the Unix epoch); `report` sees every poll's result, for
/// the UI. The first poll is at once.
pub async fn watch<D, N, R>(
    gate: &dyn MdmGate,
    w: &Watch,
    done: D,
    now: N,
    mut report: R,
) -> WatchEnd
where
    D: Future<Output = ()>,
    N: Fn() -> u64,
    R: FnMut(Result<&Progress, &GateError>),
{
    let mut ticks = tokio::time::interval(w.interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let serials = [w.serial.clone()];
    tokio::pin!(done);
    loop {
        tokio::select! {
            biased;
            () = &mut done => return WatchEnd::Done,
            _ = ticks.tick() => {}
        }
        if now() >= w.give_up_at_unix_ms {
            return WatchEnd::Overdue;
        }
        let progress = match gate.status(&serials).await {
            Ok(inventory) => assess(&inventory, &w.serial, &w.target_build),
            Err(e) => {
                report(Err(&e));
                continue;
            }
        };
        report(Ok(&progress));
        match progress {
            Progress::Pending { .. } => {}
            Progress::Failed { reason, .. } => return WatchEnd::Failed { reason },
            lost => return WatchEnd::Lost(lost),
        }
    }
}

/// The Macs whose outstanding enforcement matches no durable `updating` step:
/// `expected` maps each Mac a rollout is updating to the build its step names.
#[must_use]
pub fn stale_enforcements(
    inventory: &Inventory,
    expected: &BTreeMap<Serial, String>,
) -> Vec<Serial> {
    inventory
        .macs
        .iter()
        .filter(|mac| {
            mac.enforcement
                .as_ref()
                .is_some_and(|e| expected.get(&mac.serial) != Some(&e.target_build))
        })
        .map(|mac| mac.serial.clone())
        .collect()
}

/// Startup reconciliation with the gate (4.1): withdraws every stale enforcement and
/// returns the Macs it withdrew. Stops at the first failed withdrawal.
///
/// # Errors
/// The first [`GateError`] of `status` or of a withdrawal.
pub async fn reconcile(
    gate: &dyn MdmGate,
    expected: &BTreeMap<Serial, String>,
) -> Result<Vec<Serial>, GateError> {
    let inventory = gate.status(&[]).await?;
    let stale = stale_enforcements(&inventory, expected);
    for serial in &stale {
        gate.withdraw(serial).await?;
    }
    Ok(stale)
}

/// What the catalogue offers one Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacUpdates {
    /// Releases newer than the Mac's version, newest first. Empty if the Mac's
    /// version is not known.
    pub newer: Vec<CatalogueEntry>,
    /// When the catalogue stops listing the Mac's own build (the latest expiry among
    /// its entries); `None` if it does not list it, so it can no longer be enforced.
    pub current_build_expires: Option<Date>,
}

/// What `catalogue` offers `mac`. Nothing filters by the Mac's model yet (M2.2).
#[must_use]
pub fn updates_for(mac: &MacStatus, catalogue: &Catalogue) -> MacUpdates {
    let mut newer: Vec<CatalogueEntry> = match OsVersion::parse(&mac.os_version) {
        Ok(version) => catalogue
            .entries
            .iter()
            .filter(|e| e.product_version > version)
            .cloned()
            .collect(),
        Err(_) => Vec::new(),
    };
    newer.sort_by(|a, b| b.product_version.cmp(&a.product_version));
    newer.dedup_by(|a, b| a.product_version == b.product_version && a.build == b.build);
    let current_build_expires = catalogue
        .entries
        .iter()
        .filter(|e| e.build == mac.os_build)
        .map(|e| e.expiration_date)
        .max();
    MacUpdates {
        newer,
        current_build_expires,
    }
}

/// Whether a build that leaves the catalogue on `expires` is due an alert on `today`.
#[must_use]
pub fn expires_soon(expires: Date, today: Date) -> bool {
    expires.days_since_epoch() - today.days_since_epoch() <= EXPIRY_WARNING_DAYS
}
