//! Scripted scenarios: cores driven message by message, so each rule is shown on the
//! exact interleaving that needs it. The seeded sweep in `tests/sim` covers the same
//! rules at random; these name the one schedule per rule.

use kbf_raft::{
    AppendOutcome, Config, Effect, Entry, HardState, LogId, LogIndex, Membership, Message,
    MessageKind, Payload, Raft, Role, ServerId, Term,
};

const S1: ServerId = ServerId(1);
const S2: ServerId = ServerId(2);
const S3: ServerId = ServerId(3);
const S4: ServerId = ServerId(4);

fn config(id: ServerId, voters: &[ServerId], learners: &[ServerId]) -> Config {
    Config {
        id,
        membership: Membership::new(voters.iter().copied(), learners.iter().copied()).unwrap(),
        election_ticks: 10,
        heartbeat_ticks: 3,
        max_entries_per_append: 1,
    }
}

fn three(id: ServerId) -> Config {
    config(id, &[S1, S2, S3], &[S4])
}

fn entry(term: u64, index: u64, payload: Payload) -> Entry {
    Entry {
        id: LogId::new(Term(term), LogIndex(index)),
        payload,
    }
}

fn cmd(term: u64, index: u64) -> Entry {
    entry(
        term,
        index,
        Payload::Command(format!("{term}@{index}").into_bytes()),
    )
}

fn restore(config: Config, term: u64, log: Vec<Entry>) -> Raft {
    let hard = HardState {
        term: Term(term),
        voted_for: None,
    };
    Raft::restore(config, hard, log, 0).unwrap()
}

/// Ticks until the core produces effects (an election or a heartbeat).
fn tick_until_active(raft: &mut Raft) -> Vec<Effect> {
    for _ in 0..100 {
        let effects = raft.tick(0);
        if !effects.is_empty() {
            return effects;
        }
    }
    panic!("{} never acted", raft.id());
}

/// The message sent to `to`, if exactly one.
fn sent_to(effects: &[Effect], to: ServerId) -> Message {
    let msgs: Vec<&Message> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Send { to: t, msg } if *t == to => Some(msg),
            _ => None,
        })
        .collect();
    assert_eq!(msgs.len(), 1, "messages to {to}: {msgs:?}");
    msgs[0].clone()
}

fn applied(effects: &[Effect]) -> Vec<LogIndex> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Apply(entry) => Some(entry.id.index),
            _ => None,
        })
        .collect()
}

fn position(effects: &[Effect], pred: impl Fn(&Effect) -> bool) -> usize {
    effects.iter().position(pred).expect("effect present")
}

/// Catches: a one-voter group that cannot elect itself or commit (kbf's smallest
/// deployment runs one voter), e.g. a quorum count that leaves the leader out.
#[test]
fn a_single_voter_elects_itself_and_commits_alone() {
    let mut raft = Raft::new(config(S1, &[S1], &[]), 7).unwrap();
    let effects = tick_until_active(&mut raft);
    assert_eq!(raft.role(), Role::Leader);
    assert_eq!(applied(&effects), vec![LogIndex(1)], "the blank entry");
    let p = raft.propose(b"x".to_vec()).unwrap();
    assert_eq!(p.index, LogIndex(2));
    let persist = position(&p.effects, |e| matches!(e, Effect::PersistEntries(_)));
    let apply = position(&p.effects, |e| matches!(e, Effect::Apply(_)));
    assert!(persist < apply, "applied before persisted: {:?}", p.effects);
}

/// Catches: a server that grants a second vote in one term (two leaders could win it),
/// and a vote sent before it is persisted (a restart would forget it and vote again).
#[test]
fn one_vote_per_term_persisted_before_it_is_sent() {
    let mut voter = Raft::new(three(S3), 0).unwrap();
    let request = |term| Message {
        term: Term(term),
        kind: MessageKind::VoteRequest {
            last_log: LogId::default(),
        },
    };
    let granted = |effects: &[Effect], to| match sent_to(effects, to).kind {
        MessageKind::VoteResponse { granted } => granted,
        other => panic!("not a vote: {other:?}"),
    };
    let first = voter.receive(S1, request(1), 0);
    assert!(granted(&first, S1));
    let persist = position(
        &first,
        |e| matches!(e, Effect::PersistHardState(h) if h.voted_for == Some(S1)),
    );
    let send = position(&first, |e| matches!(e, Effect::Send { .. }));
    assert!(persist < send, "vote sent before persisted: {first:?}");
    assert!(
        !granted(&voter.receive(S2, request(1), 0), S2),
        "second vote in term 1"
    );
    assert!(
        granted(&voter.receive(S1, request(1), 0), S1),
        "a repeat is the same vote"
    );
    assert!(
        granted(&voter.receive(S2, request(2), 0), S2),
        "a new term, a new vote"
    );
}

