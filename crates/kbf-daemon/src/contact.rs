//! Contact with the server: when the daemon last provably reached it, and so when it
//! must fence.
//!
//! The scheduler re-dispatches a node's leases G = 60 s after it last heard from the
//! node. A daemon that must not run beside a re-dispatched copy stops its work T = 40 s
//! after the newest heartbeat the server acknowledged
//! (`docs/design/scheduler.md#fencing-g-and-t`). "After the heartbeat" means after the
//! daemon *sent* it: the server heard the heartbeat no earlier than it was sent, so
//! counting from the send time keeps the daemon's deadline inside the server's,
//! whatever the acknowledgement's delay. Counting from when the
//! acknowledgement arrived would let a slow acknowledgement push the deadline past G.
//!
//! Contact outlives a stream: leases keep running across a reconnect, and a new
//! stream's acknowledgements renew the same contact. Heartbeat sequence numbers are per
//! stream, so [`Contact::new_stream`] forgets the unacknowledged ones.
//!
//! This module reads no clock; every instant is an argument, a reading of the
//! suspend-counting clock (`clock`, issue #78).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::clock::Moment;

/// What one acknowledgement did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acked {
    /// The acknowledged sequence number was one this stream sent and had not yet seen
    /// acknowledged. An unknown or repeated number changes nothing.
    pub known: bool,
    /// The acknowledgement ended a heartbeat gap.
    pub restored: bool,
}

/// The daemon's view of its contact with the server.
#[derive(Debug)]
pub struct Contact {
    fence_after: Duration,
    gap_after: Option<Duration>,
    /// Send time of the newest message the server has acknowledged.
    confirmed: Option<Moment>,
    /// Send times of this stream's heartbeats that are not yet acknowledged.
    pending: BTreeMap<u64, Moment>,
    in_gap: bool,
}

impl Contact {
    /// A daemon that has not reached the server yet. `fence_after` is T.
    #[must_use]
    pub fn new(fence_after: Duration) -> Self {
        Self {
            fence_after,
            gap_after: None,
            confirmed: None,
            pending: BTreeMap::new(),
            in_gap: false,
        }
    }

    /// T: how long after the newest acknowledged send the daemon fences.
    #[must_use]
    pub fn fence_after(&self) -> Duration {
        self.fence_after
    }

    /// Starts a new stream whose server asked for a heartbeat every `interval`. A gap is
    /// declared when nothing sent in the last two intervals has been acknowledged.
    pub fn new_stream(&mut self, interval: Duration) {
        self.pending.clear();
        self.gap_after = Some(interval.saturating_mul(2));
    }

    /// Records that the server acknowledged a message the daemon sent at `sent_at`
    /// (the Hello that a Welcome answers).
    pub fn confirm(&mut self, sent_at: Moment) -> bool {
        if self.confirmed.is_none_or(|c| sent_at > c) {
            self.confirmed = Some(sent_at);
        }
        std::mem::replace(&mut self.in_gap, false)
    }

    /// Records that heartbeat `seq` of this stream was sent at `at`.
    pub fn sent(&mut self, seq: u64, at: Moment) {
        self.pending.insert(seq, at);
    }

    /// Records the server's acknowledgement of heartbeat `seq`. An acknowledgement
    /// also covers every earlier heartbeat of the stream.
    pub fn acknowledged(&mut self, seq: u64) -> Acked {
        let Some(&sent_at) = self.pending.get(&seq) else {
            return Acked {
                known: false,
                restored: false,
            };
        };
        self.pending.retain(|&s, _| s > seq);
        let restored = self.confirm(sent_at);
        Acked {
            known: true,
            restored,
        }
    }

    /// When the daemon must fence its leases: T after the newest acknowledged send.
    /// `None` before the server has acknowledged anything.
    #[must_use]
    pub fn fence_deadline(&self) -> Option<Moment> {
        self.confirmed.map(|c| c + self.fence_after)
    }

    /// Whether contact is lost at `now`: the fence deadline has passed.
    #[must_use]
    pub fn lost(&self, now: Moment) -> bool {
        self.fence_deadline().is_some_and(|d| now >= d)
    }

