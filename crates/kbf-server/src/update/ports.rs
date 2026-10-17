//! What the controller needs from the farm, as two traits: the orders it gives
//! ([`Transport`]) and the facts it reads ([`FactsSource`]). The controller never touches the
//! scheduler, the membership or a host directly, so the pure steps are driven the same way
//! over the real farm and over a model of one.
//!
//! Both are blocking and take `&mut self`, as the state store's calls block: a caller on an
//! async runtime runs the controller's tick on a blocking task, and an implementation over
//! async calls blocks on them there.
//!
//! **Orders are repeated.** The controller writes a host's row before it gives the order
//! the row's phase calls for (the orders that undo a step, when a host leaves it, come
//! first: see `Controller`), and after a restart it gives the orders of every row it finds
//! again ([`super::HostRecord::entry_commands`]). So every method here must do nothing the
//! second time: a cordon, an uncordon, a mark and its removal are states; the two orders to
//! a helper carry the row's order id, which the helper dedupes (a helper that has seen an
//! id must not act on it again, section 6.1).

use std::collections::BTreeSet;

use kbf_map::MemberId;
use kbf_types::{FarmTime, WorkerId};

use super::command::Alert;
use super::facts::{HostFacts, MemberFacts, OrderId, PlatformClass, Release};

/// An order the farm did not take, or a call that did not come back. The controller does
/// not know whether it was applied, so it gives it again.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

impl TransportError {
    /// An error that says `why`.
    #[must_use]
    pub fn new(why: impl Into<String>) -> Self {
        Self(why.into())
    }
}

/// The farm could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SourceError(pub String);

/// The orders the controller gives. See the module docs: each is repeatable.
pub trait Transport: Send {
    /// `POST /v1/nodes/{node}:cordon`: placement skips the host.
    ///
    /// # Errors
    /// [`TransportError`] when the order may not have been applied.
    fn cordon(&mut self, host: &WorkerId) -> Result<(), TransportError>;

    /// Returns the host to placement.
    ///
    /// # Errors
    /// As [`Transport::cordon`].
    fn uncordon(&mut self, host: &WorkerId) -> Result<(), TransportError>;

    /// `POST /v1/members/{id}` with `{"state":"down","reason":"restart"}`: the planned
    /// `down/<id>` mark of the host's storage `member` and a map bump. Never `Draining`.
    ///
    /// # Errors
    /// As [`Transport::cordon`].
    fn mark_down(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError>;

    /// Deletes the mark: the member returns by its next heartbeat.
    ///
    /// # Errors
    /// As [`Transport::cordon`].
    fn undown(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError>;

    /// Orders the host's helper to flip to `release` and restart. `order` is what its status
    /// names.
    ///
    /// An error means the order may or may not have reached the helper, which section 5.1
    /// counts as "swapped, maybe". The controller does not revert (the helper does, by its
    /// own timer) and does not order a second activation, which is what 5.1's "never retry"
    /// is for: it gives the same order, the same id, again at every look until the order is
    /// taken or the step's activate limit holds the run, and it never draws a new id for a
    /// host whose row names one. The same happens after a restart of the controller, which
    /// cannot know whether the order was given. So the helper must do nothing for an id
    /// equal to the highest it has acted on, and refuse a lower one (section 6.1); a
    /// transport that cannot promise that is not safe to give this order.
    ///
    /// # Errors
    /// As [`Transport::cordon`].
    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError>;

    /// Orders `CONFIRM` of the activation `order`: the helper cancels its revert timer.
    ///
    /// # Errors
    /// As [`Transport::cordon`].
    fn confirm(&mut self, host: &WorkerId, order: OrderId) -> Result<(), TransportError>;

    /// Tells people. The controller sends one alert once per phase of a host; an error is
    /// not given again until the host's row moves or the controller restarts.
    ///
    /// # Errors
    /// As [`Transport::cordon`].
    fn alert(&mut self, host: &WorkerId, alert: &Alert) -> Result<(), TransportError>;
}

/// What the farm shows at one look: everything of [`super::Facts`] that is not the
/// controller's own rows (the target, the generation, the slot, the order counter and each host's
/// record).
///
/// Every host of the run must be listed, connected or not. The host metrics carry no
/// observation time, so a source fills them and the members from the same heartbeat look
/// ([`super::MemberFacts`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FarmView {
    /// Every host. Each `record` is replaced by the controller from `update/host/<id>`; a
    /// source leaves [`super::HostRecord::unstarted`] there.
    pub hosts: Vec<HostFacts>,
    /// Every storage member.
    pub members: Vec<MemberFacts>,
    /// The Mac classes some queued operation asks for; `None` if unknown.
    pub queued_classes: Option<BTreeSet<PlatformClass>>,
}

/// Reads the farm.
pub trait FactsSource: Send {
    /// The farm as it is at `now`. The clock is the one the controller's rows use.
    ///
    /// # Errors
    /// [`SourceError`] when the farm could not be read; the controller then does nothing
    /// this tick (a fact it could not read is not "fine").
    fn look(&mut self, now: FarmTime) -> Result<FarmView, SourceError>;
}
