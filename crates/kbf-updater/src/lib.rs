//! `kbf-updater`: the root helper that installs signed software sets on a node
//! (`docs/design/fleet-updates.md` section 5, `docs/design/fleet-updates-security.md`
//! S2 to S4).
//!
//! It has exactly three verbs, served on a Unix socket only the genuine `kbf-daemon` may
//! use: `status`; `stage <set>` (verify the set, copy and hash its changed artifacts,
//! change nothing installed); `apply <set>` (verify again, install, and reboot when the
//! installer says the step needs it). There is no rollback verb and no bare reboot: a
//! rollback is a newer signed set naming old artifacts.
//!
//! - [`signed`]: envelopes, the root-signed key statement, key roles.
//! - [`set`]: the software set and the checks of S3.1.
//! - [`state`]: the state file (installed, staged, in progress, the serial floor).
//! - [`updater`]: the three verbs over an [`apply::Applier`].
//! - [`apply`]: the applier interface, a fake for tests, and the apt-snapshot applier.
//! - `caller` and `server` (Linux): the caller check, the kernel floor, the refusal to
//!   run beside `--driver native`, and the socket.
//!
//! On macOS the helper needs the per-lease user first (S4.3); until then only the
//! portable parts build there and the binary refuses to start.

pub mod apply;
#[cfg(target_os = "linux")]
pub mod caller;
#[cfg(target_os = "linux")]
pub mod server;
pub mod set;
pub mod signed;
pub mod state;
pub mod updater;

#[cfg(test)]
pub(crate) mod testkit;

/// Why a request was refused. Each check of S3.1 and S4.3 has its own variant, so a test
/// can tell which check fired.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// A field or a document does not parse.
    #[error("malformed: {0}")]
    Malformed(String),
    /// The signature does not verify.
    #[error("the signature does not verify")]
    BadSignature,
    /// A key statement signed by a key other than the pinned root key.
    #[error("the key statement is not signed by the root key")]
    NotRootSigned,
    /// No key statement stored or offered.
    #[error("no key statement")]
    NoStatement,
    /// Two different key statements with one serial.
    #[error("two different key statements with serial {0}")]
    StatementConflict(u64),
    /// The newest key statement has expired.
    #[error("key statement {0} has expired")]
    StatementExpired(u64),
    /// The set's signer is not named by the newest key statement.
    #[error("the set is signed by a key the key statement does not name")]
    UnknownKey,
    /// The set is for another pool.
    #[error("the set is for pool {0}")]
    WrongPool(String),
    /// The set is for another platform.
    #[error("the set is for platform {0}")]
    WrongPlatform(String),
    /// The set has expired.
    #[error("set {0} has expired")]
    Expired(u64),
    /// The set's serial is below the pool's floor.
    #[error("set {serial} is below the floor {floor}")]
    BelowFloor {
        /// The set's serial.
        serial: u64,
        /// The floor.
        floor: u64,
    },
    /// The set is not newer than the installed one.
    #[error("set {serial} is not newer than the installed {installed}")]
    NotNewer {
        /// The set's serial.
        serial: u64,
        /// The installed serial.
        installed: u64,
    },
    /// The set's serial is more than [`set::MAX_SERIAL_STEP`] above the installed one.
    #[error("set {serial} jumps more than the bound past the installed {installed}")]
    SerialJump {
        /// The set's serial.
        serial: u64,
        /// The installed serial (0 with nothing installed).
        installed: u64,
    },
    /// The signer's role does not cover an item the set changes.
    #[error("a component key cannot change {0}")]
    NotCovered(String),
    /// A staged artifact's SHA-256 is not the set's.
    #[error("artifact {0} does not match its digest")]
    DigestMismatch(String),
    /// An artifact could not be read from the artifacts directory.
    #[error("artifact {0}")]
    Artifact(String),
    /// `apply` of a set that was not staged.
    #[error("set {0} is not staged")]
    NotStaged(String),
    /// Another set's apply is in progress (a crash mid-apply); only it may continue.
    #[error("the apply of set {0} is in progress")]
    InProgress(String),
    /// A set signed by a key other than a platform key offered over an abandoned apply
    /// (one in progress whose set no longer passes the checks). The node may hold any
    /// mix of that set and the installed one, and only a platform key may say what the
    /// whole node runs.
    #[error("only a platform-signed set may replace the abandoned apply of set {0}")]
    Abandoned(String),
    /// The applier failed.
    #[error("apply failed: {0}")]
    Apply(String),
    /// The state file could not be read or written.
    #[error("state: {0}")]
    State(String),
}
