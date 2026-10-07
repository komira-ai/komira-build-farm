//! Raft's safety properties (Figure 3 of the paper), checked after every simulation
//! step against what every node has done so far, and the effect-order contract they
//! rest on: a message that promises durable state leaves only after that state is
//! persisted.

use std::collections::BTreeMap;

use kbf_raft::{Entry, LogId, LogIndex, Message, Payload, Role, ServerId, Term};

use crate::node::{Observed, RaftNode};

/// A broken property, with what showed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// Two servers led one term.
    ElectionSafety { term: Term, leaders: [ServerId; 2] },
    /// Two logs hold an entry with one id but different contents or prefixes.
    LogMatching { id: LogId, server: ServerId },
    /// A leader of a later term lacks an entry committed in an earlier one.
    LeaderCompleteness {
        leader: ServerId,
        term: Term,
        missing: LogId,
    },
    /// Two servers applied different entries at one index.
    StateMachineSafety { index: LogIndex, server: ServerId },
    /// A server promised durable state it had not persisted.
    UndurablePromise { server: ServerId, msg: Message },
    /// A server applied an index out of order.
    ApplyOrder { index: LogIndex, server: ServerId },
}

/// What the run has shown so far.
#[derive(Default)]
pub struct Checker {
    /// The one leader seen in each term.
    leaders: BTreeMap<Term, ServerId>,
    /// Every entry id persisted anywhere, with its payload and the term before it. By
    /// induction on the index, one (payload, previous term) per id is the log-matching
    /// property: equal ids imply equal entries and equal prefixes.
    persisted: BTreeMap<LogId, (Term, Payload)>,
    /// Every entry applied anywhere, with the lowest term any server was in when it
    /// applied it: an upper bound on the term the entry was committed in.
    applied: BTreeMap<LogIndex, (Entry, Term)>,
    /// Bumped whenever `applied` changes.
    applied_version: u64,
    /// How much of each node's history has been checked.
    seen: BTreeMap<ServerId, usize>,
    /// `applied_version` when each leader's log was last compared against `applied`.
    compared: BTreeMap<(ServerId, Term), u64>,
}

impl Checker {
    /// Checks everything that changed since the last call.
    pub fn check<'a>(
        &mut self,
        nodes: impl Iterator<Item = &'a RaftNode>,
    ) -> Result<(), Violation> {
        let nodes: Vec<&RaftNode> = nodes.collect();
        for node in &nodes {
            self.check_history(node)?;
        }
        for node in &nodes {
            self.check_leader(node)?;
        }
        Ok(())
    }

    fn check_history(&mut self, node: &RaftNode) -> Result<(), Violation> {
        let server = node.id();
        let seen = self.seen.entry(server).or_default();
        let new = &node.history()[*seen..];
        *seen = node.history().len();
        for event in new {
            match event {
                Observed::Persisted { prev_term, entry } => {
                    let it = (*prev_term, entry.payload.clone());
                    let known = self.persisted.entry(entry.id).or_insert_with(|| it.clone());
                    if *known != it {
                        return Err(Violation::LogMatching {
                            id: entry.id,
                            server,
                        });
                    }
                }
                Observed::UndurablePromise { msg } => {
                    let msg = msg.clone();
                    return Err(Violation::UndurablePromise { server, msg });
                }
                Observed::Applied {
                    term,
                    entry,
                    in_order,
                } => {
                    let index = entry.id.index;
                    if !in_order {
                        return Err(Violation::ApplyOrder { index, server });
                    }
                    let known = self.applied.entry(index).or_insert_with(|| {
                        self.applied_version += 1;
                        (entry.clone(), *term)
                    });
                    if known.0 != *entry {
                        return Err(Violation::StateMachineSafety { index, server });
                    }
                    if *term < known.1 {
                        known.1 = *term;
                        self.applied_version += 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn check_leader(&mut self, node: &RaftNode) -> Result<(), Violation> {
        let Some(core) = node.core() else {
            return Ok(());
        };
        if core.role() != Role::Leader {
            return Ok(());
        }
        let server = node.id();
        let term = core.hard_state().term;
        let first = *self.leaders.entry(term).or_insert(server);
        if first != server {
            return Err(Violation::ElectionSafety {
                term,
                leaders: [first, server],
            });
        }
        // A leader's log only grows within its term, so it need be compared only
        // when what has been applied changed since the last comparison.
        let compared = self.compared.entry((server, term)).or_default();
        if *compared == self.applied_version {
            return Ok(());
        }
        *compared = self.applied_version;
        let log = core.entries();
        for (index, (entry, committed_by)) in &self.applied {
            if *committed_by >= term {
                continue;
            }
            let held = usize::try_from(index.0 - 1).ok().and_then(|i| log.get(i));
            if held != Some(entry) {
                return Err(Violation::LeaderCompleteness {
                    leader: server,
                    term,
                    missing: entry.id,
                });
            }
        }
        Ok(())
    }
}
