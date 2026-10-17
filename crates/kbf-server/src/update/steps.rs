//! The ten steps of the node step's table (`docs/design/split-mode-updates.md` section 2.6),
//! one function each: what leaves the step, and every cell of "If it fails". [`decide`] is
//! the dispatch [`super::step`] calls; the phases, the move and the rules they keep are
//! documented there.

use std::time::Duration;

use kbf_map::MemberState;
use kbf_types::FarmTime;

use super::command::{Alert, Command, GiveBack, HoldReason, SlotOp};
use super::facts::{HelperStatus, HostRecord, MemberFacts, Observations, SlotView, Work};
use super::gate::{GateFail, GateMode};
use super::node_step::{HostPhase, Transition};
use super::policy::Policy;

/// What one call reads, besides the observations.
struct Ctx<'a> {
    obs: &'a Observations<'a>,
    policy: &'a Policy,
    now: FarmTime,
    /// How long the host has been in its phase.
    waited: Duration,
}

impl Ctx<'_> {
    fn record(&self) -> &HostRecord {
        self.obs.record()
    }

    fn phase(&self) -> &HostPhase {
        &self.obs.record().phase
    }

    fn storage(&self) -> bool {
        self.obs.host().is_storage()
    }

    fn gate(&self, mode: GateMode) -> Result<(), GateFail> {
        self.obs.gate(mode, self.policy, self.now)
    }

    fn cordoned(&self) -> Option<bool> {
        self.obs.host().cordoned
    }

    /// Stay in the phase, running `commands`.
    fn stay(&self, commands: Vec<Command>) -> Transition {
        Transition::to(self.phase().clone(), commands)
    }

    /// Enter `next` and run its entry commands. Entering `Activating` mints the order: the
    /// fleet's next id, which the move names so the controller records it in the row.
    /// Entering any other phase keeps the row's.
    fn enter(&self, next: HostPhase) -> Transition {
        let order = (next == HostPhase::Activating).then_some(self.obs.facts().next_order);
        let commands = next.entry_commands(order.unwrap_or(self.record().order));
        Transition {
            next,
            order,
            boot: None,
            commands,
        }
    }

    /// The helper reverted or refused: the run is held (steps 6 to 9).
    fn helper_failed(&self) -> Option<Transition> {
        match self.obs.report() {
            Some(HelperStatus::Reverted) => {
                Some(Transition::hold(HoldReason::Reverted, Vec::new()))
            }
            Some(HelperStatus::Refused) => {
                Some(Transition::hold(HoldReason::RevertRefused, Vec::new()))
            }
            Some(HelperStatus::ActivatedUnconfirmed | HelperStatus::Confirmed) | None => None,
        }
    }

    /// Whether the planned-down mark has outlived the restart deadline (dwell minus two
    /// time-to-lives) without the node registering again, which deletes it.
    fn restart_overdue(&self, member: &MemberFacts) -> bool {
        member.down_since.is_some_and(|marked| {
            self.now.saturating_duration_since(marked) >= self.policy.restart_deadline()
        })
    }

    /// Whether the member registered again after the mark was written: a boot counter
    /// larger than the one the row recorded. Either unknown is not.
    fn restarted(&self, member: &MemberFacts) -> bool {
        match (member.boot, self.record().boot_before) {
            (Some(now), Some(before)) => now > before,
            _ => false,
        }
    }

    /// A host that cannot be sent the activate order is given back (steps 2 to 5).
    fn disconnected(&self) -> Option<Transition> {
        (!self.obs.host().connected).then(|| self.back(GiveBack::Disconnected))
    }

    /// Give the host back: undo the mark if it is written, uncordon if the cordon was
    /// ordered, free the slot, and tell people. A Mac whose leases outlasted the wait is
    /// `Deferred` (the run goes on), every other host `Pending`.
    fn back(&self, why: GiveBack) -> Transition {
        let phase = self.phase();
        let mut commands = Vec::new();
        if phase.is_marked_down() {
            commands.push(Command::Undown);
        }
        if *phase != HostPhase::Selected {
            commands.push(Command::Uncordon);
        }
        commands.push(Command::CasSlot(SlotOp::Release));
        let next = if self.obs.host().is_mac() && why == GiveBack::LeasesNotEnded {
            HostPhase::Deferred
        } else {
            HostPhase::Pending
        };
        commands.push(Command::Alert(Alert::GaveBack(why)));
        Transition::to(next, commands)
    }

    /// The gate failed with a second member down while the node is out: put the node back
    /// and hold (steps 4 and 5).
    fn second_down(&self) -> Transition {
        let undo = vec![Command::Undown, Command::Uncordon];
        Transition::hold(HoldReason::SecondMemberDown, undo)
    }
}

