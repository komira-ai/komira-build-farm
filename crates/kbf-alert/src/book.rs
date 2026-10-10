//! The alert book: which alerts are open, with hysteresis on both edges. Pure.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::num::NonZeroU32;

use crate::alert::{Alert, Event, Key, Transition, UnixMillis};

/// How many checks in a row it takes to change an alert's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hysteresis {
    /// Bad checks in a row before a key is raised.
    pub raise_after: NonZeroU32,
    /// Good checks in a row before a raised key is resolved.
    pub resolve_after: NonZeroU32,
}

/// One key the book is tracking: pending (bad, not yet raised) or open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The alert's text as of its last bad check.
    pub alert: Alert,
    /// The first bad check of the current run.
    pub first_seen: UnixMillis,
    /// The last bad check.
    pub last_seen: UnixMillis,
    /// Whether the book has raised it.
    pub open: bool,
    /// Bad checks in a row (counted until it is raised).
    bad_run: u32,
    /// Good checks in a row since it was raised.
    good_run: u32,
}

impl Record {
    fn event(&self, transition: Transition, resolved_at: Option<UnixMillis>) -> Event {
        Event {
            transition,
            alert: self.alert.clone(),
            first_seen: self.first_seen,
            last_seen: self.last_seen,
            resolved_at,
        }
    }
}

/// Every key with a recent bad check, and whether it is raised.
///
/// The caller runs its checks and reports each result: [`AlertBook::bad`] with the
/// alert when the problem is present, [`AlertBook::good`] with the key when it is
/// not. Each returns the event to deliver, if this check changed the key's state.
#[derive(Debug, Clone)]
pub struct AlertBook {
    hysteresis: Hysteresis,
    records: BTreeMap<Key, Record>,
}

impl AlertBook {
    /// An empty book.
    #[must_use]
    pub fn new(hysteresis: Hysteresis) -> Self {
        Self {
            hysteresis,
            records: BTreeMap::new(),
        }
    }

    /// A check found the problem `alert` describes, at `now`.
    ///
    /// Returns the raise on the [`Hysteresis::raise_after`]th bad check in a row, and
    /// nothing on any other: an open alert is never raised twice. The record keeps the
    /// latest text, so a fix that changes between checks is the one delivered.
    pub fn bad(&mut self, alert: Alert, now: UnixMillis) -> Option<Event> {
        let raise_after = self.hysteresis.raise_after.get();
        let record = self.records.entry(alert.key()).or_insert_with(|| Record {
            alert: alert.clone(),
            first_seen: now,
            last_seen: now,
            open: false,
            bad_run: 0,
            good_run: 0,
        });
        record.alert = alert;
        record.last_seen = now;
        record.good_run = 0;
        if record.open {
            return None;
        }
        record.bad_run += 1;
        if record.bad_run < raise_after {
            return None;
        }
        record.open = true;
        Some(record.event(Transition::Raised, None))
    }

    /// A check found no problem for `key`, at `now`.
    ///
    /// A key that was pending and never raised is forgotten, so its next bad check
    /// starts a new run. An open key resolves on the [`Hysteresis::resolve_after`]th
    /// good check in a row, once, and is then forgotten.
    pub fn good(&mut self, key: &Key, now: UnixMillis) -> Option<Event> {
        let Entry::Occupied(mut entry) = self.records.entry(key.clone()) else {
            return None;
        };
        let record = entry.get_mut();
        if !record.open {
            entry.remove();
            return None;
        }
        record.good_run += 1;
        if record.good_run < self.hysteresis.resolve_after.get() {
            return None;
        }
        Some(entry.remove().event(Transition::Resolved, Some(now)))
    }

    /// The raised alerts, in key order.
    pub fn open(&self) -> impl Iterator<Item = &Record> {
        self.records.values().filter(|r| r.open)
    }

