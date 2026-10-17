//! Which hosts go next (`docs/design/split-mode-updates.md` sections 2.6 and 2.10): the idle
//! ones, and if the farm is never idle, the least-loaded one after `idle_wait`.
//!
//! **Candidates** are hosts whose row is open in this run ([`super::HostRecord::is_open_in`]:
//! `Pending`, `Deferred`, or `Done` by an *earlier* run, which is what lets a rollback
//! walk back the hosts the forward run finished) and whose release is known and is not the
//! target. A host `Done` in this run is never a candidate, however late its heartbeat
//! reports the new release; a `Held` host is none, and holds the run. A rollback is the
//! same run with another target and a higher generation, compared with `==`, never ordered.
//!
//! **Places.** Storage-and-daemon hosts start one at a time (section 2.1). Daemon-only hosts
//! and Macs may be in a step together, up to [`Policy::max_hosts_out`] (section 2.6). The two
//! are separate budgets ([`super::Slot`]), and the answer is the set of hosts to start in
//! this look, within what is free. What is out comes from the rows and the slot, which the
//! controller wrote itself, never from member state, which can still say `Active` for a node
//! whose update the record has begun: a host is out if the slot holds it or its record is in
//! a step. A held host holds the whole run, whatever the places.
//!
//! **Order.** Idle first, and an idle host is never skipped for a busier one (section 2.6).
//! Among idle hosts the tiebreak is storage-and-daemon, then Linux daemon-only, then Mac,
//! then the node id. That order is this module's choice, not the design's, which says only
//! that idle nodes come first: it is a preference, not a barrier, so an idle daemon-only
//! host does not wait for a busy storage node. If no host is idle, the picker waits
//! `idle_wait` from `Facts::wait_since`, then forces (D3) the eligible host with the
//! fewest leases, the same tiebreak on equal load; it forces a Mac only if
//! `mac_force_after` is set and has elapsed and no Linux host is eligible. D3 forces one
//! host in a look: the next look forces the next while the farm stays busy and a place is
//! free.
//!
//! **Together.** Hosts chosen in one look are not yet shown cordoned, so each is checked
//! against the others as if they were: the capacity floor and the last Mac of a class count
//! the hosts already chosen as gone.
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

use std::collections::BTreeSet;

use kbf_types::{FarmTime, WorkerId};

use super::facts::{Facts, HostFacts, Platform, PlatformClass};
use super::gate::{GateFail, GateMode, storage_gate};
use super::node_step::HostPhase;
use super::policy::Policy;
use super::slot::SlotClass;

/// The picker's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Start these hosts, best first, never empty: record `Selected` and claim a place for
    /// each ([`super::Transition::select`]).
    Hosts(Vec<Picked>),
    /// Start none.
    Wait {
        /// Why.
        why: WaitReason,
    },
}

/// One host to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked {
    /// The host.
    pub host: WorkerId,
    /// Whether it was forced (D3): it was not idle when it was cordoned.
    pub force: bool,
}

/// Why nothing is started now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitReason {
    /// Every place a candidate could take is held, and this host holds one that keeps it.
    SlotHeld(WorkerId),
    /// As [`WaitReason::SlotHeld`], and this host is in a step by its record (the slot does
    /// not show it yet).
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
    /// Every place of its kind is held (one for storage hosts, `max_hosts_out` for the rest).
    NoRoom,
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

/// The hosts that are out: held by the slot or in a step by their record, each once.
struct Out<'a> {
    storage: BTreeSet<&'a WorkerId>,
    hosts: BTreeSet<&'a WorkerId>,
}

impl<'a> Out<'a> {
    fn of(facts: &'a Facts) -> Self {
        let mut out = Out {
            storage: BTreeSet::new(),
            hosts: BTreeSet::new(),
        };
        for (class, holder) in facts.slot.holders() {
            out.set(class).insert(&holder.host);
        }
        for host in facts.hosts.iter().filter(|h| h.record.phase.in_step()) {
            out.set(host.slot_class()).insert(&host.id);
        }
        out
    }

    fn set(&mut self, class: SlotClass) -> &mut BTreeSet<&'a WorkerId> {
        match class {
            SlotClass::Storage => &mut self.storage,
            SlotClass::Host => &mut self.hosts,
        }
    }

    /// How many more hosts of `class` may start, with none chosen yet.
    fn room(&self, class: SlotClass, policy: &Policy) -> usize {
        match class {
            SlotClass::Storage => 1usize.saturating_sub(self.storage.len()),
            SlotClass::Host => policy.max_hosts_out.saturating_sub(self.hosts.len()),
        }
    }

    /// The wait that says which host keeps the places of `class` full.
    fn blocker(&self, facts: &Facts, class: SlotClass) -> WaitReason {
        let out = match class {
            SlotClass::Storage => &self.storage,
            SlotClass::Host => &self.hosts,
        };
        match out.first() {
            Some(host) if facts.slot.holds(host) => WaitReason::SlotHeld((*host).clone()),
            Some(host) => WaitReason::HostInStep((*host).clone()),
            None => WaitReason::NoneEligible(Vec::new()),
        }
    }
}

