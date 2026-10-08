//! The gate's southbound side: what it asks of an MDM (M2.1).
//!
//! The trait is narrow on purpose. There is no generic command: the gate builds the
//! few commands it sends itself, and every declaration it touches is a
//! [`KbfDeclaration`], whose identifier starts `kbf.` by construction.

use std::collections::BTreeMap;
use std::future::Future;

/// The prefix of every declaration the gate creates, changes or removes.
pub const DECLARATION_PREFIX: &str = "kbf.";

/// The prefix of the gate's macOS enforcement declarations, one per Mac.
pub const ENFORCEMENT_PREFIX: &str = "kbf.osupdate.";

/// A declaration identifier the gate may touch: it starts with `kbf.`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct KbfDeclaration(String);

impl KbfDeclaration {
    /// `Some` only for an identifier that starts with `kbf.`. Settings an operator
    /// manages by hand in the MDM are out of the gate's reach (M2.2).
    pub fn new(identifier: &str) -> Option<Self> {
        identifier
            .starts_with(DECLARATION_PREFIX)
            .then(|| Self(identifier.to_owned()))
    }

    /// The enforcement declaration of the Mac with this serial.
    pub fn enforcement(serial: &str) -> Self {
        Self(format!("{ENFORCEMENT_PREFIX}{serial}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Mac as the MDM knows it: its serial, and the enrollment id (the UDID) the MDM
/// keys it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device<'a> {
    pub serial: &'a str,
    pub enrollment: &'a str,
}

/// One `softwareupdate.enforcement.specific` declaration's payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enforcement {
    pub target_os_version: String,
    pub target_build_version: String,
    /// `YYYY-MM-DDTHH:MM:SS`, the Mac's local time.
    pub target_local_date_time: String,
}

/// What the MDM last heard from a Mac.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceStatus {
    /// When the Mac last reported (seconds since the epoch), if ever.
    pub last_seen: Option<i64>,
    /// The latest DDM status values, by item (`softwareupdate.install-state`, ...).
    pub items: BTreeMap<String, String>,
}

/// Why a backend call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("MDM: {0}")]
pub struct BackendError(pub String);

/// The operations the gate needs from an MDM.
pub trait MdmBackend: Send + Sync + 'static {
    /// The Mac's latest status.
    fn status(
        &self,
        device: Device<'_>,
    ) -> impl Future<Output = Result<DeviceStatus, BackendError>> + Send;

    /// Installs the DDM status subscription the gate reads (M3) on the Mac.
    fn subscribe(
        &self,
        device: Device<'_>,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Posts (or replaces) the Mac's enforcement declaration.
    fn enforce(
        &self,
        device: Device<'_>,
        enforcement: &Enforcement,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Removes a declaration from the Mac with this serial, and deletes it.
    fn withdraw(
        &self,
        serial: &str,
        declaration: &KbfDeclaration,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// The identifiers of every declaration the MDM holds.
    fn declarations(&self) -> impl Future<Output = Result<Vec<String>, BackendError>> + Send;

    /// Installs a configuration profile (its bytes) on the Mac.
    fn install_profile(
        &self,
        device: Device<'_>,
        profile: &[u8],
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Sends the Mac an erase.
    fn erase(&self, device: Device<'_>) -> impl Future<Output = Result<(), BackendError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_kbf_prefixed_identifiers_are_declarations_the_gate_may_touch() {
        // Catches: matching identifiers by substring (M10 mutant), which would let the
        // gate remove an operator's `com.example.kbf.settings`.
        assert!(KbfDeclaration::new("kbf.osupdate.C02X").is_some());
        assert!(KbfDeclaration::new("com.example.kbf.settings").is_none());
        assert!(KbfDeclaration::new("x.kbf.osupdate.C02X").is_none());
        assert!(KbfDeclaration::new("kbf").is_none());
        assert_eq!(
            KbfDeclaration::enforcement("C02X").as_str(),
            "kbf.osupdate.C02X"
        );
        assert_eq!(BackendError("down".into()).to_string(), "MDM: down");
    }
}
