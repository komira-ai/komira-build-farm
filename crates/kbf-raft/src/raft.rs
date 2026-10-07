//! The Raft core: election, replication and commit, as a sans-IO state machine.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::log::Log;
use crate::{
    AppendOutcome, Config, ConfigError, Effect, Entry, HardState, LogId, LogIndex, Message,
    MessageKind, Payload, ServerId, Term,
};

/// What a server is doing in its current term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Following a leader, or waiting for one. Learners are always followers.
    Follower,
    /// Asking for votes.
    Candidate,
    /// Leading the term.
    Leader,
}

/// A proposal reached a server that is not the leader.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("not the leader; the leader known here is {leader:?}")]
pub struct NotLeader {
    /// The leader this server last heard from in its current term, if any.
    pub leader: Option<ServerId>,
}

/// An accepted proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub struct Proposed {
    /// Where the command sits in the log. It is committed when an [`Effect::Apply`]
    /// carries this index.
    pub index: LogIndex,
    /// The effects to carry out.
    pub effects: Vec<Effect>,
}

/// What the leader knows about one follower's log.
#[derive(Clone, Copy, Debug)]
struct Progress {
    /// The next index to send.
    next: LogIndex,
    /// The highest index known to match and be durable there.
    matched: LogIndex,
}

#[derive(Clone, Debug)]
enum State {
    Follower,
    Candidate {
        votes: BTreeSet<ServerId>,
    },
    Leader {
        progress: BTreeMap<ServerId, Progress>,
        since_heartbeat: u32,
    },
}

/// One server's Raft state machine.
///
/// It reads no clock, draws no random numbers, starts no threads and does no I/O. The
/// caller delivers ticks ([`Raft::tick`]), messages ([`Raft::receive`]) and commands
/// ([`Raft::propose`]), with 64 bits of entropy for the election timeout, and carries
/// out the returned [`Effect`]s in order. Given the same inputs, two cores make the
/// same decisions, so a simulation seed replays them exactly.
#[derive(Clone, Debug)]
pub struct Raft {
    config: Config,
    hard: HardState,
    log: Log,
    commit: LogIndex,
    applied: LogIndex,
    state: State,
    leader: Option<ServerId>,
    /// Ticks since the election timer was last reset.
    elapsed: u32,
    /// The current randomized election timeout, in ticks.
    timeout: u32,
    effects: Vec<Effect>,
}

impl Raft {
    /// A server with an empty log, in term 0.
    ///
    /// # Errors
    ///
    /// The [`ConfigError`] that [`Config::validate`] finds.
    pub fn new(config: Config, entropy: u64) -> Result<Self, ConfigError> {
        Self::restore(config, HardState::default(), Vec::new(), entropy)
    }

    /// A server restarted from what it persisted: the last [`HardState`] and the
    /// entries, as its earlier persist effects left them. It starts as a follower with
    /// nothing known to be committed, and applies again from the start as the leader
    /// tells it what is committed.
    ///
    /// # Errors
    ///
    /// The [`ConfigError`] that [`Config::validate`] finds, or
    /// [`ConfigError::BadLog`] if the entries are not numbered from 1, have a falling
    /// term, or end in a term later than `hard.term`.
    pub fn restore(
        config: Config,
        hard: HardState,
        entries: Vec<Entry>,
        entropy: u64,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        let log = Log::restore(entries).map_err(ConfigError::BadLog)?;
        if log.last_id().term > hard.term {
            return Err(ConfigError::BadLog(log.last_id()));
        }
        let mut raft = Self {
            config,
            hard,
            log,
            commit: LogIndex(0),
            applied: LogIndex(0),
            state: State::Follower,
            leader: None,
            elapsed: 0,
            timeout: 0,
            effects: Vec::new(),
        };
        raft.reset_election_timer(entropy);
        Ok(raft)
    }

    /// This server.
    #[must_use]
    pub fn id(&self) -> ServerId {
        self.config.id
    }

    /// The current role.
    #[must_use]
    pub fn role(&self) -> Role {
        match self.state {
            State::Follower => Role::Follower,
            State::Candidate { .. } => Role::Candidate,
            State::Leader { .. } => Role::Leader,
        }
    }

