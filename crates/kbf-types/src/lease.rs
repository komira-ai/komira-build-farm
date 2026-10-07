//! Lease identifiers.

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

    /// Catches: a text form that drops or swaps a field, which makes logs ambiguous.
    #[test]
    fn displays_term_dot_seq() {
        assert_eq!(LeaseId::new(7, 42).to_string(), "7.42");
    }
}
