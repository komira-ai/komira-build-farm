//! What the server asks of a Mac's device management, through `kbf-mdm-gate` only
//! (`docs/design/mdm-backend.md` sections M2 and M3, `fleet-updates.md` section 7.2,
//! `fleet-updates-security.md` section S5.2).
//!
//! [`MdmGate`] is the server's side of the gate: **inventory** (`status`), **enforce**
//! one macOS build by a deadline and **withdraw** it, and **install** an allowlisted
//! profile named by its digest. It has no erase: the server cannot make one (M4), and
//! nothing here can carry one. [`GateClient`] speaks it to a gate over mutual TLS
//! (`kbf.mdmgate.v1`, crate `kbf-mdm-api`); a test or a later in-process backend
//! implements the trait directly.
//!
//! **Progress is polled** ([`progress`]): macOS 27 has no command that reports an
//! update finished, so the server reads the DDM status items through `status` while a
//! node is `updating`. They explain a failure and feed the UI; the done signal is the
//! node's own `Hello` with the target build (4.2 step 7), never the MDM's word.
//!
//! **Not wired yet:** the server's flags for the gate's address and certificates, the
//! rollout driver's calls (enforce after drain, withdraw at startup reconciliation,
//! 4.1), and the serial in `NodeStatus` that joins a node to its Mac. Each takes a
//! [`SharedGate`].

pub mod client;
pub mod progress;

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kbf_mdm_api::catalogue::CatalogueEntry;
use kbf_mdm_api::names::{
    Date, LocalDateTime, OsVersion, Serial, Sha256Hex, is_build, osupdate_declaration,
};
use kbf_mdm_api::pb;

pub use client::{GateClient, GateEndpoint};
pub use kbf_mdm_api::pb::{InstalledProfile, RefusalReason, SoftwareUpdateStatus};

/// Why a gate call did not do what was asked.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    /// The gate understood the request and refused it.
    #[error("the MDM gate refused: {reason:?}: {detail}")]
    Refused {
        /// Its reason; [`RefusalReason::Unspecified`] for one this server does not know.
        reason: RefusalReason,
        /// What the gate checked and found.
        detail: String,
    },
    /// The gate could not be reached, or failed the call.
    #[error("the MDM gate is unavailable: {0}")]
    Unavailable(String),
    /// The gate answered something outside the protocol.
    #[error("the MDM gate answered out of protocol: {0}")]
    Malformed(String),
}

/// What a gate call returns.
pub type GateFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, GateError>> + Send + 'a>>;

/// A gate shared by the parts of the server that call it.
pub type SharedGate = Arc<dyn MdmGate>;

/// The server's verbs at the gate (M2.2), less `grant-admin` and the erase relay.
pub trait MdmGate: Send + Sync {
    /// Every Mac in the gate's inventory, or those of `serials` it knows, and Apple's
    /// catalogue as the gate last read it.
    fn status<'a>(&'a self, serials: &'a [Serial]) -> GateFuture<'a, Inventory>;

    /// Asks the gate to enforce the macOS build `order.signed_set` names on one Mac by
    /// `order.by`. Returns what the gate posted.
    fn enforce<'a>(&'a self, order: &'a EnforceOrder) -> GateFuture<'a, Enforcement>;

    /// Removes the Mac's outstanding enforcement. Returns it, or `None` if none was
    /// outstanding.
    fn withdraw<'a>(&'a self, serial: &'a Serial) -> GateFuture<'a, Option<Enforcement>>;

    /// Installs the profile whose SHA-256 is `digest`, if the gate allowlists it.
    /// Returns the installed profile's identifier.
    fn install_profile<'a>(
        &'a self,
        serial: &'a Serial,
        digest: &'a Sha256Hex,
    ) -> GateFuture<'a, String>;
}

/// An enforcement to ask for: the signed software set names the version and build;
/// the server chooses only the Mac and the deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnforceOrder {
    /// The Mac.
    pub serial: Serial,
    /// The pool's signed software set, as signed. The gate checks it.
    pub signed_set: Vec<u8>,
    /// `TargetLocalDateTime`: when the Mac force-installs, in its local time.
    pub by: LocalDateTime,
}

/// What `status` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inventory {
    /// The Macs, as the gate listed them.
    pub macs: Vec<MacStatus>,
    /// Apple's catalogue as the gate last read it.
    pub catalogue: Catalogue,
}

impl Inventory {
    /// The Mac with `serial`, if the gate listed it.
    #[must_use]
    pub fn mac(&self, serial: &Serial) -> Option<&MacStatus> {
        self.macs.iter().find(|m| &m.serial == serial)
    }
}

/// Apple's catalogue, as the gate reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalogue {
    /// When the gate last read it; `None` if it never has.
    pub fetched_at_unix_ms: Option<u64>,
    /// Its macOS entries.
    pub entries: Vec<CatalogueEntry>,
}

