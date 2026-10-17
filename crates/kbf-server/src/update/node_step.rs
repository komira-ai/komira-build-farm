//! The node step (`docs/design/split-mode-updates.md` section 2.6) as a pure transition:
//! where a host is, what is observed, and the one move the controller makes next.
//!
//! ```text
//! Pending -> Selected -> Cordoned -> WaitingLeases -> PlannedDown -> Gated -> Activating
//!   ^  (1 claim) (2 cordon)  (3 leases)     (4 down)      (5 gate)    (6 activate)
//!   |                                                                      |
//!   |   Done <- RejoiningWork <- Confirming <- Repairing <- RejoiningStorage
//!   |  (10 work)    (9 confirm)     (8 repaired)  (7 rejoin storage)
//!   +-- give back (steps 1-5, and 3 for a Mac -> Deferred)       any -> Held
//! ```
//!
//! A daemon-only host skips steps 4, 5, 7 and 8: `WaitingLeases -> Activating ->
//! Confirming` (section 2.10). A phase is the step *entered*: the controller writes the
//! row, then runs the phase's entry commands ([`HostRecord::entry_commands`]), and the step
//! waits for the phase's exit condition. [`step`] reads the phase, and when it was entered,
//! from the host's own row ([`HostRecord`]); it makes at most one move per call and never
//! reads a clock. The caller sets the row's `since` to `now` whenever the phase changed.
//! The steps themselves are in `steps.rs`, one function each.
//!
//! **Give back and hold.** Up to and including the gate (steps 2 to 5), nothing about the
//! host is irreversible, so every failure there gives the host back: undo the planned-down
//! mark, uncordon, free the slot (capacity beats update, section 2.8). From the activate
//! order on, the host's restart is already running, so *no* failure uncordons it; a
//! failure holds the run for a person, and the helper reverts the host by its own timer.
//! The one deliberate uncordon after the activate order is step 10's. A held node that
//! should stay cordoned has its cordon ordered again if placement stops showing it
//! ([`HoldReason::keeps_cordon`]).
//!
//! **Every unknown fails closed.** A lease count, a write count, a placement or a boot
//! counter that is `None` never reads as zero or as "fine".

use kbf_types::FarmTime;

use super::command::{Alert, Command, HoldReason, SlotOp};
#[cfg(doc)]
use super::facts::HostRecord;
use super::facts::{Observations, OrderId};
use super::policy::Policy;
use super::steps;

/// Where one host is in the node step; the `phase` of its `update/host/<id>` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPhase {
    /// Not started. The picker chooses among `Pending` and `Deferred` hosts.
    Pending,
    /// Step 1: the slot is claimed for this host.
    Selected,
    /// Step 2: the cordon is ordered.
    Cordoned,
    /// Step 3: waiting for the leases to end by themselves.
    WaitingLeases,
    /// Step 4: the planned-down mark is written; waiting for writes in flight to reach zero.
    PlannedDown,
    /// Step 5: G_S is re-evaluated with the node out.
    Gated,
    /// Step 6: the helper is ordered to flip and restart.
    Activating,
    /// Step 7: the new process registers; `Active` at once, or `CatchingUp` while it rebuilds.
    RejoiningStorage,
    /// Step 8: the debts of the downtime are settled.
    Repairing,
    /// Step 9: `CONFIRM` is ordered.
    Confirming,
    /// Step 10: the node is uncordoned; waiting for its first lease.
    RejoiningWork,
    /// Updated and back at work. The slot is free.
    Done,
    /// A person decides (section 5.1). The slot stays held.
    Held {
        /// Why.
        reason: HoldReason,
    },
    /// A Mac that was busy (section 2.10): the run goes on, and it is picked again when it
    /// goes idle. The UI shows "done with N deferred".
    Deferred,
}

impl HostPhase {
    /// Whether the host is in a step: it holds the slot and no other host may start.
    #[must_use]
    pub const fn in_step(&self) -> bool {
        match self {
            Self::Selected
            | Self::Cordoned
            | Self::WaitingLeases
            | Self::PlannedDown
            | Self::Gated
            | Self::Activating
            | Self::RejoiningStorage
            | Self::Repairing
            | Self::Confirming
            | Self::RejoiningWork => true,
            Self::Pending | Self::Done | Self::Held { .. } | Self::Deferred => false,
        }
    }

    /// Steps 2 to 5: nothing is irreversible yet, so every failure gives the host back
    /// (section 2.6: "uncordon, `Pending`").
    #[must_use]
    pub const fn is_early(&self) -> bool {
        match self {
            Self::Cordoned | Self::WaitingLeases | Self::PlannedDown | Self::Gated => true,
            Self::Pending
            | Self::Selected
            | Self::Activating
            | Self::RejoiningStorage
            | Self::Repairing
            | Self::Confirming
            | Self::RejoiningWork
            | Self::Done
            | Self::Held { .. }
            | Self::Deferred => false,
        }
    }

