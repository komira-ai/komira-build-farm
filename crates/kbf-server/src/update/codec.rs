//! The bytes of the `update/*` rows (`docs/design/split-mode-updates.md` section 6.1 and
//! `docs/design/pull-updates.md` section 6.2, which agree on the fields).
//!
//! Each row is a protobuf message written with `prost`, as `member/<id>` is
//! (`crate::members`): an unknown field is skipped, so a later version may add one, and a
//! field the reader does not find is its zero value. So **a field added later must be valid
//! at zero** (or optional), and no field changes its number or meaning (the state-store rule
//! of section 3.4: a row is frozen across one step). `tests/update_rows.rs` holds the bytes
//! of one value of each row, and fails if they move.
//!
//! The fields are the agreed ones, numbered in the order the design lists them, and a few
//! beyond them, each said where it is declared: `update/desired.paused` (the pause button
//! writes it), `update/host.reason` (the reason of a `Held` phase), `update/host.order_epoch`
//! (the epoch an order id was drawn under) and `update/host.commit` (the release the host's
//! run drives it to), and the node step's limits in `update/policy`, each optional and absent
//! when it is the default, so that a writer that knows only the eight agreed fields leaves
//! them at the default instead of at zero.
//!
//! The enums are numbered here, not by their Rust order, from 1: a zero (an absent field) is
//! no phase and no reason, and a row that holds one is [`Corrupt`], never read as the first
//! variant. A reader that meets a number it does not know refuses the row: every unknown
//! fails closed.

use std::time::Duration;

use kbf_types::{FarmTime, WorkerId};
use prost::Message as _;

use super::command::HoldReason;
use super::facts::{Generation, HostRecord, OrderId, Release};
use super::node_step::HostPhase;
use super::policy::Policy;
use super::slot::{Holder, Slot};

/// What is wrong with a row's bytes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Corrupt(pub(super) String);

fn corrupt<T>(why: impl Into<String>) -> Result<T, Corrupt> {
    Err(Corrupt(why.into()))
}

/// `update/desired`: the run the farm is driven to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desired {
    /// The generation of the run. It grows with every change of the desired release; a
    /// rollback is the older commit under a higher generation.
    pub generation: Generation,
    /// The release every host is driven to (the design's `commit`).
    pub commit: Release,
    /// When the release was set: where the idle wait counts from until a host finishes.
    pub set_at: FarmTime,
    /// Whether no host may be started (hosts in a step finish). Beyond the agreed fields:
    /// the pause button writes it.
    pub paused: bool,
}

/// `update/host/<id>`: a host's record, and the commit its run drives it to. A host started
/// under an earlier generation finishes that step once it is past the activate order (a
/// rollback walks it back afterwards), so the commit is kept with the row and not read from
/// `update/desired`. `commit` is beyond the agreed fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRow {
    /// Where the host is, as the pure step reads it.
    pub record: HostRecord,
    /// The release the run that started the host drives to; unread while the host is not in
    /// a step.
    pub commit: Release,
}

