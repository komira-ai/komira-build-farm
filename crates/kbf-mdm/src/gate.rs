//! The gate's verbs and their rules (M2.2, M4, S5.2).
//!
//! Every verb refuses a serial outside the gate's inventory. One lock serialises the
//! verbs and the tick, so each cap is checked and taken atomically, and the state is
//! saved before a verb answers. This module holds the shared rules, the status reads,
//! `enforce`, `withdraw`, `profile` and the startup reconciliation; the erase verbs
//! and the tick are in [`erase`](self::erase).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::backend::{
    BackendError, Device, ENFORCEMENT_PREFIX, Enforcement, KbfDeclaration, MdmBackend,
};
use crate::clock::{Clock, DAY, format_rfc3339, is_local_date_time};
use crate::grant::GrantKey;
use crate::journal::{Event, Journal};
use crate::request::RequestError;
use crate::sets::{Envelope, SetError};
use crate::signers::VerifyError;
use crate::state::{Enforced, PoolSet, State, StateFile};
use crate::trusted;

#[path = "gate_erase.rs"]
pub mod erase;

#[cfg(test)]
#[path = "gate_fixture.rs"]
pub(crate) mod fixture;

/// A Mac in the gate's inventory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mac {
    pub serial: String,
    /// The MDM's enrollment id (the Mac's UDID).
    pub enrollment: String,
    pub pool: String,
    /// The Mac's CPU, as a set's `platform.arch` names it: `arm64` or `x86_64`.
    pub arch: String,
}

