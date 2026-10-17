//! The capacity floor (`docs/design/split-mode-updates.md` section 2.8, #898), the Mac lease
//! limit (section 2.10), and a seeded property test of the node step over random
//! observations: which commands a phase may issue, which phases it may reach, that an
//! activate order only goes to a cordoned, marked, quiet host, that orders and boot
//! counters are named only by the moves that set them, and that a Mac is deferred only for
//! leases that outlast its wait.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::collections::BTreeMap;

use kbf_map::MemberState;
use kbf_server::update::{
    Alert, Command, DaemonLoad, Facts, GiveBack, HelperStatus, HoldReason, HostFacts, HostPhase,
    MemberFacts, OrderId, Policy, Release, SlotOp, Work,
};
use kbf_sim::{Chance, SimRng};
use support::{
    BOOT, IDLE, NEW, NOW, OLD, ORDER, busy, facts, gave_back, host_mut, id, linux, mac, members,
    report_of, run, step_for, storage, storage_fleet, t,
};

const MIN: u64 = 60;

// The capacity floor.

/// Catches: the capacity floor (`min_serving`) re-read after the cordon, which would
/// uncordon a draining host the moment a peer dropped out and undo the wait the lease limit
/// bounds. The floor is "a node is cordoned only if the rest meet it" (section 2.8): it is
/// checked at the claim, and a host already cordoned goes on to the end of its lease wait
/// and no further, whatever the other hosts do.
#[test]
fn the_floor_is_read_at_the_claim_and_not_again_during_the_lease_wait() {
    let floor = Policy {
        min_serving: 5,
        ..support::policy()
    };
    let phase = HostPhase::WaitingLeases;
    let f = support::waiting_in(&phase);
    let waiting = step_for(&f, "s1", &phase, 14 * MIN, &floor);
    assert_eq!(waiting.next, phase);
    assert_eq!(waiting.commands, Vec::new());

    // The lease limit still ends the wait, as before.
    let at = step_for(&f, "s1", &phase, 15 * MIN, &floor);
    assert_eq!(at.next, HostPhase::Pending);
    assert_eq!(at.commands, gave_back(false, GiveBack::LeasesNotEnded));
}

// Held.

/// Catches: a hold that lets the run go on or the node move: a held host produces nothing
/// by itself, whatever it sees, except the re-order of a cordon it keeps. The hold keeps
/// its reason; nothing is minted or recorded.
#[test]
fn a_held_host_stays_held_whatever_it_sees() {
    let reason = HoldReason::RepairWait;
    let phase = HostPhase::Held { reason };
    let mut f = storage_fleet();
    host_mut(&mut f, "s1").helper = report_of(ORDER, HelperStatus::Confirmed);
    host_mut(&mut f, "s1").work = Some(Work::Leased);
    host_mut(&mut f, "s1").release = Some(Release::new(NEW));
    let moved = run(&f, "s1", &phase, 24 * 60 * MIN);
    assert_eq!(moved.next, phase);
    assert_eq!(moved.commands, Vec::new());
    assert_eq!((moved.order, moved.boot), (None, None));
}

// Macs.

/// Catches: the Linux lease limit (15 minutes) applied to a Mac, whose leases may last an
/// hour; and a Mac given back as `Pending`: a busy Mac is `Deferred` (the run goes on).
#[test]
fn a_mac_waits_60_minutes_for_its_leases_and_is_then_deferred() {
    let mut f = facts(vec![mac("m1", "arm64"), mac("m2", "arm64")], members(3));
    f.slot = Some(id("m1"));
    host_mut(&mut f, "m1").cordoned = Some(true);
    host_mut(&mut f, "m1").load = Some(busy(1));
    let phase = HostPhase::WaitingLeases;
    let at_linux_limit = run(&f, "m1", &phase, 15 * MIN);
    assert_eq!(at_linux_limit.next, phase);
    let before = run(&f, "m1", &phase, 60 * MIN - 1);
    assert_eq!(before.next, phase);
    let at = run(&f, "m1", &phase, 60 * MIN);
    assert_eq!(at.next, HostPhase::Deferred);
    assert_eq!(at.commands, gave_back(false, GiveBack::LeasesNotEnded));
}

// The node step over random observations.

fn pct(rng: &mut SimRng, p: u32) -> bool {
    rng.chance(Chance::percent(p))
}

