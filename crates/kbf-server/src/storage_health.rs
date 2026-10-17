//! Storage health: how many blobs are held once that should be held twice, and whether
//! anyone can say (`docs/design/architecture-v2.md`, section 11).
//!
//! **`under_replicated_blobs`** is the sum, over all registered storage members, of the
//! blobs each owes a second copy (its open repair-journal records). It is *known* only when
//! every registered member is `Active` and has reported within the freshness window;
//! otherwise it is *unknown*, because a member that is down, joining, catching up,
//! draining or silent may hold blobs that nobody counts, and a member that does not know
//! its own debts (it has not read its journal since it started) must not claim none. Zero
//! registered members is unknown too: an empty sum is not a count.
//!
//! The report rides each storage heartbeat ([`Debts`]); [`Members`] keeps the newest one
//! in the member's own row, so it cannot outlive the row, and a node that registers again
//! starts without one. [`storage_health`] is the whole judgement, a function of the
//! members, the store's clock and the freshness window, with no I/O.
//!
//! **What the number is not.** It is as old as the newest heartbeat, up to the freshness
//! window: a copy owed after the last report is not in it. Whoever acts on a zero must
//! keep new writes away from the node first (drain it), then read again.
//!
//! [`StorageGate`] is the question a rollout asks before it takes a storage node down.
//! `GET /v1/storage` and the metrics `kbf_under_replicated_blobs` (present only while the
//! number is known) and `kbf_storage_health_known` show the same judgement to an operator.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use kbf_map::{MemberId, MemberState};
use kbf_state::StoreTime;
use serde::Serialize;

use crate::api::{error, json};
use crate::members::Members;

/// The path of `GET /v1/storage`.
pub const STORAGE: &str = "/v1/storage";

/// What a storage node says it owes, at one heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Debts {
    /// It cannot say: it has not read a repair journal since it started (or has none).
    Unknown,
    /// The copies it owes: blobs it holds that a second owner does not.
    Known(u64),
}

impl Debts {
    /// The wire's `debts_known` and `repair_debts`. A count sent with `known` false is
    /// not a count.
    #[must_use]
    pub const fn from_wire(known: bool, debts: u64) -> Self {
        if known {
            Self::Known(debts)
        } else {
            Self::Unknown
        }
    }

    /// The wire's `debts_known` and `repair_debts`.
    #[must_use]
    pub const fn to_wire(self) -> (bool, u64) {
        match self {
            Self::Unknown => (false, 0),
            Self::Known(debts) => (true, debts),
        }
    }
}

/// The newest report of a member, and when the store received it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// What it said.
    pub debts: Debts,
    /// The store's clock when the heartbeat carrying it was received.
    pub at: StoreTime,
}

/// One registered member as [`storage_health`] judges it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberReport {
    /// The member.
    pub id: MemberId,
    /// Its state in the map.
    pub state: MemberState,
    /// Its newest report; `None` before its first heartbeat and after its row expired.
    pub report: Option<Report>,
}

/// The judgement [`storage_health`] reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageHealth {
    /// Registered members, whatever their state.
    pub members_total: usize,
    /// Of them, the `Active` ones.
    pub members_active: usize,
    /// Members that are not `Active` (`Joining`, `CatchingUp`, `Draining`, `Down`).
    pub not_active: Vec<MemberId>,
    /// `Active` members with no report, or none within the freshness window.
    pub silent: Vec<MemberId>,
    /// `Active` members whose fresh report says they do not know their debts.
    pub unreported: Vec<MemberId>,
    /// The debts of the `Active` members with a fresh, known report, summed. Counted even
    /// when the total is unknown, so an operator sees what is owed among those who said.
    pub debts_total: u64,
    /// [`debts_total`](Self::debts_total) when every registered member is accounted for:
    /// there is at least one, and each is `Active` with a fresh, known report. `None`
    /// otherwise. A gate treats `None` as not zero.
    pub under_replicated_blobs: Option<u64>,
}

/// Why a storage node must not be taken down yet.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotReady {
    /// Nothing is registered, so there is no number.
    #[error("no storage member is registered")]
    NoMembers,
    /// A member is not `Active`: it may hold the only copy of a blob.
    #[error("member {} not active", .member.0)]
    NotActive {
        /// The member.
        member: MemberId,
    },
    /// An `Active` member has not reported within the freshness window.
    #[error("member {} silent", .member.0)]
    Silent {
        /// The member.
        member: MemberId,
    },
    /// An `Active` member reported that it does not know its debts.
    #[error("member {} does not know what it owes", .member.0)]
    Unreported {
        /// The member.
        member: MemberId,
    },
    /// Every member answered, and copies are owed.
    #[error("{0} blobs owed")]
    Owed(u64),
    /// The membership could not be read.
    #[error("membership unreadable: {0}")]
    Unreadable(String),
}

/// The judgement over `members` as of `now`: a report older than `freshness` is silent.
/// Pure. Lists keep the order of `members`.
#[must_use]
pub fn storage_health(
    members: &[MemberReport],
    now: StoreTime,
    freshness: Duration,
) -> StorageHealth {
    let mut health = StorageHealth {
        members_total: members.len(),
        members_active: 0,
        not_active: Vec::new(),
        silent: Vec::new(),
        unreported: Vec::new(),
        debts_total: 0,
        under_replicated_blobs: None,
    };
    for member in members {
        if member.state != MemberState::Active {
            health.not_active.push(member.id);
            continue;
        }
        health.members_active += 1;
        match member.report {
            Some(report) if now.since(report.at) <= freshness => match report.debts {
                Debts::Known(debts) => {
                    health.debts_total = health.debts_total.saturating_add(debts);
                }
                Debts::Unknown => health.unreported.push(member.id),
            },
            _ => health.silent.push(member.id),
        }
    }
    let accounted = !members.is_empty()
        && health.not_active.is_empty()
        && health.silent.is_empty()
        && health.unreported.is_empty();
    health.under_replicated_blobs = accounted.then_some(health.debts_total);
    health
}

