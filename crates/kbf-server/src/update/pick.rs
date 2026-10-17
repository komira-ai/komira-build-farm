//! Which host goes next (`docs/design/split-mode-updates.md` sections 2.6 and 2.10):
//! the idle one, and if the farm is never idle, the least-loaded one after `idle_wait`.
//!
//! **Candidates** are hosts whose row is open in this run ([`super::HostRecord::is_open_in`]:
//! `Pending`, `Deferred`, or `Done` by an *earlier* run, which is what lets a rollback
//! walk back the hosts the forward run finished) and whose release is known and is not the
//! target. A host `Done` in this run is never a candidate, however late its heartbeat
//! reports the new release; a `Held` host is none, and holds the run. A rollback is the
//! same run with another target and a higher serial, compared with `==`, never ordered.
//!
//! **Nothing starts while anything is out.** The slot, a host in a step, and a held host
//! each answer [`WaitReason`]; what is out comes from the rows and the slot, which the
//! controller wrote itself, never from member state, which can still say `Active` for a
//! node whose update the record has begun. One slot means one host at a time, daemon-only
//! hosts included: the design lets those run in parallel up to the capacity floor (section
//! 2.6), but its `update/slot` has a single holder, and the conservative reading is kept
//! until the slot is specified for several.
//!
//! **Order.** Idle first, and an idle host is never skipped for a busier one (section 2.6).
//! Among idle hosts the tiebreak is storage-and-daemon, then Linux daemon-only, then Mac,
//! then the node id. That order is this module's choice, not the design's, which says only
//! that idle nodes come first: it is a preference, not a barrier, so an idle daemon-only
//! host does not wait for a busy storage node. If no host is idle, the picker waits
//! `idle_wait` from `Facts::wait_since`, then forces (D3) the eligible host with the
//! fewest leases, the same tiebreak on equal load; it forces a Mac only if
//! `mac_force_after` is set and has elapsed and no Linux host is eligible.
//!
//! **Eligible** means connected, not already cordoned (the cordon is somebody's, and the
//! step ends by uncordoning), with a known load, leaving at least `min_serving` other hosts
//! of its platform serving (section 2.8), and for a storage host with G_S open with the
//! host out ([`GateMode::Pick`]) and both write counts reported. Every missing fact is a
//! reason not to pick. Idle is the daemon half's (zero leases, nothing undelivered, no
//! whole-machine hold) and, for a storage host, no write open or in flight to it (section
//! 2.7: storage idle is "no in-flight writes"); the other copy's health is G_S.
//!
//! **Macs** (section 2.10) are updated when idle, never forced by default, and a busy Mac
//! is [`Skip::MacBusy`]: the run goes on and the UI counts it as deferred. The last Mac of
//! a platform class, with no other connected, uncordoned Mac of it, is updated only when
//! no queued operation needs that class; if the queue's needs are unknown, it is not.

use kbf_types::{FarmTime, WorkerId};

use super::facts::{Facts, HostFacts, Platform, PlatformClass};
use super::gate::{GateFail, GateMode, storage_gate};
use super::node_step::HostPhase;
use super::policy::Policy;

/// The picker's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Start this host: record `Selected` and claim the slot ([`super::Transition::select`]).
    Host {
        /// The host.
        host: WorkerId,
        /// Whether it was forced (D3): it was not idle when it was cordoned.
        force: bool,
    },
    /// Start none.
    Wait {
        /// Why.
        why: WaitReason,
    },
}

/// Why nothing is started now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitReason {
    /// The slot is held by this host.
    SlotHeld(WorkerId),
    /// This host is in a step.
    HostInStep(WorkerId),
    /// This host is `Held`: a person decides, and nothing else starts.
    RunHeld(WorkerId),
    /// Every host is at the target or finished: there is nothing to update.
    NothingToUpdate,
    /// These hosts have not reported a release, and no other host is left to update.
    ReleaseUnknown(Vec<WorkerId>),
    /// Hosts could be forced, but none is idle yet; D3 acts at `until`.
    IdleWait {
        /// When the idle wait ends.
        until: FarmTime,
    },
    /// No candidate may be picked now, each for its own reason. A controller counts the
    /// [`Skip::MacBusy`] and [`Skip::Deferred`] entries for "done with N deferred".
    NoneEligible(Vec<(WorkerId, Skip)>),
}

/// Why one candidate is not picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Its daemon stream is closed.
    Disconnected,
    /// Placement has it cordoned already, by someone else.
    AlreadyCordoned,
    /// Placement is not reported.
    PlacementUnknown,
    /// Its load is not reported: it is neither idle nor rankable.
    LoadUnknown,
    /// A storage host that does not report both its write counts: it could never leave
    /// step 4.
    WritesUnknown,
    /// Cordoning it would leave fewer than `min_serving` other hosts of its platform.
    BelowMinServing {
        /// How many would be left.
        serving: usize,
    },
    /// G_S is shut with this host out (an unlinked storage half included).
    Gate(GateFail),
    /// A busy Mac, and Macs are not forced.
    MacBusy,
    /// The last Mac of its class, and a queued operation needs the class (or the queue's
    /// needs are not reported).
    LastOfClass,
    /// A host that missed its wait once: picked again only when idle, never forced.
    Deferred,
}

/// A candidate that may be picked.
struct Eligible<'a> {
    host: &'a HostFacts,
    idle: bool,
    weight: u64,
}

impl<'a> Eligible<'a> {
    /// storage-and-daemon, then Linux daemon-only, then Mac, then the name.
    fn rank(&self) -> (u8, &'a WorkerId) {
        let tier = if self.host.is_storage() {
            0
        } else if self.host.is_mac() {
            2
        } else {
            1
        };
        (tier, &self.host.id)
    }
}

