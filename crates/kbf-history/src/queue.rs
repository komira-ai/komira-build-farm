//! The bounded queue the request path hands history events to.
//!
//! The frontend offers a run event after its `Result` commits and a hit event after its
//! answer. Offering never waits: past either bound the event is dropped and counted, so
//! history never delays a build; losing it loses records, never results.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use prost::Message;

use crate::keys::{Day, InvKey, WriterId};
use crate::records::{FailedRun, HitRecord, RunRecord};

/// Which invocation an event belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvRef {
    /// Its key.
    pub key: InvKey,
    /// The client's invocation id; `None` for a run whose client sent none, which goes
    /// to the writer's id-less pseudo-invocation of the day.
    pub record_id: Option<String>,
}

impl InvRef {
    /// The invocation the client named `record_id`.
    #[must_use]
    pub fn named(record_id: &str) -> InvRef {
        InvRef {
            key: InvKey::from_record_id(record_id),
            record_id: Some(record_id.to_owned()),
        }
    }

    /// `writer`'s id-less pseudo-invocation for `day`.
    #[must_use]
    pub fn idless(writer: WriterId, day: Day) -> InvRef {
        InvRef {
            key: InvKey::idless(writer, day),
            record_id: None,
        }
    }
}

/// Something the history writer records.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A run whose `Result` committed, with its failed-run section if it failed and the
    /// writer has its tails and command rendering.
    Run {
        /// The invocation.
        inv: InvRef,
        /// The run record, its id minted by the [`Minter`](crate::Minter). Boxed so a
        /// queued hit does not take a run record's room.
        record: Box<RunRecord>,
        /// The failed-run section.
        failed: Option<FailedRun>,
        /// When the run's stream opened, from its [`Minted`](crate::Minted): the
        /// stream row's key, whichever of the stream's runs the writer takes first.
        stream_opened_ms: u64,
    },
    /// A cache hit that was answered.
    Hit {
        /// The invocation.
        inv: InvRef,
        /// The hit record.
        record: HitRecord,
    },
    /// The invocation turned idle: flush it and mark it closed.
    Close {
        /// The invocation.
        inv: InvRef,
    },
}

impl Event {
    /// The bytes the event holds in memory, for the queue's byte bound: its records'
    /// encoded length plus a fixed allowance for the invocation reference.
    #[must_use]
    pub fn size(&self) -> usize {
        const OVERHEAD: usize = 64;
        OVERHEAD
            + match self {
                Event::Run { record, failed, .. } => {
                    record.encoded_len() + failed.as_ref().map_or(0, Message::encoded_len)
                }
                Event::Hit { record, .. } => record.encoded_len(),
                Event::Close { .. } => 0,
            }
    }
}

/// The queue's bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLimits {
    /// Events held at most.
    pub events: usize,
    /// Bytes held at most, by [`Event::size`].
    pub bytes: usize,
}

impl Default for QueueLimits {
    /// 131,072 events or 64 MiB: one 9,000-execution build with its hits (about 90,000
    /// events and 20 MB) fits with the writer stalled.
    fn default() -> QueueLimits {
        QueueLimits {
            events: 131_072,
            bytes: 64 * 1024 * 1024,
        }
    }
}

/// The bounded event queue, shared by the request path (which offers) and the writer
/// (which drains).
#[derive(Debug)]
pub struct EventQueue {
    state: Mutex<(VecDeque<Event>, usize)>,
    limits: QueueLimits,
    dropped: AtomicU64,
}

impl EventQueue {
    /// An empty queue with `limits`.
    #[must_use]
    pub fn new(limits: QueueLimits) -> EventQueue {
        EventQueue {
            state: Mutex::new((VecDeque::new(), 0)),
            limits,
            dropped: AtomicU64::new(0),
        }
    }

    fn state(&self) -> MutexGuard<'_, (VecDeque<Event>, usize)> {
        // A queue and a byte count; a panic elsewhere leaves both whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `event`, or drops and counts it when either bound would be passed.
    /// Returns whether it was queued. Never waits for the writer.
    pub fn offer(&self, event: Event) -> bool {
        let size = event.size();
        let mut st = self.state();
        if st.0.len() >= self.limits.events || st.1 + size > self.limits.bytes {
            drop(st);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        st.0.push_back(event);
        st.1 += size;
        true
    }

    /// Takes every queued event, oldest first.
    pub fn drain(&self) -> Vec<Event> {
        let mut st = self.state();
        st.1 = 0;
        st.0.drain(..).collect()
    }

    /// Events queued now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state().0.len()
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes queued now, as [`Event::size`] counts them.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.state().1
    }

    /// Events dropped because the queue was full (`stage="queue"`).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit() -> Event {
        Event::Hit {
            inv: InvRef::named("b"),
            record: HitRecord {
                instance: "main".into(),
                ..HitRecord::default()
            },
        }
    }

    #[test]
    fn a_full_queue_drops_and_counts_instead_of_waiting() {
        // Catches: an unbounded queue (a stalled writer would take the frontend's
        // memory), a bound that blocks the caller, or a drop that is not counted.
        let q = EventQueue::new(QueueLimits {
            events: 2,
            bytes: 1 << 20,
        });
        assert!(q.is_empty());
        assert!(q.offer(hit()));
        assert!(q.offer(hit()));
        assert!(!q.offer(hit()));
        assert_eq!((q.len(), q.dropped()), (2, 1));
        assert_eq!(
            q.bytes(),
            2 * hit().size(),
            "a dropped event holds no bytes"
        );
        assert_eq!(q.drain().len(), 2);
        assert_eq!(q.bytes(), 0);
        assert!(q.offer(hit()), "draining frees the room");
        let small = EventQueue::new(QueueLimits {
            events: 100,
            bytes: hit().size() + 1,
        });
        assert!(small.offer(hit()));
        assert!(!small.offer(hit()), "the byte bound holds too");
        small.drain();
        assert!(small.offer(hit()), "draining resets the byte count");
    }

    #[test]
    fn event_sizes_count_their_records() {
        // Catches: a size that ignores the failed-run section (32 KiB of tails per
        // failed run would escape the byte bound).
        let run = |failed| Event::Run {
            inv: InvRef::idless(WriterId(1), Day::from_number(20_261_016)),
            record: Box::default(),
            failed,
            stream_opened_ms: 0,
        };
        let section = FailedRun {
            instance: "x".repeat(1000),
            ..FailedRun::default()
        };
        assert!(run(Some(section)).size() > run(None).size() + 1000);
        assert!(hit().size() > 64);
        let close = Event::Close {
            inv: InvRef::named("b"),
        };
        assert_eq!(close.size(), 64);
        assert_eq!(QueueLimits::default().events, 131_072);
        assert_eq!(
            InvRef::named("b").key,
            InvKey::from_record_id("b"),
            "a named invocation is keyed by its id"
        );
    }
}