impl Mac {
    fn device(&self) -> Device<'_> {
        Device {
            serial: &self.serial,
            enrollment: &self.enrollment,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryFile {
    macs: Vec<Mac>,
}

/// The Macs the gate will act on, by serial: the operator's configuration of the gate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inventory(BTreeMap<String, Mac>);

impl Inventory {
    /// Reads `{"macs": [{"serial", "enrollment", "pool", "arch"}, ...]}`.
    ///
    /// # Errors
    /// The text is not that, a serial or arch is malformed, or a serial is listed twice.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: InventoryFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut macs = BTreeMap::new();
        for mac in file.macs {
            if !crate::request::valid_serial(&mac.serial) {
                return Err(format!("bad serial {:?}", mac.serial));
            }
            if !matches!(mac.arch.as_str(), "arm64" | "x86_64") {
                return Err(format!(
                    "{}: arch {:?} is not arm64 or x86_64",
                    mac.serial, mac.arch
                ));
            }
            if let Some(dup) = macs.insert(mac.serial.clone(), mac) {
                return Err(format!("serial {} listed twice", dup.serial));
            }
        }
        Ok(Self(macs))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The gate's limits: the operator's configuration on the gate's host. A signature
/// never changes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The fewest Macs that must stay available (not being erased or updated).
    pub mac_floor: usize,
    /// The cap on erases sent in the last 24 hours plus erases scheduled and not yet
    /// sent.
    pub daily_erase_cap: usize,
    /// The longest privileged lease: a granted Mac is erased this long after its grant.
    pub max_lease_secs: i64,
    /// Require the security key's "user verified" flag as well as "user present".
    pub require_user_verified: bool,
}

/// The files the gate reads on every use, so an edit takes effect without a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Files {
    pub allowed_signers: PathBuf,
    /// SHA-256 digests (lowercase hex), one per line, `#` comments.
    pub profile_allowlist: PathBuf,
    /// Holds `<digest>.mobileconfig` for every profile the gate may install.
    pub profile_dir: PathBuf,
    /// The uid that must own them (0 in the binary).
    pub owner: u32,
}

/// Why a verb was refused or failed. [`Refusal::status`] is its HTTP status.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("{0} is not in the gate's inventory")]
    NotInInventory(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("signature: {0}")]
    Signature(#[from] VerifyError),
    #[error("erase request: {0}")]
    Request(#[from] RequestError),
    #[error("the signed request names {signed}, not {asked}")]
    SerialMismatch { signed: String, asked: String },
    #[error("the request's not-after has passed")]
    Expired,
    #[error("the request's not-after is more than an hour ahead")]
    TooFarAhead,
    #[error("the request's nonce was already used")]
    Replay,
    #[error("{0}")]
    Busy(String),
    #[error("the daily erase cap ({0}) is reached")]
    DailyCap(usize),
    #[error("the Mac floor ({floor}) would not hold: {available} Macs available")]
    Floor { floor: usize, available: usize },
    #[error("no held operator-signed erase for {serial} and lease {lease}")]
    NoHeldRequest { serial: String, lease: String },
    #[error("a request for {serial} and lease {lease} is already held")]
    AlreadyHeld { serial: String, lease: String },
    #[error("the gate issued no grant for {serial} and lease {lease}")]
    NoGrant { serial: String, lease: String },
    #[error("set: {0}")]
    Set(#[from] SetError),
    #[error("the set is for pool {set}, not the Mac's pool {mac}")]
    WrongPool { set: String, mac: String },
    #[error("the set is for {0}, not macos")]
    NotMacos(String),
    #[error("the set is for {set}, not the Mac's {mac}")]
    WrongArch { set: String, mac: String },
    #[error("the set's serial {serial} is below the pool's floor {floor}")]
    BelowPoolFloor { serial: u64, floor: u64 },
    #[error("the key statement's serial {got} is older than {newest}")]
    OldStatement { got: u64, newest: u64 },
    #[error("profile {0} is not allowlisted")]
    NotAllowlisted(String),
    #[error("{0}")]
    Backend(#[from] BackendError),
    #[error("internal: {0}")]
    Internal(String),
}

impl Refusal {
    /// The HTTP status the API answers with.
    pub fn status(&self) -> u16 {
        match self {
            Self::NotInInventory(_) => 404,
            Self::BadRequest(_) | Self::Request(_) => 400,
            Self::Busy(_) | Self::AlreadyHeld { .. } | Self::DailyCap(_) | Self::Floor { .. } => {
                409
            }
            Self::Backend(_) => 502,
            Self::Internal(_) => 500,
            _ => 403,
        }
    }
}

/// What `enforce` needs: the key statement and set that name the build, and the
/// deadline the server chose.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnforceRequest {
    pub key_statement: Envelope,
    pub set: Envelope,
    /// `YYYY-MM-DDTHH:MM:SS`, the Mac's local time.
    pub deadline: String,
}

/// A Mac's status: what the MDM last heard, and what the gate is doing with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MacStatus {
    pub serial: String,
    pub pool: String,
    pub last_seen: Option<String>,
    pub items: BTreeMap<String, String>,
    pub gate: MacGate,
}

/// The gate's own view of one Mac.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MacGate {
    pub enforcement: Option<Enforced>,
    /// An erase is outstanding, or was sent and the Mac has not reported since.
    pub erasing: bool,
    /// Leases with a held operator-signed request.
    pub held_leases: Vec<String>,
    /// When the erase scheduled with a grant runs.
    pub scheduled_erase: Option<String>,
}

/// The fleet as the gate sees it, with its erase budget.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Fleet {
    pub macs: BTreeMap<String, MacGate>,
    pub mac_floor: usize,
    pub available: usize,
    pub daily_erase_cap: usize,
    /// Erases sent in the last 24 hours.
    pub erases_last_24h: usize,
    /// Erases scheduled with a grant and not yet sent; they count toward the cap too.
    pub erases_scheduled: usize,
    pub outstanding_erase: Option<String>,
}

/// The gate.
pub struct Gate<B> {
    backend: B,
    clock: Arc<dyn Clock>,
    policy: Policy,
    inventory: Inventory,
    files: Files,
    root_key: VerifyingKey,
    grant_key: GrantKey,
    journal: Journal,
    store: StateFile,
    state: tokio::sync::Mutex<State>,
}

/// Everything [`Gate::new`] takes.
pub struct Parts<B> {
    pub backend: B,
    pub clock: Arc<dyn Clock>,
    pub policy: Policy,
    pub inventory: Inventory,
    pub files: Files,
    pub root_key: VerifyingKey,
    pub grant_key: GrantKey,
    pub journal: Journal,
    pub store: StateFile,
}

fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl<B: MdmBackend> Gate<B> {
    /// A gate over `parts`, with the state the store holds.
    ///
    /// # Errors
    /// The state file cannot be read.
    pub fn new(parts: Parts<B>) -> Result<Self, crate::state::StateError> {
        let state = parts.store.load()?;
        Ok(Self {
            backend: parts.backend,
            clock: parts.clock,
            policy: parts.policy,
            inventory: parts.inventory,
            files: parts.files,
            root_key: parts.root_key,
            grant_key: parts.grant_key,
            journal: parts.journal,
            store: parts.store,
            state: tokio::sync::Mutex::new(state),
        })
    }

    fn mac(&self, serial: &str) -> Result<&Mac, Refusal> {
        self.inventory
            .0
            .get(serial)
            .ok_or_else(|| Refusal::NotInInventory(serial.to_owned()))
    }

    fn save(&self, state: &State) -> Result<(), Refusal> {
        self.store
            .save(state)
            .map_err(|e| Refusal::Internal(e.to_string()))
    }

    fn record(&self, event: &Event) -> Result<(), Refusal> {
        self.journal
            .record(event)
            .map_err(|e| Refusal::Internal(format!("audit log: {e}")))
    }

    /// Serials not available toward the floor: being erased (outstanding, sent and not
    /// seen since, or scheduled with a grant) or under an enforcement.
    fn unavailable(state: &State) -> BTreeSet<&str> {
        let mut out: BTreeSet<&str> = state.erased.keys().map(String::as_str).collect();
        out.extend(state.outstanding_erase.iter().map(|o| o.serial.as_str()));
        out.extend(state.scheduled.iter().map(|s| s.serial.as_str()));
        out.extend(state.enforced.keys().map(String::as_str));
        out
    }

    fn available(&self, state: &State) -> usize {
        let unavailable = Self::unavailable(state);
        self.inventory
            .0
            .keys()
            .filter(|s| !unavailable.contains(s.as_str()))
            .count()
    }

    /// Refuses if taking `serial` out of service would leave fewer Macs than the floor.
    fn check_floor(&self, state: &State, serial: &str) -> Result<(), Refusal> {
        let available = self.available(state);
        let leaving = usize::from(!Self::unavailable(state).contains(serial));
        if available - leaving < self.policy.mac_floor {
            return Err(Refusal::Floor {
                floor: self.policy.mac_floor,
                available,
            });
        }
        Ok(())
    }

    fn erases_last_day(state: &State, now: i64) -> usize {
        state.erase_times.iter().filter(|&&t| t > now - DAY).count()
    }

    fn mac_gate(state: &State, serial: &str) -> MacGate {
        MacGate {
            enforcement: state.enforced.get(serial).cloned(),
            erasing: state.erased.contains_key(serial)
                || state
                    .outstanding_erase
                    .as_ref()
                    .is_some_and(|o| o.serial == serial),
            held_leases: state
                .held
                .iter()
                .filter(|h| h.serial == serial)
                .map(|h| h.lease.clone())
                .collect(),
            scheduled_erase: state
                .scheduled
                .iter()
                .find(|s| s.serial == serial)
                .map(|s| format_rfc3339(s.due)),
        }
    }

    /// `status <serial>`: the MDM's latest report and the gate's view.
    ///
    /// # Errors
    /// The serial is not in the inventory, or the MDM cannot be read.
    pub async fn status(&self, serial: &str) -> Result<MacStatus, Refusal> {
        let mac = self.mac(serial)?;
        let status = self.backend.status(mac.device()).await?;
        let state = self.state.lock().await;
        Ok(MacStatus {
            serial: mac.serial.clone(),
            pool: mac.pool.clone(),
            last_seen: status.last_seen.map(format_rfc3339),
            items: status.items,
            gate: Self::mac_gate(&state, serial),
        })
    }

    /// The whole inventory with the gate's view and its erase budget.
    pub async fn fleet(&self) -> Fleet {
        let state = self.state.lock().await;
        Fleet {
            macs: self
                .inventory
                .0
                .keys()
                .map(|s| (s.clone(), Self::mac_gate(&state, s)))
                .collect(),
            mac_floor: self.policy.mac_floor,
            available: self.available(&state),
            daily_erase_cap: self.policy.daily_erase_cap,
            erases_last_24h: Self::erases_last_day(&state, self.clock.now()),
            erases_scheduled: state.scheduled.len(),
            outstanding_erase: state.outstanding_erase.as_ref().map(|o| o.serial.clone()),
        }
    }

    /// `enforce <serial> <set>` (S5.2).
    ///
    /// # Errors
    /// Any rule refuses it, or the MDM fails.
    pub async fn enforce(
        &self,
        serial: &str,
        request: &EnforceRequest,
    ) -> Result<Enforced, Refusal> {
        let now = self.clock.now();
        let result = self.enforce_inner(serial, request, now).await;
        let (outcome, detail) = match &result {
            Ok(e) => (
                "enforced",
                format!("macOS {} ({}) by {}", e.version, e.build, e.deadline),
            ),
            Err(r) => ("refused", r.to_string()),
        };
        self.record(&Event::new(now, "enforce", serial, outcome, detail, true))?;
        result
    }

    async fn enforce_inner(
        &self,
        serial: &str,
        request: &EnforceRequest,
        now: i64,
    ) -> Result<Enforced, Refusal> {
        let mac = self.mac(serial)?;
        if !is_local_date_time(&request.deadline) {
            return Err(Refusal::BadRequest(
                "deadline must be YYYY-MM-DDTHH:MM:SS".into(),
            ));
        }
        let set = crate::sets::verify(&self.root_key, &request.key_statement, &request.set, now)?;
        let mut state = self.state.lock().await;
        if set.statement_serial < state.statement_serial {
            return Err(Refusal::OldStatement {
                got: set.statement_serial,
                newest: state.statement_serial,
            });
        }
        if set.pool != mac.pool {
            return Err(Refusal::WrongPool {
                set: set.pool,
                mac: mac.pool.clone(),
            });
        }
        if set.os != "macos" {
            return Err(Refusal::NotMacos(set.os));
        }
        if set.arch != mac.arch {
            return Err(Refusal::WrongArch {
                set: set.arch,
                mac: mac.arch.clone(),
            });
        }
        let floor = state.pool_floors.get(&mac.pool).copied().unwrap_or(0);
        if set.serial < floor {
            return Err(Refusal::BelowPoolFloor {
                serial: set.serial,
                floor,
            });
        }
        // A valid set for this pool: the gate learns its floor and profiles.
        state.statement_serial = set.statement_serial;
        state
            .pool_floors
            .insert(mac.pool.clone(), floor.max(set.min_serial));
        let known = state.pool_sets.entry(mac.pool.clone()).or_default();
        if set.serial >= known.serial {
            *known = PoolSet {
                serial: set.serial,
                profiles: set.profiles.clone(),
            };
        }
        self.save(&state)?;
        if let Some((other, _)) = state.enforced.iter().find(|(_, e)| e.pool == mac.pool) {
            return Err(Refusal::Busy(format!(
                "an enforcement is outstanding in pool {} (on {other})",
                mac.pool
            )));
        }
        if Self::unavailable(&state).contains(serial) {
            return Err(Refusal::Busy(format!("{serial} is being erased")));
        }
        self.check_floor(&state, serial)?;
        let enforcement = Enforcement {
            target_os_version: set.macos_version.clone(),
            target_build_version: set.macos_build.clone(),
            target_local_date_time: request.deadline.clone(),
        };
        self.backend.enforce(mac.device(), &enforcement).await?;
        let enforced = Enforced {
            pool: mac.pool.clone(),
            version: set.macos_version,
            build: set.macos_build,
            deadline: request.deadline.clone(),
            set_serial: set.serial,
            started_at: now,
        };
        state.enforced.insert(serial.to_owned(), enforced.clone());
        self.save(&state)?;
        Ok(enforced)
    }

    /// `withdraw <serial>`: removes the Mac's enforcement (idempotent).
    ///
    /// # Errors
    /// The serial is not in the inventory, or the MDM fails.
    pub async fn withdraw(&self, serial: &str) -> Result<(), Refusal> {
        let mac = self.mac(serial)?;
        let mut state = self.state.lock().await;
        self.backend
            .withdraw(&mac.serial, &KbfDeclaration::enforcement(&mac.serial))
            .await?;
        state.enforced.remove(serial);
        self.save(&state)?;
        self.record(&Event::new(
            self.clock.now(),
            "withdraw",
            serial,
            "withdrawn",
            "",
            false,
        ))
    }

    /// `profile <serial> <digest>`: installs a profile the gate holds, if a verified set
    /// for the Mac's pool or the host's allowlist names its digest. The server names a
    /// digest; it never sends bytes.
    ///
    /// # Errors
    /// The digest is not allowlisted, the gate's copy is missing or does not match it,
    /// or the MDM fails.
    pub async fn profile(&self, serial: &str, digest: &str) -> Result<(), Refusal> {
        let now = self.clock.now();
        let result = self.profile_inner(serial, digest).await;
        let outcome = if result.is_ok() {
            "installed"
        } else {
            "refused"
        };
        let detail = result
            .as_ref()
            .err()
            .map_or(digest.to_owned(), |r| format!("{digest}: {r}"));
        self.record(&Event::new(now, "profile", serial, outcome, detail, true))?;
        result
    }

    async fn profile_inner(&self, serial: &str, digest: &str) -> Result<(), Refusal> {
        let mac = self.mac(serial)?;
        if !is_digest(digest) {
            return Err(Refusal::BadRequest(
                "digest must be 64 lowercase hex digits".into(),
            ));
        }
        let allowlist = trusted::read_text(&self.files.profile_allowlist, self.files.owner)
            .map_err(Refusal::Internal)?;
        let listed = allowlist.lines().map(str::trim).any(|line| line == digest);
        let state = self.state.lock().await;
        let in_set = state
            .pool_sets
            .get(&mac.pool)
            .is_some_and(|set| set.profiles.iter().any(|p| p == digest));
        if !listed && !in_set {
            return Err(Refusal::NotAllowlisted(digest.to_owned()));
        }
        let path = self
            .files
            .profile_dir
            .join(format!("{digest}.mobileconfig"));
        let bytes = trusted::read(&path, self.files.owner).map_err(Refusal::Internal)?;
        if hex::encode(Sha256::digest(&bytes)) != digest {
            return Err(Refusal::Internal(format!(
                "{} does not match its digest",
                path.display()
            )));
        }
        self.backend.install_profile(mac.device(), &bytes).await?;
        Ok(())
    }

    /// At startup: subscribes every Mac to the status items, and withdraws every
    /// `kbf.osupdate.` declaration the gate has no outstanding enforcement for (4.1).
    /// Declarations without the `kbf.` prefix are never touched.
    ///
    /// # Errors
    /// The MDM fails.
    pub async fn reconcile(&self) -> Result<Vec<String>, Refusal> {
        for mac in self.inventory.0.values() {
            self.backend.subscribe(mac.device()).await?;
        }
        let state = self.state.lock().await;
        let mut withdrawn = Vec::new();
        for id in self.backend.declarations().await? {
            let Some(declaration) = KbfDeclaration::new(&id) else {
                continue;
            };
            let Some(serial) = id.strip_prefix(ENFORCEMENT_PREFIX) else {
                continue;
            };
            if state.enforced.contains_key(serial) {
                continue;
            }
            self.backend.withdraw(serial, &declaration).await?;
            self.record(&Event::new(
                self.clock.now(),
                "withdraw",
                serial,
                "withdrawn",
                "stale at startup",
                false,
            ))?;
            withdrawn.push(id);
        }
        Ok(withdrawn)
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
