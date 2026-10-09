//! The control log: commits each appended record after a random delay, and tells the
//! leader its index. A record's index is given when it commits, so a slow record
//! commits after records appended later: the log order the scheduler must follow is
//! not always the order it proposed in. Every `Append` that arrives is committed, so
//! one the bus duplicated commits twice (see the module notes in `mod.rs`).

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_sim::{Chance, Event, NodeInput, Output, SimRng};
use kbf_types::{ControlRecord, Effect, FarmTime};

use super::{Msg, leader_id};

/// How long records take to commit.
#[derive(Clone, Copy, Debug)]
pub struct LogPlan {
    /// The usual commit delay, in ms (`lo..=hi`).
    pub fast: (u64, u64),
    /// The chance a record is slow, and then its delay in ms.
    pub slow: Chance,
    pub slow_ms: (u64, u64),
}

impl LogPlan {
    /// Commits within 20 ms.
    pub const PROMPT: Self = Self {
        fast: (0, 20),
        slow: Chance::never(),
        slow_ms: (0, 0),
    };
}

/// One committed record.
#[derive(Clone, Debug)]
pub struct Entry {
    pub at: FarmTime,
    pub incarnation: u64,
    pub record: ControlRecord,
}

pub struct Log {
    plan: LogPlan,
    /// Records waiting to commit, by timer tag.
    waiting: BTreeMap<u64, (u64, ControlRecord)>,
    next_tag: u64,
    /// The next index of each incarnation's log.
    next_index: BTreeMap<u64, u64>,
    /// Every committed record, in commit order.
    pub entries: Vec<Entry>,
    out: Vec<Output<Msg>>,
}

impl Log {
    #[must_use]
    pub fn new(plan: LogPlan) -> Self {
        Self {
            plan,
            waiting: BTreeMap::new(),
            next_tag: 0,
            next_index: BTreeMap::new(),
            entries: Vec::new(),
            out: Vec::new(),
        }
    }

    pub fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        let now = input.now;
        match input.event {
            Event::Message {
                msg:
                    Msg::Append {
                        incarnation,
                        record,
                    },
                ..
            } => {
                let mut rng = SimRng::from_seed(input.entropy);
                let (lo, hi) = if rng.chance(self.plan.slow) {
                    self.plan.slow_ms
                } else {
                    self.plan.fast
                };
                let tag = self.next_tag;
                self.next_tag += 1;
                self.waiting.insert(tag, (incarnation, record));
                self.out.push(Output::Timer {
                    after: Duration::from_millis(rng.between(lo, hi)),
                    tag,
                });
            }
            Event::Timer { tag } => {
                let (incarnation, record) = self.waiting.remove(&tag).expect("timers name records");
                let next = self.next_index.entry(incarnation).or_default();
                let index = *next;
                *next += 1;
                self.entries.push(Entry {
                    at: now,
                    incarnation,
                    record: record.clone(),
                });
                self.out.push(Output::Send {
                    to: leader_id(),
                    msg: Msg::Committed {
                        incarnation,
                        index,
                        record,
                    },
                });
            }
            Event::Start | Event::Message { .. } => {}
        }
        Vec::new()
    }

    pub fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }
}
