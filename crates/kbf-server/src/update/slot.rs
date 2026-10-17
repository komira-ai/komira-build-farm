//! `update/slot`: which hosts are in a step (`docs/design/split-mode-updates.md` sections
//! 2.1, 2.6 and 6.1).
//!
//! One row holds every holder, so one compare-and-set orders every claim and release. It has
//! two kinds of place, because the two kinds of host have different limits:
//!
//! - **storage**: one place. A storage-and-daemon host takes it, and no other storage host
//!   is in a step while it is held (R=2: two nodes out lose blobs, section 2.1).
//! - **host**: up to [`super::Policy::max_hosts_out`] places for the daemon-only hosts and
//!   the Macs, which have no storage coupling and may be in a step together (section 2.6).
//!
//! The two are separate budgets: a storage host and a daemon-only host may be in a step at
//! the same time, and the storage host does not use one of the daemon-only places.
//!
//! A holder is a host and the id of the order the claim was made with ([`OrderId`], which
//! carries the store's epoch). There is no time to live: a holder that dies leaves the place
//! held, and the run is held with it (section 6.1).

use kbf_types::WorkerId;

use super::facts::OrderId;

/// Which kind of place a host takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotClass {
    /// The one place for a storage-and-daemon host.
    Storage,
    /// One of the places for the daemon-only hosts and the Macs.
    Host,
}

/// A host that holds a place, and the claim it holds it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    /// The host.
    pub host: WorkerId,
    /// The order the claim was made with. It names the claim in the audit trail; the row's
    /// version is what the compare-and-set judges.
    pub order: OrderId,
}

/// The holders of `update/slot`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    /// The storage host in a step, if one is.
    pub storage: Option<Holder>,
    /// The daemon-only hosts and Macs in a step, in the order they claimed.
    pub hosts: Vec<Holder>,
}

impl Slot {
    /// Every holder with the kind of place it holds, the storage host first.
    pub fn holders(&self) -> impl Iterator<Item = (SlotClass, &Holder)> {
        let storage = self.storage.iter().map(|h| (SlotClass::Storage, h));
        storage.chain(self.hosts.iter().map(|h| (SlotClass::Host, h)))
    }

    /// The claim `host` holds a place by, if it holds one of either kind.
    #[must_use]
    pub fn holder(&self, host: &WorkerId) -> Option<&Holder> {
        self.holders()
            .map(|(_, holder)| holder)
            .find(|holder| holder.host == *host)
    }

    /// Whether `host` holds a place of either kind.
    #[must_use]
    pub fn holds(&self, host: &WorkerId) -> bool {
        self.holder(host).is_some()
    }

    /// Whether the slot has no holder.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.storage.is_none() && self.hosts.is_empty()
    }

    /// How many places of `class` are held.
    #[must_use]
    pub fn held(&self, class: SlotClass) -> usize {
        match class {
            SlotClass::Storage => usize::from(self.storage.is_some()),
            SlotClass::Host => self.hosts.len(),
        }
    }

    /// Whether a place of `class` is free, with `max_hosts_out` places for daemon-only hosts.
    #[must_use]
    pub fn has_room(&self, class: SlotClass, max_hosts_out: usize) -> bool {
        match class {
            SlotClass::Storage => self.storage.is_none(),
            SlotClass::Host => self.hosts.len() < max_hosts_out,
        }
    }

    /// Gives `holder` a place of `class` if there is room. Returns whether it holds one now:
    /// a host that already does keeps its place (the claim is repeatable), and a full class
    /// leaves the slot as it was.
    pub fn claim(&mut self, class: SlotClass, holder: Holder, max_hosts_out: usize) -> bool {
        if self.holds(&holder.host) {
            return true;
        }
        if !self.has_room(class, max_hosts_out) {
            return false;
        }
        match class {
            SlotClass::Storage => self.storage = Some(holder),
            SlotClass::Host => self.hosts.push(holder),
        }
        true
    }

    /// Frees the place `host` holds. Returns whether it held one.
    pub fn release(&mut self, host: &WorkerId) -> bool {
        let before = self.holders().count();
        if self.storage.as_ref().is_some_and(|h| h.host == *host) {
            self.storage = None;
        }
        self.hosts.retain(|h| h.host != *host);
        self.holders().count() != before
    }
}