/// Chooses the next hosts to update, or says why none.
#[must_use]
pub fn pick(facts: &Facts, policy: &Policy, now: FarmTime) -> Pick {
    let wait = |why| Pick::Wait { why };
    let held = |h: &&HostFacts| matches!(h.record.phase, HostPhase::Held { .. });
    if let Some(h) = facts.hosts.iter().find(held) {
        return wait(WaitReason::RunHeld(h.id.clone()));
    }
    let out = Out::of(facts);
    if out.room(SlotClass::Storage, policy) == 0 && out.room(SlotClass::Host, policy) == 0 {
        return wait(out.blocker(facts, SlotClass::Storage));
    }
    let mut candidates = Vec::new();
    let mut unknown = Vec::new();
    for host in facts
        .hosts
        .iter()
        .filter(|h| h.record.is_open_in(facts.generation))
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
    let (open, blocked): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|h| out.room(h.slot_class(), policy) > 0);
    let mut skipped: Vec<(WorkerId, Skip)> = blocked
        .iter()
        .map(|h| (h.id.clone(), Skip::NoRoom))
        .collect();
    if open.is_empty() {
        let class = blocked
            .first()
            .map_or(SlotClass::Storage, |h| h.slot_class());
        return wait(out.blocker(facts, class));
    }
    let mut eligible = Vec::new();
    for host in open {
        match verdict(facts, host, policy, now) {
            Ok(e) => eligible.push(e),
            Err(skip) => skipped.push((host.id.clone(), skip)),
        }
    }
    let mut idle: Vec<&Eligible<'_>> = eligible.iter().filter(|e| e.idle).collect();
    idle.sort_by_key(|e| e.rank());
    if !idle.is_empty() {
        let chosen = together(facts, policy, &out, &idle);
        return Pick::Hosts(
            chosen
                .into_iter()
                .map(|e| Picked {
                    host: e.host.id.clone(),
                    force: false,
                })
                .collect(),
        );
    }
    let until = facts.wait_since.saturating_add(policy.idle_wait);
    if !eligible.is_empty() && now < until {
        return wait(WaitReason::IdleWait { until });
    }
    // D3: nobody idle for `idle_wait`. Linux first; a Mac only when its own wait is over.
    // A deferred host was forced once already.
    let mac_ok = mac_may_be_forced(facts, policy, now);
    let forced = |macs: bool| {
        let deferred = |e: &&Eligible<'_>| e.host.record.was_deferred_in(facts.generation);
        eligible
            .iter()
            .filter(|e| e.host.is_mac() == macs && !deferred(e))
            .min_by_key(|e| (e.weight, e.rank()))
    };
    let chosen = forced(false).or_else(|| if mac_ok { forced(true) } else { None });
    match chosen {
        Some(e) => Pick::Hosts(vec![Picked {
            host: e.host.id.clone(),
            force: true,
        }]),
        None => {
            // Only deferred hosts are left: eligible, not idle, and not to be forced again.
            skipped.extend(eligible.iter().map(|e| (e.host.id.clone(), Skip::Deferred)));
            wait(WaitReason::NoneEligible(skipped))
        }
    }
}

/// The idle hosts, best first, that can start in the same look: a place of their kind free
/// after the ones chosen before them, and the floor and the last Mac of a class still met with
/// those gone. The first one always passes: `verdict` judged it alone.
fn together<'e, 'h>(
    facts: &Facts,
    policy: &Policy,
    out: &Out<'_>,
    ranked: &[&'e Eligible<'h>],
) -> Vec<&'e Eligible<'h>> {
    let mut chosen: Vec<&'e Eligible<'h>> = Vec::new();
    for &e in ranked {
        let class = e.host.slot_class();
        let taken = chosen
            .iter()
            .filter(|c| c.host.slot_class() == class)
            .count();
        if out.room(class, policy) <= taken {
            continue;
        }
        let gone: Vec<&WorkerId> = chosen.iter().map(|c| &c.host.id).collect();
        let floor_ok = facts.serving_peers_without(e.host, &gone) >= policy.min_serving;
        let class_ok = match &e.host.platform {
            Platform::Mac(mac) => {
                !(is_last_of_class(facts, e.host, mac, &gone) && needs_class(facts, mac))
            }
            Platform::Linux => true,
        };
        if floor_ok && class_ok {
            chosen.push(e);
        }
    }
    chosen
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
        if is_last_of_class(facts, host, class, &[]) && needs_class(facts, class) {
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

/// Whether no other Mac of `class` is serving (connected, uncordoned, not in a step), with
/// the hosts `gone` counted out: taking `host` out leaves the class with no serving host.
fn is_last_of_class(
    facts: &Facts,
    host: &HostFacts,
    class: &PlatformClass,
    gone: &[&WorkerId],
) -> bool {
    !facts.hosts.iter().any(|other| {
        other.id != host.id
            && matches!(&other.platform, Platform::Mac(c) if c == class)
            && other.is_serving()
            && !gone.contains(&&other.id)
    })
}

/// Whether a queued operation needs `class`. Unknown needs count as needing it.
fn needs_class(facts: &Facts, class: &PlatformClass) -> bool {
    facts
        .queued_classes
        .as_ref()
        .is_none_or(|needed| needed.contains(class))
}
