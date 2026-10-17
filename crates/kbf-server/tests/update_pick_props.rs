//! A seeded property test of the picker (`docs/design/split-mode-updates.md` sections 2.6,
//! 2.7 and 2.10, #898) over random fleets whose facts lag.
//!
//! **What "lagging" is here.** The records and the slot are the controller's own writes,
//! so they are the truth about which host is in a step. Everything else is a reading:
//! a member row still `Active` and fresh for a node whose record has begun its update; a
//! placement that does not yet show a cordon; a heartbeat anywhere from now to well past
//! its time to live; a metric that is absent; a release that still reads the old one for a
//! host the record finished; a disconnected node that looks idle; a row written by an
//! earlier run. The picker is judged against the facts it was given, by an oracle that is
//! written out below and shares no code with it.
//!
//! Asserted for every world: at most one storage host out and `max_hosts_out` others (the
//! places the slot and the records show taken, and those the answer takes, together), never
//! a storage pick while G_S is false or unknown or its writes unreported, never a
//! disconnected or cordoned host, never a finished (in this run) or held one, never a host
//! that would leave fewer than `min_serving` of its platform with the other hosts of the
//! answer gone too, Macs never forced by default, a forced pick only after its wait, alone,
//! and only when nothing is idle, an idle host never passed over, a forced pick made once
//! the wait is over and a host could be forced, the last Mac of a class only when no queued
//! operation needs it (the other Macs of the answer gone too), and the same answer twice. A
//! guard at the end fails the test if the worlds did not exercise each branch.

#[path = "update_support/mod.rs"]
mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use kbf_map::{MemberId, MemberState};
use kbf_server::update::{
    DaemonLoad, Facts, Generation, HostFacts, HostKind, HostPhase, MemberFacts, Pick, Picked,
    Platform, PlatformClass, Policy, Release, SlotClass, pick,
};
use kbf_sim::{Chance, SimRng};
use support::{GENERATION, IDLE, NEW, NOW, OLD, busy, facts, linux, mac, member, storage, t};

const MIN: u64 = 60;
const SEEDS: u64 = 10_000;

fn pct(rng: &mut SimRng, p: u32) -> bool {
    rng.chance(Chance::percent(p))
}

fn random_member(rng: &mut SimRng, n: u64) -> MemberFacts {
    let mut m = member(n);
    if !pct(rng, 95) {
        m.state = match rng.below(4) {
            0 => MemberState::CatchingUp,
            1 => MemberState::Down,
            2 => MemberState::Joining,
            _ => MemberState::Draining,
        };
    }
    m.planned_down = pct(rng, 2);
    m.heard_at = match rng.below(50) {
        0 => None,
        1 | 2 => Some(t(NOW - 16 - rng.below(60))),
        _ => Some(t(NOW - rng.below(16))),
    };
    m.repair_debt = match rng.below(50) {
        0 | 1 => None,
        2 | 3 => Some(1 + rng.below(5)),
        _ => Some(0),
    };
    m
}

fn random_phase(rng: &mut SimRng) -> HostPhase {
    match rng.below(40) {
        0 => HostPhase::Deferred,
        1..=5 => HostPhase::Done,
        6 => HostPhase::Held {
            reason: kbf_server::update::HoldReason::Reverted,
        },
        7 | 8 => HostPhase::Cordoned,
        _ => HostPhase::Pending,
    }
}