/// One Mac, as the MDM last heard from it. An empty string is a value the MDM has
/// not received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacStatus {
    /// The hardware serial.
    pub serial: Serial,
    /// The platform UUID from its first enrollment.
    pub platform_uuid: String,
    /// Its pool in the gate's configuration.
    pub pool: String,
    /// Enrolled in the MDM.
    pub enrolled: bool,
    /// Supervised (needed to enforce an update).
    pub supervised: bool,
    /// Its bootstrap token is escrowed (it authorises the update).
    pub bootstrap_token_escrowed: bool,
    /// The last MDM check-in, milliseconds since the Unix epoch.
    pub last_check_in_unix_ms: Option<u64>,
    /// DDM `device.operating-system.version`.
    pub os_version: String,
    /// DDM `device.operating-system.build-version`.
    pub os_build: String,
    /// DDM `device.operating-system.supplemental.build-version`.
    pub supplemental_build: String,
    /// The DDM software-update status items (M3).
    pub software_update: SoftwareUpdateStatus,
    /// Its installed profiles.
    pub profiles: Vec<InstalledProfile>,
    /// The kbf enforcement outstanding on it.
    pub enforcement: Option<Enforcement>,
}

/// One enforcement declaration the gate posted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enforcement {
    /// `kbf.osupdate.<serial>`.
    pub declaration_identifier: String,
    /// `TargetOSVersion`.
    pub target_os_version: OsVersion,
    /// `TargetBuildVersion`.
    pub target_build: String,
    /// `TargetLocalDateTime`.
    pub target_local_date_time: LocalDateTime,
}

/// A refusal from the gate as an error.
pub(crate) fn refused(r: &pb::Refusal) -> GateError {
    GateError::Refused {
        reason: RefusalReason::try_from(r.reason).unwrap_or(RefusalReason::Unspecified),
        detail: r.detail.clone(),
    }
}

/// `e` for `serial`, checked: its identifier must be `kbf.osupdate.<serial>`, so an
/// answer can never stand for another Mac's declaration or one kbf does not own.
pub(crate) fn enforcement(serial: &Serial, e: pb::Enforcement) -> Result<Enforcement, GateError> {
    let malformed =
        |what: String| GateError::Malformed(format!("enforcement for {serial}: {what}"));
    let want = osupdate_declaration(serial);
    if e.declaration_identifier != want {
        return Err(malformed(format!(
            "identifier {:?} is not {want:?}",
            e.declaration_identifier
        )));
    }
    if !is_build(&e.target_build) {
        return Err(malformed(format!("target build {:?}", e.target_build)));
    }
    Ok(Enforcement {
        declaration_identifier: e.declaration_identifier,
        target_os_version: OsVersion::parse(&e.target_os_version)
            .map_err(|err| malformed(err.to_string()))?,
        target_build: e.target_build,
        target_local_date_time: LocalDateTime::parse(&e.target_local_date_time)
            .map_err(|err| malformed(err.to_string()))?,
    })
}

/// A `status` answer to a request for `asked` (every Mac if empty), checked: each Mac
/// at most once, and only Macs that were asked for.
pub(crate) fn inventory(r: pb::StatusResponse, asked: &[Serial]) -> Result<Inventory, GateError> {
    let macs: Vec<MacStatus> = r.macs.into_iter().map(mac).collect::<Result<_, _>>()?;
    let mut seen = BTreeSet::new();
    for m in &macs {
        if !seen.insert(&m.serial) {
            return Err(GateError::Malformed(format!(
                "the status answer lists {} twice",
                m.serial
            )));
        }
        if !asked.is_empty() && !asked.contains(&m.serial) {
            return Err(GateError::Malformed(format!(
                "the status answer lists {}, which was not asked for",
                m.serial
            )));
        }
    }
    let catalogue = r.catalogue.unwrap_or_default();
    let entries = catalogue
        .entries
        .into_iter()
        .map(catalogue_entry)
        .collect::<Result<_, _>>()?;
    Ok(Inventory {
        macs,
        catalogue: Catalogue {
            fetched_at_unix_ms: (catalogue.fetched_at_unix_ms != 0)
                .then_some(catalogue.fetched_at_unix_ms),
            entries,
        },
    })
}

fn mac(m: pb::MacStatus) -> Result<MacStatus, GateError> {
    let serial = Serial::new(m.serial).map_err(|e| GateError::Malformed(e.to_string()))?;
    let enforcement = m.enforcement.map(|e| enforcement(&serial, e)).transpose()?;
    Ok(MacStatus {
        serial,
        platform_uuid: m.platform_uuid,
        pool: m.pool,
        enrolled: m.enrolled,
        supervised: m.supervised,
        bootstrap_token_escrowed: m.bootstrap_token_escrowed,
        last_check_in_unix_ms: (m.last_check_in_unix_ms != 0).then_some(m.last_check_in_unix_ms),
        os_version: m.os_version,
        os_build: m.os_build,
        supplemental_build: m.supplemental_build,
        software_update: m.software_update.unwrap_or_default(),
        profiles: m.profiles,
        enforcement,
    })
}

fn catalogue_entry(e: pb::CatalogueEntry) -> Result<CatalogueEntry, GateError> {
    let malformed = |what: String| GateError::Malformed(format!("catalogue entry: {what}"));
    if !is_build(&e.build) {
        return Err(malformed(format!("build {:?}", e.build)));
    }
    Ok(CatalogueEntry {
        product_version: OsVersion::parse(&e.product_version)
            .map_err(|err| malformed(err.to_string()))?,
        build: e.build,
        posting_date: Date::parse(&e.posting_date).map_err(|err| malformed(err.to_string()))?,
        expiration_date: Date::parse(&e.expiration_date)
            .map_err(|err| malformed(err.to_string()))?,
        supported_devices: e.supported_devices,
        public: e.public,
    })
}
