//! What an applied event ended, for a reader that keeps history: [`Records::apply`]
//! hands out an [`Ended`] for each attempt that settled and each cache hit that gained
//! its row, and [`Records::idle_since`] each record that has gone quiet, once.

use kbf_sched::RequeueReason;
use kbf_types::{LeaseId, OperationId};

use super::Records;
use crate::event::WallTime;
use crate::record::{RecordKey, State};

/// What ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndedKind {
    /// The report of a lease that had been given up was taken (a memory kill, a farm
    /// fault the worker reported): the attempt's run block is set, after its end.
    Ran,
    /// A lease was given up and its operation queued again, with no report to follow.
    Requeued,
    /// A lease ended its operation, accepted or failed.
    Finished,
    /// A cache hit got its row: no attempt.
    Hit,
}

/// An attempt that settled in one row, or a hit that got its row. An operation that
/// several records wait on gives one for each row that shows the attempt.
///
/// The attempt is settled when it has ended and its run block is as complete as it
/// will be: a lease given up for a reason the worker reports ([`EndedKind::Ran`]) is
/// settled by its report, which follows the give-up; every other end settles it at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    /// The record.
    pub key: RecordKey,
    /// The row's index in the record's rows.
    pub row: usize,
    /// The attempt's index in the row's attempts; `None` for a hit.
    pub attempt: Option<usize>,
    /// What ended.
    pub kind: EndedKind,
    /// When: the event's time.
    pub at: WallTime,
}

/// Whether a lease given up for `reason` is followed by the worker's report of its run.
pub(super) const fn reports(reason: RequeueReason) -> bool {
    matches!(
        reason,
        RequeueReason::OutOfMemory { .. }
            | RequeueReason::NodeMemoryPressure
            | RequeueReason::Lost(_)
            | RequeueReason::Infra
    )
}

impl Records {
    /// Notes that row `row` of `key`'s record gained a cache hit at `at`.
    pub(super) fn hit(&mut self, key: &RecordKey, row: usize, at: WallTime) {
        self.ended.push(Ended {
            key: key.clone(),
            row,
            attempt: None,
            kind: EndedKind::Hit,
            at,
        });
    }

    /// Notes that `lease`'s attempt in row `row` of `key`'s record settled, if the row
    /// shows it (a copy that did not fit within the bound leaves it out).
    pub(super) fn note_ended(
        &mut self,
        key: &RecordKey,
        row: usize,
        lease: LeaseId,
        kind: EndedKind,
        at: WallTime,
    ) {
        let attempt = self
            .invocations
            .get(key)
            .and_then(|inv| inv.rows[row].attempts.iter().position(|a| a.lease == lease));
        if let Some(attempt) = attempt {
            self.ended.push(Ended {
                key: key.clone(),
                row,
                attempt: Some(attempt),
                kind,
                at,
            });
        }
    }

    /// Notes that `lease` of `operation` settled in every row that waits on it and
    /// shows the attempt. An [`EndedKind::Ran`] counts only for an attempt that has
    /// ended already. A waiter whose row a later Execute of the same action replaced
    /// no longer shows these attempts.
    pub(super) fn settled(
        &mut self,
        operation: OperationId,
        lease: LeaseId,
        kind: EndedKind,
        at: WallTime,
    ) {
        let rows: Vec<(RecordKey, usize)> =
            self.operations.get(&operation).map_or_else(Vec::new, |op| {
                op.waiters
                    .iter()
                    .filter_map(|w| {
                        let inv = self.invocations.get(&w.key);
                        let row = inv.and_then(|inv| w.row(inv));
                        row.map(|row| (w.key.clone(), row))
                    })
                    .collect()
            });
        for (key, row) in rows {
            let ended = self.invocations[&key].rows[row]
                .attempts
                .iter()
                .any(|a| a.lease == lease && a.ended.is_some());
            if kind != EndedKind::Ran || ended {
                self.note_ended(&key, row, lease, kind, at);
            }
        }
    }

    /// Sets the history id of `lease`'s attempt in row `row` of `key`'s record: opaque
    /// bytes, which later copies of the operation's attempts into the row keep. `false`
    /// when the record, the row or the attempt is gone.
    pub fn set_run_id(
        &mut self,
        key: &RecordKey,
        row: usize,
        lease: LeaseId,
        id: [u8; 12],
    ) -> bool {
        let attempt = self
            .invocations
            .get_mut(key)
            .and_then(|inv| inv.rows.get_mut(row))
            .and_then(|r| r.attempts.iter_mut().find(|a| a.lease == lease));
        let Some(attempt) = attempt else {
            return false;
        };
        attempt.run_id = Some(id);
        true
    }

    /// The records idle at `now` ([`State::Idle`]) that have not been reported idle
    /// since their last event: each once, oldest first. A record that wakes and goes
    /// idle again is reported again.
    pub fn idle_since(&mut self, now: WallTime) -> Vec<RecordKey> {
        let mut idle = Vec::new();
        for (_, key) in &self.by_seen {
            let inv = &self.invocations[key];
            match inv.state(now) {
                State::Idle if inv.idle_reported != Some(inv.last_seen) => idle.push(key.clone()),
                State::Idle => {}
                // A record with an operation in flight is open however old it is.
                State::Open if inv.in_flight > 0 => {}
                // Every record after this one was seen later.
                State::Open => break,
            }
        }
        for key in &idle {
            let inv = self.record(key);
            inv.idle_reported = Some(inv.last_seen);
        }
        idle
    }
}
