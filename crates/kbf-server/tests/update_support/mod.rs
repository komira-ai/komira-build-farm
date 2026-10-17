//! Builders for the node-step and picker tests: hosts and members with healthy defaults,
//! and a test changes the one field it is about.
//!
//! Every test reads the world at [`NOW`]. Time passes by moving the other end: `since`
//! earlier for a step that has waited, `wait_since` earlier for a picker that has waited,
//! `down_since` earlier for a mark that has aged. So members never need to be re-heard.

#![allow(dead_code)]

use std::collections::BTreeSet;

use kbf_map::{MemberId, MemberState};
use kbf_server::update::{
    Alert, Command, DaemonLoad, Facts, GiveBack, HelperReport, HelperStatus, HostFacts, HostKind,
    HostPhase, HostRecord, MemberFacts, OrderId, Platform, PlatformClass, Policy, Release, Serial,
    SlotOp, Transition, step,
};
use kbf_types::{FarmTime, WorkerId};

/// The release every host runs at the start.
pub const OLD: &str = "v1";
/// The release the run drives to.
pub const NEW: &str = "v2";
/// The time of the look, in seconds. Large, so a test can say a step began hours before.
pub const NOW: u64 = 1_000_000;
/// The serial of the run.
pub const SERIAL: Serial = Serial(7);
/// The activation order a host's row holds (the one the host's current update was given).
pub const ORDER: OrderId = OrderId(41);
/// The id the fleet will give the next order.
pub const NEXT_ORDER: OrderId = OrderId(42);
/// A member's boot counter, and the one a host's row recorded at the mark.
pub const BOOT: u64 = 10;

/// A load of nothing.
pub const IDLE: DaemonLoad = DaemonLoad {
    leases: 0,
    undelivered: 0,
    whole_machine_hold: false,
};

/// The farm time `secs` seconds after the epoch.
pub fn t(secs: u64) -> FarmTime {
    FarmTime::from_millis(secs * 1000)
}

/// The worker called `name`.
pub fn id(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// A load of `leases` leases.
pub const fn busy(leases: u32) -> DaemonLoad {
    DaemonLoad {
        leases,
        undelivered: 0,
        whole_machine_hold: false,
    }
}

/// What a helper reports about the order [`ORDER`], the one a host's row holds.
pub const fn report(status: HelperStatus) -> Option<HelperReport> {
    Some(HelperReport {
        order: ORDER,
        status,
    })
}

/// What a helper reports about some other order.
pub const fn report_of(order: OrderId, status: HelperStatus) -> Option<HelperReport> {
    Some(HelperReport { order, status })
}

/// The activate order a move issues when it enters `Activating`: the fleet's next id.
pub const fn activate() -> Command {
    Command::Activate { order: NEXT_ORDER }
}

/// The confirm order for the activation a host's row holds.
pub const fn confirm() -> Command {
    Command::Confirm { order: ORDER }
}

fn host(name: &str, kind: HostKind, platform: Platform, member: Option<u64>) -> HostFacts {
    HostFacts {
        id: id(name),
        kind,
        platform,
        member: member.map(MemberId),
        release: Some(Release::new(OLD)),
        connected: true,
        cordoned: Some(false),
        load: Some(IDLE),
        open_writes: Some(0),
        routed_writes: Some(0),
        helper: None,
        work: None,
        record: HostRecord {
            phase: HostPhase::Pending,
            since: t(0),
            serial: SERIAL,
            order: ORDER,
            boot_before: Some(BOOT),
        },
    }
}

/// A Linux daemon-only host at [`OLD`], connected, idle, not cordoned.
pub fn linux(name: &str) -> HostFacts {
    host(name, HostKind::DaemonOnly, Platform::Linux, None)
}

/// A storage-and-daemon host whose storage half is member `member`.
pub fn storage(name: &str, member: u64) -> HostFacts {
    host(
        name,
        HostKind::StorageAndDaemon,
        Platform::Linux,
        Some(member),
    )
}

/// A Mac of class `class`.
pub fn mac(name: &str, class: &str) -> HostFacts {
    let platform = Platform::Mac(PlatformClass::new(class));
    host(name, HostKind::DaemonOnly, platform, None)
}

/// Member `n`: `Active`, heard at [`NOW`], no debt, at boot [`BOOT`].
pub fn member(n: u64) -> MemberFacts {
    MemberFacts {
        id: MemberId(n),
        state: MemberState::Active,
        planned_down: false,
        heard_at: Some(t(NOW)),
        repair_debt: Some(0),
        down_since: None,
        boot: Some(BOOT),
    }
}

/// Members `1..=n`, all healthy.
pub fn members(n: u64) -> Vec<MemberFacts> {
    (1..=n).map(member).collect()
}

/// A run to [`NEW`] over `hosts` and `members`: no slot, an empty queue that needs no Mac
/// class, and a picker that began waiting at [`NOW`].
pub fn facts(hosts: Vec<HostFacts>, members: Vec<MemberFacts>) -> Facts {
    Facts {
        target: Release::new(NEW),
        serial: SERIAL,
        next_order: NEXT_ORDER,
        hosts,
        members,
        slot: None,
        queued_classes: Some(BTreeSet::new()),
        wait_since: t(NOW),
    }
}

/// The member `n` of `facts`, to change.
pub fn member_mut(facts: &mut Facts, n: u64) -> &mut MemberFacts {
    facts
        .members
        .iter_mut()
        .find(|m| m.id == MemberId(n))
        .expect("the member is in the facts")
}

/// The host `name` of `facts`, to change.
pub fn host_mut<'a>(facts: &'a mut Facts, name: &str) -> &'a mut HostFacts {
    facts
        .hosts
        .iter_mut()
        .find(|h| h.id == id(name))
        .expect("the host is in the facts")
}

