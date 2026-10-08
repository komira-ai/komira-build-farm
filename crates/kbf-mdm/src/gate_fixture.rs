//! Test material for the gate: an in-memory MDM, a settable clock, and a gate over
//! files in a scratch directory.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::erase::SignedRequest;
use super::{Files, Gate, Inventory, Parts, Policy};
use crate::backend::{BackendError, Device, DeviceStatus, Enforcement, KbfDeclaration, MdmBackend};
use crate::clock::{Clock, HOUR};
use crate::journal::Journal;
use crate::journal::recording::Recorded;
use crate::request::{EraseRequest, Purpose, render};
use crate::sets::fixture as sets;
use crate::state::StateFile;
use crate::testkit::{SkKey, scratch};

/// The test's "now": 2027-01-15T08:00:00Z.
pub const NOW: i64 = 1_800_000_000;

/// What the fake MDM was asked, and what it answers.
#[derive(Debug, Default)]
pub struct Mdm {
    /// `verb enrollment-or-serial [detail]` per call.
    pub calls: Vec<String>,
    /// Every call fails while set.
    pub fail: bool,
    /// Erases fail while set.
    pub fail_erase: bool,
    /// Per enrollment: when the Mac last reported.
    pub last_seen: BTreeMap<String, i64>,
    /// What `declarations` lists.
    pub declarations: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct FakeMdm(pub Arc<Mutex<Mdm>>);

impl FakeMdm {
    fn call(&self, call: String) -> Result<(), BackendError> {
        let mut mdm = self.0.lock().unwrap();
        if mdm.fail {
            return Err(BackendError("down".into()));
        }
        mdm.calls.push(call);
        Ok(())
    }

    pub fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().calls.clone()
    }

    pub fn erases(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.starts_with("erase "))
            .collect()
    }

    pub fn set_fail(&self, fail: bool) {
        self.0.lock().unwrap().fail = fail;
    }

    /// The Mac with this enrollment reports at `at`.
    pub fn report(&self, enrollment: &str, at: i64) {
        self.0
            .lock()
            .unwrap()
            .last_seen
            .insert(enrollment.into(), at);
    }
}

impl MdmBackend for FakeMdm {
    async fn status(&self, device: Device<'_>) -> Result<DeviceStatus, BackendError> {
        self.call(format!("status {}", device.enrollment))?;
        let mdm = self.0.lock().unwrap();
        Ok(DeviceStatus {
            last_seen: mdm.last_seen.get(device.enrollment).copied(),
            items: BTreeMap::from([("softwareupdate.failure-reason".into(), String::new())]),
        })
    }

    async fn subscribe(&self, device: Device<'_>) -> Result<(), BackendError> {
        self.call(format!("subscribe {}", device.enrollment))
    }

    async fn enforce(&self, device: Device<'_>, e: &Enforcement) -> Result<(), BackendError> {
        self.call(format!(
            "enforce {} {} {} {}",
            device.enrollment,
            e.target_os_version,
            e.target_build_version,
            e.target_local_date_time
        ))
    }

    async fn withdraw(
        &self,
        serial: &str,
        declaration: &KbfDeclaration,
    ) -> Result<(), BackendError> {
        self.call(format!("withdraw {serial} {}", declaration.as_str()))
    }

    async fn declarations(&self) -> Result<Vec<String>, BackendError> {
        self.call("declarations".into())?;
        Ok(self.0.lock().unwrap().declarations.clone())
    }

    async fn install_profile(
        &self,
        device: Device<'_>,
        profile: &[u8],
    ) -> Result<(), BackendError> {
        self.call(format!(
            "profile {} {}",
            device.enrollment,
            String::from_utf8_lossy(profile)
        ))
    }

    async fn erase(&self, device: Device<'_>) -> Result<(), BackendError> {
        if self.0.lock().unwrap().fail_erase {
            return Err(BackendError("erase refused".into()));
        }
        self.call(format!("erase {}", device.enrollment))
    }
}

#[derive(Debug)]
pub struct FakeClock(pub AtomicI64);

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A gate over three Macs, `MAC0`..`MAC2` (enrollments `UDID-0`..), in pool
/// `mac-arm64`, and `MACX` in pool `mac-x86`; floor 1, cap 2, leases up to 8 hours.
pub struct Fixture {
    pub gate: Arc<Gate<FakeMdm>>,
    pub mdm: FakeMdm,
    pub clock: Arc<FakeClock>,
    pub alerts: Recorded,
    pub dir: PathBuf,
    pub key: SkKey,
    pub policy: Policy,
    nonce: u32,
}

