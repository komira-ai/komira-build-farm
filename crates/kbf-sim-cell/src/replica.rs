//! Properties 1 and 6 against a disk: a replica that replays the durable log.
//!
//! What a power cut now would leave of the server's disk is read back with
//! `kbf-store`, and its entries through the server's applied index are applied to a
//! fresh `MetaState` as the server's metadata machine applies them (a metadata command
//! executed at its index, the first `Bootstrap` kept). The server's applied state must
//! equal the replica's, and its log id the replica's `Bootstrap`'s: every entry the
//! server has applied, and so answered from, was durable, and a restart or a follower
//! would apply it alike.

use kbf_log::{FarmLogId, Format, LogCommand, MachineVersion};
use kbf_meta::{MetaState, Retention};
use kbf_raft::{Entry, Payload};
use kbf_store::{FaultFs, Options, Store};
use kbf_types::FarmTime;

use crate::check::Property;

/// A property that does not hold, and how.
pub type Broken = (Property, String);

fn equality(detail: String) -> Broken {
    (Property::ReplicaEquality, detail)
}

/// The entries `view` holds, as the store reads them back at open.
pub(crate) fn durable_entries(view: FaultFs) -> Result<Vec<Entry>, Broken> {
    match Store::open(view, Options::default()) {
        Ok((_, stored)) => Ok(stored.entries),
        Err(e) => Err(equality(format!("the durable log does not open: {e}"))),
    }
}

/// Farm time `after` an entry, which must not be before `before` (property 6).
pub(crate) fn monotone(before: FarmTime, after: FarmTime) -> Result<(), Broken> {
    if after < before {
        let d = format!("farm time goes back from {before:?} to {after:?}");
        return Err((Property::MonotoneTime, d));
    }
    Ok(())
}

/// The replica of `entries` through index `applied`: its metadata state and its first
/// `Bootstrap`'s log id.
#[cfg(test)]
pub(crate) fn replay(
    entries: &[Entry],
    applied: u64,
) -> Result<(MetaState, Option<FarmLogId>), Broken> {
    let mut replica = Replica::default();
    replica.advance(entries, applied)?;
    Ok(replica.state)
}

/// A replay kept from one check to the next, so a long run replays each entry once
/// rather than the whole log at every step.
#[derive(Debug)]
pub(crate) struct Replica {
    /// The metadata state and the first `Bootstrap`'s log id.
    pub(crate) state: (MetaState, Option<FarmLogId>),
    /// How many entries of the durable log it has read, and the last of them.
    read: usize,
    tail: Option<Entry>,
    /// The index of the last entry it read.
    last: u64,
    /// The machine version the entries read have raised the log to.
    version: MachineVersion,
}

impl Default for Replica {
    fn default() -> Self {
        Self {
            state: (MetaState::new(Retention::default()), None),
            read: 0,
            tail: None,
            last: 0,
            version: MachineVersion::V1,
        }
    }
}

impl Replica {
    /// Whether `entries` begins with every entry this replica has read: only then can
    /// it go on from where it stopped. (Checked by the last one read: one voter
    /// appends only, so an equal entry at that position means an equal prefix.)
    pub(crate) fn extends(&self, entries: &[Entry]) -> bool {
        self.read == 0 || entries.get(self.read - 1) == self.tail.as_ref()
    }

    /// Reads the entries after the last one read, through index `applied`.
    pub(crate) fn advance(&mut self, entries: &[Entry], applied: u64) -> Result<(), Broken> {
        let (replica, log_id) = &mut self.state;
        for entry in entries[self.read..]
            .iter()
            .take_while(|e| e.id.index.0 <= applied)
        {
            self.read += 1;
            self.last = entry.id.index.0;
            self.tail = Some(entry.clone());
            let last = self.last;
            let Payload::Command(bytes) = &entry.payload else {
                continue;
            };
            let format = self.version.format().unwrap_or(Format::V1);
            match kbf_log::decode(format, bytes) {
                Ok(LogCommand::Meta(command)) => {
                    let before = replica.now();
                    replica.execute(last, command);
                    monotone(before, replica.now())?;
                }
                Ok(LogCommand::Bootstrap(b)) => {
                    log_id.get_or_insert(b.log_id);
                }
                Ok(LogCommand::MachineVersion(v)) if v.format().is_some() => {
                    self.version = self.version.max(v);
                }
                other => return Err(equality(format!("entry {last} is {other:?}"))),
            }
        }
        if self.last < applied {
            let last = self.last;
            let d = format!("entry {applied} is applied, but the durable log ends at {last}");
            return Err(equality(d));
        }
        Ok(())
    }
}

