//! The values Raft talks about: servers, terms, log positions, entries, the durable
//! vote, group membership and the core's configuration.

use std::collections::BTreeSet;
use std::fmt;

use thiserror::Error;

/// A server in a Raft group. The caller maps it to an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerId(pub u64);

impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// A Raft term. Terms only grow; zero is the term before any election.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Term(pub u64);

/// A position in the log. The first entry is at index 1; index 0 is the empty prefix.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogIndex(pub u64);

impl LogIndex {
    /// The next index.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// The previous index, or zero at zero.
    #[must_use]
    pub const fn prev(self) -> Self {
        Self(self.0.saturating_sub(1))
    }
}

/// An entry's place: its index and the term of the leader that created it. Two logs
/// that hold the same `LogId` hold the same entry and the same prefix before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogId {
    /// The term the entry was created in.
    pub term: Term,
    /// Where the entry sits.
    pub index: LogIndex,
}

impl LogId {
    /// The id of the entry at `index` created in `term`.
    #[must_use]
    pub const fn new(term: Term, index: LogIndex) -> Self {
        Self { term, index }
    }
}

/// One log entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Where the entry sits and who created it.
    pub id: LogId,
    /// What it carries.
    pub payload: Payload,
}

/// What an entry carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    /// The entry a new leader appends at the start of its term, so that it can commit
    /// the entries of earlier terms (which it may not count replicas for). The state
    /// machine skips it.
    Blank,
    /// A command for the replicated state machine, opaque to Raft.
    Command(Vec<u8>),
}

/// What a server must hold durably about elections: the latest term it has seen and
/// whom it voted for in that term. It is written before any message that depends on
/// it is sent (see [`Effect`](crate::Effect)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HardState {
    /// The latest term seen.
    pub term: Term,
    /// The candidate voted for in `term`, if any.
    pub voted_for: Option<ServerId>,
}

/// Who is in the group: voters elect leaders and form quorums; learners receive the
/// log and apply it but never vote, campaign or count towards a quorum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Membership {
    voters: BTreeSet<ServerId>,
    learners: BTreeSet<ServerId>,
}

impl Membership {
    /// The group with `voters` and `learners`.
    ///
    /// # Errors
    ///
    /// [`ConfigError::NoVoters`] if `voters` is empty, [`ConfigError::VoterAndLearner`]
    /// if a server is in both.
    pub fn new(
        voters: impl IntoIterator<Item = ServerId>,
        learners: impl IntoIterator<Item = ServerId>,
    ) -> Result<Self, ConfigError> {
        let voters: BTreeSet<ServerId> = voters.into_iter().collect();
        let learners: BTreeSet<ServerId> = learners.into_iter().collect();
        if voters.is_empty() {
            return Err(ConfigError::NoVoters);
        }
        if let Some(&both) = voters.intersection(&learners).next() {
            return Err(ConfigError::VoterAndLearner(both));
        }
        Ok(Self { voters, learners })
    }

    /// The voters, in order.
    pub fn voters(&self) -> impl Iterator<Item = ServerId> + '_ {
        self.voters.iter().copied()
    }

    /// The learners, in order.
    pub fn learners(&self) -> impl Iterator<Item = ServerId> + '_ {
        self.learners.iter().copied()
    }

    /// Whether `id` votes.
    #[must_use]
    pub fn is_voter(&self, id: ServerId) -> bool {
        self.voters.contains(&id)
    }

    /// Whether `id` is a voter or a learner.
    #[must_use]
    pub fn contains(&self, id: ServerId) -> bool {
        self.voters.contains(&id) || self.learners.contains(&id)
    }

    /// How many voters make a majority.
    #[must_use]
    pub fn quorum(&self) -> usize {
        self.voters.len() / 2 + 1
    }
}

/// How a Raft core behaves. Time is counted in ticks, which the caller delivers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// This server.
    pub id: ServerId,
    /// The group.
    pub membership: Membership,
    /// The shortest election timeout, in ticks. Each timeout is drawn from
    /// `election_ticks..2 * election_ticks` with the entropy the caller passes in.
    pub election_ticks: u32,
    /// How often a leader sends to each follower when it has nothing new, in ticks.
    pub heartbeat_ticks: u32,
    /// The most entries one append request carries.
    pub max_entries_per_append: usize,
}