impl HostRow {
    /// The row of a host nothing has been done to.
    #[must_use]
    pub fn unstarted() -> Self {
        Self {
            record: HostRecord::unstarted(),
            commit: Release::new(""),
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct DesiredMsg {
    #[prost(uint64, tag = "1")]
    generation: u64,
    #[prost(string, tag = "2")]
    commit: String,
    #[prost(uint64, tag = "3")]
    set_at_ms: u64,
    #[prost(bool, tag = "4")]
    paused: bool,
}

/// The agreed fields are `phase`, `order_id`, `generation`, `since` and `boot_before`;
/// `reason` is the hold reason of a `Held` phase, `order_epoch` the epoch the order id was
/// drawn under (an order id is the pair), and `commit` the run's release.
#[derive(Clone, PartialEq, prost::Message)]
struct HostMsg {
    #[prost(uint32, tag = "1")]
    phase: u32,
    #[prost(uint64, tag = "2")]
    order_id: u64,
    #[prost(uint64, tag = "3")]
    generation: u64,
    #[prost(uint64, tag = "4")]
    since_ms: u64,
    #[prost(uint64, optional, tag = "5")]
    boot_before: Option<u64>,
    #[prost(uint32, tag = "6")]
    reason: u32,
    #[prost(uint64, tag = "7")]
    order_epoch: u64,
    #[prost(string, tag = "8")]
    commit: String,
}

/// One holder of a place: the agreed `{host, order_id, epoch}`.
#[derive(Clone, PartialEq, prost::Message)]
struct HolderMsg {
    #[prost(string, tag = "1")]
    host: String,
    #[prost(uint64, tag = "2")]
    order_id: u64,
    #[prost(uint64, tag = "3")]
    epoch: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct SlotMsg {
    #[prost(message, optional, tag = "1")]
    storage: Option<HolderMsg>,
    #[prost(message, repeated, tag = "2")]
    hosts: Vec<HolderMsg>,
}

/// Fields 1 to 8 are the agreed ones, in the design's order; 9 to 17 are the node step's
/// limits. `idle_wait`, `max_hosts_out` and every limit are optional: absent is the default
/// of [`Policy::default`] (so `max_hosts_out` absent is 1, not the invalid 0), and an
/// explicit zero is a zero.
#[derive(Clone, PartialEq, prost::Message)]
struct PolicyMsg {
    #[prost(bool, tag = "1")]
    auto_nodes: bool,
    #[prost(bool, tag = "2")]
    auto_frontend: bool,
    #[prost(uint64, optional, tag = "3")]
    idle_wait_ms: Option<u64>,
    #[prost(uint64, tag = "4")]
    min_serving: u64,
    #[prost(uint64, optional, tag = "5")]
    max_hosts_out: Option<u64>,
    #[prost(uint64, optional, tag = "6")]
    mac_force_after_ms: Option<u64>,
    #[prost(bool, tag = "7")]
    new_work_only_to_updated: bool,
    #[prost(bool, tag = "8")]
    accept_failing_builds: bool,
    #[prost(uint64, optional, tag = "9")]
    ttl_ms: Option<u64>,
    #[prost(uint64, optional, tag = "10")]
    dwell_ms: Option<u64>,
    #[prost(uint64, optional, tag = "11")]
    lease_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "12")]
    mac_lease_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "13")]
    write_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "14")]
    activate_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "15")]
    health_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "16")]
    rebuild_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "17")]
    repair_wait_ms: Option<u64>,
}

const fn phase_code(phase: &HostPhase) -> (u32, u32) {
    match phase {
        HostPhase::Pending => (1, 0),
        HostPhase::Selected => (2, 0),
        HostPhase::Cordoned => (3, 0),
        HostPhase::WaitingLeases => (4, 0),
        HostPhase::PlannedDown => (5, 0),
        HostPhase::Gated => (6, 0),
        HostPhase::Activating => (7, 0),
        HostPhase::RejoiningStorage => (8, 0),
        HostPhase::Repairing => (9, 0),
        HostPhase::Confirming => (10, 0),
        HostPhase::RejoiningWork => (11, 0),
        HostPhase::Done => (12, 0),
        HostPhase::Held { reason } => (13, reason_code(*reason)),
        HostPhase::Deferred => (14, 0),
    }
}

const fn reason_code(reason: HoldReason) -> u32 {
    match reason {
        HoldReason::SecondMemberDown => 1,
        HoldReason::RestartDeadline => 2,
        HoldReason::MemberGone => 3,
        HoldReason::ActivateStalled => 4,
        HoldReason::HealthLimit => 5,
        HoldReason::RebuildLimit => 6,
        HoldReason::RepairWait => 7,
        HoldReason::ConfirmStalled => 8,
        HoldReason::Reverted => 9,
        HoldReason::RevertRefused => 10,
        HoldReason::StillCordoned => 11,
    }
}

fn reason_from(code: u32) -> Option<HoldReason> {
    Some(match code {
        1 => HoldReason::SecondMemberDown,
        2 => HoldReason::RestartDeadline,
        3 => HoldReason::MemberGone,
        4 => HoldReason::ActivateStalled,
        5 => HoldReason::HealthLimit,
        6 => HoldReason::RebuildLimit,
        7 => HoldReason::RepairWait,
        8 => HoldReason::ConfirmStalled,
        9 => HoldReason::Reverted,
        10 => HoldReason::RevertRefused,
        11 => HoldReason::StillCordoned,
        _ => return None,
    })
}

