//! The rollout record (`docs/design/fleet-updates.md` section 4.1): which software set
//! goes to which nodes, with what strategy, and where each node is. Plain data and the
//! rules for changing it; storing it and acting on it are the server's.
//!
//! Each node moves through (the node states of section 3.3)
//!
//! ```text
//! pending -> cordoned -> draining -> updating -> rebooting -> qualifying -> done
//!                                             \-----------------------^
//! any step but done, failed, quarantined --> held | failed
//! updating | rebooting | qualifying --> quarantined
//! held --> the step it was held at | failed;   quarantined --> failed
//! ```
//!
//! Updating goes straight to qualifying when the update needs no reboot. A held node
//! resumes only at the step it was held at, or is given up (`failed`, the operator's
//! "skip node", section 4.3). A node is quarantined once it has been handed its update
//! and its report or leak scan does not match (sections 3.3 and 4.2, step 7): only a
//! repair or an operator returns it, so within the rollout it can only be given up.
//! `done` and `failed` are final. Every other move is refused with [`IllegalStep`], and
//! the record is left as it was.
//!
//! Two rules span nodes: a pending node is cordoned only while fewer than the
//! strategy's `max_unavailable` nodes are out of service ([`NodeStep::is_out`]), and a
//! rollout is `done` only once every node is done or failed.
//!
//! The rollout itself is `pending`, `running`, `held`, `done` or `cancelled` (section
//! 4.1; `awaiting_client`, the client gate of section 6.1, arrives with it).

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::WorkerId;

/// A rollout, numbered by the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RolloutId(pub u64);

impl fmt::Display for RolloutId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rollout-{}", self.0)
    }
}

/// Where one node of a rollout is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeStep {
    /// Not started.
    Pending,
    /// Placement skips it.
    Cordoned,
    /// Its leases are being waited for.
    Draining,
    /// It has been handed its update, which is being applied.
    Updating,
    /// It is rebooting into the update.
    Rebooting,
    /// Its qualification work runs.
    Qualifying,
    /// Updated and qualified. Final.
    Done,
    /// A gate failed; it waits for an operator.
    Held,
    /// Its report or leak scan did not match after the update; only a repair or an
    /// operator returns it.
    Quarantined,
    /// Given up. Final.
    Failed,
}

impl NodeStep {
    /// Whether the step is final: nothing moves a node on from it.
    #[must_use]
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }

    /// Whether a node at this step is out of service for the rollout: started, not
    /// finished. A held or quarantined node counts: it keeps its slot (section 4.3).
    #[must_use]
    pub const fn is_out(self) -> bool {
        !matches!(self, Self::Pending | Self::Done | Self::Failed)
    }

    /// The steps after this one on the way to `done`.
    const fn forward(self) -> &'static [Self] {
        match self {
            Self::Pending => &[Self::Cordoned],
            Self::Cordoned => &[Self::Draining],
            Self::Draining => &[Self::Updating],
            Self::Updating => &[Self::Rebooting, Self::Qualifying],
            Self::Rebooting => &[Self::Qualifying],
            Self::Qualifying => &[Self::Done],
            Self::Done | Self::Held | Self::Quarantined | Self::Failed => &[],
        }
    }
}

impl fmt::Display for NodeStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Pending => "pending",
            Self::Cordoned => "cordoned",
            Self::Draining => "draining",
            Self::Updating => "updating",
            Self::Rebooting => "rebooting",
            Self::Qualifying => "qualifying",
            Self::Done => "done",
            Self::Held => "held",
            Self::Quarantined => "quarantined",
            Self::Failed => "failed",
        };
        f.write_str(name)
    }
}

/// One node's place in a rollout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeProgress {
    step: NodeStep,
    /// While held: the step it was held at, where it resumes.
    held_at: Option<NodeStep>,
}

impl NodeProgress {
    /// A node not started.
    #[must_use]
    pub const fn pending() -> Self {
        Self {
            step: NodeStep::Pending,
            held_at: None,
        }
    }

    /// Where the node is.
    #[must_use]
    pub const fn step(&self) -> NodeStep {
        self.step
    }

    /// While held, the step it was held at.
    #[must_use]
    pub const fn held_at(&self) -> Option<NodeStep> {
        self.held_at
    }