impl Config {
    /// Checks the configuration.
    ///
    /// # Errors
    ///
    /// A [`ConfigError`] naming the first problem found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.membership.contains(self.id) {
            return Err(ConfigError::NotAMember(self.id));
        }
        if self.heartbeat_ticks == 0 || self.election_ticks <= self.heartbeat_ticks {
            return Err(ConfigError::Ticks {
                election: self.election_ticks,
                heartbeat: self.heartbeat_ticks,
            });
        }
        if self.max_entries_per_append == 0 {
            return Err(ConfigError::ZeroBatch);
        }
        Ok(())
    }
}

/// A configuration or restored state the core refuses.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// A group needs at least one voter.
    #[error("a group needs at least one voter")]
    NoVoters,
    /// A server cannot be a voter and a learner at once.
    #[error("{0} is both a voter and a learner")]
    VoterAndLearner(ServerId),
    /// The configured server is not in the group.
    #[error("{0} is not a member of the group")]
    NotAMember(ServerId),
    /// The heartbeat must be at least one tick and shorter than the election timeout.
    #[error(
        "heartbeat of {heartbeat} ticks must be at least 1 and below the election timeout of {election}"
    )]
    Ticks {
        /// The configured election timeout.
        election: u32,
        /// The configured heartbeat.
        heartbeat: u32,
    },
    /// An append request must be able to carry an entry.
    #[error("max_entries_per_append must be at least 1")]
    ZeroBatch,
    /// Restored entries must be numbered 1, 2, 3, ... with terms that never fall and
    /// never exceed the restored term.
    #[error("restored log is not contiguous at entry {0:?}")]
    BadLog(LogId),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: &[u64]) -> Vec<ServerId> {
        n.iter().copied().map(ServerId).collect()
    }

    /// Catches: a quorum that is not a strict majority (two disjoint quorums could
    /// each elect a leader), and a membership that lets a server vote and learn.
    #[test]
    fn quorum_is_a_strict_majority_and_roles_are_disjoint() {
        let q = |n: u64| Membership::new((1..=n).map(ServerId), []).unwrap().quorum();
        assert_eq!([q(1), q(2), q(3), q(4), q(5)], [1, 2, 2, 3, 3]);
        assert_eq!(Membership::new([], ids(&[1])), Err(ConfigError::NoVoters));
        assert_eq!(
            Membership::new(ids(&[1, 2]), ids(&[2])),
            Err(ConfigError::VoterAndLearner(ServerId(2)))
        );
        let m = Membership::new(ids(&[1, 2, 3]), ids(&[4])).unwrap();
        assert!(m.is_voter(ServerId(1)) && !m.is_voter(ServerId(4)));
        assert!(m.contains(ServerId(4)) && !m.contains(ServerId(5)));
    }

    /// Catches: a configuration whose heartbeat is not shorter than the election
    /// timeout (followers would time out between heartbeats), or that names a server
    /// outside the group.
    #[test]
    fn config_validation() {
        let base = Config {
            id: ServerId(1),
            membership: Membership::new(ids(&[1, 2, 3]), []).unwrap(),
            election_ticks: 10,
            heartbeat_ticks: 3,
            max_entries_per_append: 8,
        };
        assert_eq!(base.validate(), Ok(()));
        let bad = |f: fn(&mut Config)| {
            let mut c = base.clone();
            f(&mut c);
            c.validate().unwrap_err()
        };
        assert_eq!(
            bad(|c| c.id = ServerId(9)),
            ConfigError::NotAMember(ServerId(9))
        );
        assert!(matches!(
            bad(|c| c.heartbeat_ticks = 10),
            ConfigError::Ticks { .. }
        ));
        assert!(matches!(
            bad(|c| c.heartbeat_ticks = 0),
            ConfigError::Ticks { .. }
        ));
        assert_eq!(
            bad(|c| c.max_entries_per_append = 0),
            ConfigError::ZeroBatch
        );
    }
}
