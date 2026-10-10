//! The outbox: events waiting for delivery, in order, and how they are encoded. Pure.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::alert::Event;

/// The outbox encoding this build writes and the only one it reads.
const VERSION: u32 = 1;

/// One event waiting for delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// Unique within one outbox, across restarts; it increases in enqueue order, so a
    /// receiver can drop a repeat (delivery is at least once).
    pub id: u64,
    /// Failed attempts so far.
    pub attempts: u32,
    /// What to deliver.
    pub event: Event,
}

/// An outbox that cannot be decoded.
#[derive(Debug, thiserror::Error)]
pub enum OutboxError {
    /// The bytes are not an outbox.
    #[error("outbox does not parse: {0}")]
    Parse(#[from] serde_json::Error),
    /// The bytes are an outbox of another version.
    #[error("outbox version {0} is not {VERSION}")]
    Version(u32),
    /// A pending id is not below the next id, so a new event could reuse it.
    #[error("outbox id {id} is not below next_id {next_id}")]
    Id {
        /// The pending id.
        id: u64,
        /// The id the next event would get.
        next_id: u64,
    },
}

/// Events waiting for delivery, oldest first.
///
/// Delivery is in order: only the oldest is ever due, and a later event waits behind
/// it however long it fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outbox {
    version: u32,
    next_id: u64,
    pending: VecDeque<Delivery>,
}

impl Outbox {
    /// An empty outbox.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: VERSION,
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    /// Queues `event` behind everything already pending and returns its id.
    pub fn push(&mut self, event: Event) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.push_back(Delivery {
            id,
            attempts: 0,
            event,
        });
        id
    }

    /// The oldest pending delivery, the only one that may be sent.
    #[must_use]
    pub fn head(&self) -> Option<&Delivery> {
        self.pending.front()
    }

    /// The head was delivered: remove it. Another id changes nothing and returns false.
    pub fn delivered(&mut self, id: u64) -> bool {
        if self.head().is_some_and(|d| d.id == id) {
            self.pending.pop_front();
            return true;
        }
        false
    }

    /// The head failed once more: count it and return its failed attempts so far.
    /// Another id changes nothing and returns `None`.
    pub fn failed(&mut self, id: u64) -> Option<u32> {
        let head = self.pending.front_mut().filter(|d| d.id == id)?;
        head.attempts = head.attempts.saturating_add(1);
        Some(head.attempts)
    }

    /// How many events are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The pending deliveries, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &Delivery> {
        self.pending.iter()
    }

    /// The bytes a state file holds.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("an outbox serializes")
    }

    /// An outbox from [`Outbox::encode`]'s bytes.
    ///
    /// # Errors
    /// The bytes do not parse, are another version, or hold a pending id at or above
    /// `next_id` (a new event would reuse it). None is read as empty: that would drop
    /// undelivered alerts without a word.
    pub fn decode(bytes: &[u8]) -> Result<Self, OutboxError> {
        let outbox: Self = serde_json::from_slice(bytes)?;
        if outbox.version != VERSION {
            return Err(OutboxError::Version(outbox.version));
        }
        if let Some(d) = outbox.pending.iter().find(|d| d.id >= outbox.next_id) {
            return Err(OutboxError::Id {
                id: d.id,
                next_id: outbox.next_id,
            });
        }
        Ok(outbox)
    }
}

impl Default for Outbox {
    fn default() -> Self {
        Self::new()
    }
}

/// The wait before the next attempt: doubling from `initial` after each failure,
/// never more than `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the first failure.
    pub initial: Duration,
    /// The longest wait.
    pub max: Duration,
}

impl Backoff {
    /// The wait after `failures` failed attempts in a row (zero before any).
    #[must_use]
    pub fn delay(&self, failures: u32) -> Duration {
        let Some(doublings) = failures.checked_sub(1) else {
            return Duration::ZERO;
        };
        let factor = 1u32 << doublings.min(31);
        self.initial.saturating_mul(factor).min(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{Alert, Severity, Transition, UnixMillis};

    fn event(subject: &str) -> Event {
        Event {
            transition: Transition::Raised,
            alert: Alert::new("r", subject, Severity::Warning, "s", "f").expect("valid"),
            first_seen: UnixMillis(1),
            last_seen: UnixMillis(2),
            resolved_at: None,
        }
    }

    /// Catches: delivery out of order (acknowledging a later event first), an ack or
    /// failure for an id that is not the head changing the queue, and ids reused.
    #[test]
    fn only_the_head_is_delivered_or_failed() {
        let mut outbox = Outbox::default();
        assert!(outbox.is_empty());
        assert_eq!(outbox.head(), None);
        assert_eq!(outbox.failed(1), None, "empty");
        let a = outbox.push(event("a"));
        let b = outbox.push(event("b"));
        assert!(a < b);
        assert_eq!(outbox.len(), 2);
        assert!(!outbox.delivered(b), "not the head");
        assert_eq!(outbox.failed(b), None, "not the head");
        assert_eq!(outbox.failed(a), Some(1));
        assert_eq!(outbox.failed(a), Some(2));
        assert!(outbox.delivered(a));
        let head = outbox.head().expect("b");
        assert_eq!((head.id, head.attempts), (b, 0));
        let ids: Vec<u64> = outbox.iter().map(|d| d.id).collect();
        assert_eq!(ids, [b]);
    }

    /// Catches: an encoding that drops pending events, attempts or the id counter (a
    /// restart would then reuse an id a receiver has already seen), a corrupt or
    /// foreign-version file read as an empty outbox, and a counter at or below a
    /// pending id accepted.
    #[test]
    fn encoding_round_trips_and_refuses_what_it_cannot_read() {
        let mut outbox = Outbox::new();
        let a = outbox.push(event("a"));
        outbox.push(event("b"));
        outbox.failed(a);
        assert!(outbox.delivered(a));
        let mut back = Outbox::decode(&outbox.encode()).expect("decodes");
        assert_eq!(back, outbox);
        assert_eq!(back.push(event("c")), 3, "ids continue after a restart");

        let err = Outbox::decode(b"{").expect_err("truncated");
        assert!(
            err.to_string().starts_with("outbox does not parse"),
            "{err}"
        );
        let other = br#"{"version":2,"next_id":1,"pending":[]}"#;
        let err = Outbox::decode(other).expect_err("version");
        assert_eq!(err.to_string(), "outbox version 2 is not 1");

        // A hand-edited file whose counter is behind a pending id.
        let mut bytes: serde_json::Value = serde_json::from_slice(&outbox.encode()).expect("json");
        bytes["next_id"] = 2.into();
        let bytes = serde_json::to_vec(&bytes).expect("json");
        let err = Outbox::decode(&bytes).expect_err("id reuse");
        assert_eq!(err.to_string(), "outbox id 2 is not below next_id 2");
    }

    /// Catches: no backoff (a constant wait), a wait that does not double, or one that
    /// grows past its cap or overflows after many failures.
    #[test]
    fn backoff_doubles_up_to_its_cap() {
        let backoff = Backoff {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(1),
        };
        let waits: Vec<u128> = (0..6).map(|n| backoff.delay(n).as_millis()).collect();
        assert_eq!(waits, [0, 100, 200, 400, 800, 1000]);
        assert_eq!(backoff.delay(u32::MAX), Duration::from_secs(1));
        let huge = Backoff {
            initial: Duration::MAX,
            max: Duration::MAX,
        };
        assert_eq!(huge.delay(40), Duration::MAX);
    }
}
