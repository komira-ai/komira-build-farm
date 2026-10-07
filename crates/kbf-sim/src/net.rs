//! Node names, link faults and partitions.

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use crate::Chance;

/// The name of a simulated node.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

impl NodeId {
    /// The node called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for NodeId {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the network does to each message sent between nodes.
///
/// Each message is first dropped with probability `drop`. A surviving message is sent
/// twice with probability `duplicate`. Each copy gets a delay drawn uniformly from
/// `min_delay..=max_delay` (whole milliseconds). Copies on one link (sender, receiver)
/// arrive in the order they were sent unless `reorder` lets a copy skip that rule and
/// arrive whenever its own delay says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Faults {
    /// Shortest delivery delay.
    pub min_delay: Duration,
    /// Longest delivery delay.
    pub max_delay: Duration,
    /// Chance a message is lost.
    pub drop: Chance,
    /// Chance a message is delivered twice.
    pub duplicate: Chance,
    /// Chance a copy may overtake earlier messages on its link.
    pub reorder: Chance,
}

impl Default for Faults {
    /// 1 to 10 ms of delay, nothing lost, duplicated or reordered.
    fn default() -> Self {
        Self {
            min_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            drop: Chance::never(),
            duplicate: Chance::never(),
            reorder: Chance::never(),
        }
    }
}

/// Which nodes can reach which.
///
/// A partition is a list of groups. Two nodes can exchange messages when they are in
/// the same group; nodes listed in no group form one more group together, and a node
/// can always reach itself. The default, [`Partition::none`], lists no groups, so every
/// node reaches every other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Partition {
    groups: Vec<BTreeSet<NodeId>>,
}

impl Partition {
    /// No partition: every node reaches every other.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The partition into `groups`.
    ///
    /// # Panics
    ///
    /// If a node is listed in two groups.
    #[must_use]
    pub fn new<G, I>(groups: G) -> Self
    where
        G: IntoIterator<Item = I>,
        I: IntoIterator<Item = NodeId>,
    {
        let groups: Vec<BTreeSet<NodeId>> = groups
            .into_iter()
            .map(|g| g.into_iter().collect())
            .collect();
        let mut seen = BTreeSet::new();
        for node in groups.iter().flatten() {
            assert!(seen.insert(node), "{node} is in two partition groups");
        }
        Self { groups }
    }

    /// Whether a message from `a` can reach `b`.
    #[must_use]
    pub fn connected(&self, a: &NodeId, b: &NodeId) -> bool {
        a == b || self.group_of(a) == self.group_of(b)
    }

    fn group_of(&self, node: &NodeId) -> Option<usize> {
        self.groups.iter().position(|g| g.contains(node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<NodeId> {
        names.iter().map(|n| NodeId::from(*n)).collect()
    }

    /// Catches: a partition that blocks traffic inside a group, lets traffic cross
    /// groups, or isolates the nodes it does not list from each other.
    #[test]
    fn groups_decide_reachability() {
        let p = Partition::new([ids(&["a", "b"]), ids(&["c"])]);
        let [a, b, c, d, e] = ["a", "b", "c", "d", "e"].map(NodeId::from);
        assert!(p.connected(&a, &b));
        assert!(!p.connected(&a, &c));
        assert!(!p.connected(&c, &b));
        assert!(p.connected(&c, &c));
        assert!(p.connected(&d, &e), "unlisted nodes share a group");
        assert!(!p.connected(&d, &a));
        assert!(Partition::none().connected(&a, &c));
    }

    #[test]
    #[should_panic(expected = "two partition groups")]
    fn a_node_in_two_groups_is_refused() {
        let _ = Partition::new([ids(&["a"]), ids(&["a"])]);
    }
}