fn random_host(rng: &mut SimRng, name: &str, kind: u64, link: u64) -> HostFacts {
    let mut h = match kind {
        0 | 1 => storage(name, link),
        2 | 3 => linux(name),
        _ => mac(name, ["arm64", "x86"][usize::from(pct(rng, 40))]),
    };
    if h.kind == HostKind::StorageAndDaemon && pct(rng, 8) {
        h.member = None;
    }
    h.connected = pct(rng, 92);
    h.cordoned = match rng.below(20) {
        0 => None,
        1 | 2 => Some(true),
        _ => Some(false),
    };
    h.load = match rng.below(20) {
        0 | 1 => None,
        2..=11 => Some(busy(1 + u32::try_from(rng.below(4)).expect("small"))),
        12 => Some(DaemonLoad {
            whole_machine_hold: true,
            ..IDLE
        }),
        _ => Some(IDLE),
    };
    h.open_writes = match rng.below(20) {
        0 => None,
        1..=3 => Some(1 + rng.below(3)),
        _ => Some(0),
    };
    h.routed_writes = match rng.below(20) {
        0 => None,
        1..=2 => Some(1 + rng.below(3)),
        _ => Some(0),
    };
    h.release = match rng.below(20) {
        0 => None,
        1..=5 => Some(Release::new(NEW)),
        _ => Some(Release::new(OLD)),
    };
    h.record.phase = random_phase(rng);
    // The row was written by an earlier run, this one, or (a controller bug) a later one.
    h.record.generation = Generation(GENERATION.0 - 1 + rng.below(3));
    h
}

fn random_world(rng: &mut SimRng) -> (Facts, Policy) {
    let count = 1 + rng.below(6);
    let mut hosts = Vec::new();
    let mut members = Vec::new();
    for i in 0..count {
        let kind = rng.below(6);
        let member_id = members.len() as u64 + 1;
        let host = random_host(rng, &format!("h{i}"), kind, member_id);
        if host.is_storage() {
            members.push(random_member(rng, member_id));
        }
        hosts.push(host);
    }
    // Members no host of the run is linked to (a storage node of another pool).
    for _ in 0..rng.below(3) {
        let n = members.len() as u64 + 1;
        members.push(random_member(rng, n));
    }
    let mut f = facts(hosts, members);
    // This run, or the one after it (a rollback), so that earlier rows are history.
    f.generation = Generation(GENERATION.0 + rng.below(2));
    // The lag: a host the record says is in a step, whose live state says it is serving;
    // its place is held or not yet shown.
    if pct(rng, 20) {
        let at = usize::try_from(rng.below(count)).expect("an index");
        f.hosts[at].record.phase = HostPhase::WaitingLeases;
        if pct(rng, 50) {
            support::hold(&mut f, &format!("h{at}"));
        }
        // A second host in a step, so that the places of one kind can fill.
        if pct(rng, 30) {
            let other = usize::try_from(rng.below(count)).expect("an index");
            if other != at {
                f.hosts[other].record.phase = HostPhase::Gated;
            }
        }
    } else if pct(rng, 8) {
        // A place held by a host that is not in the run, of either kind.
        let class = [SlotClass::Storage, SlotClass::Host][usize::from(pct(rng, 50))];
        support::hold_as(&mut f, "somebody", class);
    }
    f.queued_classes = match rng.below(10) {
        0 => None,
        _ => {
            let mut set = BTreeSet::new();
            for class in ["arm64", "x86"] {
                if pct(rng, 40) {
                    set.insert(PlatformClass::new(class));
                }
            }
            Some(set)
        }
    };
    f.wait_since = t(NOW - rng.below(10 * MIN));
    let mut policy = support::policy();
    if pct(rng, 30) {
        policy.mac_force_after = Some(Duration::from_secs(MIN * rng.between(1, 20)));
    }
    if pct(rng, 25) {
        policy.min_serving = 1 + usize::try_from(rng.below(2)).expect("small");
    }
    if pct(rng, 50) {
        policy.max_hosts_out = 1 + usize::try_from(rng.below(3)).expect("small");
    }
    (f, policy)
}

/// G_S, written out again: every member the facts list other than the host's own is
/// `Active`, not planned down, heard within the time to live, with a repair debt reported
/// as zero; the host's own member is the same and at least one other exists.
fn gate_open(f: &Facts, host: &HostFacts, policy: &Policy) -> bool {
    let Some(own) = host.member else {
        return false;
    };
    let sound = |m: &MemberFacts| {
        let heard = m
            .heard_at
            .is_some_and(|h| t(NOW).saturating_duration_since(h) <= policy.ttl);
        m.state == MemberState::Active && !m.planned_down && heard && m.repair_debt == Some(0)
    };
    let Some(me) = f.members.iter().find(|m| m.id == MemberId(own.0)) else {
        return false;
    };
    let others: Vec<_> = f.members.iter().filter(|m| m.id != me.id).collect();
    sound(me) && !others.is_empty() && others.into_iter().all(sound)
}

