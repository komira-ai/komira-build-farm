//! G_S, the storage gate (`docs/design/split-mode-updates.md` section 2.7): may one
//! storage member go out, or come back in, without leaving a blob with one copy?
//!
//! "Never take both owners" cannot be checked per blob: under rendezvous hashing any two
//! members co-own some blob. So the rule is about the whole fleet. With the host's own
//! member out, every other member must be `Active`, heard within a time to live, free of
//! repair debt, and neither `CatchingUp` nor planned down; and at least one other member
//! must exist to hold the second copy. Every unknown fails: a member not heard, a debt
//! not reported, a member the facts do not list.

use kbf_map::{MemberId, MemberState};
use kbf_types::FarmTime;

use super::facts::{Facts, MemberFacts};
use super::policy::Policy;

/// Which question the gate answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateMode {
    /// Before a pick: the host's own member is `Active` and heard (it is not already
    /// leaving or dead), and the rest as for [`GateMode::Out`] without a freshness mark.
    Pick,
    /// With the host's member marked out (step 1, and step 5 after the mark): its own
    /// state is not asked, only its repair debt. With `fresh_since`, every other member
    /// must also have reported at or after that time, so the answer is about the fleet
    /// as it is with the node out, not before.
    Out {
        /// The time the node went out; `None` asks only for a report within the time to live.
        fresh_since: Option<FarmTime>,
    },
    /// With the host's member back (step 8): every member, its own too, as for a pick.
    In,
}

/// Why the gate is shut. The first thing found, the worst first: a second member down
/// before a debt before a report not yet in.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GateFail {
    /// The host has no storage member in the facts: not linked, or not listed.
    #[error("its storage member is not linked, or the facts do not list it")]
    Unlinked,
    /// No other member exists to hold the second copy.
    #[error("no other member holds the second copy")]
    NoPeer,
    /// A member has a planned-down mark.
    #[error("member {member} is planned down")]
    PlannedDown {
        /// The member.
        member: MemberId,
    },
    /// A member is rebuilding its index; its reads answer `UNAVAILABLE`.
    #[error("member {member} is catching up")]
    CatchingUp {
        /// The member.
        member: MemberId,
    },
    /// A member is in a state other than `Active` (`Down`, `Draining`, `Joining`).
    #[error("member {member} is {state:?}")]
    NotActive {
        /// The member.
        member: MemberId,
        /// Its state.
        state: MemberState,
    },
    /// A member was not heard within the time to live: it counts as down.
    #[error("member {member} was not heard within the time to live")]
    Silent {
        /// The member.
        member: MemberId,
    },
    /// A member has not reported since the node went out, so the answer would be about the
    /// fleet before. Not a failure yet: the report is due within a heartbeat.
    #[error("member {member} has not reported since the node went out")]
    NotReportedSince {
        /// The member.
        member: MemberId,
    },
    /// A member does not report its repair debt: unknown is not zero.
    #[error("member {member} does not report its repair debt")]
    DebtUnknown {
        /// The member.
        member: MemberId,
    },
    /// A member owes copies, so the other copy of something is missing.
    #[error("member {member} owes {blobs} blobs")]
    Debt {
        /// The member.
        member: MemberId,
        /// How many.
        blobs: u64,
    },
}

impl GateFail {
    /// Whether a member other than the host is `Down` or silent: the case in which the
    /// run holds, because blobs owned by both are `UNAVAILABLE` until one returns (section
    /// 5.2). Every other failure only gives the host back.
    #[must_use]
    pub const fn is_second_member_down(&self) -> bool {
        matches!(
            self,
            Self::NotActive {
                state: MemberState::Down,
                ..
            } | Self::Silent { .. }
        )
    }

    /// Whether waiting a heartbeat may open the gate: the report it needs is not in yet.
    #[must_use]
    pub const fn is_report_pending(&self) -> bool {
        matches!(self, Self::NotReportedSince { .. })
    }

    /// 2: a second member down; 0: a report not yet in; 1: anything else.
    const fn rank(&self) -> u8 {
        if self.is_second_member_down() {
            2
        } else if self.is_report_pending() {
            0
        } else {
            1
        }
    }
}

/// Evaluates G_S for the storage member `own` (a member the facts list, or the gate is
/// shut with [`GateFail::Unlinked`]).
///
/// # Errors
/// The worst thing that keeps the gate shut ([`GateFail`]); the first of equals, members
/// in id order.
pub fn storage_gate(
    facts: &Facts,
    own: MemberId,
    mode: GateMode,
    policy: &Policy,
    now: FarmTime,
) -> Result<(), GateFail> {
    let me = facts.member(own).ok_or(GateFail::Unlinked)?;
    let mut worst: Option<GateFail> = None;
    let mut note = |fail: GateFail| {
        if worst.as_ref().is_none_or(|w| fail.rank() > w.rank()) {
            worst = Some(fail);
        }
    };
    if !matches!(mode, GateMode::Out { .. })
        && let Err(fail) = live(me, policy, now)
    {
        note(fail);
    }
    // The debt it owes counts whatever the mode: a node going out with copies owed leaves
    // them owed.
    if let Err(fail) = debt_settled(me) {
        note(fail);
    }
    let mut peers: Vec<&MemberFacts> = facts.members.iter().filter(|m| m.id != own).collect();
    peers.sort_by_key(|m| m.id);
    if peers.is_empty() {
        note(GateFail::NoPeer);
    }
    for peer in peers {
        if let Err(fail) = live(peer, policy, now) {
            note(fail);
            continue;
        }
        if let GateMode::Out {
            fresh_since: Some(since),
        } = mode
            && peer.heard_at.is_none_or(|heard| heard < since)
        {
            note(GateFail::NotReportedSince { member: peer.id });
            continue;
        }
        if let Err(fail) = debt_settled(peer) {
            note(fail);
        }
    }
    worst.map_or(Ok(()), Err)
}

/// `member` is `Active`, not planned down, and heard within the time to live.
fn live(member: &MemberFacts, policy: &Policy, now: FarmTime) -> Result<(), GateFail> {
    let id = member.id;
    if member.planned_down {
        return Err(GateFail::PlannedDown { member: id });
    }
    match member.state {
        MemberState::Active => {}
        MemberState::CatchingUp => return Err(GateFail::CatchingUp { member: id }),
        state => return Err(GateFail::NotActive { member: id, state }),
    }
    if member.is_fresh(now, policy.ttl) {
        Ok(())
    } else {
        Err(GateFail::Silent { member: id })
    }
}

/// `member` reports a repair debt of zero. An absent report is not zero.
fn debt_settled(member: &MemberFacts) -> Result<(), GateFail> {
    match member.repair_debt {
        Some(0) => Ok(()),
        Some(blobs) => Err(GateFail::Debt {
            member: member.id,
            blobs,
        }),
        None => Err(GateFail::DebtUnknown { member: member.id }),
    }
}
