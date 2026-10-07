//! How late a `Start` may be acted on (issue #23).
//!
//! A `Start` names the newest heartbeat of the stream the server had taken when it
//! sent the `Start` (`heartbeat_seq`, 0 for the stream's Hello), and a window
//! (`valid_for_ms`). The server sent the `Start` after it took that heartbeat, and the
//! daemon sent the heartbeat before that, so a `Start` that arrives within the window
//! of the heartbeat's send time was in flight for less than the window. Only the
//! daemon's clock is read, so the two clocks need not agree. A `Start` that arrives
//! later may belong to a lease the scheduler has already given up and granted again;
//! the daemon does not run it.
//!
//! The server takes heartbeats in order and acknowledges each after taking it, so once
//! heartbeat `k` is acknowledged every later `Start` on the stream names `k` or a newer
//! one: the send times of older heartbeats are forgotten then. A `Start` that names a
//! heartbeat this stream never sent, or one already forgotten, is refused too.
//!
//! This module reads no clock; every instant is an argument.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::time::Instant;

/// The send times a `Start` on the current stream may name.
#[derive(Debug, Default)]
pub struct StartWindow {
    /// Send time by heartbeat seq; 0 is the stream's Hello.
    sends: BTreeMap<u64, Instant>,
}

impl StartWindow {
    /// A new stream whose Hello was sent at `hello_sent`: the send times of the old
    /// stream's heartbeats name nothing on this one.
    pub fn new_stream(&mut self, hello_sent: Instant) {
        self.sends.clear();
        self.sends.insert(0, hello_sent);
    }

    /// Heartbeat `seq` of this stream was sent at `at`.
    pub fn sent(&mut self, seq: u64, at: Instant) {
        self.sends.insert(seq, at);
    }

    /// The server acknowledged heartbeat `seq`: no later `Start` names an older one.
    pub fn acknowledged(&mut self, seq: u64) {
        if self.sends.contains_key(&seq) {
            self.sends.retain(|&s, _| s >= seq);
        }
    }

    /// Whether a `Start` naming heartbeat `seq` with window `valid_for` may be acted on
    /// at `now`. A zero window is no bound: a server that predates the field sends it.
    #[must_use]
    pub fn allows(&self, seq: u64, valid_for: Duration, now: Instant) -> bool {
        valid_for.is_zero()
            || self
                .sends
                .get(&seq)
                .is_some_and(|&sent| now.saturating_duration_since(sent) < valid_for)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_secs(14);

    /// Catches: a window counted from the `Start`'s arrival or from anything later than
    /// the named heartbeat's send (a `Start` delayed past the window would run), one
    /// millisecond of slack at the boundary, and the Hello not standing for seq 0 (the
    /// first `Start`s of a stream, before any heartbeat, would all be refused).
    #[test]
    fn a_start_is_allowed_only_within_the_window_of_the_named_send() {
        let t0 = Instant::now();
        let mut w = StartWindow::default();
        w.new_stream(t0);
        assert!(w.allows(0, W, t0 + W - Duration::from_millis(1)));
        assert!(!w.allows(0, W, t0 + W));
        let beat = t0 + Duration::from_secs(5);
        w.sent(1, beat);
        assert!(w.allows(1, W, beat + W - Duration::from_millis(1)));
        assert!(!w.allows(1, W, beat + W));
        // An instant before the send (a clock that is not monotonic) is no lateness.
        assert!(w.allows(1, W, t0));
    }

    /// Catches: a `Start` naming a heartbeat never sent on this stream allowed, and a
    /// zero window (a server that predates the field) treated as a bound, which would
    /// refuse every `Start` of such a server.
    #[test]
    fn an_unknown_heartbeat_is_refused_and_no_window_is_no_bound() {
        let t0 = Instant::now();
        let mut w = StartWindow::default();
        assert!(!w.allows(0, W, t0), "nothing sent yet");
        w.new_stream(t0);
        assert!(!w.allows(7, W, t0));
        assert!(w.allows(7, Duration::ZERO, t0 + 10 * W));
    }

    /// Catches: send times forgotten that a later `Start` may still name (the newest
    /// acknowledged heartbeat itself, or a newer one), older ones kept forever, an
    /// unknown acknowledgement forgetting anything, and a new stream that keeps the
    /// old stream's seqs.
    #[test]
    fn an_acknowledgement_forgets_only_older_sends() {
        let t0 = Instant::now();
        let mut w = StartWindow::default();
        w.new_stream(t0);
        w.sent(1, t0);
        w.sent(2, t0);
        w.sent(3, t0);
        w.acknowledged(9);
        assert!(w.allows(0, W, t0), "an unknown ack forgot the Hello");
        w.acknowledged(2);
        assert!(!w.allows(0, W, t0));
        assert!(!w.allows(1, W, t0));
        assert!(w.allows(2, W, t0));
        assert!(w.allows(3, W, t0));
        let later = t0 + Duration::from_secs(1);
        w.new_stream(later);
        assert!(!w.allows(2, W, later), "the old stream's seq names a send");
        assert!(w.allows(0, W, later));
    }
}