/// The move for the host `obs` looks at: its phase's step, then the cordon. The cordon holds
/// from step 2 to step 9, and through a hold that keeps it. Placement showing it gone, from
/// a frontend that restarted or an operator, is ordered again. Only `Some(false)` is: an
/// unknown placement is not read as a lost cordon, and the step does not proceed on it
/// either.
pub(super) fn decide(obs: &Observations<'_>, policy: &Policy, now: FarmTime) -> Transition {
    let ctx = Ctx {
        obs,
        policy,
        now,
        waited: now.saturating_duration_since(obs.record().since),
    };
    let mut moved = match ctx.phase() {
        HostPhase::Pending | HostPhase::Deferred | HostPhase::Done | HostPhase::Held { .. } => {
            ctx.stay(Vec::new())
        }
        HostPhase::Selected => selected(&ctx),
        HostPhase::Cordoned => cordoned(&ctx),
        HostPhase::WaitingLeases => waiting_leases(&ctx),
        HostPhase::PlannedDown => planned_down(&ctx),
        HostPhase::Gated => gated(&ctx),
        HostPhase::Activating => activating(&ctx),
        HostPhase::RejoiningStorage => rejoining_storage(&ctx),
        HostPhase::Repairing => repairing(&ctx),
        HostPhase::Confirming => confirming(&ctx),
        HostPhase::RejoiningWork => rejoining_work(&ctx),
    };
    if moved.next.holds_cordon()
        && ctx.cordoned() == Some(false)
        && !moved.commands.contains(&Command::Cordon)
    {
        moved.commands.insert(0, Command::Cordon);
    }
    moved
}

/// Step 1: the claim is visible, G_S still holds and the capacity floor is met.
fn selected(ctx: &Ctx<'_>) -> Transition {
    match ctx.obs.slot() {
        // Another host holds the slot: nothing changed here.
        SlotView::Other => return Transition::to(HostPhase::Pending, Vec::new()),
        // The claim may not be visible yet; it is claimed again, for the health limit.
        SlotView::Free if ctx.waited >= ctx.policy.health_limit => {
            return ctx.back(GiveBack::ClaimNotVisible);
        }
        SlotView::Free => {
            return ctx.stay(ctx.phase().entry_commands(ctx.record().order));
        }
        SlotView::Mine => {}
    }
    if let Some(moved) = ctx.disconnected() {
        return moved;
    }
    if let Err(fail) = ctx.gate(GateMode::Out { fresh_since: None }) {
        return ctx.back(GiveBack::GateClosed(fail));
    }
    let serving = ctx.obs.facts().serving_peers(ctx.obs.host());
    if serving < ctx.policy.min_serving {
        return ctx.back(GiveBack::BelowMinServing { serving });
    }
    ctx.enter(HostPhase::Cordoned)
}

/// Step 2: placement shows the cordon.
fn cordoned(ctx: &Ctx<'_>) -> Transition {
    if let Some(moved) = ctx.disconnected() {
        return moved;
    }
    match ctx.cordoned() {
        Some(true) => ctx.enter(HostPhase::WaitingLeases),
        _ if ctx.waited >= ctx.policy.health_limit => ctx.back(GiveBack::CordonNotTaken),
        _ => ctx.stay(Vec::new()),
    }
}