/// Whether the run may start the host again, written out again: not begun, deferred, or
/// finished by an earlier run.
fn open(f: &Facts, h: &HostFacts) -> bool {
    match h.record.phase {
        HostPhase::Pending | HostPhase::Deferred => true,
        HostPhase::Done => h.record.generation < f.generation,
        _ => false,
    }
}

/// Hosts of the same platform, other than `h` and the `gone` ones, connected and shown
/// uncordoned, and not in a step by their record.
fn serving_others(f: &Facts, h: &HostFacts, gone: &[&str]) -> usize {
    let serves = |o: &&HostFacts| {
        o.id != h.id
            && o.platform == h.platform
            && o.connected
            && o.cordoned == Some(false)
            && !o.record.phase.in_step()
            && !gone.contains(&o.id.as_str())
    };
    f.hosts.iter().filter(serves).count()
}

/// The places taken: storage hosts and other hosts that are in a step by their record or
/// hold a place in the slot, each counted once.
fn out_counts(f: &Facts) -> (usize, usize) {
    let mut storage = BTreeSet::new();
    let mut others = BTreeSet::new();
    for h in f.hosts.iter().filter(|h| h.record.phase.in_step()) {
        if h.is_storage() {
            storage.insert(h.id.as_str().to_owned());
        } else {
            others.insert(h.id.as_str().to_owned());
        }
    }
    if let Some(holder) = &f.slot.storage {
        storage.insert(holder.host.as_str().to_owned());
    }
    for holder in &f.slot.hosts {
        others.insert(holder.host.as_str().to_owned());
    }
    (storage.len(), others.len())
}

/// Whether a Linux or storage host may be picked now, and whether it is idle: connected,
/// shown uncordoned, with a known load and a known release other than the target, leaving
/// the floor, and for a storage host with G_S open and both write counts reported.
fn eligible_linux(f: &Facts, h: &HostFacts, policy: &Policy) -> Option<bool> {
    let shape = match h.kind {
        HostKind::DaemonOnly => h.platform == Platform::Linux,
        HostKind::StorageAndDaemon => {
            gate_open(f, h, policy) && h.open_writes.is_some() && h.routed_writes.is_some()
        }
    };
    let ok = shape
        && open(f, h)
        && h.release.as_ref().is_some_and(|r| *r != f.target)
        && h.connected
        && h.cordoned == Some(false)
        && serving_others(f, h, &[]) >= policy.min_serving;
    let load = h.load.filter(|_| ok)?;
    let quiet = h.open_writes.unwrap_or(1) == 0 && h.routed_writes.unwrap_or(1) == 0;
    let writes_ok = h.kind == HostKind::DaemonOnly || quiet;
    Some(load.is_idle() && writes_ok)
}

/// What the run saw, to check the worlds were not vacuous.
#[derive(Debug, Default)]
struct Seen {
    picks: u64,
    storage: u64,
    macs: u64,
    forced: u64,
    forced_macs: u64,
    waits_in_step: u64,
    idle_linux_checked: u64,
    forced_expected: u64,
    last_mac_checked: u64,
    gate_shut_storage_idle: u64,
    earlier_run_picks: u64,
    floor_picks: u64,
    busy_writes_storage_picks: u64,
    /// Answers that start more than one host.
    multi: u64,
}

