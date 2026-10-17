//! The daemon's end of the update client (`kbf-update`): the frontend's `UpdateOffer` and
//! `UpdateOrder` go to the client, its status goes back as `UpdateStatus` after each
//! Welcome and on every change, and while an accepted activation holds the update gate
//! the daemon takes no lease. All the update logic is the client's; this file is the
//! seam.
//!
//! A link without a client (`UpdateLink::manual`) is a node that updates by hand: it
//! reports that, and ignores what it is offered or ordered.

use std::collections::BTreeMap;

use futures::channel::mpsc::UnboundedSender;
use kbf_proto::update as wire;
use kbf_proto::worker::{self, DaemonMessage, daemon_message};
use kbf_types::LeaseId;
use kbf_update::{Handle, Order, Status};
use tokio::sync::watch;

use crate::lease::Leases;

/// The update client as the session sees it: a client, a node that updates by hand, or
/// nothing at all (the default, which sends no status).
pub struct UpdateLink {
    /// The client; `None` for a node that updates by hand or has no link.
    handle: Option<Handle>,
    /// The client's status, or the one fixed status of a node that updates by hand;
    /// `None` for no link.
    status: Option<watch::Receiver<Status>>,
    /// Keeps the fixed status's channel open.
    _fixed: Option<watch::Sender<Status>>,
    /// The lease epoch the newest Welcome named (0 for none): the only epoch an order
    /// may carry.
    epoch: u64,
    /// The status this stream last carried.
    sent: Option<Status>,
    /// Whether the update gate was closed at the last look.
    gated: bool,
}

impl UpdateLink {
    fn with(handle: Option<Handle>, status: Option<watch::Receiver<Status>>) -> Self {
        Self {
            handle,
            status,
            _fixed: None,
            epoch: 0,
            sent: None,
            gated: false,
        }
    }

    /// No link: the daemon sends no status and ignores offers and orders.
    #[must_use]
    pub fn off() -> Self {
        Self::with(None, None)
    }

    /// A link to the update client behind `handle`.
    #[must_use]
    pub fn new(handle: Handle) -> Self {
        let status = handle.status();
        Self::with(Some(handle), Some(status))
    }

    /// A link for a node that updates by hand, running the build `running_commit`: it
    /// reports that, and ignores what it is offered or ordered.
    #[must_use]
    pub fn manual(running_commit: &str) -> Self {
        let (fixed, status) = watch::channel(Status::manual(running_commit));
        Self {
            _fixed: Some(fixed),
            ..Self::with(None, Some(status))
        }
    }

    /// A new stream began and its Welcome named lease epoch `epoch` (0: none). The
    /// status goes out on it, read afresh from the helper.
    pub(crate) fn new_stream(&mut self, epoch: u64, tx: &UnboundedSender<DaemonMessage>) {
        self.epoch = epoch;
        self.sent = None;
        if let Some(handle) = &self.handle {
            handle.refresh();
        }
        self.settle(tx);
    }

    /// Hands the client the offer the frontend sent.
    pub(crate) fn offer(&self, offer: wire::UpdateOffer) {
        match &self.handle {
            Some(handle) => handle.offer(offer.into()),
            None => tracing::debug!("an update offer ignored: this node takes none"),
        }
    }

    /// Hands the client the order the frontend sent. The leases that are live now are
    /// counted again just before an activation is written.
    pub(crate) fn order(
        &self,
        order: wire::UpdateOrder,
        leases: &Leases,
        unacked: &BTreeMap<LeaseId, worker::Result>,
    ) {
        let Some(handle) = &self.handle else {
            tracing::debug!("an update order ignored: this node takes none");
            return;
        };
        let order = Order::from(order);
        let id = order.order_id;
        let decision = handle.order(&order, self.epoch, &|| live(leases, unacked));
        tracing::info!(order = id, ?decision, "update order handled");
    }

    /// Sends the status if it is not what this stream last carried. Returns whether the
    /// update gate opened or closed since the last call, which changes what a Hello says.
    pub(crate) fn settle(&mut self, tx: &UnboundedSender<DaemonMessage>) -> bool {
        let Some(status) = &mut self.status else {
            return false;
        };
        let status = status.borrow_and_update().clone();
        let gate_changed = status.gated != self.gated;
        self.gated = status.gated;
        if self.sent.as_ref() != Some(&status) {
            let message = daemon_message::Message::UpdateStatus(wire::UpdateStatus::from(&status));
            crate::daemon::send(tx, message);
            self.sent = Some(status);
        }
        gate_changed
    }

    /// While the update gate is closed, why a lease cannot start.
    pub(crate) fn gate(&self) -> Option<String> {
        self.handle.as_ref().and_then(Handle::gate)
    }

    /// Completes when the status changes; never without a link, or when its client is
    /// gone (`None` then disables the select arm).
    pub(crate) async fn changed(&mut self) -> Option<()> {
        self.status.as_mut()?.changed().await.ok()
    }
}

/// The leases that are live: those running, and those whose Result went out and has not
/// been acknowledged (the scheduler still counts them as the node's).
fn live(leases: &Leases, unacked: &BTreeMap<LeaseId, worker::Result>) -> usize {
    let mut live = leases.running();
    live.extend(unacked.keys().copied());
    live.sort();
    live.dedup();
    live.len()
}