/// Whether the server's `live` state and `log_id` are the replica's.
pub(crate) fn same_as_live(
    replica: &(MetaState, Option<FarmLogId>),
    live: &MetaState,
    log_id: FarmLogId,
) -> Result<(), Broken> {
    if *live != replica.0 {
        return Err(equality(
            "the applied state is not the durable log's".to_owned(),
        ));
    }
    if replica.1 != Some(log_id) {
        let d = format!("the durable log names log {:?}, not {log_id}", replica.1);
        return Err(equality(d));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use kbf_log::{Bootstrap, MachineVersion};
    use kbf_meta::Command;
    use kbf_raft::{LogId, LogIndex, Term};
    use kbf_store::Fault;

    use super::*;

    fn entry(index: u64, command: &LogCommand) -> Entry {
        let bytes = kbf_log::encode(Format::V1, command).expect("encode");
        Entry {
            id: LogId::new(Term(1), LogIndex(index)),
            payload: Payload::Command(bytes),
        }
    }

    fn id(n: u64) -> FarmLogId {
        FarmLogId::new(NonZeroU64::new(n).expect("non-zero"))
    }

    fn log() -> Vec<Entry> {
        let blank = Entry {
            id: LogId::new(Term(1), LogIndex(1)),
            payload: Payload::Blank,
        };
        let bootstrap = LogCommand::Bootstrap(Bootstrap {
            log_id: id(7),
            store_prefix: "cell/0000000000000007/".to_owned(),
        });
        let tick = LogCommand::Meta(Command::Tick(FarmTime::from_millis(5)));
        vec![blank, entry(2, &bootstrap), entry(3, &tick)]
    }

    /// Catches an incremental replay that skips or repeats an entry between checks,
    /// or goes on over a log that no longer begins with what it read.
    #[test]
    fn a_replay_in_steps_equals_one_replay() {
        let mut r = Replica::default();
        r.advance(&log(), 2).expect("through 2");
        assert!(r.extends(&log()));
        r.advance(&log(), 3).expect("through 3");
        assert_eq!(r.state, replay(&log(), 3).expect("in one"));
        r.advance(&log(), 3).expect("nothing new");
        assert_eq!(r.state, replay(&log(), 3).expect("in one"));
        let mut other = log();
        other[2] = entry(
            3,
            &LogCommand::Meta(Command::Tick(FarmTime::from_millis(1))),
        );
        assert!(!r.extends(&other));
        assert!(Replica::default().extends(&other));
    }

    /// Catches a replay that skips the metadata commands or the `Bootstrap`, or reads
    /// past the applied index.
    #[test]
    fn a_replay_applies_the_log_through_the_applied_index() {
        let (meta, log_id) = replay(&log(), 3).expect("a replica");
        assert_eq!(meta.now(), FarmTime::from_millis(5));
        assert_eq!(log_id, Some(id(7)));
        let (early, _) = replay(&log(), 2).expect("a replica");
        assert_eq!(early.now(), FarmTime::default());
        assert!(same_as_live(&(meta.clone(), Some(id(7))), &meta, id(7)).is_ok());
    }

    /// Catches each way property 1 can break going unreported: an applied entry the
    /// disk lacks, an entry the metadata machine does not apply, a state or a log id
    /// that differs, a durable log that does not open.
    #[test]
    fn each_difference_from_the_durable_log_breaks_replica_equality() {
        let broken = |r: Result<(), Broken>| r.expect_err("broken").0;
        let short = replay(&log(), 4).map(|_| ());
        assert_eq!(broken(short), Property::ReplicaEquality);
        let version = MachineVersion::new(NonZeroU32::new(5).expect("non-zero"));
        let mut control = log();
        control.push(entry(4, &LogCommand::MachineVersion(version)));
        assert_eq!(
            broken(replay(&control, 4).map(|_| ())),
            Property::ReplicaEquality
        );
        let (meta, _) = replay(&log(), 3).expect("a replica");
        let (other, _) = replay(&log(), 2).expect("a replica");
        assert_eq!(
            broken(same_as_live(&(meta.clone(), Some(id(7))), &other, id(7))),
            Property::ReplicaEquality
        );
        assert_eq!(
            broken(same_as_live(&(meta.clone(), Some(id(7))), &meta, id(8))),
            Property::ReplicaEquality
        );
        let dead = FaultFs::new();
        dead.fail_at(0, Fault::Error);
        assert_eq!(
            broken(durable_entries(dead).map(|_| ())),
            Property::ReplicaEquality
        );
    }

    /// Catches a replica that reads every entry as format 1 (a tier command after the
    /// raise to machine version 2 would not replay: a false property-1 alarm), or
    /// reads one as format 2 before the raise (a log an older binary refuses taken
    /// for a good one).
    #[test]
    fn a_raise_to_version_2_lets_the_tier_commands_replay() {
        let add = LogCommand::Meta(Command::AddTier {
            server: kbf_meta::ServerId::new(1),
            tier: kbf_meta::TierId::new(1),
        });
        let tier_entry = |index| Entry {
            id: LogId::new(Term(1), LogIndex(index)),
            payload: Payload::Command(kbf_log::encode(Format::V2, &add).expect("encode")),
        };
        let mut early = log();
        early.push(tier_entry(4));
        assert!(
            replay(&early, 4).is_err(),
            "a tier command before the raise"
        );
        let mut raised = log();
        raised.push(entry(4, &LogCommand::MachineVersion(MachineVersion::V2)));
        raised.push(tier_entry(5));
        let (meta, _) = replay(&raised, 5).expect("replays after the raise");
        assert_eq!(
            meta.tier(kbf_meta::ServerId::new(1)),
            Some(kbf_meta::TierId::new(1))
        );
    }

    /// Catches a time check that lets farm time go back (the mutant "`max(now)`
    /// dropped from apply" would need it).
    #[test]
    fn farm_time_going_back_breaks_monotone_time() {
        let (t1, t2) = (FarmTime::from_millis(1), FarmTime::from_millis(2));
        assert!(monotone(t1, t2).is_ok());
        assert_eq!(
            monotone(t2, t1).expect_err("back").0,
            Property::MonotoneTime
        );
    }
}