fn check(f: &Facts, policy: &Policy, seen: &mut Seen, why: &str) {
    let now = t(NOW);
    let answer = pick(f, policy, now);
    assert_eq!(
        answer,
        pick(f, policy, now),
        "{why}: not the same answer twice"
    );

    let held = f
        .hosts
        .iter()
        .any(|h| matches!(h.record.phase, HostPhase::Held { .. }));
    let (out_storage, out_others) = out_counts(f);
    let room_storage = 1usize.saturating_sub(out_storage);
    let room_others = policy.max_hosts_out.saturating_sub(out_others);
    if held || (room_storage == 0 && room_others == 0) {
        seen.waits_in_step += 1;
        assert!(
            matches!(answer, Pick::Wait { .. }),
            "{why}: a host was picked with the run held or every place taken: {answer:?}"
        );
        return;
    }
    let room = |h: &HostFacts| {
        if h.is_storage() {
            room_storage
        } else {
            room_others
        }
    };

    let elapsed = now.saturating_duration_since(f.wait_since);
    // An idle host that can be picked, a Linux daemon-only one or a storage one with G_S
    // open, with a place free for its kind, is never passed over for a forced pick, nor
    // left waiting: the answer is idle hosts, none forced.
    let idle_ok = f
        .hosts
        .iter()
        .any(|h| room(h) > 0 && eligible_linux(f, h, policy) == Some(true));
    if idle_ok {
        seen.idle_linux_checked += 1;
        assert!(
            matches!(&answer, Pick::Hosts(p) if p.iter().all(|x| !x.force)),
            "{why}: an idle host was passed over: {answer:?}"
        );
    }
    // Once the idle wait is over and a host could be forced (a busy one that is not a
    // Mac and was not deferred in this run, with a place free), some host is picked.
    let forceable = f.hosts.iter().any(|h| {
        let deferred = h.record.phase == HostPhase::Deferred && h.record.generation >= f.generation;
        room(h) > 0 && eligible_linux(f, h, policy) == Some(false) && !deferred
    });
    if !idle_ok && forceable && elapsed >= policy.idle_wait {
        seen.forced_expected += 1;
        assert!(
            matches!(answer, Pick::Hosts(_)),
            "{why}: nothing was picked though a host could be forced: {answer:?}"
        );
    }

    let Pick::Hosts(picked) = answer else {
        for h in f
            .hosts
            .iter()
            .filter(|h| h.is_storage() && !gate_open(f, h, policy))
        {
            let idle = h.load.is_some_and(|l| l.is_idle()) && h.connected;
            seen.gate_shut_storage_idle += u64::from(idle);
        }
        return;
    };
    // The answer as a set: no host twice, the places of each kind not overrun, and a
    // forced host alone.
    let names: Vec<&str> = picked.iter().map(|p| p.host.as_str()).collect();
    let distinct: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        names.len(),
        "{why}: a host twice: {names:?}"
    );
    let in_storage = |p: &Picked| f.host(&p.host).is_some_and(HostFacts::is_storage);
    let storage_n = picked.iter().filter(|p| in_storage(p)).count();
    assert!(
        storage_n <= room_storage,
        "{why}: {storage_n} storage hosts with {out_storage} out: {names:?}"
    );
    assert!(
        picked.len() - storage_n <= room_others,
        "{why}: {names:?} with {out_others} others out and {} places",
        policy.max_hosts_out
    );
    assert!(
        picked.len() == 1 || picked.iter().all(|p| !p.force),
        "{why}: a forced host with others: {picked:?}"
    );
    seen.multi += u64::from(picked.len() > 1);
    for p in &picked {
        check_host(f, policy, seen, why, &names, &p.host, p.force, elapsed);
    }
}

