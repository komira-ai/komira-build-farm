//! Lease identifiers and lease kinds.

use std::fmt;

/// Identifies one lease: one grant of capacity to run an action.
///
/// `term` is the consensus term of the leader that granted the lease and `seq` counts
/// grants within that term. Leases order by `(term, seq)`: every lease granted by a
/// later leader is newer than every lease granted by an earlier one, whatever their
/// `seq`. A worker uses this order to tell a stale grant from a current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseId {
    // Field order is the ordering: the derives compare `term` first, then `seq`.
    /// The term of the leader that granted the lease.
    pub term: u64,
    /// The grant's sequence number within `term`.
    pub seq: u64,
}

impl LeaseId {
    /// Builds a lease id.
    #[must_use]
    pub const fn new(term: u64, seq: u64) -> Self {
        Self { term, seq }
    }
}

impl fmt::Display for LeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.term, self.seq)
    }
}

/// What a lease takes on its worker, named by an action's `kbf-lease` platform property.
///
/// Placement gives a lease only to a worker whose node report lists a driver that
/// serves its kind ([`LeaseKind::drivers`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LeaseKind {
    /// A share of a machine: the cores, memory and GPUs its request books. The default.
    #[default]
    Action,
    /// The whole machine: every core, byte and GPU of the worker, with no other lease
    /// beside it.
    WholeMachine,
    /// A macOS guest VM. Planned: no front accepts it and no driver reports `vm` yet.
    Vm,
}

impl LeaseKind {
    /// Every kind, in order.
    pub const ALL: [Self; 3] = [Self::Action, Self::WholeMachine, Self::Vm];

    /// Its name, as `kbf-lease` and the worker protocol's `Start.kind` spell it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::WholeMachine => "whole_machine",
            Self::Vm => "vm",
        }
    }

    /// The kind named `name`, if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The node report `drivers` values that serve this kind.
    #[must_use]
    pub const fn drivers(self) -> &'static [&'static str] {
        match self {
            Self::Action => &["container", "native", "fake"],
            Self::WholeMachine => &["native-whole-machine"],
            Self::Vm => &["vm"],
        }
    }

    /// Whether a worker that reports driver `driver` serves this kind.
    #[must_use]
    pub fn served_by(self, driver: &str) -> bool {
        self.drivers().contains(&driver)
    }
}

impl fmt::Display for LeaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: an order on `seq` alone (or `seq` before `term`), under which a worker
    /// would accept a grant from a deposed leader over a newer leader's grant.
    #[test]
    fn orders_by_term_then_seq() {
        let old_leader = LeaseId::new(1, 900);
        let new_leader = LeaseId::new(2, 1);
        assert!(old_leader < new_leader);
        assert!(LeaseId::new(2, 1) < LeaseId::new(2, 2));
        assert_eq!(LeaseId::new(3, 4), LeaseId::new(3, 4));

        let mut ids = vec![
            LeaseId::new(2, 1),
            LeaseId::new(1, 7),
            LeaseId::new(2, 0),
            LeaseId::new(1, 9),
        ];
        ids.sort();
        let want = [(1, 7), (1, 9), (2, 0), (2, 1)].map(|(t, s)| LeaseId::new(t, s));
        assert_eq!(ids, want);
    }

    /// Catches: a kind spelt differently from `kbf-lease` and `Start.kind`, a name that
    /// does not read back, and a driver serving a kind it does not run (a container
    /// worker offered a whole-machine lease, or a whole-machine driver offered actions).
    #[test]
    fn kinds_name_and_drivers() {
        for kind in LeaseKind::ALL {
            assert_eq!(LeaseKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(LeaseKind::from_name("whole-machine"), None);
        assert_eq!(LeaseKind::default(), LeaseKind::Action);
        assert_eq!(LeaseKind::WholeMachine.to_string(), "whole_machine");
        for driver in ["container", "native", "fake"] {
            assert!(LeaseKind::Action.served_by(driver));
            assert!(!LeaseKind::WholeMachine.served_by(driver));
            assert!(!LeaseKind::Vm.served_by(driver));
        }
        assert!(LeaseKind::WholeMachine.served_by("native-whole-machine"));
        assert!(!LeaseKind::Action.served_by("native-whole-machine"));
        assert!(LeaseKind::Vm.served_by("vm"));
        assert!(!LeaseKind::Action.served_by("vm"));
    }

    /// Catches: a text form that drops or swaps a field, which makes logs ambiguous.
    #[test]
    fn displays_term_dot_seq() {
        assert_eq!(LeaseId::new(7, 42).to_string(), "7.42");
    }
}
