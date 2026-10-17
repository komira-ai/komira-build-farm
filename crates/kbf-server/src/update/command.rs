//! What the controller must do after a transition, and what it must tell people.

use super::facts::OrderId;
use super::gate::GateFail;

/// An operation on `update/slot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotOp {
    /// Compare-and-set a place for this host, if the places of its kind have room.
    Claim,
    /// Free the place this host holds, if it holds one.
    Release,
}

/// One thing the controller does to the farm. A transition lists them in the order to
/// run them. After a restart the controller re-issues the commands of the phase it reads
/// ([`super::HostRecord::entry_commands`]). This module cannot make that safe; it needs
/// each command to be a no-op the second time. The slot, the cordon and the mark are
/// states, so they are. The two orders are the helper's to dedupe: both carry the row's
/// order id, and a helper that has seen an id must not act on it again (section 6.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Claim or free the host's place in `update/slot`.
    CasSlot(SlotOp),
    /// `POST /v1/nodes/{node}:cordon`: placement skips the node.
    Cordon,
    /// Return the node to placement.
    Uncordon,
    /// `POST /v1/members/{id}` with `{"state":"down","reason":"restart"}`: the planned
    /// `down/<id>` mark and a map bump, so the routers stop choosing the node as a write
    /// primary. Never `Draining` (section 2.6).
    MarkDown,
    /// Delete the mark: the member returns by its next heartbeat.
    Undown,
    /// Order the host's helper to flip to the host's commit and restart. When the order is
    /// not taken the controller cannot know whether the helper swapped (section 5.1 counts
    /// that as "swapped, maybe"). It does not revert: the helper reverts itself by its own
    /// timer. Section 5.1 also says "never retry", which this controller keeps in the sense
    /// that it never orders a second activation: it gives the *same* order id again at every
    /// look until the order is taken or the step's activate limit holds the run, and never
    /// draws a new id for the host ([`super::Transport::activate`]). A helper acts on an id
    /// once, so the repeat cannot swap twice. That is a choice the design leaves open.
    Activate {
        /// The id of the order; the helper's status names it.
        order: OrderId,
    },
    /// Order `CONFIRM` of the activation `order`: the helper cancels its revert timer.
    Confirm {
        /// The activation being confirmed.
        order: OrderId,
    },
    /// Tell people. Level-triggered: the same alert can come on several looks while its
    /// condition holds, and the controller sends it once per `(host, alert)`.
    Alert(Alert),
}

/// How loudly an alert is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A warning: the banner and the log.
    Warn,
    /// A page.
    Page,
}

/// Why a host was given back: uncordoned, the slot freed, back to `Pending` (or
/// `Deferred` for a Mac whose leases outlasted the wait). Capacity beats update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GiveBack {
    /// The host's daemon stream is closed before the restart: it cannot be told to update.
    /// Not in the design's table; a host that cannot be sent the activate order is not
    /// kept in a step.
    Disconnected,
    /// The claim on the slot did not show within the health limit.
    ClaimNotVisible,
    /// Fewer than `min_serving` other hosts of its platform would be left (section 2.8).
    BelowMinServing {
        /// How many were.
        serving: usize,
    },
    /// Placement did not show the cordon within the health limit.
    CordonNotTaken,
    /// The leases did not end within their limit (step 3).
    LeasesNotEnded,
    /// The member does not report a boot counter, so its return cannot be told from the
    /// old process still running (step 7).
    BootUnknown,
    /// The write sessions did not reach zero within their limit (step 4, C31).
    WritesNotQuiet,
    /// G_S is shut (steps 1, 3, 4 and 5).
    GateClosed(GateFail),
    /// A higher generation was offered (a rollback, or a newer release) while the host had
    /// done nothing irreversible, so the release it was started for is not activated on it.
    /// Not in the design's table: it says a rollback is a higher generation and walks the
    /// hosts back, not what a host in steps 1 to 5 does meanwhile.
    Superseded,
}

/// Why a run is held for a person (section 5.1: a failure holds the run, never retried,
/// never skipped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldReason {
    /// Another member is `Down` or silent while this node is out (steps 4 and 5): the
    /// node is put back first.
    SecondMemberDown,
    /// The node did not register again before the dwell would drop it from the map (dwell
    /// minus two time-to-lives): the dwell may start re-replication.
    RestartDeadline,
    /// The node's member is gone from the map.
    MemberGone,
    /// The helper did not report the activation within its limit.
    ActivateStalled,
    /// An intact node was not `Active` again within the health limit, or its daemon half
    /// was not back at the target: the helper reverts it by its own timer.
    HealthLimit,
    /// A rebuilding node passed the rebuild limit; the other owner serves.
    RebuildLimit,
    /// The debts of the downtime did not settle within the repair wait. The binary is
    /// healthy, so it is confirmed and the node stays cordoned.
    RepairWait,
    /// The helper did not report `CONFIRMED` within the health limit.
    ConfirmStalled,
    /// The helper reverted the node itself (the window expired, the binary kept dying, or
    /// the confirm was lost).
    Reverted,
    /// The helper could not revert: the previous release is missing.
    RevertRefused,
    /// The node is updated and confirmed but placement still has it cordoned.
    StillCordoned,
}

impl HoldReason {
    /// How loudly it is delivered: a person is needed at once when data availability is at
    /// risk or a host cannot be put back.
    #[must_use]
    pub const fn severity(self) -> Severity {
        match self {
            Self::SecondMemberDown
            | Self::RestartDeadline
            | Self::MemberGone
            | Self::RevertRefused => Severity::Page,
            Self::ActivateStalled
            | Self::HealthLimit
            | Self::RebuildLimit
            | Self::RepairWait
            | Self::ConfirmStalled
            | Self::Reverted
            | Self::StillCordoned => Severity::Warn,
        }
    }

    /// Whether the node stays cordoned while the run is held ("Held, cordoned", section
    /// 5.2), so the step re-orders a cordon that placement stops showing. Not after a
    /// second member went down (the node was put back first), and not when the node is
    /// already confirmed and step 10's uncordon is what did not take.
    #[must_use]
    pub const fn keeps_cordon(self) -> bool {
        match self {
            Self::SecondMemberDown | Self::StillCordoned => false,
            Self::RestartDeadline
            | Self::MemberGone
            | Self::ActivateStalled
            | Self::HealthLimit
            | Self::RebuildLimit
            | Self::RepairWait
            | Self::ConfirmStalled
            | Self::Reverted
            | Self::RevertRefused => true,
        }
    }
}

/// Something to tell people (the failure cells of section 2.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alert {
    /// The host was given back.
    GaveBack(GiveBack),
    /// The run is held.
    Held(HoldReason),
    /// A rebuilding node has been `CatchingUp` past the health limit, and the policy has no
    /// rebuild limit: the step waits (the other owner serves) and a person should look.
    RebuildWaiting,
}

impl Alert {
    /// How loudly it is delivered.
    #[must_use]
    pub const fn severity(&self) -> Severity {
        match self {
            Self::GaveBack(_) | Self::RebuildWaiting => Severity::Warn,
            Self::Held(reason) => reason.severity(),
        }
    }
}