/// Step 3: zero leases. A lost cordon is ordered again and nothing proceeds without it. A
/// storage host reads G_S once more before the mark, which may come up to the lease limit
/// after the last look.
fn waiting_leases(ctx: &Ctx<'_>) -> Transition {
    if let Some(moved) = ctx.disconnected() {
        return moved;
    }
    let host = ctx.obs.host();
    let idle = host.load.is_some_and(|load| load.is_idle());
    if idle && ctx.cordoned() == Some(true) {
        if !ctx.storage() {
            return ctx.enter(HostPhase::Activating);
        }
        if let Err(fail) = ctx.gate(GateMode::Out { fresh_since: None }) {
            return ctx.back(GiveBack::GateClosed(fail));
        }
        let Some(boot) = ctx.obs.member().and_then(|member| member.boot) else {
            return ctx.back(GiveBack::BootUnknown);
        };
        let mut moved = ctx.enter(HostPhase::PlannedDown);
        moved.boot = Some(boot);
        return moved;
    }
    if ctx.waited >= ctx.policy.lease_limit_for(&host.platform) {
        return ctx.back(GiveBack::LeasesNotEnded);
    }
    ctx.stay(Vec::new())
}

/// Step 4: the mark is visible and no write is open or on its way.
fn planned_down(ctx: &Ctx<'_>) -> Transition {
    if let Some(moved) = ctx.disconnected() {
        return moved;
    }
    if let Err(fail) = ctx.gate(GateMode::Out { fresh_since: None })
        && fail.is_second_member_down()
    {
        return ctx.second_down();
    }
    let Some(member) = ctx.obs.member() else {
        return ctx.back(GiveBack::GateClosed(GateFail::Unlinked));
    };
    if member.planned_down && ctx.obs.host().writes_quiet() {
        return ctx.enter(HostPhase::Gated);
    }
    if ctx.waited >= ctx.policy.write_limit {
        return ctx.back(GiveBack::WritesNotQuiet);
    }
    if member.planned_down {
        ctx.stay(Vec::new())
    } else {
        ctx.stay(ctx.phase().entry_commands(ctx.record().order))
    }
}

/// Step 5: G_S with the node out, from reports newer than the mark.
fn gated(ctx: &Ctx<'_>) -> Transition {
    if let Some(moved) = ctx.disconnected() {
        return moved;
    }
    let mode = GateMode::Out {
        fresh_since: Some(ctx.record().since),
    };
    match ctx.gate(mode) {
        Err(fail) if fail.is_second_member_down() => return ctx.second_down(),
        // A report is due within a heartbeat; a member that never sends it is `Silent`
        // within a time to live, which holds the run.
        Err(fail) if fail.is_report_pending() => return ctx.stay(Vec::new()),
        Err(fail) => return ctx.back(GiveBack::GateClosed(fail)),
        Ok(()) => {}
    }
    // The gate is open for a host with no storage half too; that host has no mark to
    // keep, and is not in this step.
    let Some(member) = ctx.obs.member() else {
        return ctx.back(GiveBack::GateClosed(GateFail::Unlinked));
    };
    let marked = member.planned_down;
    if marked && ctx.obs.host().writes_quiet() && ctx.cordoned() == Some(true) {
        return ctx.enter(HostPhase::Activating);
    }
    if ctx.waited >= ctx.policy.write_limit {
        return ctx.back(GiveBack::WritesNotQuiet);
    }
    let reassert = if marked {
        Vec::new()
    } else {
        vec![Command::MarkDown]
    };
    ctx.stay(reassert)
}

/// Step 6: the helper reports the flip, naming this update's order. A daemon-only host also
/// waits for its daemon to be back at the target, which stands in for step 7.
fn activating(ctx: &Ctx<'_>) -> Transition {
    if let Some(held) = ctx.helper_failed() {
        return held;
    }
    let host = ctx.obs.host();
    if ctx.storage()
        && let Some(member) = ctx.obs.member()
        && ctx.restart_overdue(member)
    {
        return Transition::hold(HoldReason::RestartDeadline, Vec::new());
    }
    let activated = matches!(
        ctx.obs.report(),
        Some(HelperStatus::ActivatedUnconfirmed | HelperStatus::Confirmed)
    );
    if activated {
        if ctx.storage() {
            return ctx.enter(HostPhase::RejoiningStorage);
        }
        if host.is_back_at(ctx.obs.target()) {
            return ctx.enter(HostPhase::Confirming);
        }
    }
    if ctx.waited >= ctx.policy.activate_limit {
        return Transition::hold(HoldReason::ActivateStalled, Vec::new());
    }
    ctx.stay(Vec::new())
}