    /// The current term and vote.
    #[must_use]
    pub fn hard_state(&self) -> HardState {
        self.hard
    }

    /// The leader of the current term, if this server knows it.
    #[must_use]
    pub fn leader(&self) -> Option<ServerId> {
        self.leader
    }

    /// The highest index known to be committed.
    #[must_use]
    pub fn commit_index(&self) -> LogIndex {
        self.commit
    }

    /// The log, in index order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        self.log.entries()
    }

    /// The last entry's id (index 0, term 0 when the log is empty).
    #[must_use]
    pub fn last_log_id(&self) -> LogId {
        self.log.last_id()
    }

    /// One tick of logical time. A follower or candidate whose election timeout has run
    /// out starts an election; a leader sends heartbeats when they are due. `entropy`
    /// draws the next election timeout if one is reset.
    #[must_use]
    pub fn tick(&mut self, entropy: u64) -> Vec<Effect> {
        if let State::Leader {
            since_heartbeat, ..
        } = &mut self.state
        {
            *since_heartbeat += 1;
            if *since_heartbeat >= self.config.heartbeat_ticks {
                *since_heartbeat = 0;
                self.broadcast_append();
            }
        } else if self.config.membership.is_voter(self.config.id) {
            self.elapsed += 1;
            if self.elapsed >= self.timeout {
                self.campaign(entropy);
            }
        }
        self.take_effects()
    }

    /// A message from `from`. Messages from servers outside the group are ignored.
    #[must_use]
    pub fn receive(&mut self, from: ServerId, msg: Message, entropy: u64) -> Vec<Effect> {
        if from == self.config.id || !self.config.membership.contains(from) {
            return Vec::new();
        }
        if msg.term > self.hard.term {
            self.become_follower(msg.term, entropy);
        }
        if msg.term < self.hard.term {
            self.answer_stale(from, &msg.kind);
            return self.take_effects();
        }
        match msg.kind {
            MessageKind::VoteRequest { last_log } => self.on_vote_request(from, last_log, entropy),
            MessageKind::VoteResponse { granted } => self.on_vote_response(from, granted),
            MessageKind::AppendRequest {
                prev,
                entries,
                commit,
            } => self.on_append_request(from, prev, entries, commit, entropy),
            MessageKind::AppendResponse { outcome } => self.on_append_response(from, outcome),
        }
        self.take_effects()
    }

    /// Appends `command` to the log, if this server leads.
    ///
    /// # Errors
    ///
    /// [`NotLeader`], with the leader this server knows of, when it does not lead.
    pub fn propose(&mut self, command: Vec<u8>) -> Result<Proposed, NotLeader> {
        if !matches!(self.state, State::Leader { .. }) {
            return Err(NotLeader {
                leader: self.leader,
            });
        }
        let index = self.append_local(Payload::Command(command));
        self.broadcast_append();
        self.advance_commit();
        Ok(Proposed {
            index,
            effects: self.take_effects(),
        })
    }

    fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    fn send(&mut self, to: ServerId, kind: MessageKind) {
        let msg = Message {
            term: self.hard.term,
            kind,
        };
        self.effects.push(Effect::Send { to, msg });
    }

    fn persist_hard_state(&mut self) {
        self.effects.push(Effect::PersistHardState(self.hard));
    }

    fn reset_election_timer(&mut self, entropy: u64) {
        let span = u64::from(self.config.election_ticks);
        // The remainder is below `election_ticks`, a u32, so the cast keeps every bit.
        let jitter = (entropy % span) as u32;
        self.elapsed = 0;
        self.timeout = self.config.election_ticks + jitter;
    }

    /// Adopts `term` (a later one than the current) as a follower with no vote cast.
    fn become_follower(&mut self, term: Term, entropy: u64) {
        self.hard = HardState {
            term,
            voted_for: None,
        };
        self.persist_hard_state();
        self.state = State::Follower;
        self.leader = None;
        self.reset_election_timer(entropy);
    }

    fn campaign(&mut self, entropy: u64) {
        let me = self.config.id;
        self.hard = HardState {
            term: Term(self.hard.term.0 + 1),
            voted_for: Some(me),
        };
        self.persist_hard_state();
        self.state = State::Candidate {
            votes: BTreeSet::from([me]),
        };
        self.leader = None;
        self.reset_election_timer(entropy);
        let last_log = self.log.last_id();
        let peers: Vec<ServerId> = self
            .config
            .membership
            .voters()
            .filter(|&v| v != me)
            .collect();
        for peer in peers {
            self.send(peer, MessageKind::VoteRequest { last_log });
        }
        self.maybe_win();
    }

    /// Answers a request from an earlier term with the current term, so the sender
    /// steps down. Responses from earlier terms are dropped.
    fn answer_stale(&mut self, from: ServerId, kind: &MessageKind) {
        match kind {
            MessageKind::VoteRequest { .. } => {
                self.send(from, MessageKind::VoteResponse { granted: false });
            }
            MessageKind::AppendRequest { prev, .. } => {
                let outcome = AppendOutcome::Rejected {
                    at: prev.index,
                    last: self.log.last_index(),
                };
                self.send(from, MessageKind::AppendResponse { outcome });
            }
            MessageKind::VoteResponse { .. } | MessageKind::AppendResponse { .. } => {}
        }
    }

    fn on_vote_request(&mut self, from: ServerId, last_log: LogId, entropy: u64) {
        let membership = &self.config.membership;
        let can_vote = self.hard.voted_for.is_none_or(|v| v == from);
        // LogId orders by term, then index: the paper's "at least as up-to-date".
        let up_to_date = last_log >= self.log.last_id();
        let granted = can_vote
            && up_to_date
            && membership.is_voter(self.config.id)
            && membership.is_voter(from);
        if granted {
            self.hard.voted_for = Some(from);
            self.persist_hard_state();
            self.reset_election_timer(entropy);
        }
        self.send(from, MessageKind::VoteResponse { granted });
    }

    fn on_vote_response(&mut self, from: ServerId, granted: bool) {
        if let State::Candidate { votes } = &mut self.state
            && granted
            && self.config.membership.is_voter(from)
        {
            votes.insert(from);
            self.maybe_win();
        }
    }

    fn maybe_win(&mut self) {
        let State::Candidate { votes } = &self.state else {
            return;
        };
        if votes.len() >= self.config.membership.quorum() {
            self.become_leader();
        }
    }

    fn become_leader(&mut self) {
        let me = self.config.id;
        let next = self.log.last_index().next();
        let matched = LogIndex(0);
        let progress = self
            .peers()
            .into_iter()
            .map(|p| (p, Progress { next, matched }));
        self.state = State::Leader {
            progress: progress.collect(),
            since_heartbeat: 0,
        };
        self.leader = Some(me);
        // A leader may count replicas only for entries of its own term, so it commits
        // the entries of earlier terms by committing this one.
        self.append_local(Payload::Blank);
        self.broadcast_append();
        self.advance_commit();
    }

    /// Every member but this server, voters and learners.
    fn peers(&self) -> Vec<ServerId> {
        let me = self.config.id;
        let m = &self.config.membership;
        m.voters()
            .chain(m.learners())
            .filter(|&p| p != me)
            .collect()
    }

    /// Appends one entry in the current term to the leader's own log.
    fn append_local(&mut self, payload: Payload) -> LogIndex {
        let id = LogId::new(self.hard.term, self.log.last_index().next());
        let entry = Entry { id, payload };
        self.log.push(entry.clone());
        self.effects.push(Effect::PersistEntries(vec![entry]));
        id.index
    }

    fn broadcast_append(&mut self) {
        for peer in self.peers() {
            self.send_append(peer);
        }
    }

    fn send_append(&mut self, to: ServerId) {
        let State::Leader { progress, .. } = &self.state else {
            return;
        };
        let Some(p) = progress.get(&to) else {
            return;
        };
        let prev_index = p.next.prev();
        let Some(prev_term) = self.log.term_at(prev_index) else {
            return;
        };
        let entries = self.log.slice(p.next, self.config.max_entries_per_append);
        let kind = MessageKind::AppendRequest {
            prev: LogId::new(prev_term, prev_index),
            entries,
            commit: self.commit,
        };
        self.send(to, kind);
    }

    fn on_append_request(
        &mut self,
        from: ServerId,
        prev: LogId,
        entries: Vec<Entry>,
        commit: LogIndex,
        entropy: u64,
    ) {
        match self.state {
            // Two leaders in one term cannot happen; ignore rather than follow.
            State::Leader { .. } => return,
            State::Candidate { .. } => self.state = State::Follower,
            State::Follower => {}
        }
        self.leader = Some(from);
        self.reset_election_timer(entropy);
        if self.log.term_at(prev.index) != Some(prev.term) {
            let outcome = AppendOutcome::Rejected {
                at: prev.index,
                last: self.log.last_index(),
            };
            self.send(from, MessageKind::AppendResponse { outcome });
            return;
        }
        let numbered = (prev.index.0 + 1..).map(LogIndex);
        if numbered.zip(&entries).any(|(i, e)| e.id.index != i) {
            return; // malformed request: the entries do not follow `prev`
        }
        let matched = LogIndex(prev.index.0 + entries.len() as u64);
        let mut fresh = Vec::new();
        for entry in entries {
            let expected = entry.id.index;
            if fresh.is_empty() {
                match self.log.term_at(expected) {
                    Some(t) if t == entry.id.term => continue,
                    // Never drop a committed entry; a leader that asks is broken.
                    Some(_) if expected <= self.commit => return,
                    Some(_) => self.log.truncate_from(expected),
                    None => {}
                }
            }
            self.log.push(entry.clone());
            fresh.push(entry);
        }
        if !fresh.is_empty() {
            self.effects.push(Effect::PersistEntries(fresh));
        }
        // The acknowledgement follows the persist effect: it promises durability.
        let outcome = AppendOutcome::Accepted { matched };
        self.send(from, MessageKind::AppendResponse { outcome });
        // Only the prefix this request proved may be committed: entries past `matched`
        // may be stale ones from an earlier leader.
        let known = commit.min(matched);
        if known > self.commit {
            self.commit = known;
            self.apply_committed();
        }
    }

    fn on_append_response(&mut self, from: ServerId, outcome: AppendOutcome) {
        let last = self.log.last_index();
        let State::Leader { progress, .. } = &mut self.state else {
            return;
        };
        let Some(p) = progress.get_mut(&from) else {
            return;
        };
        match outcome {
            AppendOutcome::Accepted { matched } => {
                if matched > last {
                    return; // not an answer to anything this leader sent
                }
                p.matched = p.matched.max(matched);
                p.next = p.next.max(matched.next());
                let behind = p.next <= last;
                self.advance_commit();
                if behind {
                    self.send_append(from);
                }
            }
            AppendOutcome::Rejected { at, last: theirs } => {
                if at >= p.next {
                    return; // answers a request older than the current position
                }
                p.next = at.min(theirs.next()).max(p.matched.next());
                self.send_append(from);
            }
        }
    }

    /// Moves the commit index to the highest index of the current term that a majority
    /// of voters hold.
    fn advance_commit(&mut self) {
        let State::Leader { progress, .. } = &self.state else {
            return;
        };
        let me = self.config.id;
        let last = self.log.last_index();
        let quorum = self.config.membership.quorum();
        let mut n = last;
        while n > self.commit {
            // Raft's commit rule (Figure 8 in the paper): never count replicas for an
            // entry of an earlier term; it is committed by a later entry of this term.
            if self.log.term_at(n) != Some(self.hard.term) {
                return;
            }
            let held = self
                .config
                .membership
                .voters()
                .filter(|&v| v == me || progress.get(&v).is_some_and(|p| p.matched >= n))
                .count();
            if held >= quorum {
                self.commit = n;
                self.apply_committed();
                return;
            }
            n = n.prev();
        }
    }

    fn apply_committed(&mut self) {
        while self.applied < self.commit {
            // The commit index never passes the end of the log, so this always finds one.
            let Some(entry) = self.log.entry(self.applied.next()) else {
                break;
            };
            self.effects.push(Effect::Apply(entry.clone()));
            self.applied = self.applied.next();
        }
    }
}