fn phase_from(code: u32, reason: u32) -> Result<HostPhase, Corrupt> {
    let phase = match code {
        1 => HostPhase::Pending,
        2 => HostPhase::Selected,
        3 => HostPhase::Cordoned,
        4 => HostPhase::WaitingLeases,
        5 => HostPhase::PlannedDown,
        6 => HostPhase::Gated,
        7 => HostPhase::Activating,
        8 => HostPhase::RejoiningStorage,
        9 => HostPhase::Repairing,
        10 => HostPhase::Confirming,
        11 => HostPhase::RejoiningWork,
        12 => HostPhase::Done,
        13 => {
            let Some(reason) = reason_from(reason) else {
                return corrupt(format!("hold reason {reason} is not one this build knows"));
            };
            return Ok(HostPhase::Held { reason });
        }
        14 => HostPhase::Deferred,
        other => return corrupt(format!("phase {other} is not one this build knows")),
    };
    if reason == 0 {
        Ok(phase)
    } else {
        corrupt(format!("{phase:?} carries the hold reason {reason}"))
    }
}

fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn size(n: u64, field: &str) -> Result<usize, Corrupt> {
    usize::try_from(n).or_else(|_| corrupt(format!("{field} {n} does not fit")))
}

/// The bytes of `update/desired`.
#[must_use]
pub(super) fn encode_desired(desired: &Desired) -> Vec<u8> {
    DesiredMsg {
        generation: desired.generation.0,
        commit: desired.commit.as_str().to_owned(),
        set_at_ms: desired.set_at.as_millis(),
        paused: desired.paused,
    }
    .encode_to_vec()
}

/// # Errors
/// [`Corrupt`]: the bytes are not a message, the commit is empty, or the generation is 0.
pub(super) fn decode_desired(bytes: &[u8]) -> Result<Desired, Corrupt> {
    let msg = DesiredMsg::decode(bytes).map_err(|e| Corrupt(e.to_string()))?;
    if msg.commit.is_empty() {
        return corrupt("the run has no commit");
    }
    // Generation 0 is the generation of a host no run has touched, so a run of it would
    // read every host as having finished it.
    if msg.generation == 0 {
        return corrupt("the run has no generation");
    }
    Ok(Desired {
        generation: Generation(msg.generation),
        commit: Release::new(msg.commit),
        set_at: FarmTime::from_millis(msg.set_at_ms),
        paused: msg.paused,
    })
}

/// The bytes of `update/host/<id>`.
#[must_use]
pub(super) fn encode_host(row: &HostRow) -> Vec<u8> {
    let (phase, reason) = phase_code(&row.record.phase);
    HostMsg {
        phase,
        order_id: row.record.order.seq,
        generation: row.record.generation.0,
        since_ms: row.record.since.as_millis(),
        boot_before: row.record.boot_before,
        reason,
        order_epoch: row.record.order.epoch,
        commit: row.commit.as_str().to_owned(),
    }
    .encode_to_vec()
}

/// # Errors
/// [`Corrupt`]: the bytes are not a message, or name a phase or reason this build does not
/// know.
pub(super) fn decode_host(bytes: &[u8]) -> Result<HostRow, Corrupt> {
    let msg = HostMsg::decode(bytes).map_err(|e| Corrupt(e.to_string()))?;
    Ok(HostRow {
        record: HostRecord {
            phase: phase_from(msg.phase, msg.reason)?,
            since: FarmTime::from_millis(msg.since_ms),
            generation: Generation(msg.generation),
            order: OrderId::new(msg.order_epoch, msg.order_id),
            boot_before: msg.boot_before,
        },
        commit: Release::new(msg.commit),
    })
}

fn holder_msg(holder: &Holder) -> HolderMsg {
    HolderMsg {
        host: holder.host.as_str().to_owned(),
        order_id: holder.order.seq,
        epoch: holder.order.epoch,
    }
}

fn holder_from(msg: HolderMsg) -> Result<Holder, Corrupt> {
    if msg.host.is_empty() {
        return corrupt("a holder has no host");
    }
    Ok(Holder {
        host: WorkerId::new(msg.host),
        order: OrderId::new(msg.epoch, msg.order_id),
    })
}

/// The bytes of `update/slot`.
#[must_use]
pub(super) fn encode_slot(slot: &Slot) -> Vec<u8> {
    SlotMsg {
        storage: slot.storage.as_ref().map(holder_msg),
        hosts: slot.hosts.iter().map(holder_msg).collect(),
    }
    .encode_to_vec()
}