    /// The record for `key`, pending or open.
    #[must_use]
    pub fn get(&self, key: &Key) -> Option<&Record> {
        self.records.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::Severity;

    fn hysteresis(raise: u32, resolve: u32) -> Hysteresis {
        Hysteresis {
            raise_after: NonZeroU32::new(raise).expect("nonzero"),
            resolve_after: NonZeroU32::new(resolve).expect("nonzero"),
        }
    }

    fn alert(subject: &str, fix: &str) -> Alert {
        Alert::new("disconnected", subject, Severity::Critical, "gone", fix).expect("valid")
    }

    /// Catches: raising on the first bad check (no hysteresis), raising before the
    /// threshold (`<=` for `<`), or raising again while open (no "already open"
    /// check). Repeated bad checks send exactly one raise, on the third.
    #[test]
    fn raises_once_after_n_bad_checks_in_a_row() {
        let mut book = AlertBook::new(hysteresis(3, 2));
        assert_eq!(book.bad(alert("n1", "fix-a"), UnixMillis(10)), None);
        assert_eq!(book.bad(alert("n1", "fix-a"), UnixMillis(20)), None);
        assert_eq!(book.open().count(), 0);
        let raised = book
            .bad(alert("n1", "fix-b"), UnixMillis(30))
            .expect("raised");
        assert_eq!(raised.transition, Transition::Raised);
        assert_eq!(
            raised.alert.fix, "fix-b",
            "the latest fix text is delivered"
        );
        assert_eq!(raised.first_seen, UnixMillis(10));
        assert_eq!(raised.last_seen, UnixMillis(30));
        assert_eq!(raised.resolved_at, None);
        for t in 40..80 {
            assert_eq!(book.bad(alert("n1", "fix-b"), UnixMillis(t)), None);
        }
        let open: Vec<_> = book.open().collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].last_seen, UnixMillis(79));
        assert_eq!(open[0].first_seen, UnixMillis(10));
    }

    /// Catches: counting bad checks that are not in a row (a good check that does not
    /// reset a pending run). A 40x flap inside the threshold sends nothing, and a
    /// pending key is forgotten on a good check.
    #[test]
    fn a_flap_shorter_than_the_threshold_sends_nothing() {
        let mut book = AlertBook::new(hysteresis(2, 1));
        let key = alert("n1", "f").key();
        for t in 0..40 {
            assert_eq!(book.bad(alert("n1", "f"), UnixMillis(2 * t)), None);
            assert_eq!(book.good(&key, UnixMillis(2 * t + 1)), None);
            assert_eq!(book.get(&key), None);
        }
        // A new run starts from its own first bad check.
        assert_eq!(book.bad(alert("n1", "f"), UnixMillis(100)), None);
        let raised = book.bad(alert("n1", "f"), UnixMillis(101)).expect("raised");
        assert_eq!(raised.first_seen, UnixMillis(100));
    }

    /// Catches: resolving on the first good check (no resolve hysteresis), sending a
    /// resolve on every later good check, or a bad check that does not reset the good
    /// run. The resolve is sent exactly once, after two good checks in a row.
    #[test]
    fn resolves_once_after_m_good_checks_in_a_row() {
        let mut book = AlertBook::new(hysteresis(1, 2));
        let key = alert("n1", "f").key();
        assert_eq!(book.good(&key, UnixMillis(0)), None, "unknown key");
        assert!(book.bad(alert("n1", "f"), UnixMillis(5)).is_some());
        assert_eq!(book.good(&key, UnixMillis(6)), None);
        assert_eq!(
            book.bad(alert("n1", "f"), UnixMillis(7)),
            None,
            "still open"
        );
        assert_eq!(book.good(&key, UnixMillis(8)), None, "good run was reset");
        let resolved = book.good(&key, UnixMillis(9)).expect("resolved");
        assert_eq!(resolved.transition, Transition::Resolved);
        assert_eq!(resolved.first_seen, UnixMillis(5));
        assert_eq!(resolved.last_seen, UnixMillis(7));
        assert_eq!(resolved.resolved_at, Some(UnixMillis(9)));
        for t in 10..20 {
            assert_eq!(book.good(&key, UnixMillis(t)), None);
        }
        assert_eq!(book.open().count(), 0);
    }

    /// Catches: keying by rule alone, so one node's alert raises or resolves
    /// another's.
    #[test]
    fn subjects_are_independent() {
        let mut book = AlertBook::new(hysteresis(1, 1));
        assert!(book.bad(alert("n1", "f"), UnixMillis(1)).is_some());
        assert!(book.bad(alert("n2", "f"), UnixMillis(1)).is_some());
        let resolved = book
            .good(&alert("n1", "f").key(), UnixMillis(2))
            .expect("resolved");
        assert_eq!(resolved.alert.subject, "n1");
        let open: Vec<_> = book.open().map(|r| r.alert.subject.as_str()).collect();
        assert_eq!(open, ["n2"]);
    }
}