/// The defaults of the design.
pub fn policy() -> Policy {
    Policy::default()
}

/// Three storage hosts `s1..s3` on members `1..3`, `s1` cordoned and holding the slot.
pub fn storage_fleet() -> Facts {
    let hosts = vec![storage("s1", 1), storage("s2", 2), storage("s3", 3)];
    let mut f = facts(hosts, members(3));
    f.slot = Some(id("s1"));
    host_mut(&mut f, "s1").cordoned = Some(true);
    f
}

/// Two Linux daemon-only hosts, `d1` cordoned and holding the slot, over three healthy
/// members that no host is linked to.
pub fn linux_fleet() -> Facts {
    let mut f = facts(vec![linux("d1"), linux("d2")], members(3));
    f.slot = Some(id("d1"));
    host_mut(&mut f, "d1").cordoned = Some(true);
    f
}

/// A world in which `s1` is in `phase` (a step from 2 to 9) and every condition lets it
/// stay: nothing it waits for has happened, and it is cordoned.
pub fn waiting_in(phase: &HostPhase) -> Facts {
    let mut f = storage_fleet();
    match phase {
        HostPhase::Cordoned => host_mut(&mut f, "s1").cordoned = Some(false),
        HostPhase::WaitingLeases => host_mut(&mut f, "s1").load = Some(busy(1)),
        HostPhase::PlannedDown => {
            member_mut(&mut f, 1).planned_down = true;
            host_mut(&mut f, "s1").open_writes = Some(2);
        }
        HostPhase::Gated => {
            member_mut(&mut f, 1).planned_down = true;
            member_mut(&mut f, 2).heard_at = Some(t(NOW - 12));
        }
        HostPhase::Activating => {
            member_mut(&mut f, 1).planned_down = true;
            member_mut(&mut f, 1).down_since = Some(t(NOW - 20));
        }
        HostPhase::RejoiningStorage => {
            member_mut(&mut f, 1).planned_down = true;
            member_mut(&mut f, 1).down_since = Some(t(NOW - 20));
            host_mut(&mut f, "s1").helper = report(HelperStatus::ActivatedUnconfirmed);
        }
        HostPhase::Repairing => {
            member_mut(&mut f, 1).boot = Some(BOOT + 1);
            member_mut(&mut f, 2).repair_debt = Some(4);
            host_mut(&mut f, "s1").helper = report(HelperStatus::ActivatedUnconfirmed);
            host_mut(&mut f, "s1").release = Some(Release::new(NEW));
        }
        HostPhase::Confirming => {
            host_mut(&mut f, "s1").helper = report(HelperStatus::ActivatedUnconfirmed);
        }
        other => panic!("{other:?} is not a waiting step"),
    }
    f
}

/// One call of [`step`] for host `name`, whose row is made to say it entered `phase`
/// `waited` seconds before [`NOW`], under `policy`.
pub fn step_for(
    facts: &Facts,
    name: &str,
    phase: &HostPhase,
    waited: u64,
    policy: &Policy,
) -> Transition {
    let mut f = facts.clone();
    let record = &mut host_mut(&mut f, name).record;
    record.phase = phase.clone();
    record.since = t(NOW - waited);
    let obs = f.observe(&id(name)).expect("the host is in the facts");
    step(&obs, policy, t(NOW))
}

/// [`step_for`] under the default policy.
pub fn run(facts: &Facts, name: &str, phase: &HostPhase, waited: u64) -> Transition {
    step_for(facts, name, phase, waited, &policy())
}

/// [`step`] for host `name` as its row stands, under the default policy, at [`NOW`].
pub fn run_row(facts: &Facts, name: &str) -> Transition {
    let obs = facts.observe(&id(name)).expect("the host is in the facts");
    step(&obs, &policy(), t(NOW))
}

/// Records a move in the row of `name` the way the controller does: the phase, the time it
/// was entered, and the order and boot counter the move names (counting the order used).
pub fn record_move(facts: &mut Facts, name: &str, moved: &Transition, now: FarmTime) {
    let host = host_mut(facts, name);
    if moved.next != host.record.phase {
        host.record.since = now;
    }
    host.record.phase = moved.next.clone();
    if let Some(boot) = moved.boot {
        host.record.boot_before = Some(boot);
    }
    if let Some(order) = moved.order {
        host.record.order = order;
        facts.next_order = order.next();
    }
}

/// The commands of giving a host back: the mark undone if it was written, uncordon, the
/// slot freed, the alert.
pub fn gave_back(undo_mark: bool, why: GiveBack) -> Vec<Command> {
    let mut commands = Vec::new();
    if undo_mark {
        commands.push(Command::Undown);
    }
    commands.push(Command::Uncordon);
    commands.push(Command::CasSlot(SlotOp::Release));
    commands.push(Command::Alert(Alert::GaveBack(why)));
    commands
}