/// Catches: a follower that acknowledges entries before the effect that persists them
/// (a crash in between loses entries the leader counted towards a commit).
#[test]
fn a_follower_persists_before_it_acknowledges() {
    let mut follower = Raft::new(three(S2), 0).unwrap();
    let msg = Message {
        term: Term(1),
        kind: MessageKind::AppendRequest {
            prev: LogId::default(),
            entries: vec![cmd(1, 1)],
            commit: LogIndex(0),
        },
    };
    let effects = follower.receive(S1, msg, 0);
    let persist = position(&effects, |e| matches!(e, Effect::PersistEntries(_)));
    let ack = position(&effects, |e| matches!(e, Effect::Send { .. }));
    assert!(persist < ack, "acknowledged before persisted: {effects:?}");
    let outcome = AppendOutcome::Accepted {
        matched: LogIndex(1),
    };
    assert_eq!(
        sent_to(&effects, S1).kind,
        MessageKind::AppendResponse { outcome }
    );
}

/// Catches: a follower that keeps a conflicting suffix, or commits past what the
/// request proved: entries after `prev` + `entries` may be stale ones from an earlier
/// leader, so the leader's commit index applies only up to the proven prefix.
#[test]
fn a_follower_replaces_a_conflicting_suffix_and_commits_only_the_proven_prefix() {
    let stale = vec![cmd(1, 1), cmd(1, 2), cmd(1, 3)];
    let mut follower = restore(three(S2), 1, stale);
    let heartbeat = Message {
        term: Term(2),
        kind: MessageKind::AppendRequest {
            prev: LogId::new(Term(1), LogIndex(1)),
            entries: vec![],
            commit: LogIndex(3),
        },
    };
    let effects = follower.receive(S1, heartbeat, 0);
    assert_eq!(applied(&effects), vec![LogIndex(1)], "{effects:?}");
    let replace = Message {
        term: Term(2),
        kind: MessageKind::AppendRequest {
            prev: LogId::new(Term(1), LogIndex(1)),
            entries: vec![cmd(2, 2)],
            commit: LogIndex(3),
        },
    };
    let effects = follower.receive(S1, replace, 0);
    assert!(effects.contains(&Effect::PersistEntries(vec![cmd(2, 2)])));
    assert_eq!(follower.entries(), &[cmd(1, 1), cmd(2, 2)]);
    assert_eq!(applied(&effects), vec![LogIndex(2)]);
    assert_eq!(follower.commit_index(), LogIndex(2));
}