/// Each field of a world is drawn from the chaotic distribution with probability `noise`
/// percent, and is the healthy value otherwise: noise 100 explores everything, a low noise
/// reaches the long paths (a host that goes all the way through a step).
fn random_member(rng: &mut SimRng, n: u64, noise: u32) -> MemberFacts {
    let mut m = support::member(n);
    if pct(rng, noise) {
        m.state = match rng.below(10) {
            0 => MemberState::CatchingUp,
            1 => MemberState::Down,
            2 => MemberState::Joining,
            3 => MemberState::Draining,
            _ => MemberState::Active,
        };
    }
    m.planned_down = if pct(rng, noise) {
        pct(rng, 40)
    } else {
        n == 1 && pct(rng, 50)
    };
    if pct(rng, noise) {
        m.heard_at = match rng.below(10) {
            0 => None,
            1 | 2 => Some(t(NOW - 16 - rng.below(100))),
            _ => Some(t(NOW - rng.below(15))),
        };
    }
    if pct(rng, noise) {
        m.repair_debt = match rng.below(10) {
            0 | 1 => None,
            2 | 3 => Some(1 + rng.below(5)),
            _ => Some(0),
        };
    }
    m.down_since = pct(rng, 30).then(|| t(NOW - rng.below(400)));
    m.boot = match rng.below(10) {
        0 => None,
        1..=5 => Some(BOOT + 1),
        _ => Some(BOOT),
    };
    m
}

/// One of `items`, uniformly.
fn one_of<T: Clone>(rng: &mut SimRng, items: &[T]) -> T {
    let at = usize::try_from(rng.below(items.len() as u64)).expect("an index");
    items[at].clone()
}

/// What a host is, for a world.
#[derive(Clone, Copy)]
enum Shape {
    Storage,
    Linux,
    Mac,
}

fn random_host(rng: &mut SimRng, shape: Shape, noise: u32) -> HostFacts {
    let mut h = match shape {
        Shape::Storage => storage("h", 1),
        Shape::Linux => linux("h"),
        Shape::Mac => mac("h", "arm64"),
    };
    h.connected = !pct(rng, noise) || pct(rng, 90);
    h.cordoned = if pct(rng, noise) {
        one_of(rng, &[None, Some(false), Some(false), Some(true)])
    } else {
        Some(!pct(rng, 15))
    };
    if pct(rng, noise) {
        h.load = match rng.below(10) {
            0 => None,
            1..=3 => Some(busy(1 + u32::try_from(rng.below(3)).expect("small"))),
            4 => Some(DaemonLoad {
                undelivered: 1,
                ..IDLE
            }),
            _ => Some(IDLE),
        };
    }
    if pct(rng, noise) {
        h.open_writes = one_of(rng, &[Some(0), Some(0), Some(2), None]);
    }
    if pct(rng, noise) {
        h.routed_writes = one_of(rng, &[Some(0), Some(0), Some(1), None]);
    }
    // A status names the row's order, an older one or one the row does not know.
    let order = match rng.below(10) {
        0 => OrderId(ORDER.0 - 1),
        1 => OrderId(ORDER.0 + 1),
        _ => ORDER,
    };
    h.helper = if pct(rng, noise) {
        one_of(
            rng,
            &[
                None,
                report_of(order, HelperStatus::Reverted),
                report_of(order, HelperStatus::Refused),
            ],
        )
    } else {
        one_of(
            rng,
            &[
                None,
                report_of(order, HelperStatus::ActivatedUnconfirmed),
                report_of(order, HelperStatus::Confirmed),
            ],
        )
    };
    h.work = one_of(
        rng,
        &[
            None,
            Some(Work::Waiting),
            Some(Work::Leased),
            Some(Work::QueueEmpty),
        ],
    );
    h.release = if pct(rng, noise) {
        one_of(rng, &[None, Some(Release::new(OLD))])
    } else {
        Some(Release::new(if pct(rng, 50) { OLD } else { NEW }))
    };
    h.record.boot_before = one_of(rng, &[None, Some(BOOT), Some(BOOT), Some(BOOT + 1)]);
    h
}

fn random_facts(rng: &mut SimRng, shape: Shape, noise: u32) -> (Facts, Policy) {
    let host = random_host(rng, shape, noise);
    // A peer of the same platform, serving or not: what the capacity floor counts.
    let mut peer = host.clone();
    peer.id = id("peer");
    peer.member = None;
    peer.kind = kbf_server::update::HostKind::DaemonOnly;
    peer.connected = pct(rng, 70);
    peer.cordoned = one_of(rng, &[None, Some(false), Some(false), Some(true)]);
    let mut f = facts(vec![host, peer], Vec::new());
    let count = 2 + rng.below(3);
    for n in 1..=count {
        if n == 1 && pct(rng, noise) && pct(rng, 5) {
            continue;
        }
        f.members.push(random_member(rng, n, noise));
    }
    f.slot = if pct(rng, noise) {
        one_of(rng, &[None, Some(id("someone-else"))])
    } else {
        Some(id("h"))
    };
    let mut policy = support::policy();
    policy.min_serving = one_of(rng, &[0, 0, 1, 2]);
    (f, policy)
}