    /// Whether the node may move to `next` (see the module docs).
    #[must_use]
    pub fn allows(&self, next: NodeStep) -> bool {
        use NodeStep::{Failed, Held, Qualifying, Quarantined, Rebooting, Updating};
        match (self.step, next) {
            (Held, next) => next == Failed || Some(next) == self.held_at,
            (Quarantined, next) => next == Failed,
            (from, Held | Failed) if !from.is_final() => true,
            (Updating | Rebooting | Qualifying, Quarantined) => true,
            // A final step has no step forward.
            (from, next) => from.forward().contains(&next),
        }
    }

    /// Moves the node to `next`.
    ///
    /// # Errors
    /// The move is not allowed; the node is unchanged.
    pub fn advance(&mut self, next: NodeStep) -> Result<(), IllegalStep> {
        if !self.allows(next) {
            return Err(IllegalStep::Node {
                from: self.step,
                to: next,
            });
        }
        self.held_at = match next {
            NodeStep::Held => Some(self.step),
            _ => None,
        };
        self.step = next;
        Ok(())
    }
}

/// Where a rollout is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RolloutState {
    /// Recorded; no node touched yet.
    Pending,
    /// Moving nodes.
    Running,
    /// A gate failed: nothing proceeds until an operator resumes or cancels it.
    Held,
    /// Every node is done or failed. Final.
    Done,
    /// Stopped by an operator. Final.
    Cancelled,
}

impl RolloutState {
    /// Whether the rollout may move to `next`: `pending -> running`, `running -> held`,
    /// `held -> running`, `running -> done`, and to `cancelled` from any state that is
    /// not final.
    #[must_use]
    pub const fn allows(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Running)
                | (Self::Running, Self::Held | Self::Done)
                | (Self::Held, Self::Running)
                | (Self::Pending | Self::Running | Self::Held, Self::Cancelled)
        )
    }
}

/// Who started a rollout, and when (section 4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    /// The operator or client identity that asked for it.
    pub who: String,
    /// When it was asked for: milliseconds since the Unix epoch, by the server's clock.
    pub at_unix_ms: u64,
}

impl Actor {
    /// `who`, at `at_unix_ms`.
    #[must_use]
    pub fn new(who: impl Into<String>, at_unix_ms: u64) -> Self {
        Self {
            who: who.into(),
            at_unix_ms,
        }
    }
}

/// Which nodes a rollout covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    /// These nodes.
    Nodes(Vec<WorkerId>),
    /// Every node of these pools, resolved to nodes when the rollout is recorded.
    Pools(Vec<String>),
}

/// How a rollout moves: the server's policy, never the request's (security section
/// S9: the CI `rollout` role sends only a target and a selector).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Strategy {
    /// Nodes of one pool out at once.
    pub max_unavailable: u32,
    /// Nodes updated first, per pool.
    pub canary: u32,
    /// The wait after the canary before the rest.
    pub soak: Duration,
    /// How long running leases may finish before the drain pauses and the node is held.
    pub drain_deadline: Duration,
}

impl Default for Strategy {
    /// The defaults of section 4.1: one node at a time, one canary, a two-hour soak, a
    /// thirty-minute drain.
    fn default() -> Self {
        Self {
            max_unavailable: 1,
            canary: 1,
            soak: Duration::from_secs(2 * 60 * 60),
            drain_deadline: Duration::from_secs(30 * 60),
        }
    }
}

/// A rollout: the software set it installs, where, how, by whom, and how far it got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rollout {
    /// Its number.
    pub id: RolloutId,
    /// The software set it installs, by digest.
    pub target: String,
    /// The nodes it was asked to cover.
    pub selector: Selector,
    /// How it moves.
    pub strategy: Strategy,
    /// Who started it, and when.
    pub actor: Actor,
    state: RolloutState,
    nodes: BTreeMap<WorkerId, NodeProgress>,
}

impl Rollout {
    /// A pending rollout of `target` over `nodes` (the `selector` resolved), every node
    /// pending.
    #[must_use]
    pub fn new(
        id: RolloutId,
        target: impl Into<String>,
        selector: Selector,
        strategy: Strategy,
        actor: Actor,
        nodes: impl IntoIterator<Item = WorkerId>,
    ) -> Self {
        Self {
            id,
            target: target.into(),
            selector,
            strategy,
            actor,
            state: RolloutState::Pending,
            nodes: nodes
                .into_iter()
                .map(|n| (n, NodeProgress::pending()))
                .collect(),
        }
    }