/// Step 7: the new process registers (a boot counter larger than the one recorded at the
/// mark). `Active` leaves the step; `CatchingUp` waits and is never reverted; any other
/// state, or a member that has not restarted, is the health limit's.
fn rejoining_storage(ctx: &Ctx<'_>) -> Transition {
    if let Some(held) = ctx.helper_failed() {
        return held;
    }
    let Some(member) = ctx.obs.member() else {
        return Transition::hold(HoldReason::MemberGone, Vec::new());
    };
    if ctx.restart_overdue(member) {
        return Transition::hold(HoldReason::RestartDeadline, Vec::new());
    }
    // The mark is deleted when the new process registers, so `Active` with the mark gone
    // and a new boot counter is the new process; the mark still there is the row the old
    // one wrote, and a mark gone with the same counter is the old process again (an undo,
    // an operator, a lapse).
    let restarted = ctx.restarted(member);
    if restarted && member.state == MemberState::Active && !member.planned_down {
        return ctx.enter(HostPhase::Repairing);
    }
    if restarted && member.state == MemberState::CatchingUp {
        if ctx
            .policy
            .rebuild_limit
            .is_some_and(|limit| ctx.waited >= limit)
        {
            return Transition::hold(HoldReason::RebuildLimit, Vec::new());
        }
        let alert = (ctx.waited >= ctx.policy.health_limit).then_some(Alert::RebuildWaiting);
        return ctx.stay(alert.map(Command::Alert).into_iter().collect());
    }
    if ctx.waited >= ctx.policy.health_limit {
        return Transition::hold(HoldReason::HealthLimit, Vec::new());
    }
    ctx.stay(Vec::new())
}

/// Step 8: G_S with the node in, and its daemon half back at the target. At the repair
/// wait the run holds; a healthy binary is confirmed first, so the helper does not revert
/// it (section 2.6: "no revert (the binary is healthy)").
fn repairing(ctx: &Ctx<'_>) -> Transition {
    if let Some(held) = ctx.helper_failed() {
        return held;
    }
    let back = ctx.obs.host().is_back_at(ctx.obs.target());
    if back && ctx.gate(GateMode::In).is_ok() {
        return ctx.enter(HostPhase::Confirming);
    }
    if ctx.waited >= ctx.policy.repair_wait {
        return if back {
            let confirm = Command::Confirm {
                order: ctx.record().order,
            };
            Transition::hold(HoldReason::RepairWait, vec![confirm])
        } else {
            Transition::hold(HoldReason::HealthLimit, Vec::new())
        };
    }
    ctx.stay(Vec::new())
}

/// Step 9: the helper reports `CONFIRMED` for this update's order.
fn confirming(ctx: &Ctx<'_>) -> Transition {
    if let Some(held) = ctx.helper_failed() {
        return held;
    }
    if ctx.obs.report() == Some(HelperStatus::Confirmed) {
        return ctx.enter(HostPhase::RejoiningWork);
    }
    if ctx.waited >= ctx.policy.health_limit {
        return Transition::hold(HoldReason::ConfirmStalled, Vec::new());
    }
    ctx.stay(Vec::new())
}

/// Step 10: the node was given work, or there was none for it and placement shows it
/// uncordoned. A first lease proves the uncordon; an empty queue does not, so it counts
/// only with the cordon shown gone. A node that placement still has cordoned is
/// uncordoned again, until the health limit.
fn rejoining_work(ctx: &Ctx<'_>) -> Transition {
    let cordoned = ctx.cordoned();
    let served = match ctx.obs.host().work {
        Some(Work::Leased) => true,
        Some(Work::QueueEmpty) => cordoned == Some(false),
        Some(Work::Waiting) | None => false,
    };
    if served {
        return ctx.enter(HostPhase::Done);
    }
    if cordoned == Some(true) {
        if ctx.waited >= ctx.policy.health_limit {
            return Transition::hold(HoldReason::StillCordoned, Vec::new());
        }
        return ctx.stay(vec![Command::Uncordon]);
    }
    ctx.stay(Vec::new())
}
