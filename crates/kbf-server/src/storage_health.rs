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
//! **Per node.** The same report says how many writes the node has open ([`Writes`]) and
//! which map it held when it said so. [`NodeHealth`] shows each member's debts and open
//! writes, and [`node_idle`] is the judgement a rollout makes about one node: idle only
//! when every *other* member is `Active`, fresh and knows its debts, no copy is owed
//! anywhere, and the node itself reports, freshly and knowing, no open write, from a map at
//! least as new as the one that stopped routing writes to it. Every value that is unknown
//! (never reported, stale, not counted by an older node) is a reason, never a zero.
//!
//! [`StorageGate`] is the question a rollout asks before it takes a storage node down.
//! `GET /v1/storage` and the metrics `kbf_under_replicated_blobs` (present only while the
//! number is known), `kbf_storage_health_known`, `kbf_member_repair_debts` and
//! `kbf_member_open_writes` (one sample per member, present only while that member's value
//! is known) show the same judgement to an operator.

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

/// What a storage node says about the writes it has open, at one heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writes {
    /// It cannot say: an older node, or one that does not count.
    Unknown,
    /// The writes it has open.
    Known(u64),
}

impl Writes {
    /// The wire's `writes_known` and `open_writes`. A count sent with `known` false is not
    /// a count.
    #[must_use]
    pub const fn from_wire(known: bool, open: u64) -> Self {
        if known {
            Self::Known(open)
        } else {
            Self::Unknown
        }
    }

    /// The wire's `writes_known` and `open_writes`.
    #[must_use]
    pub const fn to_wire(self) -> (bool, u64) {
        match self {
            Self::Unknown => (false, 0),
            Self::Known(open) => (true, open),
        }
    }
}

/// What one heartbeat says about the node: the copies it owes, the writes it has open and
/// the generation of the map it held when it said so (0: none, or an older node).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reported {
    /// The copies it owes.
    pub debts: Debts,
    /// The writes it has open.
    pub writes: Writes,
    /// The map generation it held.
    pub generation: u64,
}

impl Default for Reported {
    /// A node that says nothing: everything unknown.
    fn default() -> Self {
        Self {
            debts: Debts::Unknown,
            writes: Writes::Unknown,
            generation: 0,
        }
    }
}

impl Reported {
    /// A report of `debts` alone.
    #[must_use]
    pub const fn debts(debts: Debts) -> Self {
        Self {
            debts,
            writes: Writes::Unknown,
            generation: 0,
        }
    }

    /// This report, with the writes `writes` and the map generation `generation`.
    #[must_use]
    pub const fn with_writes(self, writes: Writes, generation: u64) -> Self {
        Self {
            writes,
            generation,
            ..self
        }
    }
}

/// The newest report of a member, and when the store received it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// What it said.
    pub said: Reported,
    /// The store's clock when the heartbeat carrying it was received.
    pub at: StoreTime,
}

/// An operator's planned restart of a member ([`Members::plan_down`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Planned {
    /// None: the member is not down by plan.
    No,
    /// The member is beating and out of the write path until this time on the store's
    /// clock; its first heartbeat after that returns it to `Active`.
    Until(StoreTime),
    /// The member stopped (its row expired) and its slot is held for the dwell by the
    /// plan's mark.
    Stopped,
}

impl Planned {
    /// Whether the plan holds at `now`: the member is out of the write path by plan.
    #[must_use]
    pub fn in_force(self, now: StoreTime) -> bool {
        match self {
            Self::No => false,
            Self::Until(until) => now < until,
            Self::Stopped => true,
        }
    }
}

/// One registered member as [`storage_health`] judges it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberReport {
    /// The member.
    pub id: MemberId,
    /// Its state in the map.
    pub state: MemberState,
    /// Whether it is `Down` because an operator planned a restart
    /// ([`Members::plan_down`]).
    pub planned: Planned,
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
    /// Every registered member, in the order of `members`, with what it reports.
    pub nodes: Vec<NodeHealth>,
}

