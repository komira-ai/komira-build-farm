//! An in-memory [`Storage`] that models a crash: writes stay pending until a sync, and
//! a crash keeps the durable state plus a prefix of the pending writes.

use std::convert::Infallible;

use crate::{Entry, HardState, Snapshot, Storage, Stored};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Write {
    HardState(HardState),
    Entries(Vec<Entry>),
}

/// Storage in memory, for tests and simulations. It never fails.
///
/// It holds two states: what the host has written ([`MemStorage::written`]) and what
/// a crash cannot take away ([`MemStorage::durable`]). [`Storage::sync`] makes the
/// first the second; [`MemStorage::crash`] throws away the writes since the last sync,
/// except a prefix of them, as a disk that flushed part of its cache before power was
/// lost would keep. A snapshot write is durable at once.
#[derive(Clone, Debug, Default)]
pub struct MemStorage {
    written: Stored,
    durable: Stored,
    pending: Vec<Write>,
}

impl MemStorage {
    /// Empty storage: term 0, no vote, no snapshot, no entries.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything written, synced or not: what the server reads while it runs.
    #[must_use]
    pub fn written(&self) -> &Stored {
        &self.written
    }

    /// What survives a crash that keeps none of the pending writes.
    #[must_use]
    pub fn durable(&self) -> &Stored {
        &self.durable
    }

    /// How many writes are not yet synced.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// A crash: the first `keep` pending writes reach the disk (all of them if there
    /// are fewer), the rest are lost, and what was written becomes what is durable.
    pub fn crash(&mut self, keep: usize) {
        for write in self.pending.drain(..).take(keep) {
            apply(&mut self.durable, write);
        }
        self.written.clone_from(&self.durable);
    }
}

fn apply(stored: &mut Stored, write: Write) {
    match write {
        Write::HardState(hard) => stored.hard = hard,
        Write::Entries(entries) => {
            let base = stored.base().index;
            let first = entries.first().map_or(base.next(), |e| e.id.index);
            stored.entries.retain(|e| e.id.index < first);
            stored
                .entries
                .extend(entries.into_iter().filter(|e| e.id.index > base));
        }
    }
}

impl Storage for MemStorage {
    type Error = Infallible;

    fn load(&mut self) -> Result<Stored, Infallible> {
        Ok(self.written.clone())
    }

    fn write_hard_state(&mut self, hard: HardState) -> Result<(), Infallible> {
        let write = Write::HardState(hard);
        apply(&mut self.written, write.clone());
        self.pending.push(write);
        Ok(())
    }

    fn write_entries(&mut self, entries: &[Entry]) -> Result<(), Infallible> {
        let write = Write::Entries(entries.to_vec());
        apply(&mut self.written, write.clone());
        self.pending.push(write);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Infallible> {
        self.pending.clear();
        self.durable.clone_from(&self.written);
        Ok(())
    }

    fn write_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), Infallible> {
        for stored in [&mut self.written, &mut self.durable] {
            stored.entries.retain(|e| e.id.index > snapshot.base.index);
            stored.snapshot = Some(snapshot.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LogId, LogIndex, Payload, ServerId, Term};

    fn e(term: u64, index: u64) -> Entry {
        Entry {
            id: LogId::new(Term(term), LogIndex(index)),
            payload: Payload::Blank,
        }
    }

    fn hard(term: u64) -> HardState {
        HardState {
            term: Term(term),
            voted_for: Some(ServerId(1)),
        }
    }

    fn ids(s: &Stored) -> Vec<(u64, u64)> {
        s.entries
            .iter()
            .map(|e| (e.id.term.0, e.id.index.0))
            .collect()
    }

    /// Catches: a write that appends without dropping the suffix it replaces, and a
    /// write that counts as durable before a sync.
    #[test]
    fn writes_replace_the_suffix_and_wait_for_a_sync() {
        let mut m = MemStorage::new();
        m.write_hard_state(hard(1)).unwrap();
        m.write_entries(&[e(1, 1), e(1, 2), e(1, 3)]).unwrap();
        m.sync().unwrap();
        m.write_hard_state(hard(2)).unwrap();
        m.write_entries(&[e(2, 2)]).unwrap();
        assert_eq!(ids(m.written()), [(1, 1), (2, 2)]);
        assert_eq!(m.written().hard, hard(2));
        assert_eq!(ids(m.durable()), [(1, 1), (1, 2), (1, 3)]);
        assert_eq!(m.durable().hard, hard(1));
        assert_eq!(m.pending(), 2);
        assert_eq!(m.load().unwrap(), m.written().clone());
    }

    /// Catches: a crash that keeps writes past the prefix it was asked to keep, or
    /// loses synced ones.
    #[test]
    fn a_crash_keeps_the_durable_state_and_a_prefix_of_the_pending_writes() {
        let mut m = MemStorage::new();
        m.write_entries(&[e(1, 1)]).unwrap();
        m.sync().unwrap();
        m.write_hard_state(hard(2)).unwrap();
        m.write_entries(&[e(2, 2)]).unwrap();
        let mut kept_one = m.clone();
        kept_one.crash(1);
        assert_eq!(kept_one.durable().hard, hard(2));
        assert_eq!(ids(kept_one.durable()), [(1, 1)]);
        assert_eq!(kept_one.written(), kept_one.durable());
        assert_eq!(kept_one.pending(), 0);
        let mut kept_all = m.clone();
        kept_all.crash(9);
        assert_eq!(ids(kept_all.durable()), [(1, 1), (2, 2)]);
        m.crash(0);
        assert_eq!(m.durable().hard, HardState::default());
        assert_eq!(ids(m.written()), [(1, 1)]);
    }

    /// Catches: a snapshot that leaves the entries it folds in, or that a crash can
    /// take back; and an entry write at or below the base that lands in the log.
    #[test]
    fn a_snapshot_is_durable_at_once_and_trims_the_log() {
        let mut m = MemStorage::new();
        m.write_entries(&[e(1, 1), e(1, 2), e(1, 3)]).unwrap();
        m.sync().unwrap();
        let snapshot = Snapshot {
            base: LogId::new(Term(1), LogIndex(2)),
            state: b"s".to_vec(),
        };
        m.write_snapshot(&snapshot).unwrap();
        m.crash(0);
        assert_eq!(m.durable().snapshot.as_ref(), Some(&snapshot));
        assert_eq!(m.durable().base(), snapshot.base);
        assert_eq!(ids(m.durable()), [(1, 3)]);
        m.write_entries(&[e(1, 2), e(2, 3)]).unwrap();
        assert_eq!(ids(m.written()), [(2, 3)]);
    }
}