    /// Whether the node must be cordoned in this phase: steps 2 to 9, and a hold that keeps
    /// it so. Step 10 uncordons it.
    #[must_use]
    pub const fn holds_cordon(&self) -> bool {
        match self {
            Self::Cordoned
            | Self::WaitingLeases
            | Self::PlannedDown
            | Self::Gated
            | Self::Activating
            | Self::RejoiningStorage
            | Self::Repairing
            | Self::Confirming => true,
            Self::Held { reason } => reason.keeps_cordon(),
            Self::Pending | Self::Selected | Self::RejoiningWork | Self::Done | Self::Deferred => {
                false
            }
        }
    }

    /// Whether the planned-down mark is written (steps 4 and 5) and must be undone to give
    /// the host back.
    #[must_use]
    pub const fn is_marked_down(&self) -> bool {
        match self {
            Self::PlannedDown | Self::Gated => true,
            Self::Pending
            | Self::Selected
            | Self::Cordoned
            | Self::WaitingLeases
            | Self::Activating
            | Self::RejoiningStorage
            | Self::Repairing
            | Self::Confirming
            | Self::RejoiningWork
            | Self::Done
            | Self::Held { .. }
            | Self::Deferred => false,
        }
    }

    /// The commands that entering this phase issues. The controller writes the row first
    /// and runs these after, so after a restart it runs them again for the phase it reads
    /// ([`HostRecord::entry_commands`]). `order` is the row's activation order, which the
    /// two orders carry.
    #[must_use]
    pub fn entry_commands(&self, order: OrderId) -> Vec<Command> {
        match self {
            Self::Selected => vec![Command::CasSlot(SlotOp::Claim)],
            Self::Cordoned => vec![Command::Cordon],
            Self::PlannedDown => vec![Command::MarkDown],
            Self::Activating => vec![Command::Activate { order }],
            Self::Confirming => vec![Command::Confirm { order }],
            Self::RejoiningWork => vec![Command::Uncordon],
            Self::Done => vec![Command::CasSlot(SlotOp::Release)],
            Self::Pending
            | Self::WaitingLeases
            | Self::Gated
            | Self::RejoiningStorage
            | Self::Repairing
            | Self::Held { .. }
            | Self::Deferred => Vec::new(),
        }
    }
}

/// The move [`step`] decides: the phase to record, the row fields this move sets, and the
/// commands to run after it is recorded. `next` equal to the phase passed in is "stay"
/// (its commands are re-asserts and alerts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    /// The phase to record.
    pub next: HostPhase,
    /// The id of the activation order this move issues, to record in the row with `next`
    /// ([`HostRecord::order`]); the controller counts it used. `Some` exactly when the
    /// move enters `Activating`, and then it is [`super::Facts::next_order`].
    pub order: Option<OrderId>,
    /// The member's boot counter, to record in the row with `next`
    /// ([`HostRecord::boot_before`]). `Some` exactly when the move enters `PlannedDown`.
    pub boot: Option<u64>,
    /// What to run once it is recorded, in order.
    pub commands: Vec<Command>,
}

impl Transition {
    /// The move from `Pending` or `Deferred` once the picker chose the host: record
    /// `Selected` and claim the slot.
    #[must_use]
    pub fn select() -> Self {
        Self::to(
            HostPhase::Selected,
            HostPhase::Selected.entry_commands(OrderId(0)),
        )
    }

    /// Move to `next`, running `commands`.
    pub(super) const fn to(next: HostPhase, commands: Vec<Command>) -> Self {
        Self {
            next,
            order: None,
            boot: None,
            commands,
        }
    }

    /// Hold the run: `Held`, `undo` first, then the alert.
    pub(super) fn hold(reason: HoldReason, mut undo: Vec<Command>) -> Self {
        undo.push(Command::Alert(Alert::Held(reason)));
        Self::to(HostPhase::Held { reason }, undo)
    }
}

/// The move for the host `obs` looks at, from the phase and entry time of its row, given
/// the policy and the time.
///
/// `Pending`, `Deferred`, `Done` and `Held` do not move by themselves: the picker moves
/// the first two ([`Transition::select`]), a person the last, and `Done` is final. A held
/// node that keeps its cordon is the one exception to "nothing": its cordon is ordered
/// again if placement shows it gone.
#[must_use]
pub fn step(obs: &Observations<'_>, policy: &Policy, now: FarmTime) -> Transition {
    steps::decide(obs, policy, now)
}