    /// Where the rollout is.
    #[must_use]
    pub const fn state(&self) -> RolloutState {
        self.state
    }

    /// Every node and where it is, in node order.
    #[must_use]
    pub const fn nodes(&self) -> &BTreeMap<WorkerId, NodeProgress> {
        &self.nodes
    }

    /// Where `node` is, if the rollout covers it.
    #[must_use]
    pub fn node(&self, node: &WorkerId) -> Option<NodeProgress> {
        self.nodes.get(node).copied()
    }

    /// Moves the rollout to `next`. It is `done` only once every node is done or
    /// failed.
    ///
    /// # Errors
    /// The move is not allowed, or `next` is done while a node is not finished; the
    /// rollout is unchanged.
    pub fn set_state(&mut self, next: RolloutState) -> Result<(), IllegalStep> {
        if !self.state.allows(next) {
            return Err(IllegalStep::Rollout {
                from: self.state,
                to: next,
            });
        }
        if next == RolloutState::Done
            && let Some((node, progress)) = self.nodes.iter().find(|(_, p)| !p.step.is_final())
        {
            return Err(IllegalStep::Unfinished {
                node: node.clone(),
                step: progress.step,
            });
        }
        self.state = next;
        Ok(())
    }

    /// How many nodes are out of service ([`NodeStep::is_out`]).
    #[must_use]
    pub fn out_of_service(&self) -> usize {
        self.nodes.values().filter(|p| p.step.is_out()).count()
    }

    /// Moves `node` to `next`. Only a running rollout moves nodes, except that a node
    /// of a held rollout may be held too, quarantined, or given up. A pending node is
    /// cordoned only while fewer than `max_unavailable` nodes are out of service.
    ///
    /// # Errors
    /// The rollout does not cover `node`, is not running, the move is not allowed, or
    /// it would take more than `max_unavailable` nodes out; the rollout is unchanged.
    pub fn advance(&mut self, node: &WorkerId, next: NodeStep) -> Result<(), IllegalStep> {
        let moving = match self.state {
            RolloutState::Running => true,
            RolloutState::Held => matches!(
                next,
                NodeStep::Held | NodeStep::Quarantined | NodeStep::Failed
            ),
            _ => false,
        };
        if !moving {
            return Err(IllegalStep::NotRunning(self.state));
        }
        let out = self.out_of_service();
        let max_unavailable = self.strategy.max_unavailable;
        let progress = self
            .nodes
            .get_mut(node)
            .ok_or_else(|| IllegalStep::NotCovered(node.clone()))?;
        let full = usize::try_from(max_unavailable).is_ok_and(|max| out >= max);
        if progress.step == NodeStep::Pending && next == NodeStep::Cordoned && full {
            return Err(IllegalStep::Unavailable { max_unavailable });
        }
        progress.advance(next)
    }
}

/// A change the rollout rules refuse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IllegalStep {
    /// A node may not move so.
    #[error("a node may not move from {from} to {to}")]
    Node {
        /// Where it is.
        from: NodeStep,
        /// Where it was asked to go.
        to: NodeStep,
    },
    /// The rollout may not move so.
    #[error("a rollout may not move from {from:?} to {to:?}")]
    Rollout {
        /// Where it is.
        from: RolloutState,
        /// Where it was asked to go.
        to: RolloutState,
    },
    /// The rollout does not move nodes in this state.
    #[error("a {0:?} rollout moves no node")]
    NotRunning(RolloutState),
    /// The rollout does not cover the node.
    #[error("the rollout does not cover node {0}")]
    NotCovered(WorkerId),
    /// The rollout cannot be done: a node is not finished.
    #[error("a rollout is done only when every node is done or failed; {node} is {step}")]
    Unfinished {
        /// The first unfinished node, in node order.
        node: WorkerId,
        /// Where it is.
        step: NodeStep,
    },
    /// Taking another node out would exceed the strategy's `max_unavailable`.
    #[error("{max_unavailable} node(s) are already out of service, the most the rollout allows")]
    Unavailable {
        /// The strategy's limit.
        max_unavailable: u32,
    },
}