/// One member as an operator sees it: its state, and the debts and open writes it reported
/// if the report is fresh. `None` is unknown (never reported, silent past the freshness
/// window, or a node that does not say), never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeHealth {
    /// The member.
    pub id: MemberId,
    /// Its state in the map.
    pub state: MemberState,
    /// Whether it is `Down` because an operator planned a restart.
    pub planned_down: bool,
    /// The copies it owes.
    pub repair_debts: Option<u64>,
    /// The writes it has open.
    pub open_writes: Option<u64>,
    /// The generation of the map it held when it reported.
    pub generation: Option<u64>,
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
        nodes: members
            .iter()
            .map(|m| node_health(m, now, freshness))
            .collect(),
    };
    for member in members {
        if member.state != MemberState::Active {
            health.not_active.push(member.id);
            continue;
        }
        health.members_active += 1;
        match member.report {
            Some(report) if now.since(report.at) <= freshness => match report.said.debts {
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

/// What `member` reported, if it did so within `freshness` of `now`.
fn fresh(member: &MemberReport, now: StoreTime, freshness: Duration) -> Option<Reported> {
    member
        .report
        .filter(|report| now.since(report.at) <= freshness)
        .map(|report| report.said)
}

fn node_health(member: &MemberReport, now: StoreTime, freshness: Duration) -> NodeHealth {
    let said = fresh(member, now, freshness);
    NodeHealth {
        id: member.id,
        state: member.state,
        planned_down: member.planned.in_force(now),
        repair_debts: said.and_then(|said| match said.debts {
            Debts::Known(debts) => Some(debts),
            Debts::Unknown => None,
        }),
        open_writes: said.and_then(|said| match said.writes {
            Writes::Known(open) => Some(open),
            Writes::Unknown => None,
        }),
        generation: said.map(|said| said.generation),
    }
}

/// Why a storage node is not idle yet: not safe to stop without failing a write or losing
/// the only copy of a blob. Each reason is a [`Class`] a controller acts on: wait, or hold.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotIdle {
    /// The node is not a registered member.
    #[error("member {} is not registered", .member.0)]
    UnknownNode {
        /// The node asked about.
        member: MemberId,
    },
    /// The node is the only member: no other copy of its blobs exists.
    #[error("no other storage member is registered")]
    NoOtherMembers,
    /// Another member is not `Active` (joining, catching up, draining or down): it may be
    /// the only holder of a blob this node's copy is the other of.
    #[error("member {} not active", .member.0)]
    OtherMemberNotActive {
        /// The other member.
        member: MemberId,
    },
    /// Another `Active` member has not reported within the freshness window: it counts as
    /// down.
    #[error("member {} silent", .member.0)]
    OtherMemberStale {
        /// The other member.
        member: MemberId,
    },
    /// Another `Active` member reported that it does not know what it owes.
    #[error("member {} does not know what it owes", .member.0)]
    OtherMemberDebtsUnknown {
        /// The other member.
        member: MemberId,
    },
    /// Copies are owed somewhere in the farm (the node's own included): its blobs may have
    /// no second copy yet.
    #[error("{n} blobs owed")]
    CopiesOwed {
        /// The copies owed, summed.
        n: u64,
    },
    /// The node is not `Active`.
    #[error("member {} is {}, not active", .member.0, .state.name())]
    NodeNotActive {
        /// The node.
        member: MemberId,
        /// Its state.
        state: MemberState,
    },
    /// The node's planned restart ran out before it was stopped: it is beating, so it
    /// returns to `Active` at its next heartbeat and is a node in service again.
    #[error("member {}'s planned restart ran out; it returns to active at its next beat", .member.0)]
    PlannedDownExpired {
        /// The node.
        member: MemberId,
    },
    /// The node has not reported within the freshness window.
    #[error("member {} silent", .member.0)]
    NodeStale {
        /// The node.
        member: MemberId,
    },
    /// The node does not know what it owes.
    #[error("member {} does not know what it owes", .member.0)]
    DebtsUnknown {
        /// The node.
        member: MemberId,
    },
    /// The node does not count its open writes.
    #[error("member {} does not count its open writes", .member.0)]
    OpenWritesUnknown {
        /// The node.
        member: MemberId,
    },
    /// The node has writes open.
    #[error("member {} has {n} open writes", .member.0)]
    OpenWrites {
        /// The node.
        member: MemberId,
        /// The writes it reported open.
        n: u64,
    },
    /// The node reported from a map older than the one that stopped routing writes to it,
    /// so a write routed by the old map may have begun after the count.
    #[error("member {} reported at map {reported}, before map {wanted}", .member.0)]
    ReportOlderThan {
        /// The node.
        member: MemberId,
        /// The generation it held when it reported.
        reported: u64,
        /// The generation the caller needs it to have seen.
        wanted: u64,
    },
    /// The membership could not be read.
    #[error("membership unreadable: {0}")]
    Unreadable(String),
}

/// What a controller does about a [`NotIdle`] reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// It passes by itself: writes end, copies are made, a report arrives. Look again.
    Wait,
    /// The farm is not in a state to take a node down: a second member is not `Active` or
    /// has gone silent, the node is the only one, or the question cannot be answered. Do
    /// not stop the node; waiting is not the remedy.
    Held,
}

impl NotIdle {
    /// Whether to wait or to hold. Exhaustive: a new reason must be classed here.
    #[must_use]
    pub const fn class(&self) -> Class {
        match self {
            Self::CopiesOwed { .. }
            | Self::OpenWrites { .. }
            | Self::OpenWritesUnknown { .. }
            | Self::DebtsUnknown { .. }
            | Self::OtherMemberDebtsUnknown { .. }
            | Self::NodeStale { .. }
            | Self::PlannedDownExpired { .. }
            | Self::ReportOlderThan { .. } => Class::Wait,
            Self::UnknownNode { .. }
            | Self::NoOtherMembers
            | Self::OtherMemberNotActive { .. }
            | Self::OtherMemberStale { .. }
            | Self::NodeNotActive { .. }
            | Self::Unreadable(_) => Class::Held,
        }
    }
}

/// Why `node` is not idle as of `now`, every reason, the first being the one
/// [`Members::node_idle`] returns; empty when it is idle. Idle is judged with `node` marked
/// out: every other member must be `Active`, within `freshness` and knowing its debts, and
/// no copy may be owed anywhere (the node's own debts included). The node itself must be
/// `Active`, fresh, knowing its debts and its open writes, with none open, and must have
/// held a map of at least `at_generation` when it reported: the map that stopped routing
/// writes to it (0 asks no more than a report). Pure.
#[must_use]
pub fn node_idle(
    members: &[MemberReport],
    now: StoreTime,
    freshness: Duration,
    node: MemberId,
    at_generation: u64,
) -> Vec<NotIdle> {
    let Some(me) = members.iter().find(|m| m.id == node) else {
        return vec![NotIdle::UnknownNode { member: node }];
    };
    let others: Vec<MemberReport> = members.iter().filter(|m| m.id != node).copied().collect();
    let farm = storage_health(&others, now, freshness);
    let mut reasons = Vec::new();
    if others.is_empty() {
        reasons.push(NotIdle::NoOtherMembers);
    }
    reasons.extend(
        farm.not_active
            .iter()
            .map(|&member| NotIdle::OtherMemberNotActive { member }),
    );
    reasons.extend(
        farm.silent
            .iter()
            .map(|&member| NotIdle::OtherMemberStale { member }),
    );
    reasons.extend(
        farm.unreported
            .iter()
            .map(|&member| NotIdle::OtherMemberDebtsUnknown { member }),
    );
    // A node taken out of the write path for its restart is the one that is stopped.
    let out = me.state == MemberState::Down && me.planned.in_force(now);
    let ran_out = me.state == MemberState::Down && matches!(me.planned, Planned::Until(_)) && !out;
    if ran_out {
        reasons.push(NotIdle::PlannedDownExpired { member: node });
    } else if me.state != MemberState::Active && !out {
        reasons.push(NotIdle::NodeNotActive {
            member: node,
            state: me.state,
        });
    }
    let Some(said) = fresh(me, now, freshness) else {
        reasons.push(NotIdle::NodeStale { member: node });
        return reasons;
    };
    match said.debts {
        Debts::Known(own) => {
            // Known only when the others are all accounted for; the reasons above say so
            // when they are not.
            let owed = farm
                .under_replicated_blobs
                .map(|others| others.saturating_add(own));
            if let Some(n @ 1..) = owed {
                reasons.push(NotIdle::CopiesOwed { n });
            }
        }
        Debts::Unknown => reasons.push(NotIdle::DebtsUnknown { member: node }),
    }
    match said.writes {
        Writes::Known(0) => {}
        Writes::Known(n) => reasons.push(NotIdle::OpenWrites { member: node, n }),
        Writes::Unknown => reasons.push(NotIdle::OpenWritesUnknown { member: node }),
    }
    if said.generation < at_generation {
        reasons.push(NotIdle::ReportOlderThan {
            member: node,
            reported: said.generation,
            wanted: at_generation,
        });
    }
    reasons
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
    nodes: Vec<NodeView>,
}

/// One member in `GET /v1/storage`: `null` is unknown, never zero.
#[derive(Debug, Serialize)]
struct NodeView {
    id: u64,
    state: &'static str,
    planned_down: bool,
    repair_debts: Option<u64>,
    open_writes: Option<u64>,
    generation: Option<u64>,
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
            nodes: health
                .nodes
                .iter()
                .map(|node| NodeView {
                    id: node.id.0,
                    state: node.state.name(),
                    planned_down: node.planned_down,
                    repair_debts: node.repair_debts,
                    open_writes: node.open_writes,
                    generation: node.generation,
                })
                .collect(),
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

const MEMBER_DEBTS_HELP: &str = "Copies one storage member owes: the open records of its repair journal. One sample per \
     member, present only while that member's report is fresh and says it knows";
const MEMBER_WRITES_HELP: &str = "Writes one storage member has open now (a Write call it is taking in, or an action-cache \
     commit). One sample per member, present only while that member's report is fresh and \
     says it counts; absent is unknown, never zero";

/// Appends the families to `out`: `kbf_storage_health_known` always,
/// `kbf_under_replicated_blobs` only while the number is known (a gauge of 0 for an
/// unknown number is the lie this exists to prevent), and a sample per member of
/// `kbf_member_repair_debts` and `kbf_member_open_writes` for each value that is known.
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
    let nodes = health.map_or(&[][..], |health| &health.nodes[..]);
    member_family(
        out,
        "kbf_member_repair_debts",
        MEMBER_DEBTS_HELP,
        nodes,
        |n| n.repair_debts,
    );
    member_family(
        out,
        "kbf_member_open_writes",
        MEMBER_WRITES_HELP,
        nodes,
        |n| n.open_writes,
    );
}

/// One gauge family with a sample for each member that `value` gives a number for; none
/// at all (not even the header) when no member does.
fn member_family(
    out: &mut String,
    name: &str,
    help: &str,
    nodes: &[NodeHealth],
    value: impl Fn(&NodeHealth) -> Option<u64>,
) {
    let samples: Vec<(MemberId, u64)> = nodes
        .iter()
        .filter_map(|node| value(node).map(|v| (node.id, v)))
        .collect();
    if samples.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
    for (id, v) in samples {
        let _ = writeln!(out, "{name}{{member=\"{}\"}} {v}", id.0);
    }
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
