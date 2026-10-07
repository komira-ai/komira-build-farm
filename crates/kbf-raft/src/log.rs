//! The core's copy of the log.

use crate::{Entry, LogId, LogIndex, Term};

/// The entries after `base`, in index order.
///
/// `base` is the last entry folded into a snapshot; with no snapshot it is index 0,
/// term 0, which every log holds. Snapshots arrive in a later change; until then
/// `base` stays at zero and the log holds every entry.
#[derive(Clone, Debug, Default)]
pub(crate) struct Log {
    base: LogId,
    entries: Vec<Entry>,
}

impl Log {
    /// The log holding `entries`, which must be numbered from 1 with terms that never
    /// fall. Returns the first entry that breaks the rule, if one does.
    pub(crate) fn restore(entries: Vec<Entry>) -> Result<Self, LogId> {
        let mut prev = LogId::default();
        for e in &entries {
            if e.id.index != prev.index.next() || e.id.term < prev.term {
                return Err(e.id);
            }
            prev = e.id;
        }
        Ok(Self {
            base: LogId::default(),
            entries,
        })
    }

    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(crate) fn last_id(&self) -> LogId {
        self.entries.last().map_or(self.base, |e| e.id)
    }

    pub(crate) fn last_index(&self) -> LogIndex {
        self.last_id().index
    }

    /// The position of `index` in `entries`, if it is there.
    fn offset(&self, index: LogIndex) -> Option<usize> {
        let off = index.0.checked_sub(self.base.index.0)?.checked_sub(1)?;
        let off = usize::try_from(off).ok()?;
        (off < self.entries.len()).then_some(off)
    }

    /// The term of the entry at `index`, or `None` past the end.
    pub(crate) fn term_at(&self, index: LogIndex) -> Option<Term> {
        if index == self.base.index {
            return Some(self.base.term);
        }
        self.offset(index).map(|i| self.entries[i].id.term)
    }

    pub(crate) fn entry(&self, index: LogIndex) -> Option<&Entry> {
        self.offset(index).map(|i| &self.entries[i])
    }

    /// Up to `max` entries starting at `from`.
    pub(crate) fn slice(&self, from: LogIndex, max: usize) -> Vec<Entry> {
        match self.offset(from) {
            Some(i) => self.entries[i..].iter().take(max).cloned().collect(),
            None => Vec::new(),
        }
    }

    /// Appends one entry at the end.
    pub(crate) fn push(&mut self, entry: Entry) {
        debug_assert_eq!(entry.id.index, self.last_index().next());
        self.entries.push(entry);
    }

    /// Drops every entry at or after `index`.
    pub(crate) fn truncate_from(&mut self, index: LogIndex) {
        if let Some(i) = self.offset(index) {
            self.entries.truncate(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Payload;

    fn e(term: u64, index: u64) -> Entry {
        Entry {
            id: LogId::new(Term(term), LogIndex(index)),
            payload: Payload::Blank,
        }
    }

    /// Catches: an off-by-one between indexes and vector positions (index 1 is the
    /// first entry; index 0 is the empty prefix, term 0), a slice that runs past its
    /// cap, and a truncation that keeps the entry at the cut.
    #[test]
    fn indexes_terms_slices_and_truncation() {
        let mut log = Log::restore(vec![e(1, 1), e(1, 2), e(3, 3)]).unwrap();
        assert_eq!(log.term_at(LogIndex(0)), Some(Term(0)));
        assert_eq!(log.term_at(LogIndex(3)), Some(Term(3)));
        assert_eq!(log.term_at(LogIndex(4)), None);
        assert_eq!(log.slice(LogIndex(2), 1), vec![e(1, 2)]);
        assert_eq!(log.slice(LogIndex(2), 9), vec![e(1, 2), e(3, 3)]);
        assert!(log.slice(LogIndex(4), 9).is_empty());
        log.truncate_from(LogIndex(2));
        assert_eq!(log.last_id(), LogId::new(Term(1), LogIndex(1)));
        log.push(e(4, 2));
        assert_eq!(log.entry(LogIndex(2)), Some(&e(4, 2)));
    }

    /// Catches: restoring a log with a gap, a repeated index or a falling term, which
    /// would break the log-matching property from the start.
    #[test]
    fn restore_refuses_a_malformed_log() {
        assert!(Log::restore(vec![]).is_ok());
        assert_eq!(
            Log::restore(vec![e(1, 1), e(1, 3)]).unwrap_err(),
            e(1, 3).id
        );
        assert_eq!(
            Log::restore(vec![e(2, 1), e(1, 2)]).unwrap_err(),
            e(1, 2).id
        );
        assert_eq!(Log::restore(vec![e(1, 2)]).unwrap_err(), e(1, 2).id);
    }
}