pub fn default_policy() -> Policy {
    Policy {
        mac_floor: 1,
        daily_erase_cap: 2,
        max_lease_secs: 8 * HOUR,
        require_user_verified: false,
    }
}

const INVENTORY: &str = r#"{"macs": [
    {"serial": "MAC0", "enrollment": "UDID-0", "pool": "mac-arm64", "arch": "arm64"},
    {"serial": "MAC1", "enrollment": "UDID-1", "pool": "mac-arm64", "arch": "arm64"},
    {"serial": "MAC2", "enrollment": "UDID-2", "pool": "mac-arm64", "arch": "arm64"},
    {"serial": "MACX", "enrollment": "UDID-X", "pool": "mac-x86", "arch": "x86_64"}
]}"#;

fn write(path: &std::path::Path, text: &[u8]) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

impl Fixture {
    pub fn new(name: &str) -> Self {
        Self::with_policy(name, default_policy())
    }

    pub fn with_policy(name: &str, policy: Policy) -> Self {
        let dir = scratch(name);
        let key = SkKey::new(1);
        write(
            &dir.join("allowed_signers"),
            key.allowed_line("alice@example.org").as_bytes(),
        );
        write(&dir.join("allowlist"), b"# digests\n");
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        let clock = Arc::new(FakeClock(AtomicI64::new(NOW)));
        let mdm = FakeMdm::default();
        let alerts = Recorded::default();
        let gate = Self::build(&dir, &mdm, &clock, &alerts, &policy, &dir.join("audit.log"));
        Self {
            gate,
            mdm,
            clock,
            alerts,
            dir,
            key,
            policy,
            nonce: 0,
        }
    }

    fn build(
        dir: &std::path::Path,
        mdm: &FakeMdm,
        clock: &Arc<FakeClock>,
        alerts: &Recorded,
        policy: &Policy,
        audit: &std::path::Path,
    ) -> Arc<Gate<FakeMdm>> {
        let seed = STANDARD.encode([5u8; 32]);
        let gate = Gate::new(Parts {
            backend: mdm.clone(),
            clock: Arc::clone(clock) as Arc<dyn Clock>,
            policy: policy.clone(),
            inventory: Inventory::parse(INVENTORY).unwrap(),
            files: Files {
                allowed_signers: dir.join("allowed_signers"),
                profile_allowlist: dir.join("allowlist"),
                profile_dir: dir.join("profiles"),
                owner: crate::trusted::owner_of(dir).unwrap(),
            },
            root_key: sets::key(sets::ROOT).verifying_key(),
            grant_key: crate::grant::GrantKey::parse(&seed).unwrap(),
            journal: Journal::open(audit, Box::new(alerts.clone())).unwrap(),
            store: StateFile::new(&dir.join("state.json")),
        });
        Arc::new(gate.unwrap())
    }

    /// Stops this gate and starts another over the same files and state.
    pub fn restart(&mut self) {
        self.restart_with_audit_log(&self.dir.join("audit.log"));
    }

    /// [`Self::restart`], writing the audit log to `audit`.
    pub fn restart_with_audit_log(&mut self, audit: &std::path::Path) {
        self.gate = Self::build(
            &self.dir,
            &self.mdm,
            &self.clock,
            &self.alerts,
            &self.policy,
            audit,
        );
    }

    pub fn now(&self) -> i64 {
        self.clock.now()
    }

    pub fn advance(&self, secs: i64) {
        self.clock.0.fetch_add(secs, Ordering::SeqCst);
    }

    /// A request text with a fresh nonce, expiring in 30 minutes.
    pub fn request(&mut self, serial: &str, purpose: Purpose) -> EraseRequest {
        self.nonce += 1;
        EraseRequest {
            serial: serial.into(),
            purpose,
            reason: "test".into(),
            nonce: format!("{:032x}", self.nonce),
            not_after: self.now() + HOUR / 2,
        }
    }

    /// `request`, signed by the allowed operator key with a touch.
    pub fn signed(&mut self, serial: &str, purpose: Purpose) -> SignedRequest {
        let request = self.request(serial, purpose);
        self.sign(&request)
    }

    pub fn sign(&self, request: &EraseRequest) -> SignedRequest {
        let message = render(request);
        SignedRequest {
            signature: self.key.sign(message.as_bytes()),
            message,
        }
    }

    pub fn lease(lease: &str) -> Purpose {
        Purpose::PrivilegedLease(lease.into())
    }

    pub fn write_file(&self, name: &str, text: &[u8]) {
        write(&self.dir.join(name), text);
    }
}