/// Catches: a leader that commits an entry of an earlier term by counting its
/// replicas: the paper's Figure 8, on three voters.
///
/// s1 holds `a` (term 2) at index 1, s3 holds `c` (term 3) there, s2 holds nothing.
/// s1 wins term 4 with s2's vote and replicates `a` to s2, one entry per request.
/// Two of three hold `a`, but it is from term 2: had s1 committed it, s3 could still
/// win term 5 with s2's vote (its last term, 3, beats s2's 2) and overwrite it. Once
/// s2 also holds s1's term-4 entry, `a` commits with it, and s3 can no longer win.
#[test]
fn figure_8_an_earlier_terms_entry_commits_only_under_one_of_this_term() {
    let a = cmd(2, 1);
    let mut s1 = restore(three(S1), 3, vec![a.clone()]);
    let mut s2 = restore(three(S2), 3, vec![]);
    // s3 has since seen term 4 (it lost an election there), so it next campaigns in 5.
    let mut s3 = restore(three(S3), 4, vec![cmd(3, 1)]);

    let campaign = tick_until_active(&mut s1);
    let vote = s2.receive(S1, sent_to(&campaign, S2), 0);
    let mut to_s2 = sent_to(&s1.receive(S2, sent_to(&vote, S1), 0), S2);
    assert_eq!(s1.role(), Role::Leader);
    assert_eq!(s1.hard_state().term, Term(4));

    // s2 lacks the entry before the blank, rejects, and gets `a` alone.
    let reply = sent_to(&s2.receive(S1, to_s2, 0), S1);
    to_s2 = sent_to(&s1.receive(S2, reply, 0), S2);
    let reply = sent_to(&s2.receive(S1, to_s2, 0), S1);
    let outcome = AppendOutcome::Accepted {
        matched: LogIndex(1),
    };
    assert_eq!(reply.kind, MessageKind::AppendResponse { outcome });
    let effects = s1.receive(S2, reply, 0);
    assert_eq!(
        s1.commit_index(),
        LogIndex(0),
        "committed a term-2 entry by count"
    );
    assert!(applied(&effects).is_empty());

    // Here a was not safe: s3 could still be elected with s2's vote.
    let mut s3_probe = s3.clone();
    let mut s2_probe = s2.clone();
    let campaign = tick_until_active(&mut s3_probe);
    let vote = s2_probe.receive(S3, sent_to(&campaign, S2), 0);
    let _ = s3_probe.receive(S2, sent_to(&vote, S3), 0);
    assert_eq!(
        s3_probe.role(),
        Role::Leader,
        "the hazard the rule guards against"
    );

    // Once s2 holds the term-4 blank, both commit together and s3 cannot win.
    let reply = sent_to(&s2.receive(S1, sent_to(&effects, S2), 0), S1);
    let effects = s1.receive(S2, reply, 0);
    assert_eq!(applied(&effects), vec![LogIndex(1), LogIndex(2)]);
    let campaign = tick_until_active(&mut s3);
    let vote = s2.receive(S3, sent_to(&campaign, S2), 0);
    let _ = s3.receive(S2, sent_to(&vote, S3), 0);
    assert_eq!(s3.role(), Role::Candidate);
}

/// Catches: a learner that campaigns or votes (it would disturb elections or form a
/// quorum it is not part of), and a learner left out of replication.
#[test]
fn a_learner_never_campaigns_or_votes_but_receives_the_log() {
    let mut learner = Raft::new(three(S4), 0).unwrap();
    for _ in 0..100 {
        assert!(learner.tick(0).is_empty());
    }
    let request = Message {
        term: Term(1),
        kind: MessageKind::VoteRequest {
            last_log: LogId::default(),
        },
    };
    let effects = learner.receive(S1, request, 0);
    assert_eq!(
        sent_to(&effects, S1).kind,
        MessageKind::VoteResponse { granted: false }
    );

    let mut leader = Raft::new(config(S1, &[S1], &[S4]), 0).unwrap();
    let effects = tick_until_active(&mut leader);
    let reply = sent_to(&learner.receive(S1, sent_to(&effects, S4), 0), S1);
    let MessageKind::AppendResponse { outcome } = reply.kind else {
        panic!("{reply:?}");
    };
    assert_eq!(
        outcome,
        AppendOutcome::Accepted {
            matched: LogIndex(1)
        }
    );
    assert_eq!(learner.entries().len(), 1);
}

/// Catches: a server that obeys a deposed leader (a stale request must be refused with
/// the current term), and a leader that does not step down on seeing a later term.
#[test]
fn stale_terms_are_refused_and_later_terms_depose() {
    let mut s2 = restore(three(S2), 5, vec![]);
    let stale = Message {
        term: Term(4),
        kind: MessageKind::AppendRequest {
            prev: LogId::default(),
            entries: vec![cmd(4, 1)],
            commit: LogIndex(1),
        },
    };
    let effects = s2.receive(S1, stale, 0);
    let reply = sent_to(&effects, S1);
    assert_eq!(reply.term, Term(5));
    assert!(matches!(
        reply.kind,
        MessageKind::AppendResponse {
            outcome: AppendOutcome::Rejected { .. }
        }
    ));
    assert!(s2.entries().is_empty());

    let mut leader = Raft::new(config(S1, &[S1], &[S4]), 0).unwrap();
    let _ = tick_until_active(&mut leader);
    assert_eq!(leader.role(), Role::Leader);
    let _ = leader.receive(S4, reply_with_term(9), 0);
    assert_eq!(leader.role(), Role::Follower);
    assert_eq!(leader.hard_state().term, Term(9));
    assert!(leader.propose(vec![]).is_err());
}

fn reply_with_term(term: u64) -> Message {
    Message {
        term: Term(term),
        kind: MessageKind::AppendResponse {
            outcome: AppendOutcome::Accepted {
                matched: LogIndex(0),
            },
        },
    }
}