/// The rules for one host of the answer `names`.
#[allow(clippy::too_many_arguments)]
fn check_host(
    f: &Facts,
    policy: &Policy,
    seen: &mut Seen,
    why: &str,
    names: &[&str],
    host: &kbf_types::WorkerId,
    force: bool,
    elapsed: Duration,
) {
    seen.picks += 1;
    let h = f.host(host).expect("a picked host is in the facts");
    assert!(h.connected, "{why}: {host} is disconnected");
    assert_eq!(
        h.cordoned,
        Some(false),
        "{why}: {host} is cordoned or unplaced"
    );
    assert!(open(f, h), "{why}: {host} is {:?}", h.record.phase);
    seen.earlier_run_picks += u64::from(h.record.phase == HostPhase::Done);
    let release = h.release.as_ref();
    assert!(
        release.is_some_and(|r| *r != f.target),
        "{why}: {host} runs {release:?}"
    );
    // The other hosts of the answer are not yet shown cordoned, but they are going.
    let serving = serving_others(f, h, names);
    assert!(
        serving >= policy.min_serving,
        "{why}: {host} leaves {serving} serving, under the floor {}",
        policy.min_serving
    );
    seen.floor_picks += u64::from(policy.min_serving > 0);
    let load = h.load.expect("a picked host has a known load");
    // Idle is the daemon half's, and for a storage node no write in flight to it.
    let idle = load.is_idle()
        && (!h.is_storage() || (h.open_writes == Some(0) && h.routed_writes == Some(0)));
    if force {
        seen.forced += 1;
        assert!(!idle, "{why}: forced an idle host");
        assert!(
            elapsed >= policy.idle_wait,
            "{why}: forced after {elapsed:?}"
        );
        let deferred = h.record.phase == HostPhase::Deferred && h.record.generation >= f.generation;
        assert!(!deferred, "{why}: {host} was forced after it was deferred");
    } else {
        assert!(idle, "{why}: {host} is not idle and was not forced");
    }
    if h.is_storage() {
        seen.storage += 1;
        assert!(
            gate_open(f, h, policy),
            "{why}: {host} was picked with G_S shut or unknown"
        );
        assert!(
            h.open_writes.is_some() && h.routed_writes.is_some(),
            "{why}: {host} was picked with its writes unreported"
        );
        seen.busy_writes_storage_picks += u64::from(!idle);
    }
    if let Platform::Mac(class) = &h.platform {
        seen.macs += 1;
        match policy.mac_force_after {
            None => assert!(!force && idle, "{why}: a Mac was forced by default"),
            Some(after) if force => {
                seen.forced_macs += 1;
                assert!(elapsed >= after, "{why}: a Mac forced early");
            }
            Some(_) => {}
        }
        // Another Mac of the class that keeps serving: not in a step, not in the answer.
        let served = f.hosts.iter().any(|o| {
            o.id != h.id
                && o.platform == h.platform
                && o.connected
                && o.cordoned == Some(false)
                && !o.record.phase.in_step()
                && !names.contains(&o.id.as_str())
        });
        let needed = f
            .queued_classes
            .as_ref()
            .is_none_or(|set| set.contains(class));
        seen.last_mac_checked += 1;
        assert!(
            served || !needed,
            "{why}: the last Mac of {class:?} was picked"
        );
    }
}

/// Catches, over 10,000 seeded worlds with lagging facts, each of: a second storage host
/// started while one is in a step, more daemon-only hosts and Macs than `max_hosts_out`
/// (the record, not the member row, says what is out; a storage host out takes none of their
/// places), a host started twice in an answer, a storage host picked with G_S shut, any part
/// of it unknown, or its writes unreported, a disconnected, cordoned, finished or held host
/// picked, a host picked that leaves fewer than the floor serving (the others of the same
/// answer counted as gone), a Mac forced by default or before its own wait, the last Mac of
/// a class updated under a queue that needs it (or two Macs of one class together), a forced
/// pick before `idle_wait` or of an idle host or of a host already deferred or with others,
/// an idle host passed over, no pick once the wait is over and a host could be forced, a
/// host finished by an earlier run not offered again to a later one, and an answer that
/// changes when asked twice.
#[test]
fn the_picker_keeps_its_rules_over_random_fleets_with_lagging_facts() {
    let mut seen = Seen::default();
    for seed in 0..SEEDS {
        let mut rng = SimRng::from_seed(seed);
        let (f, policy) = random_world(&mut rng);
        check(&f, &policy, &mut seen, &format!("seed {seed}"));
    }
    // The worlds reached the branches the rules are about.
    let reached = seen.picks > 500
        && seen.storage > 100
        && seen.macs > 50
        && seen.forced > 100
        && seen.forced_macs > 0
        && seen.waits_in_step > 500
        && seen.idle_linux_checked > 150
        && seen.forced_expected > 50
        && seen.last_mac_checked > 50
        && seen.gate_shut_storage_idle > 100
        && seen.earlier_run_picks > 20
        && seen.floor_picks > 50
        && seen.busy_writes_storage_picks > 10
        && seen.multi > 100;
    assert!(reached, "the worlds did not reach every rule: {seen:?}");
}