/// # Errors
/// [`Corrupt`]: the bytes are not a message, a holder has no host, or one host holds two
/// places.
pub(super) fn decode_slot(bytes: &[u8]) -> Result<Slot, Corrupt> {
    let msg = SlotMsg::decode(bytes).map_err(|e| Corrupt(e.to_string()))?;
    let slot = Slot {
        storage: msg.storage.map(holder_from).transpose()?,
        hosts: msg
            .hosts
            .into_iter()
            .map(holder_from)
            .collect::<Result<_, _>>()?,
    };
    let held = slot.holders().count();
    let mut names: Vec<&WorkerId> = slot.holders().map(|(_, h)| &h.host).collect();
    names.sort();
    names.dedup();
    if names.len() != held {
        return corrupt("a host holds two places");
    }
    Ok(slot)
}

/// `Some` of the whole milliseconds of `value` when it is not `default`; the row leaves a
/// default out.
fn unless_default(value: Duration, default: Duration) -> Option<u64> {
    (value != default).then(|| ms(value))
}

/// The duration a row holds, or `default` for a field it does not.
fn or_default(stored: Option<u64>, default: Duration) -> Duration {
    stored.map_or(default, Duration::from_millis)
}

/// The bytes of `update/policy`. Durations are whole milliseconds, and a field that is the
/// default of [`Policy::default`] is left out.
#[must_use]
pub(super) fn encode_policy(policy: &Policy) -> Vec<u8> {
    let default = Policy::default();
    PolicyMsg {
        auto_nodes: policy.auto_nodes,
        auto_frontend: policy.auto_frontend,
        idle_wait_ms: unless_default(policy.idle_wait, default.idle_wait),
        min_serving: count(policy.min_serving),
        max_hosts_out: (policy.max_hosts_out != default.max_hosts_out)
            .then(|| count(policy.max_hosts_out)),
        mac_force_after_ms: policy.mac_force_after.map(ms),
        new_work_only_to_updated: policy.new_work_only_to_updated,
        accept_failing_builds: policy.accept_failing_builds,
        ttl_ms: unless_default(policy.ttl, default.ttl),
        dwell_ms: unless_default(policy.dwell, default.dwell),
        lease_limit_ms: unless_default(policy.lease_limit, default.lease_limit),
        mac_lease_limit_ms: unless_default(policy.mac_lease_limit, default.mac_lease_limit),
        write_limit_ms: unless_default(policy.write_limit, default.write_limit),
        activate_limit_ms: unless_default(policy.activate_limit, default.activate_limit),
        health_limit_ms: unless_default(policy.health_limit, default.health_limit),
        rebuild_limit_ms: policy.rebuild_limit.map(ms),
        repair_wait_ms: unless_default(policy.repair_wait, default.repair_wait),
    }
    .encode_to_vec()
}

/// # Errors
/// [`Corrupt`]: the bytes are not a message, or a count does not fit.
pub(super) fn decode_policy(bytes: &[u8]) -> Result<Policy, Corrupt> {
    let msg = PolicyMsg::decode(bytes).map_err(|e| Corrupt(e.to_string()))?;
    let default = Policy::default();
    Ok(Policy {
        auto_nodes: msg.auto_nodes,
        auto_frontend: msg.auto_frontend,
        new_work_only_to_updated: msg.new_work_only_to_updated,
        accept_failing_builds: msg.accept_failing_builds,
        ttl: or_default(msg.ttl_ms, default.ttl),
        dwell: or_default(msg.dwell_ms, default.dwell),
        idle_wait: or_default(msg.idle_wait_ms, default.idle_wait),
        min_serving: size(msg.min_serving, "min_serving")?,
        max_hosts_out: match msg.max_hosts_out {
            Some(n) => size(n, "max_hosts_out")?,
            None => default.max_hosts_out,
        },
        lease_limit: or_default(msg.lease_limit_ms, default.lease_limit),
        mac_lease_limit: or_default(msg.mac_lease_limit_ms, default.mac_lease_limit),
        write_limit: or_default(msg.write_limit_ms, default.write_limit),
        activate_limit: or_default(msg.activate_limit_ms, default.activate_limit),
        health_limit: or_default(msg.health_limit_ms, default.health_limit),
        rebuild_limit: msg.rebuild_limit_ms.map(Duration::from_millis),
        repair_wait: or_default(msg.repair_wait_ms, default.repair_wait),
        mac_force_after: msg.mac_force_after_ms.map(Duration::from_millis),
    })
}