    /// When a heartbeat gap begins, if one is not already declared.
    #[must_use]
    pub fn gap_deadline(&self) -> Option<Moment> {
        match (self.in_gap, self.confirmed, self.gap_after) {
            (false, Some(c), Some(g)) => Some(c + g),
            _ => None,
        }
    }

    /// Declares a gap if its deadline has passed. Returns how long the server has been
    /// silent when it declares one, once per gap.
    pub fn check_gap(&mut self, now: Moment) -> Option<Duration> {
        let deadline = self.gap_deadline()?;
        if now < deadline {
            return None;
        }
        self.in_gap = true;
        self.confirmed.map(|c| now.saturating_duration_since(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(40);
    const I: Duration = Duration::from_secs(5);

    fn connected(t0: Moment) -> Contact {
        let mut c = Contact::new(T);
        c.new_stream(I);
        c.confirm(t0);
        c
    }

    /// Catches: a fence deadline counted from when the acknowledgement arrived rather
    /// than from when the acknowledged heartbeat was sent. A slow acknowledgement would
    /// then move the daemon's deadline past the server's re-dispatch, and two copies of
    /// a lease could run.
    #[test]
    fn fence_counts_from_the_send_of_the_acknowledged_heartbeat() {
        let t0 = Moment::from_origin(Duration::from_secs(1000));
        let mut c = connected(t0);
        let sent = t0 + Duration::from_secs(10);
        c.sent(1, sent);
        // The acknowledgement arrives 20 s later; the clock is not an input here.
        assert!(c.acknowledged(1).known);
        assert_eq!(c.fence_deadline(), Some(sent + T));
        assert!(!c.lost(sent + T - Duration::from_millis(1)));
        assert!(c.lost(sent + T));
    }

    /// Catches: an acknowledgement of an older heartbeat (or a repeated or unknown
    /// sequence number, or a Welcome for an older send) moving the deadline backwards
    /// or forwards.
    #[test]
    fn only_newer_acknowledgements_move_the_deadline() {
        let t0 = Moment::from_origin(Duration::from_secs(1000));
        let mut c = connected(t0);
        c.sent(1, t0 + I);
        c.sent(2, t0 + 2 * I);
        assert!(c.acknowledged(2).known);
        assert!(!c.acknowledged(1).known, "1 is covered by the ack of 2");
        assert!(!c.acknowledged(9).known, "9 was never sent");
        assert_eq!(c.fence_deadline(), Some(t0 + 2 * I + T));
        // A Welcome answering an older Hello does not move it back either.
        c.confirm(t0 + I);
        assert_eq!(c.fence_deadline(), Some(t0 + 2 * I + T));
    }

    /// Catches: a new stream that forgets contact (fencing work that a reconnect should
    /// keep), or that lets the old stream's sequence numbers acknowledge new sends.
    #[test]
    fn a_new_stream_keeps_contact_and_drops_pending_heartbeats() {
        let t0 = Moment::from_origin(Duration::from_secs(1000));
        let mut c = connected(t0);
        c.sent(1, t0 + I);
        c.new_stream(I);
        assert_eq!(c.fence_deadline(), Some(t0 + T));
        assert!(!c.acknowledged(1).known);
    }

    /// Catches: a gap that is never declared, declared more than once, or not cleared by
    /// the next acknowledgement.
    #[test]
    fn a_gap_is_declared_once_and_cleared_by_an_acknowledgement() {
        let t0 = Moment::from_origin(Duration::from_secs(1000));
        let mut c = connected(t0);
        assert_eq!(c.check_gap(t0 + 2 * I - Duration::from_millis(1)), None);
        assert_eq!(c.check_gap(t0 + 2 * I), Some(2 * I));
        assert_eq!(c.check_gap(t0 + 3 * I), None, "declared once");
        c.sent(7, t0 + 3 * I);
        assert!(c.acknowledged(7).restored);
        assert_eq!(c.gap_deadline(), Some(t0 + 5 * I));
    }
}