impl StorageHealth {
    /// Every reason the number is not zero, the first being the one [`StorageHealth::ready`]
    /// returns: no members; members not active, silent, then not knowing their debts; and,
    /// when all of them answered, the copies owed. Empty when it is known to be zero.
    #[must_use]
    pub fn reasons(&self) -> Vec<NotReady> {
        let mut reasons = Vec::new();
        if self.members_total == 0 {
            reasons.push(NotReady::NoMembers);
        }
        reasons.extend(
            self.not_active
                .iter()
                .map(|&member| NotReady::NotActive { member }),
        );
        reasons.extend(
            self.silent
                .iter()
                .map(|&member| NotReady::Silent { member }),
        );
        reasons.extend(
            self.unreported
                .iter()
                .map(|&member| NotReady::Unreported { member }),
        );
        if let Some(owed @ 1..) = self.under_replicated_blobs {
            reasons.push(NotReady::Owed(owed));
        }
        reasons
    }

    /// Whether every blob is known to have its copies elsewhere.
    ///
    /// # Errors
    /// The first of [`StorageHealth::reasons`].
    pub fn ready(&self) -> Result<(), NotReady> {
        self.reasons().into_iter().next().map_or(Ok(()), Err)
    }
}

/// The question a rollout asks before it takes a storage node down: is every blob known to
/// have its copies elsewhere?
pub trait StorageGate: Send + Sync {
    /// `Ok` only when `under_replicated_blobs` is known and zero.
    ///
    /// # Errors
    /// [`NotReady`] naming the reason; a membership that cannot be read is one.
    fn ready(&self) -> Result<(), NotReady>;
}

impl StorageGate for Members {
    fn ready(&self) -> Result<(), NotReady> {
        self.storage_health()
            .map_err(|e| NotReady::Unreadable(e.to_string()))?
            .ready()
    }
}

/// [`StorageHealth`] as `GET /v1/storage` answers it: the number is `null` while unknown,
/// and `reasons` says why (empty when it is known to be zero).
#[derive(Debug, Serialize)]
struct View {
    members_total: usize,
    members_active: usize,
    not_active: Vec<u64>,
    silent: Vec<u64>,
    unreported: Vec<u64>,
    debts_total: u64,
    under_replicated_blobs: Option<u64>,
    reasons: Vec<String>,
}

impl From<&StorageHealth> for View {
    fn from(health: &StorageHealth) -> Self {
        let ids = |ids: &[MemberId]| ids.iter().map(|id| id.0).collect();
        Self {
            members_total: health.members_total,
            members_active: health.members_active,
            not_active: ids(&health.not_active),
            silent: ids(&health.silent),
            unreported: ids(&health.unreported),
            debts_total: health.debts_total,
            under_replicated_blobs: health.under_replicated_blobs,
            reasons: health.reasons().iter().map(ToString::to_string).collect(),
        }
    }
}

/// `GET /v1/storage` over `members`.
pub fn router(members: Arc<Members>) -> Router {
    Router::new()
        .route(STORAGE, get(storage))
        .with_state(members)
}

async fn storage(State(members): State<Arc<Members>>) -> Response {
    match read(members).await {
        Ok(health) => json(StatusCode::OK, &View::from(&health)),
        Err(why) => error(StatusCode::SERVICE_UNAVAILABLE, &why),
    }
}

/// The health on a blocking thread: the store's calls block.
async fn read(members: Arc<Members>) -> Result<StorageHealth, String> {
    tokio::task::spawn_blocking(move || members.storage_health())
        .await
        .map_err(|e| format!("storage health task: {e}"))?
        .map_err(|e| e.to_string())
}

/// What the metric families say, for the scrape: HELP text carries the definition.
const UNDER_REPLICATED_HELP: &str = "Blobs held by one storage member that no second owner holds yet: the sum over every \
     registered storage member of its open repair-journal records. Exported only while \
     known: every registered member is ACTIVE and has reported within the freshness window";
const KNOWN_HELP: &str = "1 when the count of under-replicated blobs is known, 0 when it is unknown (a member is \
     not ACTIVE, is silent, or does not know its debts, or the membership cannot be read)";

/// Appends the two families to `out`: `kbf_storage_health_known` always, and
/// `kbf_under_replicated_blobs` only while the number is known (a gauge of 0 for an
/// unknown number is the lie this exists to prevent).
pub fn render(out: &mut String, health: Option<&StorageHealth>) {
    let known = health.and_then(|health| health.under_replicated_blobs);
    if let Some(blobs) = known {
        let _ = writeln!(
            out,
            "# HELP kbf_under_replicated_blobs {UNDER_REPLICATED_HELP}\n\
             # TYPE kbf_under_replicated_blobs gauge\n\
             kbf_under_replicated_blobs {blobs}"
        );
    }
    let _ = writeln!(
        out,
        "# HELP kbf_storage_health_known {KNOWN_HELP}\n\
         # TYPE kbf_storage_health_known gauge\n\
         kbf_storage_health_known {}",
        u8::from(known.is_some())
    );
}

/// Appends the families for a scrape of `members`; a membership that cannot be read is
/// logged and scraped as unknown.
pub async fn scrape(out: &mut String, members: &Arc<Members>) {
    let health = read(Arc::clone(members)).await;
    if let Err(why) = &health {
        tracing::warn!("storage health for the scrape: {why}");
    }
    render(out, health.as_ref().ok());
}