/// Chooses the next host to update, or says why none.
#[must_use]
pub fn pick(facts: &Facts, policy: &Policy, now: FarmTime) -> Pick {
    let wait = |why| Pick::Wait { why };
    if let Some(holder) = &facts.slot {
        return wait(WaitReason::SlotHeld(holder.clone()));
    }
    if let Some(h) = facts.hosts.iter().find(|h| h.record.phase.in_step()) {
        return wait(WaitReason::HostInStep(h.id.clone()));
    }
    let held = |h: &&HostFacts| matches!(h.record.phase, HostPhase::Held { .. });
    if let Some(h) = facts.hosts.iter().find(held) {
        return wait(WaitReason::RunHeld(h.id.clone()));
    }
    let mut candidates = Vec::new();
    let mut unknown = Vec::new();
    for host in facts
        .hosts
        .iter()
        .filter(|h| h.record.is_open_in(facts.serial))
    {
        match &host.release {
            Some(release) if *release == facts.target => {}
            Some(_) => candidates.push(host),
            None => unknown.push(host.id.clone()),
        }
    }
    if candidates.is_empty() {
        return wait(if unknown.is_empty() {
            WaitReason::NothingToUpdate
        } else {
            WaitReason::ReleaseUnknown(unknown)
        });
    }
    let mut eligible = Vec::new();
    let mut skipped = Vec::new();
    for host in candidates {
        match verdict(facts, host, policy, now) {
            Ok(e) => eligible.push(e),
            Err(skip) => skipped.push((host.id.clone(), skip)),
        }
    }
    if let Some(idle) = eligible.iter().filter(|e| e.idle).min_by_key(|e| e.rank()) {
        return Pick::Host {
            host: idle.host.id.clone(),
            force: false,
        };
    }
    let until = facts.wait_since.saturating_add(policy.idle_wait);
    if !eligible.is_empty() && now < until {
        return wait(WaitReason::IdleWait { until });
    }
    // D3: nobody idle for `idle_wait`. Linux first; a Mac only when its own wait is over.
    // A deferred host was forced once already.
    let mac_ok = mac_may_be_forced(facts, policy, now);
    let forced = |macs: bool| {
        let deferred = |e: &&Eligible<'_>| e.host.record.was_deferred_in(facts.serial);
        eligible
            .iter()
            .filter(|e| e.host.is_mac() == macs && !deferred(e))
            .min_by_key(|e| (e.weight, e.rank()))
    };
    let chosen = forced(false).or_else(|| if mac_ok { forced(true) } else { None });
    match chosen {
        Some(e) => Pick::Host {
            host: e.host.id.clone(),
            force: true,
        },
        None => {
            // Only deferred hosts are left: eligible, not idle, and not to be forced again.
            skipped.extend(eligible.iter().map(|e| (e.host.id.clone(), Skip::Deferred)));
            wait(WaitReason::NoneEligible(skipped))
        }
    }
}

/// Whether `host` may be picked, and if so how loaded it is; else the reason it may not.
fn verdict<'a>(
    facts: &'a Facts,
    host: &'a HostFacts,
    policy: &Policy,
    now: FarmTime,
) -> Result<Eligible<'a>, Skip> {
    if !host.connected {
        return Err(Skip::Disconnected);
    }
    match host.cordoned {
        Some(false) => {}
        Some(true) => return Err(Skip::AlreadyCordoned),
        None => return Err(Skip::PlacementUnknown),
    }
    let load = host.load.ok_or(Skip::LoadUnknown)?;
    let mut idle = load.is_idle();
    if host.is_storage() {
        let own = host.member.ok_or(Skip::Gate(GateFail::Unlinked))?;
        storage_gate(facts, own, GateMode::Pick, policy, now).map_err(Skip::Gate)?;
        if !host.writes_reported() {
            return Err(Skip::WritesUnknown);
        }
        idle = idle && host.writes_quiet();
    }
    let serving = facts.serving_peers(host);
    if serving < policy.min_serving {
        return Err(Skip::BelowMinServing { serving });
    }
    if let Platform::Mac(class) = &host.platform {
        if !idle && !mac_may_be_forced(facts, policy, now) {
            return Err(Skip::MacBusy);
        }
        if is_last_of_class(facts, host, class) && needs_class(facts, class) {
            return Err(Skip::LastOfClass);
        }
    }
    Ok(Eligible {
        host,
        idle,
        weight: load.weight(),
    })
}

/// Whether `mac_force_after` is set and has passed since the picker began waiting. Unset,
/// which is the default, no Mac is forced.
fn mac_may_be_forced(facts: &Facts, policy: &Policy, now: FarmTime) -> bool {
    policy
        .mac_force_after
        .is_some_and(|after| now >= facts.wait_since.saturating_add(after))
}

/// Whether no other Mac of `class` is connected and uncordoned: taking `host` out leaves
/// the class with no serving host.
fn is_last_of_class(facts: &Facts, host: &HostFacts, class: &PlatformClass) -> bool {
    !facts.hosts.iter().any(|other| {
        other.id != host.id
            && matches!(&other.platform, Platform::Mac(c) if c == class)
            && other.connected
            && other.cordoned == Some(false)
    })
}

/// Whether a queued operation needs `class`. Unknown needs count as needing it.
fn needs_class(facts: &Facts, class: &PlatformClass) -> bool {
    facts
        .queued_classes
        .as_ref()
        .is_none_or(|needed| needed.contains(class))
}