const PHASES: [HostPhase; 10] = [
    HostPhase::Selected,
    HostPhase::Cordoned,
    HostPhase::WaitingLeases,
    HostPhase::PlannedDown,
    HostPhase::Gated,
    HostPhase::Activating,
    HostPhase::RejoiningStorage,
    HostPhase::Repairing,
    HostPhase::Confirming,
    HostPhase::RejoiningWork,
];

/// The phases a phase may be followed by: the table's moves and nothing else.
fn may_follow(phase: &HostPhase, next: &HostPhase) -> bool {
    use HostPhase as P;
    if phase == next {
        return true;
    }
    match phase {
        P::Selected => matches!(next, P::Pending | P::Cordoned),
        P::Cordoned => matches!(next, P::WaitingLeases | P::Pending),
        P::WaitingLeases => matches!(
            next,
            P::PlannedDown | P::Activating | P::Pending | P::Deferred
        ),
        P::PlannedDown => matches!(next, P::Gated | P::Pending | P::Held { .. }),
        P::Gated => matches!(next, P::Activating | P::Pending | P::Held { .. }),
        P::Activating => matches!(next, P::RejoiningStorage | P::Confirming | P::Held { .. }),
        P::RejoiningStorage => matches!(next, P::Repairing | P::Held { .. }),
        P::Repairing => matches!(next, P::Confirming | P::Held { .. }),
        P::Confirming => matches!(next, P::RejoiningWork | P::Held { .. }),
        P::RejoiningWork => matches!(next, P::Done | P::Held { .. }),
        _ => false,
    }
}

/// Catches, over 6,000 random worlds (half chaotic, half mostly healthy so that hosts reach
/// the long paths; storage, Linux and Mac hosts) each read in all ten phases: an activate
/// order to a host that is not cordoned, not marked down, not quiet or not idle; an order id
/// or a boot counter that is not the move's own to set; an uncordon, an undo or a freed slot
/// from the activate order on (step 10's uncordon excepted); a move the table does not have
/// (a step skipped, a host sent back after its restart began, a hold from a step that has
/// none); a hold without its alert; a Mac deferred for anything but leases that outlast its
/// wait; a held host that produces anything but its cordon.
#[test]
fn the_node_step_obeys_its_table_over_random_observations() {
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    for seed in 0..1_000 {
        let mut rng = SimRng::from_seed(seed);
        let worlds = [
            (Shape::Storage, 100),
            (Shape::Linux, 100),
            (Shape::Mac, 100),
            (Shape::Storage, 12),
            (Shape::Linux, 12),
            (Shape::Mac, 12),
        ];
        for (shape, noise) in worlds {
            let (f, policy) = random_facts(&mut rng, shape, noise);
            let host = f.host(&id("h")).expect("the host");
            for phase in &PHASES {
                // Mostly a few seconds into the step (so waits and re-orders are reached),
                // sometimes up to two hours (so limits are).
                let waited = if pct(&mut rng, 40) {
                    rng.below(150)
                } else {
                    rng.below(2 * 60 * MIN)
                };
                let moved = step_for(&f, "h", phase, waited, &policy);
                let why = format!(
                    "seed {seed} {phase:?} -> {:?} {:?}",
                    moved.next, moved.commands
                );
                assert!(may_follow(phase, &moved.next), "{why}");
                if matches!(moved.next, HostPhase::Held { .. }) {
                    *seen.entry("held").or_default() += 1;
                }
                if moved.next == HostPhase::Pending {
                    *seen.entry("pending").or_default() += 1;
                }
                if moved.next == HostPhase::Deferred {
                    *seen.entry("deferred").or_default() += 1;
                    assert_eq!(*phase, HostPhase::WaitingLeases, "{why}");
                    assert!(host.is_mac(), "{why}");
                    let alert = Command::Alert(Alert::GaveBack(GiveBack::LeasesNotEnded));
                    assert!(moved.commands.contains(&alert), "{why}");
                    assert!(waited >= 60 * MIN, "{why}");
                }
                // The row fields a move sets are named by the move that sets them.
                let enters = |p: &HostPhase| moved.next == *p && phase != p;
                assert_eq!(
                    moved.order.is_some(),
                    enters(&HostPhase::Activating),
                    "{why}"
                );
                assert_eq!(
                    moved.boot.is_some(),
                    enters(&HostPhase::PlannedDown),
                    "{why}"
                );
                if let Some(boot) = moved.boot {
                    let member = f.members.iter().find(|m| Some(m.id) == host.member);
                    assert_eq!(member.and_then(|m| m.boot), Some(boot), "{why}");
                }
                for command in &moved.commands {
                    *seen.entry(kind(command)).or_default() += 1;
                    match command {
                        Command::Activate { order } => {
                            let marked = f.members.iter().any(|m| m.id.0 == 1 && m.planned_down);
                            let ok = match phase {
                                HostPhase::WaitingLeases => {
                                    !host.is_storage() && host.load.is_some_and(|l| l.is_idle())
                                }
                                HostPhase::Gated => marked && host.writes_quiet(),
                                _ => false,
                            };
                            assert!(ok, "{why}");
                            assert_eq!(host.cordoned, Some(true), "{why}");
                            assert!(host.connected, "{why}");
                            assert_eq!(*order, f.next_order, "{why}");
                            assert_eq!(moved.order, Some(*order), "{why}");
                        }
                        Command::MarkDown => {
                            assert!(host.is_storage(), "{why}");
                            let ok = matches!(
                                phase,
                                HostPhase::WaitingLeases
                                    | HostPhase::PlannedDown
                                    | HostPhase::Gated
                            );
                            assert!(ok, "{why}");
                        }
                        Command::Undown => {
                            assert!(phase.is_marked_down(), "{why}");
                        }
                        Command::Uncordon => {
                            let early = phase.is_early();
                            let step_ten = *phase == HostPhase::Confirming
                                && moved.next == HostPhase::RejoiningWork
                                || *phase == HostPhase::RejoiningWork;
                            assert!(early || step_ten, "{why}");
                        }
                        Command::Confirm { order } => {
                            let ok = matches!(phase, HostPhase::Activating | HostPhase::Repairing);
                            assert!(ok, "{why}");
                            assert_eq!(*order, ORDER, "{why}");
                        }
                        Command::CasSlot(SlotOp::Release) => {
                            let left = matches!(
                                moved.next,
                                HostPhase::Pending | HostPhase::Deferred | HostPhase::Done
                            );
                            assert!(left, "{why}");
                        }
                        Command::CasSlot(SlotOp::Claim) => {
                            assert_eq!(*phase, HostPhase::Selected, "{why}");
                        }
                        Command::Cordon | Command::Alert(_) => {}
                    }
                }
                if let HostPhase::Held { reason } = &moved.next {
                    let alert = Command::Alert(Alert::Held(*reason));
                    assert_eq!(moved.commands.last(), Some(&alert), "{why}");
                }
                // Back in `Pending` or `Deferred` from a step it freed the slot, unless
                // the slot was never its own.
                let freed = moved.commands.contains(&Command::CasSlot(SlotOp::Release));
                let lost = *phase == HostPhase::Selected && f.slot.as_ref() != Some(&id("h"));
                let left = matches!(moved.next, HostPhase::Pending | HostPhase::Deferred);
                assert!(!left || freed || lost, "{why}");
            }
            // A held host produces nothing but the cordon it keeps, and only when placement
            // shows it gone.
            for reason in REASONS {
                let phase = HostPhase::Held { reason };
                let moved = step_for(&f, "h", &phase, 1_000, &policy);
                assert_eq!(moved.next, phase, "seed {seed} {reason:?}");
                let lost = host.cordoned == Some(false) && reason.keeps_cordon();
                let want = if lost {
                    vec![Command::Cordon]
                } else {
                    Vec::new()
                };
                assert_eq!(moved.commands, want, "seed {seed} {reason:?}");
            }
        }
    }
    // The observations reached every command and the two ways out of a step.
    let wanted = [
        "claim",
        "cordon",
        "uncordon",
        "mark_down",
        "undown",
        "activate",
        "confirm",
        "release",
        "alert",
        "held",
        "pending",
        "deferred",
    ];
    for kind in wanted {
        let count = seen.get(kind).copied().unwrap_or(0);
        assert!(count >= 100, "{kind} was issued {count} times: {seen:?}");
    }
}

const REASONS: [HoldReason; 11] = [
    HoldReason::SecondMemberDown,
    HoldReason::RestartDeadline,
    HoldReason::MemberGone,
    HoldReason::ActivateStalled,
    HoldReason::HealthLimit,
    HoldReason::RebuildLimit,
    HoldReason::RepairWait,
    HoldReason::ConfirmStalled,
    HoldReason::Reverted,
    HoldReason::RevertRefused,
    HoldReason::StillCordoned,
];

/// The name of a command, for counting.
fn kind(command: &Command) -> &'static str {
    match command {
        Command::CasSlot(SlotOp::Claim) => "claim",
        Command::CasSlot(SlotOp::Release) => "release",
        Command::Cordon => "cordon",
        Command::Uncordon => "uncordon",
        Command::MarkDown => "mark_down",
        Command::Undown => "undown",
        Command::Activate { .. } => "activate",
        Command::Confirm { .. } => "confirm",
        Command::Alert(_) => "alert",
    }
}
